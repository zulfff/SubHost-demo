use ark_bls12_381::{G1Projective, G2Projective, Fr as Scalar};
use ark_ec::{pairing::Pairing, Group};
use ark_ff::{Field, PrimeField, Zero};
use ark_serialize::CanonicalSerialize;
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use chacha20poly1305::aead::{Aead, KeyInit};
use rand::RngCore;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub type PublicKey = G1Projective;

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct PrivateKey(pub Scalar);

pub type Signature = G2Projective;

pub struct BLSScheme;

impl BLSScheme {
    pub fn keygen() -> (PrivateKey, PublicKey) {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        
        let mut scalar = Scalar::from_le_bytes_mod_order(&bytes);
        while scalar.is_zero() {
            rng.fill_bytes(&mut bytes);
            scalar = Scalar::from_le_bytes_mod_order(&bytes);
        }
        let sk = PrivateKey(scalar);
        let pk = G1Projective::generator() * scalar;
        
        bytes.zeroize();
        (sk, pk)
    }

    pub fn sign(sk: &PrivateKey, message: &[u8]) -> Signature {
        let message_point = Self::hash_to_g2(message);
        message_point * sk.0
    }

    pub fn verify(pk: &PublicKey, message: &[u8], signature: &Signature) -> bool {
        if pk.is_zero() || signature.is_zero() {
            return false;
        }
        let message_point = Self::hash_to_g2(message);
        let lhs = ark_bls12_381::Bls12_381::pairing(*pk, message_point);
        let rhs = ark_bls12_381::Bls12_381::pairing(G1Projective::generator(), *signature);
        lhs == rhs
    }

    pub fn aggregate_public_keys(pks: &[PublicKey]) -> PublicKey {
        pks.iter().fold(G1Projective::default(), |acc, pk| acc + pk)
    }

    pub fn aggregate_signatures(sigs: &[Signature]) -> Signature {
        sigs.iter().fold(G2Projective::default(), |acc, sig| acc + sig)
    }

    pub fn verify_aggregate(pks: &[PublicKey], messages: &[&[u8]], agg_sig: &Signature) -> bool {
        if pks.is_empty()
            || pks.len() != messages.len()
            || agg_sig.is_zero()
            || pks.iter().any(|pk| pk.is_zero())
        {
            return false;
        }

        let mut pairing_sum = <ark_bls12_381::Bls12_381 as Pairing>::TargetField::ONE;
        
        for (pk, msg) in pks.iter().zip(messages.iter()) {
            let msg_point = Self::hash_to_g2(msg);
            let pairing = ark_bls12_381::Bls12_381::pairing(*pk, msg_point);
            pairing_sum *= pairing.0;
        }

        let rhs = ark_bls12_381::Bls12_381::pairing(G1Projective::generator(), *agg_sig);
        pairing_sum == rhs.0
    }

    /// Proof-of-possession: sign the (canonical serialization of the) public key
    /// itself. A validator set MUST require and check each participant's PoP
    /// before including its public key in an aggregated set / committee. Without
    /// it, `aggregate_public_keys` + a single shared-message `aggregate_signatures`
    /// is vulnerable to the classic rogue-key attack (a malicious participant
    /// picks its key as `g - sum(others)` and forges the aggregate).
    pub fn proof_of_possession(sk: &PrivateKey) -> Signature {
        let pk = G1Projective::generator() * sk.0;
        let mut pk_bytes = Vec::new();
        pk.serialize_compressed(&mut pk_bytes)
            .expect("Bls12-381 G1 serialization cannot fail");
        Self::sign(sk, &pk_bytes)
    }

    /// Verify a proof-of-possession against the expected public key.
    pub fn verify_possession(pk: &PublicKey, pop: &Signature) -> bool {
        let mut pk_bytes = Vec::new();
        pk.serialize_compressed(&mut pk_bytes)
            .expect("Bls12-381 G1 serialization cannot fail");
        Self::verify(pk, &pk_bytes, pop)
    }

