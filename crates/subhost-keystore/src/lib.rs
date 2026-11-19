//! Encrypted BLS producer keystore (S9.6).
//!
//! A producer key is a validator's identity: whoever holds it can produce
//! blocks. S9.6 replaces the plaintext `producer.key` hex file with a
//! purpose-specific JSON keystore: scrypt-derived AES-256-GCM over the
//! canonical 32-byte BLS secret, with the file metadata authenticated as
//! associated data. The accepted contract and the rejected alternatives are in
//! ADR-011.
//!
//! This format is deliberately *not* ERC-2335. It uses AES-256-GCM, the
//! repository's canonical little-endian scalar encoding and a Subhost purpose
//! string, so it makes no interoperability claim with Ethereum BLS keystores.
//!
//! # Security scope
//!
//! - The whole header (purpose, version, public key, KDF parameters and salt)
//!   is authenticated as AES-GCM associated data, so downgrading the KDF or
//!   swapping the public key breaks the tag rather than slipping past a cheap
//!   check. The nonce is the AEAD nonce; changing it also breaks the tag.
//! - The scrypt cost is restricted to `log_n` 15..=18 with `r = 8`, `p = 1`,
//!   `dklen = 32` and checked memory/work estimates before the KDF runs, so a
//!   crafted file cannot make the process allocate gigabytes. The worst
//!   accepted case is exactly 256 MiB (`128 * 2^18 * 8`).
//! - Files are read with `O_NOFOLLOW`, an owner-only mode check, an ownership
//!   check and a 16 KiB bounded read. Writes publish a `0600` sibling tempfile
//!   with no-clobber hard-link semantics, sync the file and its directory, and
//!   verify the result by decrypting the new file back.
//! - Plaintext, derived keys, encodings and JSON buffers are wiped with
//!   [`zeroize::Zeroizing`]. Scrypt's own work buffers and BLS `PrivateKey`
//!   clones outside this module are not covered; encryption protects the file
//!   at rest, not a running node.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use subhost_crypto::{BLSScheme, PrivateKey, SECRET_KEY_BYTES};
use zeroize::Zeroizing;

/// Purpose string stored in every keystore file.
pub const PURPOSE: &str = "subhost-producer-keystore/v1";
/// Default keystore file name inside a data directory.
pub const KEYSTORE_FILE_NAME: &str = "producer.keystore.json";
/// Current keystore format version.
pub const KEYSTORE_VERSION: u32 = 1;
/// Refuse to read a keystore larger than this.
pub const MAX_KEYSTORE_FILE_BYTES: u64 = 16 * 1024;
/// Passwords are capped before any KDF work, on creation and on unlock.
pub const MAX_PASSWORD_BYTES: usize = 4 * 1024;
/// Minimum creation password length. No entropy claim is made from length.
pub const MIN_CREATION_PASSWORD_BYTES: usize = 12;

/// Default scrypt cost: `N = 2^17` (~128 MiB, roughly half a second).
pub const DEFAULT_LOG_N: u8 = 17;
/// Lowest accepted scrypt cost; pinned rather than derived from the default.
pub const MIN_LOG_N: u8 = 15;
/// Highest accepted scrypt cost: 256 MiB of ROMix memory.
pub const MAX_LOG_N: u8 = 18;
/// Fixed parallelism factor of the accepted cost range.
pub const KDF_R: u32 = 8;
/// Fixed parallelism factor of the accepted cost range.
pub const KDF_P: u32 = 1;
/// Derived key length in bytes.
pub const KDF_DKLEN: u32 = 32;

const SALT_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const PUBLIC_KEY_BYTES: usize = subhost_crypto::PUBLIC_KEY_BYTES;
/// Ceiling for the `128 * N * r` working set, shared with the wallet fix
/// (SUBHOST-2026-002) so the two loaders cannot drift apart silently.
const MAX_KDF_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
/// Ceiling for the `N * r * p` work estimate.
const MAX_KDF_WORK: u64 = 1 << 22;
/// Domain separator for the associated-data framing.
const AAD_DOMAIN: &[u8] = b"subhost-producer-keystore";

