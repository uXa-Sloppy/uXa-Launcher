//! Pre-alpha invite keys, minted on the admin PC. Only the SHA-256 is sent to the
//! server agent, which appends it to invite_keys.txt; the plaintext key goes to the
//! player (the Overseer bot DMs it). Same format as Tools/fnf_invite_key.py.

use sha2::{Digest, Sha256};

use crate::{Error, Result};

const KEY_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// `FNF-XXXX-XXXX-XXXX` and its SHA-256 (of the trimmed, upper-cased key).
pub fn mint_invite_key() -> Result<(String, String)> {
    let mut raw = [0u8; 12];
    getrandom::getrandom(&mut raw).map_err(|e| Error::msg(format!("no OS randomness: {e}")))?;
    // 256 is a multiple of 32, so `% 32` has no bias.
    let chars: String = raw.iter().map(|b| KEY_ALPHABET[(*b as usize) % 32] as char).collect();
    let key = format!("FNF-{}-{}-{}", &chars[0..4], &chars[4..8], &chars[8..12]);
    let hash = invite_key_hash(&key);
    Ok((key, hash))
}

pub fn invite_key_hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.trim().to_uppercase().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_and_hash_match_python() {
        let (key, hash) = mint_invite_key().unwrap();
        assert_eq!(key.len(), 18);
        assert!(key.starts_with("FNF-"));
        assert_eq!(hash, invite_key_hash(&key.to_lowercase()));
        // python: hashlib.sha256("FNF-TEST-ABCD-EFGH".encode()).hexdigest()
        assert_eq!(
            invite_key_hash(" fnf-test-abcd-efgh "),
            hex::encode(Sha256::digest(b"FNF-TEST-ABCD-EFGH"))
        );
    }
}
