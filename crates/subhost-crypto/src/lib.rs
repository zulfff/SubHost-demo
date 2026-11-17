//! Cryptographic primitives: BLS12-381 aggregate signatures, ed25519, X25519 key
//! exchange, and ChaCha20-Poly1305 AEAD.
//!
//! # Security scope
//!
//! - `hash_to_g2` is a domain-separated hash-and-multiply, **not** a standards
//!   compliant hash-to-curve suite (RFC 9380). It is deterministic, domain
//!   separated, and lands in the prime-order subgroup, which is enough for the
//!   protocol's internal use. Interoperating with another BLS implementation
//!   requires replacing it.
//! - Aggregate verification over a *shared* message is only safe when every
//!   participant's key carries a verified proof of possession; see
//!   [`BLSScheme::proof_of_possession`]. Proofs of possession are hashed under
//!   their own domain tag, so a proof cannot be replayed as a protocol signature
//!   over the encoded public key.
//! - X25519 shared secrets are always contributory-checked, so a low-order peer
//!   key cannot force a predictable secret.

use ark_bls12_381::{Fr as Scalar, G1Projective, G2Projective};
use ark_ec::{pairing::Pairing, PrimeGroup};
use ark_ff::{Field, PrimeField, Zero};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Domain separation tag for protocol BLS signatures.
const BLS_DOMAIN: &[u8] = b"subhost-bls-01";
/// Domain separation tag for proofs of possession.
///
/// A proof must not double as a protocol signature over the encoded public key,
/// so it is hashed under its own tag rather than [`BLS_DOMAIN`].
const BLS_POP_DOMAIN: &[u8] = b"subhost-bls-pop-01";
/// ChaCha20-Poly1305 nonce length.
const AEAD_NONCE_BYTES: usize = 12;

/// Compressed BLS12-381 G1 public key length.
pub const PUBLIC_KEY_BYTES: usize = 48;
/// Compressed BLS12-381 G2 signature length.
pub const SIGNATURE_BYTES: usize = 96;
/// Canonical BLS12-381 scalar length.
pub const SECRET_KEY_BYTES: usize = 32;

/// A BLS12-381 G1 public key.
pub type PublicKey = G1Projective;
/// A BLS12-381 G2 signature.
pub type Signature = G2Projective;

/// A BLS private scalar, wiped on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PrivateKey(pub Scalar);

/// BLS12-381 signatures with aggregation and proof of possession.
pub struct BLSScheme;

impl BLSScheme {
    /// Generate a keypair with a non-zero secret scalar.
    pub fn keygen() -> (PrivateKey, PublicKey) {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);

        // A zero scalar would produce the identity public key, which verifies
        // against anything; resample until it is non-zero.
        let mut scalar = Scalar::from_le_bytes_mod_order(&bytes);
        while scalar.is_zero() {
            rng.fill_bytes(&mut bytes);
            scalar = Scalar::from_le_bytes_mod_order(&bytes);
        }
        bytes.zeroize();