/// Accepted scrypt parameters for a producer keystore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KdfParams {
    /// `log2(N)`.
    pub log_n: u8,
    /// Block size factor.
    pub r: u32,
    /// Parallelism factor.
    pub p: u32,
    /// Derived key length.
    pub dklen: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self { log_n: DEFAULT_LOG_N, r: KDF_R, p: KDF_P, dklen: KDF_DKLEN }
    }
}

impl KdfParams {
    /// Check the allowlist and the checked allocation estimates.
    ///
    /// This runs before `scrypt::Params::new` and before any work buffer is
    /// allocated. The memory comparison is inclusive: `log_n = 18, r = 8`
    /// sits exactly on the 256 MiB ceiling and is accepted.
    pub fn validate(&self) -> Result<(), ProducerKeystoreError> {
        if !(MIN_LOG_N..=MAX_LOG_N).contains(&self.log_n)
            || self.r != KDF_R
            || self.p != KDF_P
            || self.dklen != KDF_DKLEN
        {
            return Err(ProducerKeystoreError::InvalidKdfParameters);
        }
        let n = 1u64
            .checked_shl(u32::from(self.log_n))
            .ok_or(ProducerKeystoreError::InvalidKdfParameters)?;
        let memory = 128u64
            .checked_mul(n)
            .and_then(|value| value.checked_mul(u64::from(self.r)))
            .ok_or(ProducerKeystoreError::InvalidKdfParameters)?;
        let work = n
            .checked_mul(u64::from(self.r))
            .and_then(|value| value.checked_mul(u64::from(self.p)))
            .ok_or(ProducerKeystoreError::InvalidKdfParameters)?;
        if memory > MAX_KDF_MEMORY_BYTES || work > MAX_KDF_WORK {
            return Err(ProducerKeystoreError::InvalidKdfParameters);
        }
        Ok(())
    }
}

