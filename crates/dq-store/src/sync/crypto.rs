//! The encrypted envelope of a sync file.
//!
//! Layout (all integers little-endian), 65 header bytes then the ciphertext:
//!
//! | bytes | field                                                     |
//! |-------|-----------------------------------------------------------|
//! | 6     | magic `DQSYNC`                                            |
//! | 1     | format version (1)                                        |
//! | 1     | KDF id (1 = Argon2id v1.3)                                |
//! | 4+4+4 | Argon2 memory (KiB), iterations, parallelism              |
//! | 16    | random salt (new for every passphrase/key)                |
//! | 16    | key check: bytes 32..48 of the Argon2 output              |
//! | 1     | cipher id (1 = AES-256-GCM)                               |
//! | 12    | random nonce (new for every write)                        |
//! | n+16  | AES-256-GCM ciphertext and tag; the header is the AAD     |
//!
//! The key check tells a wrong passphrase apart from a damaged file; it adds
//! nothing for an attacker, who can test a guess against the tag anyway.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use dq_core::{DbError, DbResult, ErrorKind};
use zeroize::Zeroizing;

const MAGIC: &[u8; 6] = b"DQSYNC";
const VERSION: u8 = 1;
const KDF_ARGON2ID: u8 = 1;
const CIPHER_AES_256_GCM: u8 = 1;
const SALT_LEN: usize = 16;
const CHECK_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
pub const HEADER_LEN: usize = 6 + 1 + 1 + 12 + SALT_LEN + CHECK_LEN + 1 + NONCE_LEN;

/// Argon2id cost. Files may carry other values; [`KdfParams::check`] bounds
/// them so a crafted file cannot make the app allocate gigabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for KdfParams {
    /// OWASP's Argon2id baseline: 19 MiB, 2 iterations, 1 lane.
    fn default() -> Self {
        Self { memory_kib: Params::DEFAULT_M_COST, iterations: Params::DEFAULT_T_COST, parallelism: 1 }
    }
}

impl KdfParams {
    fn check(self) -> DbResult<Params> {
        let sane = (8..=256 * 1024).contains(&self.memory_kib)
            && (1..=16).contains(&self.iterations)
            && (1..=8).contains(&self.parallelism);
        if !sane {
            return Err(unreadable("its key settings are out of range"));
        }
        Params::new(self.memory_kib, self.iterations, self.parallelism, Some(32 + CHECK_LEN))
            .map_err(|e| unreadable(&e.to_string()))
    }
}

/// A key derived from the passphrase, cached so every sync does not pay for Argon2.
pub struct SyncKey {
    salt: [u8; SALT_LEN],
    params: KdfParams,
    key: Zeroizing<[u8; 32]>,
    check: [u8; CHECK_LEN],
}

impl std::fmt::Debug for SyncKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncKey").field("params", &self.params).finish_non_exhaustive()
    }
}

fn unreadable(reason: &str) -> DbError {
    DbError::invalid(format!("The sync file cannot be read: {reason}."))
}

pub fn wrong_passphrase() -> DbError {
    DbError::new(ErrorKind::Auth, "The sync passphrase does not match the one this sync file was written with.")
        .with_code("sync_passphrase")
}

fn random<const N: usize>() -> DbResult<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| DbError::internal(format!("No secure random source: {e}")))?;
    Ok(bytes)
}

impl SyncKey {
    fn derive(passphrase: &str, salt: [u8; SALT_LEN], params: KdfParams) -> DbResult<Self> {
        let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params.check()?);
        let mut out = Zeroizing::new([0u8; 32 + CHECK_LEN]);
        argon
            .hash_password_into(passphrase.as_bytes(), &salt, out.as_mut())
            .map_err(|e| DbError::internal(format!("Key derivation failed: {e}")))?;
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(&out[..32]);
        let mut check = [0u8; CHECK_LEN];
        check.copy_from_slice(&out[32..]);
        Ok(Self { salt, params, key, check })
    }

    /// A key with a fresh random salt, for a file that does not exist yet.
    pub fn generate(passphrase: &str, params: KdfParams) -> DbResult<Self> {
        Self::derive(passphrase, random()?, params)
    }
}

struct Header {
    params: KdfParams,
    salt: [u8; SALT_LEN],
    check: [u8; CHECK_LEN],
}

