//! Counterparty light client and commitment proof verification.
//!
//! Scope: a minimal, honest light client for a *single* ed25519 signer. The
//! genesis header is trusted out of band, every later header must be signed by
//! the same key, and height and time must strictly increase. Packet delivery can
//! then be tied to a Merkle commitment root rather than to the relayer's word.
//!
//! What this deliberately does not do: validator-set rotation, multi-signature
//! or Tendermint-style quorum certificates, non-membership proofs, fork choice,
//! or skipping headers. It is a trust-minimized bridge primitive for a single
//! sequencer, not a general IBC client.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use subhost_core::merkle::{key_value_leaf, merkle_proof, merkle_root, MerkleProof};
use subhost_core::{encode_canonical, BlockHeight, Hash, Timestamp, PUBLIC_KEY_SIZE};

use crate::{IbcError, IbcPacket};

/// Domain tag so a header signature can never be replayed as a transaction or a
/// vote signature.
const HEADER_DOMAIN: &[u8] = b"subhost-ibc-header-v1";

/// A counterparty header the light client can trust once verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedHeader {
    pub chain_id: String,
    pub height: BlockHeight,
    pub timestamp: Timestamp,
    /// Commitment root the counterparty published for this height.
    pub app_hash: Hash,
    pub public_key: [u8; PUBLIC_KEY_SIZE],
    pub signature: HeaderSignature,
}

/// A raw ed25519 signature split into two 32-byte halves, because serde only
/// implements arrays up to 32 elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderSignature {
    pub r: [u8; 32],
    pub s: [u8; 32],
}

impl HeaderSignature {
    /// The cleared signature used while building the signing payload.
    pub const EMPTY: Self = Self { r: [0u8; 32], s: [0u8; 32] };

    pub fn from_ed25519(signature: &[u8; 64]) -> Self {
        let mut r = [0u8; 32];
        let mut s = [0u8; 32];
        r.copy_from_slice(&signature[..32]);
        s.copy_from_slice(&signature[32..]);
        Self { r, s }
    }

    pub fn to_ed25519(self) -> [u8; 64] {
        let mut bytes = [0u8; 64];
        bytes[..32].copy_from_slice(&self.r);
        bytes[32..].copy_from_slice(&self.s);
        bytes
    }
}

impl SignedHeader {
    /// The exact bytes the signer must have signed.
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(HEADER_DOMAIN.len() + 96);
        payload.extend_from_slice(HEADER_DOMAIN);
        payload.extend_from_slice(&encode_canonical(&(
            &self.chain_id,
            self.height,
            self.timestamp,
            self.app_hash,
        )));
        payload
    }

    /// Verify the ed25519 signature over the header fields.
    pub fn verify_signature(&self) -> Result<(), IbcError> {
        let key = VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| IbcError::InvalidHeaderSignature)?;
        key.verify(&self.signing_payload(), &Signature::from_bytes(&self.signature.to_ed25519()))
            .map_err(|_| IbcError::InvalidHeaderSignature)
    }
}

/// A Merkle membership proof for one `key -> value` pair at one commitment root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentProof {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub proof: MerkleProof,
}

impl CommitmentProof {
    /// Verify the value is committed under `root` at this key.
    pub fn verify(&self, root: &Hash) -> bool {
        self.proof.leaf == key_value_leaf(&self.key, &self.value) && self.proof.verify(root)
    }
}

/// A store that produces a deterministic commitment root and per-key proofs.
///
/// Keys are kept in byte order, so the root and every proof index are stable
/// across processes and insertion orders.
#[derive(Debug, Clone, Default)]
pub struct CommitmentStore {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl CommitmentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.entries.insert(key, value);
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries.get(key).map(Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn leaves(&self) -> Vec<Hash> {
        self.entries.iter().map(|(key, value)| key_value_leaf(key, value)).collect()
    }

    /// The root a counterparty header must publish for this store.
    pub fn app_hash(&self) -> Hash {
        merkle_root(&self.leaves())
    }

    /// Build an inclusion proof for `key`.
    pub fn proof(&self, key: &[u8]) -> Option<CommitmentProof> {
        let index = self.entries.keys().position(|candidate| candidate.as_slice() == key)?;
        let (stored_key, value) = self.entries.iter().nth(index)?;
        let proof = merkle_proof(&self.leaves(), index)?;
        Some(CommitmentProof { key: stored_key.clone(), value: value.clone(), proof })
    }
}

/// Tracks the newest verified header for one counterparty chain.
#[derive(Debug, Clone)]
pub struct LightClient {
    chain_id: String,
    trusted_key: [u8; PUBLIC_KEY_SIZE],
    height: BlockHeight,
    timestamp: Timestamp,
    app_hash: Hash,
}

impl LightClient {
    /// Trust `genesis` out of band and adopt its key as the only signer.
    pub fn new(genesis: &SignedHeader) -> Result<Self, IbcError> {
        genesis.verify_signature()?;
        Ok(Self {
            chain_id: genesis.chain_id.clone(),
            trusted_key: genesis.public_key,
            height: genesis.height,
            timestamp: genesis.timestamp,
            app_hash: genesis.app_hash,
        })
    }