/// A producer keystore failure.
///
/// No variant carries password or secret material; the messages are safe to
/// print and log.
#[derive(Debug, thiserror::Error)]
pub enum ProducerKeystoreError {
    /// Filesystem failure.
    #[error("keystore I/O failed")]
    Io(#[source] std::io::Error),

    /// The file is not strict keystore JSON.
    #[error("file is not a valid subhost producer keystore")]
    InvalidFormat,

    /// The file belongs to a different format.
    #[error("keystore purpose does not match this loader")]
    UnsupportedPurpose,

    /// The version is newer or otherwise unknown.
    #[error("unsupported keystore version {0}")]
    UnsupportedVersion(u32),

    /// The file exceeded the bounded read.
    #[error("keystore file is larger than the accepted limit")]
    FileTooLarge,

    /// The path is a symlink or not a regular file.
    #[error("keystore path must be a regular file, not a symlink or special file")]
    UnsafeFile,

    /// Group or other permission bits are set.
    #[error("keystore file permissions are too open (mode {mode:o})")]
    InsecurePermissions {
        /// The offending permission bits.
        mode: u32,
    },

    /// The file is owned by someone else.
    #[error("keystore file is not owned by the running user")]
    InsecureOwner,

    /// This platform has no tested owner-only file semantics yet.
    #[error("producer keystores require a Unix platform with owner-only file permissions")]
    UnsupportedPlatform,

    /// The scrypt parameters are outside the accepted allowlist.
    #[error("scrypt parameters are outside the accepted ranges")]
    InvalidKdfParameters,

    /// scrypt itself failed.
    #[error("key derivation failed")]
    KeyDerivationFailed,

    /// Encryption failed.
    #[error("keystore encryption failed")]
    EncryptionFailed,

    /// The tag did not verify.
    #[error("keystore decryption failed (wrong password or corrupt file)")]
    WrongPasswordOrCorrupt,

    /// The decrypted bytes are not a canonical BLS scalar.
    #[error("keystore secret is not a canonical BLS scalar")]
    InvalidSecret,

    /// The authenticated public key does not belong to the secret.
    #[error("keystore public key does not match the decrypted secret")]
    PublicKeyMismatch,

    /// No-clobber publication found an existing destination.
    #[error("keystore destination already exists")]
    DestinationExists,

    /// Reading the new file back did not reproduce the written key.
    #[error("keystore read-back did not reproduce the written key")]
    ReadBackMismatch,

    /// The password is longer than the accepted cap.
    #[error("password is longer than the accepted limit")]
    PasswordTooLong,

    /// The creation password is too short.
    #[error("creation password must be at least {MIN_CREATION_PASSWORD_BYTES} bytes")]
    PasswordTooShort,

    /// The credential file is empty or carries embedded control bytes.
    #[error("credential file is empty or contains an embedded CR, LF or NUL")]
    CredentialFormat,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeystoreFile {
    purpose: String,
    version: u32,
    public_key: String,
    kdf: KdfParams,
    salt: String,
    nonce: String,
    ciphertext: String,
}

/// Decoded and length-checked keystore fields.
struct ParsedFile {
    public_key: Vec<u8>,
    kdf: KdfParams,
    salt: Vec<u8>,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

/// Write a new encrypted producer keystore and verify it by reading it back.
///
/// Fails without touching the destination if `path` already exists or if the
/// password fails the creation checks. The destination is published with a
/// `0600` sibling tempfile, a no-clobber hard link and a directory sync.
pub fn write_encrypted_producer_key_new(
    path: &Path,
    secret: &PrivateKey,
    password: &[u8],
) -> Result<(), ProducerKeystoreError> {
    write_with_params(path, secret, password, KdfParams::default())
}

fn write_with_params(
    path: &Path,
    secret: &PrivateKey,
    password: &[u8],
    kdf: KdfParams,
) -> Result<(), ProducerKeystoreError> {
    check_password_cap(password)?;
    check_creation_password(password)?;
    kdf.validate()?;

    let public_key = BLSScheme::encode_public_key(&BLSScheme::public_key(secret));
    if public_key.len() != PUBLIC_KEY_BYTES {
        return Err(ProducerKeystoreError::EncryptionFailed);
    }

    let mut salt = [0u8; SALT_BYTES];
    rand::fill(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    rand::fill(&mut nonce_bytes);

    let key = derive_key(password, &salt, &kdf)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|_| ProducerKeystoreError::EncryptionFailed)?;
    let aad = associated_data(&public_key, &kdf, &salt);
    let plaintext = Zeroizing::new(BLSScheme::encode_secret_key(secret));
    let ciphertext = cipher
        .encrypt(&nonce(&nonce_bytes)?, Payload { msg: plaintext.as_slice(), aad: &aad })
        .map_err(|_| ProducerKeystoreError::EncryptionFailed)?;
    if ciphertext.len() != SECRET_KEY_BYTES + TAG_BYTES {
        return Err(ProducerKeystoreError::EncryptionFailed);
    }

    let file = KeystoreFile {
        purpose: PURPOSE.to_string(),
        version: KEYSTORE_VERSION,
        public_key: hex::encode(&public_key),
        kdf,
        salt: hex::encode(salt),
        nonce: hex::encode(nonce_bytes),
        ciphertext: hex::encode(&ciphertext),
    };
    let json = Zeroizing::new(
        serde_json::to_vec(&file).map_err(|_| ProducerKeystoreError::InvalidFormat)?,
    );
    if json.len() as u64 > MAX_KEYSTORE_FILE_BYTES {
        return Err(ProducerKeystoreError::InvalidFormat);
    }
    publish_new(path, json.as_slice())?;

    // The write is only complete when the file unlocks back to the same key.
    let read_back = read_encrypted_producer_key(path, password)?;
    let written = Zeroizing::new(BLSScheme::encode_secret_key(secret));
    let found = Zeroizing::new(BLSScheme::encode_secret_key(&read_back));
    if written.as_slice() != found.as_slice() {
        return Err(ProducerKeystoreError::ReadBackMismatch);
    }
    Ok(())
}

/// Unlock an encrypted producer keystore.
///
/// The password is an exact byte string; it is never trimmed or normalized
/// here. Credential files are read with [`load_credential_file`], which applies
/// the one shared newline convention.
pub fn read_encrypted_producer_key(
    path: &Path,
    password: &[u8],
) -> Result<PrivateKey, ProducerKeystoreError> {
    check_password_cap(password)?;
    let bytes = read_bounded_owner_only(path, MAX_KEYSTORE_FILE_BYTES)?;
    let parsed = parse(&bytes)?;

    let key = derive_key(password, &parsed.salt, &parsed.kdf)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|_| ProducerKeystoreError::WrongPasswordOrCorrupt)?;
    let aad = associated_data(&parsed.public_key, &parsed.kdf, &parsed.salt);
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &nonce(&parsed.nonce)?,
                Payload { msg: parsed.ciphertext.as_slice(), aad: &aad },
            )
            .map_err(|_| ProducerKeystoreError::WrongPasswordOrCorrupt)?,
    );

