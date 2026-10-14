//! HMAC-SHA256 request signing shared by the launcher and the server agent.
//! The canonical string is `{unix_seconds}\n{METHOD}\n{path}\n{hex_sha256(body)}`.
//! The path does not include the query string. The secret is never logged.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

/// Reject signatures whose timestamp is further than this from the local clock.
pub const MAX_SKEW_SECS: i64 = 300;

pub fn body_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

pub fn canonical(timestamp: &str, method: &str, path: &str, body: &[u8]) -> String {
    format!(
        "{timestamp}\n{}\n{path}\n{}",
        method.to_ascii_uppercase(),
        body_hash(body)
    )
}

pub fn sign_request(secret: &str, timestamp: &str, method: &str, path: &str, body: &[u8]) -> Result<String> {
    if secret.is_empty() {
        return Err(Error::msg("admin key is empty"));
    }
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| Error::msg("admin key is empty"))?;
    mac.update(canonical(timestamp, method, path, body).as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_request(
    secret: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body: &[u8],
    signature: &str,
    now_unix: i64,
) -> Result<()> {
    let ts: i64 = timestamp
        .trim()
        .parse()
        .map_err(|_| Error::msg("request timestamp is not a unix time"))?;
    if (now_unix - ts).abs() > MAX_SKEW_SECS {
        return Err(Error::msg("request timestamp is outside the allowed window"));
    }
    let expected = sign_request(secret, timestamp.trim(), method, path, body)?;
    let Some(got) = decode_hex(signature.trim()) else {
        return Err(Error::msg("request signature does not match"));
    };
    let Some(want) = decode_hex(&expected) else {
        return Err(Error::msg("request signature does not match"));
    };
    if got.len() != want.len() || got.ct_eq(&want).unwrap_u8() != 1 {
        return Err(Error::msg("request signature does not match"));
    }
    Ok(())
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    hex::decode(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rejection() {
        let body = br#"{"tag":"v1"}"#;
        let sig = sign_request("guild-secret", "1700000000", "post", "/v1/deploy", body).unwrap();
        verify_request(
            "guild-secret",
            "1700000000",
            "POST",
            "/v1/deploy",
            body,
            &sig,
            1700000000,
        )
        .unwrap();
        assert!(verify_request(
            "other",
            "1700000000",
            "POST",
            "/v1/deploy",
            body,
            &sig,
            1700000000,
        )
        .is_err());
        assert!(verify_request(
            "guild-secret",
            "1700000000",
            "POST",
            "/v1/deploy",
            body,
            &sig,
            1700000000 + MAX_SKEW_SECS + 1,
        )
        .is_err());
        let mut tampered = body.to_vec();
        tampered.push(b' ');
        assert!(verify_request(
            "guild-secret",
            "1700000000",
            "POST",
            "/v1/deploy",
            &tampered,
            &sig,
            1700000000,
        )
        .is_err());
    }
}