    pub fn chain_id(&self) -> &str {
        &self.chain_id
    }

    pub fn trusted_key(&self) -> &[u8; PUBLIC_KEY_SIZE] {
        &self.trusted_key
    }

    pub fn height(&self) -> BlockHeight {
        self.height
    }

    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub fn app_hash(&self) -> Hash {
        self.app_hash
    }

    /// Advance to a newer header.
    ///
    /// The header must be for the same chain, be signed by the trusted key, and
    /// strictly increase both height and timestamp. Anything else is refused
    /// before the trusted state moves.
    pub fn update(&mut self, header: &SignedHeader) -> Result<(), IbcError> {
        if header.chain_id != self.chain_id {
            return Err(IbcError::UntrustedHeader(format!(
                "header is for chain {} but this client tracks {}",
                header.chain_id, self.chain_id
            )));
        }
        if header.public_key != self.trusted_key {
            return Err(IbcError::UntrustedHeader(
                "header is not signed by the client's trusted key".into(),
            ));
        }
        header.verify_signature()?;
        if header.height <= self.height {
            return Err(IbcError::UntrustedHeader(format!(
                "header height {} does not advance past trusted height {}",
                header.height, self.height
            )));
        }
        if header.timestamp <= self.timestamp {
            return Err(IbcError::UntrustedHeader(format!(
                "header timestamp {} does not advance past trusted timestamp {}",
                header.timestamp, self.timestamp
            )));
        }

        self.height = header.height;
        self.timestamp = header.timestamp;
        self.app_hash = header.app_hash;
        Ok(())
    }

    /// Verify a commitment proof against the newest trusted commitment root.
    pub fn verify_commitment(&self, proof: &CommitmentProof) -> Result<(), IbcError> {
        if !proof.verify(&self.app_hash) {
            return Err(IbcError::InvalidCommitment(
                "proof does not commit this key/value under the trusted root".into(),
            ));
        }
        Ok(())
    }
}

/// Commitment key for one packet on its sending chain.
pub fn packet_commitment_key(source_port: &str, source_channel: &str, sequence: u64) -> Vec<u8> {
    format!("packet-commitment/{source_port}/{source_channel}/{sequence}").into_bytes()
}

/// The committed value is the exact packet payload bytes.
pub fn packet_commitment_value(packet: &IbcPacket) -> Vec<u8> {
    packet.data.clone()
}
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const CHAIN: &str = "counterparty-1";

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn header(
        key: &SigningKey,
        chain_id: &str,
        height: BlockHeight,
        timestamp: Timestamp,
        app_hash: Hash,
    ) -> SignedHeader {
        let mut header = SignedHeader {
            chain_id: chain_id.to_string(),
            height,
            timestamp,
            app_hash,
            public_key: key.verifying_key().to_bytes(),
            signature: HeaderSignature::EMPTY,
        };
        header.signature =
            HeaderSignature::from_ed25519(&key.sign(&header.signing_payload()).to_bytes());
        header
    }

    fn store() -> CommitmentStore {
        let mut store = CommitmentStore::new();
        store.insert(b"a".to_vec(), b"1".to_vec());
        store.insert(b"b".to_vec(), b"2".to_vec());
        store.insert(b"c".to_vec(), b"3".to_vec());
        store
    }

    #[test]
    fn a_valid_header_advances_the_client() {
        let key = signing_key(7);
        let genesis = header(&key, CHAIN, 10, 1_000, Hash::from_data(b"genesis"));
        let mut client = LightClient::new(&genesis).unwrap();
        assert_eq!(client.height(), 10);
        assert_eq!(client.app_hash(), genesis.app_hash);

        let next = header(&key, CHAIN, 11, 1_001, Hash::from_data(b"next"));
        client.update(&next).unwrap();
        assert_eq!(client.height(), 11);
        assert_eq!(client.timestamp(), 1_001);
        assert_eq!(client.app_hash(), next.app_hash);
    }

    #[test]
    fn a_genesis_header_with_a_bad_signature_is_refused() {
        let key = signing_key(7);
        let mut genesis = header(&key, CHAIN, 1, 1_000, Hash::from_data(b"genesis"));
        genesis.signature.r[0] ^= 0xff;
        assert!(matches!(LightClient::new(&genesis), Err(IbcError::InvalidHeaderSignature)));

        // A header field changed after signing also fails.
        let mut renamed = header(&key, CHAIN, 1, 1_000, Hash::from_data(b"genesis"));
        renamed.app_hash = Hash::from_data(b"other");
        assert!(matches!(LightClient::new(&renamed), Err(IbcError::InvalidHeaderSignature)));
    }