    let secret = BLSScheme::decode_secret_key(plaintext.as_slice())
        .ok_or(ProducerKeystoreError::InvalidSecret)?;
    let derived = BLSScheme::encode_public_key(&BLSScheme::public_key(&secret));
    if derived != parsed.public_key {
        return Err(ProducerKeystoreError::PublicKeyMismatch);
    }
    Ok(secret)
}

/// Read the authenticated public key without unlocking the file.
///
/// This checks the header and the lengths, but it cannot tell a valid keystore
/// from a corrupt one: the tag is only verified by
/// [`read_encrypted_producer_key`]. Useful to print the key identity during a
/// migration.
pub fn read_public_key(path: &Path) -> Result<Vec<u8>, ProducerKeystoreError> {
    let bytes = read_bounded_owner_only(path, MAX_KEYSTORE_FILE_BYTES)?;
    Ok(parse(&bytes)?.public_key)
}

/// Whether these bytes look like a keystore rather than a legacy hex key file.
///
/// Only the first non-whitespace byte is inspected, so this is a routing hint
/// for CLI consumers; the strict parser still decides whether the file is
/// usable.
pub fn is_keystore_contents(bytes: &[u8]) -> bool {
    bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{')
}

/// Read an owner-only credential file using the one shared convention.
///
/// Exactly one trailing `LF` or `CRLF` is stripped and every other byte is
/// preserved. Embedded `CR`, `LF` and `NUL` are refused, and so is an empty
/// result. The cap is checked after stripping.
pub fn load_credential_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, ProducerKeystoreError> {
    let limit = MAX_PASSWORD_BYTES as u64 + 2;
    let bytes = read_bounded_owner_only(path, limit)?;
    let mut password = Zeroizing::new(bytes);
    if password.last() == Some(&b'\n') {
        password.pop();
        if password.last() == Some(&b'\r') {
            password.pop();
        }
    }
    if password.is_empty() || password.iter().any(|byte| matches!(byte, b'\r' | b'\n' | 0)) {
        return Err(ProducerKeystoreError::CredentialFormat);
    }
    check_password_cap(password.as_slice())?;
    Ok(password)
}

/// Enforce the creation timeout: minimum length and the hard cap.
pub fn check_creation_password(password: &[u8]) -> Result<(), ProducerKeystoreError> {
    check_password_cap(password)?;
    if password.len() < MIN_CREATION_PASSWORD_BYTES {
        return Err(ProducerKeystoreError::PasswordTooShort);
    }
    Ok(())
}

fn check_password_cap(password: &[u8]) -> Result<(), ProducerKeystoreError> {
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(ProducerKeystoreError::PasswordTooLong);
    }
    Ok(())
}

fn derive_key(
    password: &[u8],
    salt: &[u8],
    kdf: &KdfParams,
) -> Result<Zeroizing<[u8; KDF_DKLEN as usize]>, ProducerKeystoreError> {
    kdf.validate()?;
    let params = scrypt::Params::new(kdf.log_n, kdf.r, kdf.p, KDF_DKLEN as usize)
        .map_err(|_| ProducerKeystoreError::InvalidKdfParameters)?;
    let mut key = Zeroizing::new([0u8; KDF_DKLEN as usize]);
    scrypt::scrypt(password, salt, &params, key.as_mut())
        .map_err(|_| ProducerKeystoreError::KeyDerivationFailed)?;
    Ok(key)
}

