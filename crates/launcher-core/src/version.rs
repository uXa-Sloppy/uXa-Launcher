use regex::Regex;
use std::cmp::Ordering;
use std::fmt;
use std::sync::OnceLock;

use crate::{Error, Result};

/// A release version with 2 to 8 numeric parts (`0.9`, `0.9.6`, `0.9.6.1`, `0.9.8.3.1`) and an optional
/// `-pre` suffix. Missing parts count as 0, so `0.9.6` == `0.9.6.0` < `0.9.6.1` < `0.9.7`.
/// A pre-release sorts before the same numbers without one (`1.0.0-rc.1` < `1.0.0`).
/// Most numeric parts a tag may have; longer tags are treated as unparsable.
pub const MAX_PARTS: usize = 8;

#[derive(Debug, Clone)]
pub struct GameVersion {
    parts: [u64; MAX_PARTS],
    count: usize,
    pre: Option<String>,
}

impl GameVersion {
    pub fn parts(&self) -> &[u64] {
        &self.parts[..self.count]
    }
    pub fn pre(&self) -> Option<&str> {
        self.pre.as_deref()
    }
}

impl fmt::Display for GameVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nums: Vec<String> = self.parts().iter().map(|p| p.to_string()).collect();
        write!(f, "{}", nums.join("."))?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl Ord for GameVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts
            .cmp(&other.parts)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_pre(a, b),
            })
    }
}

// Equality follows ordering, so `0.9.8.3` == `0.9.8.3.0`.
impl PartialEq for GameVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GameVersion {}

impl PartialOrd for GameVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Semver-style pre-release order: dot-separated, numeric ids compare as numbers and sort before text.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut ia = a.split('.');
    let mut ib = b.split('.');
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(n), Ok(m)) => n.cmp(&m),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[^0-9.])(\d+(?:\.\d+)+)(?:-([0-9A-Za-z][0-9A-Za-z.-]*))?(?:\+[0-9A-Za-z.-]+)?",
        )
        .expect("version regex")
    })
}

