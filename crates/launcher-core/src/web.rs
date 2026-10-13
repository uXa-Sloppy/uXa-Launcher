use regex::Regex;

use crate::{Error, Result, DEFAULT_WEB_VERSION_PATTERN, USER_AGENT};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebBuild {
    pub build: String,
    pub updated: String,
    pub line: String,
}

pub fn parse_web_build(body: &str, pattern: &str) -> Result<WebBuild> {
    let pattern = if pattern.trim().is_empty() {
        DEFAULT_WEB_VERSION_PATTERN
    } else {
        pattern
    };
    let re = Regex::new(pattern)?;
    let Some(caps) = re.captures(body) else {
        return Err(Error::msg(
            "The page did not contain a \"Build NN, last updated …\" line.",
        ));
    };
    let build = caps
        .get(1)
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::msg("Build number missing from the page."))?;
    let updated = caps
        .get(2)
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default();
    let line = format!("Build {build}, last updated {updated}");
    Ok(WebBuild {
        build,
        updated,
        line,
    })
}

pub async fn fetch_web_build(
    client: &reqwest::Client,
    url: &str,
    pattern: &str,
) -> Result<WebBuild> {
    let response = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(Error::msg(format!(
            "Could not open the game page ({status})."
        )));
    }
    parse_web_build(&text, pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grave_march_markup() {
        let html = r#"<span class="gm-build">Build 72, last updated Oct 13, 2026
</span>"#;
        let parsed = parse_web_build(html, "").unwrap();
        assert_eq!(parsed.build, "72");
        assert_eq!(parsed.updated, "Oct 13, 2026");
        assert_eq!(parsed.line, "Build 72, last updated Oct 13, 2026");
    }
}