fn nonce(bytes: &[u8]) -> Result<Nonce<aes_gcm::aead::consts::U12>, ProducerKeystoreError> {
    Nonce::try_from(bytes).map_err(|_| ProducerKeystoreError::InvalidFormat)
}

/// Fixed-width, domain-separated framing of the authenticated header.
///
/// This is binary framing, not incidental JSON serialization: every field has a
/// fixed width, so a change in the file that reaches the tag computation cannot
/// be reinterpreted by an encoder change.
fn associated_data(public_key: &[u8], kdf: &KdfParams, salt: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(2 + AAD_DOMAIN.len() + 4 + public_key.len() + 17 + salt.len());
    aad.extend_from_slice(&(AAD_DOMAIN.len() as u16).to_be_bytes());
    aad.extend_from_slice(AAD_DOMAIN);
    aad.extend_from_slice(&KEYSTORE_VERSION.to_be_bytes());
    aad.extend_from_slice(public_key);
    aad.push(kdf.log_n);
    aad.extend_from_slice(&kdf.r.to_be_bytes());
    aad.extend_from_slice(&kdf.p.to_be_bytes());
    aad.extend_from_slice(&kdf.dklen.to_be_bytes());
    aad.extend_from_slice(salt);
    aad
}

fn parse(bytes: &[u8]) -> Result<ParsedFile, ProducerKeystoreError> {
    let file: KeystoreFile =
        serde_json::from_slice(bytes).map_err(|_| ProducerKeystoreError::InvalidFormat)?;
    if file.purpose != PURPOSE {
        return Err(ProducerKeystoreError::UnsupportedPurpose);
    }
    if file.version != KEYSTORE_VERSION {
        return Err(ProducerKeystoreError::UnsupportedVersion(file.version));
    }
    file.kdf.validate()?;

    let public_key = decode_exact(&file.public_key, PUBLIC_KEY_BYTES)?;
    let salt = decode_exact(&file.salt, SALT_BYTES)?;
    let nonce = decode_exact(&file.nonce, NONCE_BYTES)?;
    let ciphertext = decode_exact(&file.ciphertext, SECRET_KEY_BYTES + TAG_BYTES)?;
    Ok(ParsedFile { public_key, kdf: file.kdf, salt, nonce, ciphertext })
}

fn decode_exact(value: &str, length: usize) -> Result<Vec<u8>, ProducerKeystoreError> {
    let bytes = hex::decode(value).map_err(|_| ProducerKeystoreError::InvalidFormat)?;
    if bytes.len() != length {
        return Err(ProducerKeystoreError::InvalidFormat);
    }
    Ok(bytes)
}

/// Read at most `limit` bytes from an owner-only regular file.
///
/// Applies the same platform rules as every keystore input: no symlinks,
/// owner-only mode, ownership check and a bounded read. CLI consumers use it
/// to route between a legacy hex key file and a keystore before deciding which
/// loader applies; the strict parser still decides whether the file is usable.
pub fn read_bounded_owner_only(path: &Path, limit: u64) -> Result<Vec<u8>, ProducerKeystoreError> {
    let file = open_owner_only(path)?;
    let mut buffer = Vec::new();
    Read::take(file, limit + 1).read_to_end(&mut buffer).map_err(ProducerKeystoreError::Io)?;
    if buffer.len() as u64 > limit {
        return Err(ProducerKeystoreError::FileTooLarge);
    }
    Ok(buffer)
}