    #[test]
    fn non_monotonic_height_or_time_is_refused() {
        let key = signing_key(7);
        let genesis = header(&key, CHAIN, 10, 1_000, Hash::from_data(b"genesis"));
        let mut client = LightClient::new(&genesis).unwrap();

        let same_height = header(&key, CHAIN, 10, 2_000, Hash::from_data(b"a"));
        assert!(matches!(client.update(&same_height), Err(IbcError::UntrustedHeader(_))));

        let older_height = header(&key, CHAIN, 9, 2_000, Hash::from_data(b"b"));
        assert!(matches!(client.update(&older_height), Err(IbcError::UntrustedHeader(_))));

        let stale_time = header(&key, CHAIN, 11, 1_000, Hash::from_data(b"c"));
        assert!(matches!(client.update(&stale_time), Err(IbcError::UntrustedHeader(_))));

        // Nothing was adopted from the rejected headers.
        assert_eq!(client.height(), 10);
        assert_eq!(client.timestamp(), 1_000);
        assert_eq!(client.app_hash(), genesis.app_hash);
    }

    #[test]
    fn a_header_from_another_key_or_chain_is_refused() {
        let key = signing_key(7);
        let genesis = header(&key, CHAIN, 1, 1_000, Hash::from_data(b"genesis"));
        let mut client = LightClient::new(&genesis).unwrap();

        // Correctly signed, but for a different chain.
        let other_chain = header(&key, "attacker-1", 2, 1_001, Hash::from_data(b"x"));
        assert!(matches!(client.update(&other_chain), Err(IbcError::UntrustedHeader(_))));

        // Same chain and shape, signed by a key the client never trusted.
        let impostor = signing_key(9);
        let forged = header(&impostor, CHAIN, 2, 1_001, Hash::from_data(b"y"));
        assert!(matches!(client.update(&forged), Err(IbcError::UntrustedHeader(_))));

        assert_eq!(client.height(), 1);
    }

    #[test]
    fn commitment_proofs_verify_and_tampering_is_caught() {
        let key = signing_key(7);
        let committed = store();
        let genesis = header(&key, CHAIN, 1, 1_000, committed.app_hash());
        let client = LightClient::new(&genesis).unwrap();

        let proof = committed.proof(b"b").expect("b is committed");
        assert_eq!(proof.key, b"b".to_vec());
        assert_eq!(proof.value, b"2".to_vec());
        client.verify_commitment(&proof).unwrap();

        // A value the counterparty never committed.
        let mut wrong_value = proof.clone();
        wrong_value.value = b"999".to_vec();
        assert!(matches!(
            client.verify_commitment(&wrong_value),
            Err(IbcError::InvalidCommitment(_))
        ));

        // The right value claimed under another key.
        let mut wrong_key = proof.clone();
        wrong_key.key = b"c".to_vec();
        assert!(matches!(
            client.verify_commitment(&wrong_key),
            Err(IbcError::InvalidCommitment(_))
        ));

        // A valid proof from a different tree.
        let other_header =
            header(&signing_key(8), CHAIN, 1, 1_000, Hash::from_data(b"another tree"));
        let other_client = LightClient::new(&other_header).unwrap();
        assert!(!proof.verify(&other_client.app_hash()));

        // The same digest at a different index folds to a different root.
        let mut alien = proof.clone();
        alien.proof.index ^= 1;
        assert!(!alien.verify(&client.app_hash()));

        // An unknown key has no proof at all.
        assert!(committed.proof(b"missing").is_none());
    }

    #[test]
    fn the_root_and_proofs_do_not_depend_on_insertion_order() {
        let mut reversed = CommitmentStore::new();
        reversed.insert(b"c".to_vec(), b"3".to_vec());
        reversed.insert(b"b".to_vec(), b"2".to_vec());
        reversed.insert(b"a".to_vec(), b"1".to_vec());

        assert_eq!(reversed.app_hash(), store().app_hash());
        assert_eq!(reversed.proof(b"a"), store().proof(b"a"));
    }

    #[test]
    fn packet_commitment_keys_are_unique_per_channel_and_sequence() {
        assert_ne!(
            packet_commitment_key("transfer", "channel-0", 1),
            packet_commitment_key("transfer", "channel-0", 2)
        );
        assert_ne!(
            packet_commitment_key("transfer", "channel-0", 1),
            packet_commitment_key("transfer", "channel-1", 1)
        );
        assert_eq!(
            packet_commitment_key("transfer", "channel-0", 1),
            b"packet-commitment/transfer/channel-0/1".to_vec()
        );
    }
}
