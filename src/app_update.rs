//! GitHub release/update helpers.
//!
//! Keeping these helpers out of `app.rs` makes the large Slint callback hub less
//! tangled and keeps repository URLs in one place for forked builds.

pub const REPO_URL: &str = "https://github.com/OnlyChallenger/probe-shell";
pub const RELEASE_URL: &str = "https://github.com/OnlyChallenger/probe-shell/releases/latest";
pub const RELEASE_API_URL: &str = "https://api.github.com/repos/OnlyChallenger/probe-shell/releases/latest";
pub const USER_AGENT: &str = "probe-shell-update-check";

/// Open a URL with the platform default browser/file handler.
pub fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Best-effort latest release query. Returns `None` on network/API/JSON errors.
pub fn fetch_latest_tag() -> Option<String> {
    let body = ureq::get(RELEASE_API_URL)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(8))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = json["tag_name"].as_str()?.trim().to_string();
    if tag.is_empty() { None } else { Some(tag) }
}

pub fn is_newer_than_current(tag: &str) -> bool {
    matches!(
        (parse_version(tag), parse_version(env!("CARGO_PKG_VERSION"))),
        (Some(latest), Some(cur)) if latest > cur
    )
}

/// Parse a "vX.Y.Z" / "X.Y.Z" tag into a comparable tuple, or None if it isn't
/// a three-part numeric version. A pre-release suffix on the patch (e.g.
/// "3-rc1" / "3-probe1") is tolerated by taking its leading digits.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_release_tags() {
        assert_eq!(parse_version("v0.5.2"), Some((0, 5, 2)));
        assert_eq!(parse_version("0.5.2-probe1"), Some((0, 5, 2)));
        assert_eq!(parse_version("bad"), None);
    }
}