#[cfg(unix)]
fn open_owner_only(path: &Path) -> Result<File, ProducerKeystoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path).map_err(map_open_error)?;
    let metadata = file.metadata().map_err(ProducerKeystoreError::Io)?;
    if !metadata.is_file() {
        return Err(ProducerKeystoreError::UnsafeFile);
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ProducerKeystoreError::InsecurePermissions { mode });
        }
    }
    check_owner(path, &metadata)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_owner_only(_path: &Path) -> Result<File, ProducerKeystoreError> {
    Err(ProducerKeystoreError::UnsupportedPlatform)
}

#[cfg(unix)]
fn check_owner(path: &Path, metadata: &std::fs::Metadata) -> Result<(), ProducerKeystoreError> {
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "linux")]
    if let Ok(me) = std::fs::metadata("/proc/self") {
        return if metadata.uid() == me.uid() {
            Ok(())
        } else {
            Err(ProducerKeystoreError::InsecureOwner)
        };
    }
    // Portability fallback: the containing directory is expected to be under the
    // same owner. This is weaker than the /proc/self check and documented as
    // such; the mode check remains the primary control.
    let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent_metadata = std::fs::metadata(parent).map_err(ProducerKeystoreError::Io)?;
    if metadata.uid() == parent_metadata.uid() {
        Ok(())
    } else {
        Err(ProducerKeystoreError::InsecureOwner)
    }
}

#[cfg(unix)]
fn map_open_error(error: std::io::Error) -> ProducerKeystoreError {
    if error.raw_os_error() == Some(libc::ELOOP) {
        ProducerKeystoreError::UnsafeFile
    } else {
        ProducerKeystoreError::Io(error)
    }
}

/// Publish a new secret file with no-clobber, owner-only semantics.
#[cfg(unix)]
fn publish_new(path: &Path, contents: &[u8]) -> Result<(), ProducerKeystoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut suffix = [0u8; 8];
    rand::fill(&mut suffix);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or(ProducerKeystoreError::InvalidFormat)?;
    let temp_path: PathBuf = parent.join(format!(".{name}.tmp-{}", hex::encode(suffix)));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&temp_path).map_err(ProducerKeystoreError::Io)?;
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp_path);
        return Err(ProducerKeystoreError::Io(error));
    }

    // A hard link publishes the inode atomically and fails if the destination
    // exists, so a preexisting keystore is never replaced.
    match std::fs::hard_link(&temp_path, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&temp_path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(ProducerKeystoreError::DestinationExists);
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(ProducerKeystoreError::Io(error));
        }
    }

    // Survive a crash between publication and the directory entry hitting disk.
    File::open(parent).and_then(|dir| dir.sync_all()).map_err(ProducerKeystoreError::Io)
}

