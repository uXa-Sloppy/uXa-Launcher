use regex::Regex;
use semver::Version;
use std::sync::OnceLock;

use crate::{Error, Result};

fn embedded_version() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)").expect("version regex"))
}

/// Parse a release tag such as `v0.9.5` or `0.9.5-rc.1`.
pub fn parse_version(tag: &str) -> Option<Version> {
    let trimmed = tag.trim().trim_start_matches(['v', 'V']);
    if let Ok(version) = Version::parse(trimmed) {
        return Some(version);
    }
    let found = embedded_version().captures(trimmed)?.get(1)?.as_str();
    Version::parse(found).ok()
}

/// True when `remote` should replace an install recorded as `installed`.
/// Missing local version means the game is not installed.
pub fn is_newer(remote: &str, installed: Option<&str>) -> Result<bool> {
    let Some(installed) = installed.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(true);
    };
    match (parse_version(remote), parse_version(installed)) {
        (Some(remote_v), Some(local_v)) => Ok(remote_v > local_v),
        _ => {
            if remote.trim().is_empty() {
                return Err(Error::msg("release tag is empty"));
            }
            Ok(!remote.trim().eq_ignore_ascii_case(installed))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_v_and_orders_semver() {
        assert_eq!(parse_version("v0.9.5").unwrap().to_string(), "0.9.5");
        assert!(!is_newer("v0.9.5", Some("0.9.5")).unwrap());
        assert!(is_newer("v0.9.6", Some("v0.9.5")).unwrap());
        assert!(!is_newer("0.9.4", Some("0.9.5")).unwrap());
        assert!(is_newer("1.0.0", Some("1.0.0-rc.1")).unwrap());
        assert!(!is_newer("1.0.0-rc.1", Some("1.0.0")).unwrap());
        assert!(is_newer("v0.9.5", None).unwrap());
    }
}