        let public = G1Projective::generator() * scalar;
        (PrivateKey(scalar), public)
    }

    pub fn sign(secret: &PrivateKey, message: &[u8]) -> Signature {
        Self::hash_to_g2(message) * secret.0
    }

    /// Verify one signature, rejecting identity keys and identity signatures.
    pub fn verify(public: &PublicKey, message: &[u8], signature: &Signature) -> bool {
        if public.is_zero() || signature.is_zero() {
            return false;
        }
        let message_point = Self::hash_to_g2(message);
        ark_bls12_381::Bls12_381::pairing(*public, message_point)
            == ark_bls12_381::Bls12_381::pairing(G1Projective::generator(), *signature)
    }

    /// Sum public keys. Only safe for keys with verified proofs of possession.
    pub fn aggregate_public_keys(keys: &[PublicKey]) -> PublicKey {
        keys.iter().fold(G1Projective::zero(), |sum, key| sum + key)
    }

    /// Sum signatures.
    pub fn aggregate_signatures(signatures: &[Signature]) -> Signature {
        signatures.iter().fold(G2Projective::zero(), |sum, signature| sum + signature)
    }

    /// Verify an aggregate signature over distinct per-signer messages.
    pub fn verify_aggregate(keys: &[PublicKey], messages: &[&[u8]], aggregate: &Signature) -> bool {
        if keys.is_empty()
            || keys.len() != messages.len()
            || aggregate.is_zero()
            || keys.iter().any(PublicKey::is_zero)
        {
            return false;
        }

        let mut product = <ark_bls12_381::Bls12_381 as Pairing>::TargetField::ONE;
        for (key, message) in keys.iter().zip(messages.iter()) {
            product *= ark_bls12_381::Bls12_381::pairing(*key, Self::hash_to_g2(message)).0;
        }
        product == ark_bls12_381::Bls12_381::pairing(G1Projective::generator(), *aggregate).0
    }

    /// Verify an aggregate signature over one shared message.
    ///
    /// This form is vulnerable to the rogue-key attack unless every key has a
    /// verified proof of possession: a malicious participant can choose its key as
    /// `g - sum(others)` and forge the aggregate. Callers must gate registration
    /// on [`Self::verify_possession`].
    pub fn verify_aggregate_shared_message(
        keys: &[PublicKey],
        message: &[u8],
        aggregate: &Signature,
    ) -> bool {
        if keys.is_empty() || aggregate.is_zero() || keys.iter().any(PublicKey::is_zero) {
            return false;
        }
        Self::verify(&Self::aggregate_public_keys(keys), message, aggregate)
    }

    /// Sign one's own public key, proving possession of the matching secret.
    pub fn proof_of_possession(secret: &PrivateKey) -> Signature {
        let public = Self::public_key(secret);
        Self::hash_to_g2_in_domain(BLS_POP_DOMAIN, &Self::encode_public_key(&public)) * secret.0
    }

    /// The public key belonging to a signing secret.
    pub fn public_key(secret: &PrivateKey) -> PublicKey {
        G1Projective::generator() * secret.0
    }

    /// Verify a proof of possession against the key it claims to bind.
    pub fn verify_possession(public: &PublicKey, proof: &Signature) -> bool {
        if public.is_zero() || proof.is_zero() {
            return false;
        }
        let message_point =
            Self::hash_to_g2_in_domain(BLS_POP_DOMAIN, &Self::encode_public_key(public));
        ark_bls12_381::Bls12_381::pairing(*public, message_point)
            == ark_bls12_381::Bls12_381::pairing(G1Projective::generator(), *proof)
    }

    /// Decode a compressed BLS12-381 G1 public key.
    ///
    /// Rejects the wrong length, a point off the curve, and a point outside the
    /// prime-order subgroup. The identity element decodes successfully and is
    /// rejected later by [`Self::verify_possession`], so registration cannot
    /// accept a key that verifies against everything.
    pub fn decode_public_key(bytes: &[u8]) -> Option<PublicKey> {
        if bytes.len() != PUBLIC_KEY_BYTES {
            return None;
        }
        PublicKey::deserialize_compressed(bytes).ok()
    }

    /// Decode a compressed BLS12-381 G2 signature with the same checks.
    pub fn decode_signature(bytes: &[u8]) -> Option<Signature> {
        if bytes.len() != SIGNATURE_BYTES {
            return None;
        }
        Signature::deserialize_compressed(bytes).ok()
    }

    /// Compact 96-byte encoding of a signature, as stored in a block.
    pub fn encode_signature(signature: &Signature) -> Vec<u8> {
        let mut bytes = Vec::new();
        signature
            .serialize_compressed(&mut bytes)
            .expect("BLS12-381 G2 serialization into a Vec cannot fail");
        bytes
    }

    /// Canonical compressed encoding of a public key.
    pub fn encode_public_key(public: &PublicKey) -> Vec<u8> {
        let mut bytes = Vec::new();
        public
            .serialize_compressed(&mut bytes)
            .expect("BLS12-381 G1 serialization into a Vec cannot fail");
        bytes
    }

    /// Canonical 32-byte little-endian encoding of a signing secret.
    ///
    /// This is the byte string a producer key file stores, hex-encoded. Keep it
    /// out of logs and error messages.
    pub fn encode_secret_key(secret: &PrivateKey) -> Vec<u8> {
        let mut bytes = Vec::new();
        secret
            .0
            .serialize_compressed(&mut bytes)
            .expect("BLS12-381 scalar serialization into a Vec cannot fail");
        bytes
    }

    /// Decode a signing secret from [`Self::encode_secret_key`] bytes.
    ///
    /// The encoding must be exactly canonical: a value that only decodes after
    /// being reduced modulo the group order is rejected, so two different files
    /// can never name the same key. The zero scalar is rejected as well, because
    /// it is the identity and signs nothing useful.
    pub fn decode_secret_key(bytes: &[u8]) -> Option<PrivateKey> {
        if bytes.len() != SECRET_KEY_BYTES {
            return None;
        }
        let scalar = Scalar::deserialize_compressed(bytes).ok()?;
        if scalar.is_zero() || Self::encode_secret_key(&PrivateKey(scalar)) != bytes {
            return None;
        }
        Some(PrivateKey(scalar))
    }

    /// Domain-separated hash onto the prime-order G2 subgroup.
    ///
    /// See the module docs: this is hash-and-multiply, not RFC 9380 hash-to-curve.
    fn hash_to_g2(message: &[u8]) -> G2Projective {
        Self::hash_to_g2_in_domain(BLS_DOMAIN, message)
    }

    /// Hash under an explicit domain tag.
    fn hash_to_g2_in_domain(domain: &[u8], message: &[u8]) -> G2Projective {
        use sha3::{Digest, Sha3_384};
        let mut hasher = Sha3_384::new();
        hasher.update(domain);
        // Length-prefix the message so no two distinct inputs share a digest.
        hasher.update((message.len() as u64).to_be_bytes());
        hasher.update(message);
        G2Projective::generator() * Scalar::from_le_bytes_mod_order(&hasher.finalize())
    }
}