#[cfg(not(unix))]
fn publish_new(_path: &Path, _contents: &[u8]) -> Result<(), ProducerKeystoreError> {
    Err(ProducerKeystoreError::UnsupportedPlatform)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const PASSWORD: &[u8] = b"correct horse battery staple";

    fn params_for_tests() -> KdfParams {
        // The accepted floor keeps the suite fast; the default cost is covered
        // by its own test.
        KdfParams { log_n: MIN_LOG_N, r: KDF_R, p: KDF_P, dklen: KDF_DKLEN }
    }

    fn write(path: &Path, secret: &PrivateKey) {
        write_with_params(path, secret, PASSWORD, params_for_tests()).unwrap();
    }

    fn read(path: &Path) -> PrivateKey {
        read_encrypted_producer_key(path, PASSWORD).unwrap()
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Rewrite a keystore as tampered JSON, keeping the owner-only mode.
    fn tamper(path: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        mutate(&mut value);
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        set_mode(path, 0o600);
    }

    fn keystore_and_secret(dir: &TempDir) -> (PathBuf, PrivateKey, Vec<u8>) {
        let path = dir.path().join("producer.keystore.json");
        let (secret, public) = BLSScheme::keygen();
        write(&path, &secret);
        (path, secret, BLSScheme::encode_public_key(&public))
    }

    #[test]
    fn round_trip_writes_and_unlocks_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let (path, secret, public) = keystore_and_secret(&dir);
        let unlocked = read(&path);
        assert_eq!(BLSScheme::encode_secret_key(&unlocked), BLSScheme::encode_secret_key(&secret));
        assert_eq!(read_public_key(&path).unwrap(), public);
        assert_eq!(BLSScheme::encode_public_key(&BLSScheme::public_key(&unlocked)), public);
    }

    #[test]
    fn the_default_cost_keystore_unlocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("producer.keystore.json");
        let (secret, _) = BLSScheme::keygen();
        write_encrypted_producer_key_new(&path, &secret, PASSWORD).unwrap();
        let unlocked = read_encrypted_producer_key(&path, PASSWORD).unwrap();
        assert_eq!(BLSScheme::encode_secret_key(&unlocked), BLSScheme::encode_secret_key(&secret));
    }

    #[test]
    fn a_wrong_password_is_refused_without_an_oracle() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = keystore_and_secret(&dir);
        assert!(matches!(
            read_encrypted_producer_key(&path, b"wrong password"),
            Err(ProducerKeystoreError::WrongPasswordOrCorrupt)
        ));
    }

    #[test]
    fn a_tampered_header_breaks_authentication() {
        for mutate in [
            (|value: &mut serde_json::Value| {
                value["public_key"] = serde_json::json!("aa".repeat(PUBLIC_KEY_BYTES));
            }) as fn(&mut serde_json::Value),
            |value: &mut serde_json::Value| {
                value["salt"] = serde_json::json!("11".repeat(SALT_BYTES));
            },
            |value: &mut serde_json::Value| {
                value["kdf"]["log_n"] = serde_json::json!(16);
            },
            |value: &mut serde_json::Value| {
                value["nonce"] = serde_json::json!("22".repeat(NONCE_BYTES));
            },
            |value: &mut serde_json::Value| {
                value["ciphertext"] = serde_json::json!("33".repeat(SECRET_KEY_BYTES + TAG_BYTES));
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (path, _, _) = keystore_and_secret(&dir);
            tamper(&path, mutate);
            assert!(matches!(
                read_encrypted_producer_key(&path, PASSWORD),
                Err(ProducerKeystoreError::WrongPasswordOrCorrupt)
            ));
        }
    }

    #[test]
    fn a_newer_version_or_foreign_purpose_is_named_not_decrypted() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = keystore_and_secret(&dir);
        tamper(&path, |value| value["version"] = serde_json::json!(2));
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::UnsupportedVersion(2))
        ));

        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = keystore_and_secret(&dir);
        tamper(&path, |value| value["purpose"] = serde_json::json!("subhost-wallet/v1"));
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::UnsupportedPurpose)
        ));
    }

    #[test]
    fn malformed_json_is_refused_before_the_kdf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("producer.keystore.json");

        // Unknown field.
        std::fs::write(&path, br#"{"purpose":"subhost-producer-keystore/v1","extra":1}"#).unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::InvalidFormat)
        ));

        // Duplicate field.
        std::fs::write(
            &path,
            br#"{"purpose":"subhost-producer-keystore/v1","purpose":"subhost-producer-keystore/v1"}"#,
        )
        .unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::InvalidFormat)
        ));

        // Trailing content after the object.
        std::fs::write(&path, b"{} trailing").unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::InvalidFormat)
        ));
    }

    #[test]
    fn oversized_and_loosely_permissioned_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = keystore_and_secret(&dir);

        set_mode(&path, 0o644);
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::InsecurePermissions { .. })
        ));
        set_mode(&path, 0o600);

        std::fs::write(&path, vec![b' '; (MAX_KEYSTORE_FILE_BYTES + 1) as usize]).unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            read_encrypted_producer_key(&path, PASSWORD),
            Err(ProducerKeystoreError::FileTooLarge)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_never_followed_for_a_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _, _) = keystore_and_secret(&dir);
        let link = dir.path().join("link.keystore.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(
            read_encrypted_producer_key(&link, PASSWORD),
            Err(ProducerKeystoreError::UnsafeFile)
        ));
        assert!(matches!(read_public_key(&link), Err(ProducerKeystoreError::UnsafeFile)));
    }

    #[test]
    fn publication_never_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let (path, secret, _) = keystore_and_secret(&dir);
        let (other, _) = BLSScheme::keygen();
        assert!(matches!(
            write_with_params(&path, &other, PASSWORD, params_for_tests()),
            Err(ProducerKeystoreError::DestinationExists)
        ));
        // The original still unlocks to the original key.
        assert_eq!(
            BLSScheme::encode_secret_key(&read(&path)),
            BLSScheme::encode_secret_key(&secret)
        );
    }

    #[test]
    fn the_kdf_allowlist_pins_both_edges() {
        let ok = |log_n| KdfParams { log_n, r: KDF_R, p: KDF_P, dklen: KDF_DKLEN };
        assert!(ok(MIN_LOG_N).validate().is_ok());
        assert!(ok(MAX_LOG_N).validate().is_ok());
        assert!(KdfParams::default().validate().is_ok());
        assert!(ok(14).validate().is_err());
        assert!(ok(19).validate().is_err());
        assert!(KdfParams { log_n: 17, r: 4, p: KDF_P, dklen: KDF_DKLEN }.validate().is_err());
        assert!(KdfParams { log_n: 17, r: KDF_R, p: 2, dklen: KDF_DKLEN }.validate().is_err());
        assert!(KdfParams { log_n: 17, r: KDF_R, p: KDF_P, dklen: 16 }.validate().is_err());
    }

    #[test]
    fn credentials_follow_one_shared_convention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");

        std::fs::write(&path, b"  spaced pass  \n").unwrap();
        set_mode(&path, 0o600);
        assert_eq!(load_credential_file(&path).unwrap().as_slice(), b"  spaced pass  ");

        std::fs::write(&path, b"line\r\n").unwrap();
        set_mode(&path, 0o600);
        assert_eq!(load_credential_file(&path).unwrap().as_slice(), b"line");

        std::fs::write(&path, b"two\nlines").unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            load_credential_file(&path),
            Err(ProducerKeystoreError::CredentialFormat)
        ));

        std::fs::write(&path, b"nul\0byte").unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            load_credential_file(&path),
            Err(ProducerKeystoreError::CredentialFormat)
        ));

        std::fs::write(&path, b"\n").unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(
            load_credential_file(&path),
            Err(ProducerKeystoreError::CredentialFormat)
        ));

        std::fs::write(&path, b"password").unwrap();
        set_mode(&path, 0o644);
        assert!(matches!(
            load_credential_file(&path),
            Err(ProducerKeystoreError::InsecurePermissions { .. })
        ));

        // Inside the read limit (+2 for a trailing newline), no newline: the
        // cap fires after the read, so this is PasswordTooLong.
        std::fs::write(&path, vec![b'a'; MAX_PASSWORD_BYTES + 2]).unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(load_credential_file(&path), Err(ProducerKeystoreError::PasswordTooLong)));

        // Beyond the read limit: the bounded read refuses before the cap.
        std::fs::write(&path, vec![b'a'; MAX_PASSWORD_BYTES + 3]).unwrap();
        set_mode(&path, 0o600);
        assert!(matches!(load_credential_file(&path), Err(ProducerKeystoreError::FileTooLarge)));
    }

    #[test]
    fn creation_password_policy_is_length_and_cap_only() {
        assert!(matches!(
            check_creation_password(b"eleven byte"),
            Err(ProducerKeystoreError::PasswordTooShort)
        ));
        assert!(check_creation_password(b"twelve bytes").is_ok());
        assert!(matches!(
            check_creation_password(&vec![b'x'; MAX_PASSWORD_BYTES + 1]),
            Err(ProducerKeystoreError::PasswordTooLong)
        ));
    }

    #[test]
    fn keystore_routing_only_looks_at_the_first_byte() {
        assert!(is_keystore_contents(br#"{"purpose":"x"}"#));
        assert!(is_keystore_contents(b"\n  {\"purpose\":\"x\"}"));
        assert!(!is_keystore_contents(b"0a1b2c3d"));
        assert!(!is_keystore_contents(b""));
    }
}