fn parse_header(bytes: &[u8]) -> DbResult<Header> {
    if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
        return Err(unreadable("it is not a DataQuery sync file"));
    }
    if bytes.len() < HEADER_LEN + TAG_LEN {
        return Err(unreadable("it is incomplete (the cloud client may still be downloading it)"));
    }
    if bytes[6] != VERSION || bytes[7] != KDF_ARGON2ID || bytes[HEADER_LEN - NONCE_LEN - 1] != CIPHER_AES_256_GCM {
        return Err(unreadable("it was written by a newer DataQuery; update this copy"));
    }
    let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&bytes[20..20 + SALT_LEN]);
    let mut check = [0u8; CHECK_LEN];
    check.copy_from_slice(&bytes[36..36 + CHECK_LEN]);
    Ok(Header {
        params: KdfParams { memory_kib: u32_at(8), iterations: u32_at(12), parallelism: u32_at(16) },
        salt,
        check,
    })
}

/// Constant-time comparison of the key check.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Encrypts `plaintext` under `key` with a fresh nonce.
pub fn seal(key: &SyncKey, plaintext: &[u8]) -> DbResult<Vec<u8>> {
    let nonce: [u8; NONCE_LEN] = random()?;
    let mut out = Vec::with_capacity(HEADER_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[VERSION, KDF_ARGON2ID]);
    for v in [key.params.memory_kib, key.params.iterations, key.params.parallelism] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&key.salt);
    out.extend_from_slice(&key.check);
    out.push(CIPHER_AES_256_GCM);
    out.extend_from_slice(&nonce);
    let cipher = Aes256Gcm::new_from_slice(key.key.as_ref()).map_err(|e| DbError::internal(e.to_string()))?;
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad: &out })
        .map_err(|_| DbError::internal("Encryption failed."))?;
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Decrypts a sync file. Reuses `cached` when the file was written with the
/// same salt and cost; otherwise derives the key from `passphrase` and returns
/// it for the next call.
pub fn open(passphrase: &str, bytes: &[u8], cached: Option<SyncKey>) -> DbResult<(Vec<u8>, SyncKey)> {
    let header = parse_header(bytes)?;
    let key = match cached {
        Some(k) if k.salt == header.salt && k.params == header.params => k,
        _ => SyncKey::derive(passphrase, header.salt, header.params)?,
    };
    if !same(&key.check, &header.check) {
        return Err(wrong_passphrase());
    }
    let (aad, sealed) = bytes.split_at(HEADER_LEN);
    let nonce: [u8; NONCE_LEN] = aad[HEADER_LEN - NONCE_LEN..].try_into().expect("nonce length");
    let cipher = Aes256Gcm::new_from_slice(key.key.as_ref()).map_err(|e| DbError::internal(e.to_string()))?;
    let plain = cipher
        .decrypt(&Nonce::from(nonce), Payload { msg: sealed, aad })
        .map_err(|_| unreadable("it is damaged or incomplete, or was changed outside DataQuery"))?;
    Ok((plain, key))
}

#[cfg(test)]
pub(crate) const TEST_PARAMS: KdfParams = KdfParams { memory_kib: 64, iterations: 1, parallelism: 1 };

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_wrong_passphrase_and_tampering() {
        let key = SyncKey::generate("correct horse", TEST_PARAMS).unwrap();
        let sealed = seal(&key, b"{\"hello\":1}").unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"hello"), "plaintext must not be visible");
        let (plain, cached) = open("correct horse", &sealed, None).unwrap();
        assert_eq!(plain, b"{\"hello\":1}");
        // The cached key is reused and still opens the next write.
        let again = seal(&cached, b"second").unwrap();
        assert_eq!(open("ignored: cached key matches", &again, Some(cached)).unwrap().0, b"second");

        let err = open("wrong horse", &sealed, None).unwrap_err();
        assert_eq!(err.code.as_deref(), Some("sync_passphrase"));

        let mut body = sealed.clone();
        *body.last_mut().unwrap() ^= 1;
        assert!(open("correct horse", &body, None).unwrap_err().message.contains("damaged"));
        let mut header = sealed.clone();
        header[HEADER_LEN - 1] ^= 1; // the nonce is authenticated too
        assert!(open("correct horse", &header, None).unwrap_err().message.contains("damaged"));
        assert!(open("correct horse", &sealed[..HEADER_LEN], None).unwrap_err().message.contains("incomplete"));
        assert!(open("correct horse", b"{\"json\":true}", None).unwrap_err().message.contains("not a DataQuery"));
        let mut costly = sealed;
        costly[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(open("correct horse", &costly, None).unwrap_err().message.contains("out of range"));
    }
}