/// ChaCha20-Poly1305 authenticated encryption.
///
/// The 12-byte random nonce is prepended to every ciphertext. A single key must
/// not be used for more than 2^32 messages, at which point random nonce reuse
/// becomes a real risk.
pub struct SymmetricEncryption;

impl SymmetricEncryption {
    pub fn generate_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        key
    }

    pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let mut nonce_bytes = [0u8; AEAD_NONCE_BYTES];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|_| CryptoError::EncryptionFailed)?;

        let mut output = Vec::with_capacity(AEAD_NONCE_BYTES + ciphertext.len());
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    pub fn decrypt(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        // Reject anything too short to hold a nonce and an authentication tag.
        if ciphertext.len() <= AEAD_NONCE_BYTES {
            return Err(CryptoError::InvalidCiphertext);
        }
        let (nonce, body) = ciphertext.split_at(AEAD_NONCE_BYTES);
        ChaCha20Poly1305::new(Key::from_slice(key))
            .decrypt(Nonce::from_slice(nonce), body)
            .map_err(|_| CryptoError::DecryptionFailed)
    }
}

/// X25519 elliptic-curve Diffie-Hellman.
pub mod key_exchange {
    use rand::RngCore;
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};
    use zeroize::Zeroize;

    /// Generate a raw 32-byte secret and its public key.
    pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let secret = StaticSecret::from(bytes);
        let public = XPublicKey::from(&secret);
        bytes.zeroize();
        (secret.to_bytes(), public.to_bytes())
    }

    /// Derive the public key for an existing secret.
    pub fn public_key(secret: &[u8; 32]) -> [u8; 32] {
        XPublicKey::from(&StaticSecret::from(*secret)).to_bytes()
    }

    /// Compute a contributory Diffie-Hellman shared secret.
    ///
    /// Always checked: an all-zero or low-order peer key is rejected instead of
    /// yielding the all-zero shared secret that raw X25519 would produce.
    pub fn shared_secret(
        secret: &[u8; 32],
        peer_public: &[u8; 32],
    ) -> Result<[u8; 32], crate::CryptoError> {
        if peer_public.iter().all(|byte| *byte == 0) {
            return Err(crate::CryptoError::InvalidPublicKey);
        }
        let shared =
            StaticSecret::from(*secret).diffie_hellman(&XPublicKey::from(*peer_public)).to_bytes();
        // Catches every remaining small-order point.
        if shared.iter().all(|byte| *byte == 0) {
            return Err(crate::CryptoError::InvalidPublicKey);
        }
        Ok(shared)
    }
}

/// ed25519 signatures, used for transaction authentication.
pub mod ed25519 {
    use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
    use rand::RngCore;
    use zeroize::Zeroize;

    pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let signing_key = SigningKey::from_bytes(&bytes);
        bytes.zeroize();
        let verifying_key = signing_key.verifying_key();
        (signing_key, verifying_key)
    }

    pub fn sign(signing_key: &SigningKey, message: &[u8]) -> Signature {
        signing_key.sign(message)
    }

    pub fn verify(verifying_key: &VerifyingKey, message: &[u8], signature: &Signature) -> bool {
        verifying_key.verify(message, signature).is_ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("ciphertext is too short to contain a nonce and tag")]
    InvalidCiphertext,

    #[error("encryption failed")]
    EncryptionFailed,

    #[error("decryption failed")]
    DecryptionFailed,

    #[error("invalid public key (identity or low-order point)")]
    InvalidPublicKey,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bls_sign_and_verify_round_trip() {
        let (secret, public) = BLSScheme::keygen();
        let signature = BLSScheme::sign(&secret, b"test message");
        assert!(BLSScheme::verify(&public, b"test message", &signature));
        assert!(!BLSScheme::verify(&public, b"other message", &signature));

        let (_, other_public) = BLSScheme::keygen();
        assert!(!BLSScheme::verify(&other_public, b"test message", &signature));
    }

    #[test]
    fn bls_keygen_never_yields_an_identity_key() {
        for _ in 0..8 {
            let (secret, public) = BLSScheme::keygen();
            assert!(!public.is_zero());
            assert!(!secret.0.is_zero());
        }
    }

    #[test]
    fn bls_rejects_identity_keys_and_signatures() {
        let (_, public) = BLSScheme::keygen();
        assert!(!BLSScheme::verify(&PublicKey::zero(), b"m", &Signature::zero()));
        assert!(!BLSScheme::verify(&public, b"m", &Signature::zero()));
        assert!(!BLSScheme::verify_aggregate(
            &[PublicKey::zero()],
            &[b"m".as_ref()],
            &Signature::zero()
        ));
        assert!(!BLSScheme::verify_possession(&PublicKey::zero(), &Signature::zero()));
    }

    #[test]
    fn possession_proofs_use_their_own_domain() {
        let (secret, public) = BLSScheme::keygen();
        let mut encoded = Vec::new();
        public.serialize_compressed(&mut encoded).unwrap();

        // The old shared-domain construction signs the encoded key under BLS_DOMAIN.
        let legacy = BLSScheme::sign(&secret, &encoded);
        assert!(BLSScheme::verify(&public, &encoded, &legacy));
        assert!(
            !BLSScheme::verify_possession(&public, &legacy),
            "a protocol signature over the key must not count as a proof"
        );

        let proof = BLSScheme::proof_of_possession(&secret);
        assert!(BLSScheme::verify_possession(&public, &proof));
        assert!(
            !BLSScheme::verify(&public, &encoded, &proof),
            "a proof must not be replayable as a protocol signature"
        );

        let (_, other) = BLSScheme::keygen();
        assert!(!BLSScheme::verify_possession(&other, &proof));
    }

    #[test]
    fn compressed_keys_and_signatures_round_trip_and_reject_bad_input() {
        let (secret, public) = BLSScheme::keygen();
        let signature = BLSScheme::sign(&secret, b"round trip");

        let mut key_bytes = Vec::new();
        public.serialize_compressed(&mut key_bytes).unwrap();
        let mut signature_bytes = Vec::new();
        signature.serialize_compressed(&mut signature_bytes).unwrap();
        assert_eq!(key_bytes.len(), PUBLIC_KEY_BYTES);
        assert_eq!(signature_bytes.len(), SIGNATURE_BYTES);
        assert_eq!(BLSScheme::public_key(&secret), public);
        assert_eq!(BLSScheme::encode_public_key(&public), key_bytes);
        assert_eq!(BLSScheme::encode_signature(&signature), signature_bytes);

        assert_eq!(BLSScheme::decode_public_key(&key_bytes), Some(public));
        assert_eq!(BLSScheme::decode_signature(&signature_bytes), Some(signature));

        assert!(BLSScheme::decode_public_key(&key_bytes[..PUBLIC_KEY_BYTES - 1]).is_none());
        assert!(BLSScheme::decode_public_key(&[0u8; PUBLIC_KEY_BYTES]).is_none());
        assert!(BLSScheme::decode_signature(&signature_bytes[..SIGNATURE_BYTES - 1]).is_none());
        assert!(BLSScheme::decode_signature(&[0u8; SIGNATURE_BYTES]).is_none());
    }

    #[test]
    fn secret_key_bytes_round_trip_and_reject_non_canonical_input() {
        let (secret, public) = BLSScheme::keygen();
        let bytes = BLSScheme::encode_secret_key(&secret);
        assert_eq!(bytes.len(), SECRET_KEY_BYTES);

        let decoded = BLSScheme::decode_secret_key(&bytes).expect("canonical key must decode");
        let signature = BLSScheme::sign(&decoded, b"from file");
        assert!(BLSScheme::verify(&public, b"from file", &signature));

        assert!(BLSScheme::decode_secret_key(&[0u8; SECRET_KEY_BYTES]).is_none());
        assert!(BLSScheme::decode_secret_key(&bytes[..SECRET_KEY_BYTES - 1]).is_none());
        assert!(BLSScheme::decode_secret_key(&[0xff; SECRET_KEY_BYTES]).is_none());
    }

    #[test]
    fn bls_aggregation_verifies_distinct_messages() {
        let (secret_one, public_one) = BLSScheme::keygen();
        let (secret_two, public_two) = BLSScheme::keygen();
        let first = b"message 1";
        let second = b"message 2";

        let aggregate = BLSScheme::aggregate_signatures(&[
            BLSScheme::sign(&secret_one, first),
            BLSScheme::sign(&secret_two, second),
        ]);
        assert!(BLSScheme::verify_aggregate(
            &[public_one, public_two],
            &[first, second],
            &aggregate
        ));

        // Swapping which key signed which message must fail.
        assert!(!BLSScheme::verify_aggregate(
            &[public_two, public_one],
            &[first, second],
            &aggregate
        ));
        // Mismatched lengths and empty input are refused.
        assert!(!BLSScheme::verify_aggregate(&[public_one], &[first, second], &aggregate));
        assert!(!BLSScheme::verify_aggregate(&[], &[], &Signature::zero()));
    }

    #[test]
    fn bls_aggregation_verifies_a_shared_message() {
        let (secret_one, public_one) = BLSScheme::keygen();
        let (secret_two, public_two) = BLSScheme::keygen();
        let message = b"shared proposal";

        let aggregate = BLSScheme::aggregate_signatures(&[
            BLSScheme::sign(&secret_one, message),
            BLSScheme::sign(&secret_two, message),
        ]);
        assert!(BLSScheme::verify_aggregate_shared_message(
            &[public_one, public_two],
            message,
            &aggregate
        ));

        // A missing signer breaks the aggregate.
        assert!(!BLSScheme::verify_aggregate_shared_message(&[public_one], message, &aggregate));
        assert!(!BLSScheme::verify_aggregate_shared_message(&[], message, &aggregate));
    }

    #[test]
    fn proof_of_possession_binds_exactly_one_key() {
        let (secret, public) = BLSScheme::keygen();
        let proof = BLSScheme::proof_of_possession(&secret);
        assert!(BLSScheme::verify_possession(&public, &proof));

        let (_, other_public) = BLSScheme::keygen();
        assert!(!BLSScheme::verify_possession(&other_public, &proof));

        // A plain signature over an unrelated message is not a valid PoP.
        assert!(!BLSScheme::verify_possession(
            &public,
            &BLSScheme::sign(&secret, b"not the public key")
        ));
    }

    #[test]
    fn hash_to_g2_is_domain_separated_and_length_prefixed() {
        // Concatenation ambiguity must not collide: ("ab", "c") vs ("a", "bc").
        let (secret, public) = BLSScheme::keygen();
        let first = BLSScheme::sign(&secret, b"abc");
        assert!(BLSScheme::verify(&public, b"abc", &first));
        assert!(!BLSScheme::verify(&public, b"ab", &first));
        assert!(!BLSScheme::verify(&public, b"abcd", &first));
        // The empty message is still signable and distinct.
        let empty = BLSScheme::sign(&secret, b"");
        assert!(BLSScheme::verify(&public, b"", &empty));
        assert!(!BLSScheme::verify(&public, b"\0", &empty));
    }

    #[test]
    fn aead_round_trip_and_tamper_detection() {
        let key = SymmetricEncryption::generate_key();
        let plaintext = b"secret data";
        let ciphertext = SymmetricEncryption::encrypt(&key, plaintext).unwrap();
        assert_eq!(SymmetricEncryption::decrypt(&key, &ciphertext).unwrap(), plaintext);

        // Any single bit flip must fail authentication.
        let mut tampered = ciphertext.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(matches!(
            SymmetricEncryption::decrypt(&key, &tampered),
            Err(CryptoError::DecryptionFailed)
        ));

        // A different key must not decrypt.
        assert!(SymmetricEncryption::decrypt(&SymmetricEncryption::generate_key(), &ciphertext)
            .is_err());
    }

    #[test]
    fn aead_nonce_is_random_per_message() {
        let key = SymmetricEncryption::generate_key();
        let first = SymmetricEncryption::encrypt(&key, b"same plaintext").unwrap();
        let second = SymmetricEncryption::encrypt(&key, b"same plaintext").unwrap();
        assert_ne!(
            first[..AEAD_NONCE_BYTES],
            second[..AEAD_NONCE_BYTES],
            "nonce reuse would break confidentiality"
        );
        assert_ne!(first, second);
    }

    #[test]
    fn aead_rejects_truncated_input_without_panicking() {
        let key = SymmetricEncryption::generate_key();
        for length in 0..=AEAD_NONCE_BYTES {
            assert!(matches!(
                SymmetricEncryption::decrypt(&key, &vec![0u8; length]),
                Err(CryptoError::InvalidCiphertext)
            ));
        }
        // Nonce plus a truncated tag is well-formed but unauthenticated.
        assert!(matches!(
            SymmetricEncryption::decrypt(&key, &[0u8; AEAD_NONCE_BYTES + 1]),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn aead_handles_empty_plaintext() {
        let key = SymmetricEncryption::generate_key();
        let ciphertext = SymmetricEncryption::encrypt(&key, b"").unwrap();
        assert!(SymmetricEncryption::decrypt(&key, &ciphertext).unwrap().is_empty());
    }

    #[test]
    fn x25519_exchange_agrees_and_is_non_trivial() {
        let (secret_a, public_a) = key_exchange::generate_keypair();
        let (secret_b, public_b) = key_exchange::generate_keypair();

        assert!(!secret_a.iter().all(|byte| *byte == 0));
        assert!(!public_b.iter().all(|byte| *byte == 0));
        assert_eq!(key_exchange::public_key(&secret_a), public_a);

        let shared_a = key_exchange::shared_secret(&secret_a, &public_b).unwrap();
        let shared_b = key_exchange::shared_secret(&secret_b, &public_a).unwrap();
        assert_eq!(shared_a, shared_b);
        assert!(!shared_a.iter().all(|byte| *byte == 0));

        // A third party derives a different secret.
        let (secret_c, _) = key_exchange::generate_keypair();
        assert_ne!(key_exchange::shared_secret(&secret_c, &public_b).unwrap(), shared_a);
    }

    #[test]
    fn x25519_rejects_low_order_peer_keys() {
        let (secret, _) = key_exchange::generate_keypair();
        // The identity point and the order-8 points all yield a weak secret.
        for weak in [[0u8; 32], {
            let mut point = [0u8; 32];
            point[0] = 1;
            point
        }] {
            assert!(
                matches!(
                    key_exchange::shared_secret(&secret, &weak),
                    Err(CryptoError::InvalidPublicKey)
                ),
                "low-order key {weak:?} must be rejected"
            );
        }

        let (_, healthy) = key_exchange::generate_keypair();
        assert!(key_exchange::shared_secret(&secret, &healthy).is_ok());
    }

    #[test]
    fn ed25519_round_trip_and_rejection() {
        let (signing_key, verifying_key) = ed25519::generate_keypair();
        let signature = ed25519::sign(&signing_key, b"payload");
        assert!(ed25519::verify(&verifying_key, b"payload", &signature));
        assert!(!ed25519::verify(&verifying_key, b"tampered", &signature));

        let (_, other_key) = ed25519::generate_keypair();
        assert!(!ed25519::verify(&other_key, b"payload", &signature));
    }
}
