use ark_bls12_381::{G1Projective, G2Projective, Fr as Scalar};
use ark_ec::{pairing::Pairing, Group, CurveGroup};
use ark_ff::{Field, PrimeField};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
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
        
        let scalar = Scalar::from_le_bytes_mod_order(&bytes);
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
        if pks.len() != messages.len() {
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

    fn hash_to_g2(message: &[u8]) -> G2Projective {
        use sha3::{Sha3_256, Digest};
        let mut hasher = Sha3_256::new();
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
    pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
        ([0u8; 32], [0u8; 32])
    }

    pub fn shared_secret(_secret: &[u8; 32], _other_public: &[u8; 32]) -> [u8; 32] {
        [0u8; 32]
    }
}

pub mod ed25519 {
    use ed25519_dalek::{Signer, SigningKey, Signature, VerifyingKey, Verifier};
    use rand::RngCore;

    pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        
        let signing_key = SigningKey::from_bytes(&bytes);
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
    #[error("Invalid ciphertext length")]
    InvalidCiphertext,
    
    #[error("Decryption failed")]
    DecryptionFailed,
    
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
        
        let agg_pk = BLSScheme::aggregate_public_keys(&[pk1, pk2]);
        let agg_sig = BLSScheme::aggregate_signatures(&[sig1, sig2]);
        
        assert!(BLSScheme::verify_aggregate(
            &[pk1, pk2],
            &[message1, message2],
            &agg_sig
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
}