    fn hash_to_g2(message: &[u8]) -> G2Projective {
        use sha3::{Sha3_384, Digest};
        // Domain-separated hash-and-multiply onto the prime-order subgroup.
        // A production validator swap should replace this with a proper
        // hash-to-curve suite (RFC 9380 / try-and-increment with cofactor clearing);
        // this keeps the demo round-trippable while at least disambiguating
        // message domains (e.g. proposals vs votes) so a signature cannot be
        // replayed across domains.
        let mut hasher = Sha3_384::new();
        hasher.update(b"subhost-bls-01");
        hasher.update(message);
        let hash = hasher.finalize();

        let scalar = Scalar::from_le_bytes_mod_order(&hash);
        G2Projective::generator() * scalar
    }
}

pub struct SymmetricEncryption;

impl SymmetricEncryption {
    pub fn generate_key() -> [u8; 32] {
        let mut rng = rand::thread_rng();
        let mut key = [0u8; 32];
        rng.fill_bytes(&mut key);
        key
    }

    pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        
        let mut rng = rand::thread_rng();
        let mut nonce_bytes = [0u8; 12];
        rng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        
        let ciphertext = cipher.encrypt(nonce, plaintext)
            .expect("encryption should not fail");
        
        let mut result = Vec::with_capacity(12 + ciphertext.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&ciphertext);
        result
    }

    pub fn decrypt(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if ciphertext.len() < 12 {
            return Err(CryptoError::InvalidCiphertext);
        }
        
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let nonce = Nonce::from_slice(&ciphertext[..12]);
        
        cipher.decrypt(nonce, &ciphertext[12..])
            .map_err(|_| CryptoError::DecryptionFailed)
    }
}

pub mod key_exchange {
    //! X25519 Elliptic-Curve Diffie-Hellman on Curve25519.
    //!
    //! This was previously a stub that returned all-zero keys and shared
    //! secrets, which silently broke any handshake that relied on it.

    use rand::RngCore;
    use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};
    use zeroize::Zeroize;

    /// Generate an X25519 keypair as raw 32-byte secret and 32-byte public.
    pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let secret = StaticSecret::from(bytes);
        let public = XPublicKey::from(&secret);
        bytes.zeroize();
        (secret.to_bytes(), public.to_bytes())
    }

    /// Compute a contributory Diffie-Hellman shared secret.
    ///
    /// Untrusted peer keys are always checked; callers cannot accidentally use
    /// the raw X25519 primitive and accept an all-zero shared secret.
    pub fn shared_secret(
        secret: &[u8; 32],
        other_public: &[u8; 32],
    ) -> Result<[u8; 32], crate::CryptoError> {
        shared_secret_checked(secret, other_public)
    }

    /// Same as [`shared_secret`], but enforces contributory behavior so an active
    /// attacker cannot force a weak shared secret by supplying a low-order (or
    /// all-zero) public key. Returns [`CryptoError::InvalidPublicKey`] on failure.
    pub fn shared_secret_checked(
        secret: &[u8; 32],
        other_public: &[u8; 32],
    ) -> Result<[u8; 32], crate::CryptoError> {
        // Reject the identity / all-zero public key outright: X25519's contract
        // maps it (and other small-order points) onto the all-zero shared secret.
        if other_public.iter().all(|&b| b == 0) {
            return Err(crate::CryptoError::InvalidPublicKey);
        }
        let secret = StaticSecret::from(*secret);
        let other_public = XPublicKey::from(*other_public);
        let shared = secret.diffie_hellman(&other_public).to_bytes();
        if shared.iter().all(|&b| b == 0) {
            return Err(crate::CryptoError::InvalidPublicKey);
        }
        Ok(shared)
    }
}

pub mod ed25519 {
    use ed25519_dalek::{Signer, SigningKey, Signature, VerifyingKey, Verifier};
    use rand::RngCore;
    use zeroize::Zeroize;

    pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        
        let signing_key = SigningKey::from_bytes(&bytes);
        let verifying_key = signing_key.verifying_key();
        bytes.zeroize();
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
    #[error("Invalid ciphertext length")]
    InvalidCiphertext,