/// Parse a release tag such as `v0.9.5`, `0.9.6.1`, `0.9.5-rc.1` or `FNF 0.9.6.1 (hotfix)`.
pub fn parse_version(tag: &str) -> Option<GameVersion> {
    let trimmed = tag.trim().trim_start_matches(['v', 'V']);
    let caps = version_re().captures(trimmed)?;
    let core = caps.get(1)?.as_str();
    let mut parts = [0u64; MAX_PARTS];
    let mut count = 0;
    for (i, p) in core.split('.').enumerate() {
        if i >= MAX_PARTS {
            return None;
        }
        parts[i] = p.parse().ok()?;
        count = i + 1;
    }
    let pre = caps
        .get(2)
        .map(|m| m.as_str().trim_end_matches(['.', '-']).to_string())
        .filter(|s| !s.is_empty());
    Some(GameVersion { parts, count, pre })
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

    #[test]
    fn four_part_hotfix_versions() {
        assert_eq!(parse_version("v0.9.6.1").unwrap().to_string(), "0.9.6.1");
        assert!(is_newer("v0.9.6.1", Some("v0.9.6")).unwrap());
        assert!(!is_newer("v0.9.6", Some("v0.9.6.1")).unwrap());
        assert!(is_newer("v0.9.7", Some("v0.9.6.1")).unwrap());
        assert!(!is_newer("v0.9.6.1", Some("v0.9.7")).unwrap());
        assert!(is_newer("v0.9.6.2", Some("0.9.6.1")).unwrap());
        assert!(!is_newer("0.9.6.1", Some("v0.9.6.1")).unwrap());
    }

    #[test]
    fn missing_parts_are_zero() {
        assert!(!is_newer("0.9.6.0", Some("0.9.6")).unwrap());
        assert!(!is_newer("0.9.6", Some("0.9.6.0")).unwrap());
        assert!(!is_newer("1.0", Some("1.0.0")).unwrap());
        assert!(is_newer("1.0.1", Some("1.0")).unwrap());
        assert_eq!(parse_version("v1.2").unwrap().parts(), &[1, 2]);
    }

    #[test]
    fn four_part_suffix_orders_above_0_9_6() {
        let suffixed = "v0.9.7.2-build18g";
        let parsed = parse_version(suffixed).unwrap();
        assert_eq!(parsed.to_string(), "0.9.7.2-build18g");
        assert_eq!(parsed.pre(), Some("build18g"));
        assert_eq!(parse_version("v0.9.7.2").unwrap().to_string(), "0.9.7.2");
        assert!(parsed.parts().len() == 4);
        assert!(is_newer("v0.9.7.2", Some("0.9.6")).unwrap());
        assert!(is_newer("v0.9.7.2", Some("v0.9.6.1")).unwrap());
        assert!(is_newer(suffixed, Some("0.9.6")).unwrap());
        assert!(is_newer(suffixed, Some("v0.9.6.1")).unwrap());
        assert!(is_newer(suffixed, Some("v0.9.7.1")).unwrap());
        assert!(is_newer("v0.9.7.2", Some(suffixed)).unwrap());
        assert!(!is_newer(suffixed, Some("v0.9.7.2")).unwrap());
        assert!(!is_newer("v0.9.6.1", Some(suffixed)).unwrap());
        assert!(is_newer("v0.9.7.2-rc.1", Some("0.9.6.1")).unwrap());
    }

    #[test]
    fn prerelease_and_embedded_tags() {
        assert!(is_newer("0.9.6.1", Some("0.9.6.1-rc.2")).unwrap());
        assert!(is_newer("0.9.6.1-rc.10", Some("0.9.6.1-rc.2")).unwrap());
        assert_eq!(
            parse_version("FNF 0.9.6.1 (pre-alpha hotfix)")
                .unwrap()
                .to_string(),
            "0.9.6.1"
        );
        assert_eq!(parse_version("v0.9.4-ds1").unwrap().pre(), Some("ds1"));
        assert_eq!(parse_version("0.9.6+build.7").unwrap().to_string(), "0.9.6");
        assert_eq!(
            parse_version("v1.2.3.4.5").unwrap().parts(),
            &[1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn unparsable_tags_fall_back_to_text() {
        assert!(is_newer("nightly", Some("v0.9.6")).unwrap());
        assert!(!is_newer("nightly", Some("NIGHTLY")).unwrap());
        assert!(is_newer("", Some("x")).is_err());
    }

    #[test]
    fn five_part_and_longer_versions() {
        assert_eq!(
            parse_version("v0.9.8.3.1").unwrap().parts(),
            &[0, 9, 8, 3, 1]
        );
        assert!(is_newer("v0.9.8.3.1", Some("v0.9.8.3")).unwrap());
        assert!(!is_newer("v0.9.8.3", Some("v0.9.8.3.1")).unwrap());
        assert!(!is_newer("v0.9.8.3.1", Some("v0.9.8.3.1")).unwrap());
        assert!(is_newer("v0.9.8.4", Some("v0.9.8.3.1")).unwrap());
        assert!(parse_version("0.9.8.3.1") < parse_version("0.9.8.3.2"));
        assert_eq!(parse_version("1.2.3.4.5.6.7.8").unwrap().parts().len(), 8);
        assert!(parse_version("1.2.3.4.5.6.7.8.9").is_none());
        assert_eq!(
            parse_version("FNF 0.9.8.3.1 (hotfix)").unwrap().to_string(),
            "0.9.8.3.1"
        );
    }

    #[test]
    fn numeric_not_text_compare_and_trailing_zero_equal() {
        assert!(parse_version("0.9.8.10") > parse_version("0.9.8.9"));
        assert!(is_newer("v0.9.8.10", Some("v0.9.8.9")).unwrap());
        assert_eq!(parse_version("0.9.8.3"), parse_version("0.9.8.3.0"));
        assert!(!is_newer("v0.9.8.3.0", Some("v0.9.8.3")).unwrap());
        assert!(!is_newer("v0.9.8.3", Some("v0.9.8.3.0")).unwrap());
    }
}
