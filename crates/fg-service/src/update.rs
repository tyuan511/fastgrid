//! Update checks: check-and-notify only. When the user turns update
//! checks on (off by default, see `PrivacySettings::auto_update`), the
//! launcher asks the GitHub releases API for the latest tag and shows a
//! notice if it is newer than the running version. fastgrid never
//! downloads or installs an update by itself; installing stays a manual
//! step from the releases page.

use std::time::Duration;

/// The releases endpoint the check reads. Kept as a constant so tests can
/// point `fetch_latest_tag` at a local server instead.
pub const RELEASES_URL: &str = "https://api.github.com/repos/tyuan511/fastgrid/releases/latest";

/// One request timeout. The check runs off the UI thread, but a hung
/// request must still end: a failed check is silent, never a hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// True when `tag` names a release newer than the running `current`
/// version. Both sides tolerate a leading `v`; missing components count
/// as zero (`1.10` beats `1.9`). Anything unparsable on either side
/// answers false: a check must never claim an update on garbage.
pub fn newer_than(current: &str, tag: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.strip_prefix('v')
            .unwrap_or(v)
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()
    };
    let (Some(current), Some(tag)) = (parse(current), parse(tag)) else {
        return false;
    };
    let len = current.len().max(tag.len());
    for i in 0..len {
        let c = current.get(i).copied().unwrap_or(0);
        let t = tag.get(i).copied().unwrap_or(0);
        if t != c {
            return t > c;
        }
    }
    false
}

/// Fetch the latest release tag from a releases-API URL (the real one is
/// `RELEASES_URL`; tests pass a local server). Errors are strings, not
/// `fg_core::Error`: the caller treats every failure the same way —
/// silently, with no notice shown.
pub fn fetch_latest_tag(url: &str) -> Result<String, String> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header("User-Agent", "fastgrid-update-check")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| e.to_string())?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    json.get("tag_name")
        .and_then(|tag| tag.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "releases response has no tag_name".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn newer_than_compares_versions() {
        assert!(newer_than("0.0.1", "v0.0.2"));
        assert!(newer_than("1.9", "1.10"));
        assert!(newer_than("1.9.9", "v1.10"));
        assert!(!newer_than("0.0.2", "v0.0.1"));
        assert!(!newer_than("0.0.1", "v0.0.1"));
        assert!(!newer_than("0.0.1", "garbage"));
        assert!(!newer_than("garbage", "v9.9.9"));
    }

    /// Serve one canned releases-API response on a local port, then fetch
    /// it through the real code path (no network beyond loopback).
    fn serve_once(body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_owned();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(reply.as_bytes());
        });
        format!("http://{addr}/releases/latest")
    }

    #[test]
    fn fetch_latest_tag_reads_tag_name() {
        let url = serve_once(r#"{"tag_name":"v0.2.0","name":"fastgrid 0.2.0"}"#);
        assert_eq!(fetch_latest_tag(&url).as_deref(), Ok("v0.2.0"));
    }

    #[test]
    fn fetch_latest_tag_without_a_tag_is_an_error() {
        let url = serve_once(r#"{"name":"no tag here"}"#);
        assert!(fetch_latest_tag(&url).is_err());
    }
}