    #[error("Decryption failed")]
    DecryptionFailed,

    #[error("Invalid public key (low-order / all-zero)")]
    InvalidPublicKey,
    
    #[error("Serialization failed: {0}")]
    Serialization(String),
    
    #[error("Deserialization failed: {0}")]
    Deserialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bls_sign_verify() {
        let (sk, pk) = BLSScheme::keygen();
        let message = b"test message";
        let sig = BLSScheme::sign(&sk, message);
        assert!(BLSScheme::verify(&pk, message, &sig));
    }

    #[test]
    fn test_bls_aggregation() {
        let (sk1, pk1) = BLSScheme::keygen();
        let (sk2, pk2) = BLSScheme::keygen();
        
        let message1 = b"message 1";
        let message2 = b"message 2";
        
        let sig1 = BLSScheme::sign(&sk1, message1);
        let sig2 = BLSScheme::sign(&sk2, message2);
        
        let _agg_pk = BLSScheme::aggregate_public_keys(&[pk1, pk2]);
        let agg_sig = BLSScheme::aggregate_signatures(&[sig1, sig2]);
        
        assert!(BLSScheme::verify_aggregate(
            &[pk1, pk2],
            &[message1, message2],
            &agg_sig
        ));
        assert!(!BLSScheme::verify_aggregate(&[], &[], &Signature::default()));
    }

    #[test]
    fn bls_rejects_identity_key_and_signature() {
        assert!(!BLSScheme::verify(
            &PublicKey::default(),
            b"message",
            &Signature::default()
        ));
        let (_, pk) = BLSScheme::keygen();
        assert!(!BLSScheme::verify(&pk, b"message", &Signature::default()));
        assert!(!BLSScheme::verify_aggregate(
            &[PublicKey::default()],
            &[b"message".as_ref()],
            &Signature::default()
        ));
    }

    #[test]
    fn test_symmetric_encryption() {
        let key = SymmetricEncryption::generate_key();
        let plaintext = b"secret data";
        
        let ciphertext = SymmetricEncryption::encrypt(&key, plaintext);
        let decrypted = SymmetricEncryption::decrypt(&key, &ciphertext).unwrap();
        
        assert_eq!(plaintext.as_slice(), &decrypted);
    }

    #[test]
    fn test_proof_of_possession_roundtrip() {
        let (sk, pk) = BLSScheme::keygen();
        let pop = BLSScheme::proof_of_possession(&sk);
        assert!(BLSScheme::verify_possession(&pk, &pop));
        // PoP must NOT verify against a different key.
        let (_, other_pk) = BLSScheme::keygen();
        assert!(!BLSScheme::verify_possession(&other_pk, &pop));
    }

    #[test]
    fn test_x25519_key_exchange() {
        let (sk_a, pk_a) = key_exchange::generate_keypair();
        let (sk_b, pk_b) = key_exchange::generate_keypair();

        // keypairs must be non-trivial (old stub returned all zeros)
        assert!(!sk_a.iter().all(|&b| b == 0));
        assert!(!pk_b.iter().all(|&b| b == 0));

        let s_a = key_exchange::shared_secret(&sk_a, &pk_b).unwrap();
        let s_b = key_exchange::shared_secret(&sk_b, &pk_a).unwrap();
        assert_eq!(s_a, s_b);
        assert!(!s_a.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_x25519_rejects_low_order_public_key() {
        let (sk, _) = key_exchange::generate_keypair();
        // All-zero public key is the identity element and yields a weak shared secret.
        let zero_pk = [0u8; 32];
        assert!(matches!(
            key_exchange::shared_secret_checked(&sk, &zero_pk),
            Err(CryptoError::InvalidPublicKey)
        ));
        // A normal peer still succeeds through the checked path.
        let (_sk_b, pk_b) = key_exchange::generate_keypair();
        assert!(key_exchange::shared_secret_checked(&sk, &pk_b).is_ok());
    }
}
