//! WebDAV backend: PROPFIND listings, GET/PUT/DELETE over ureq.
//!
//! ureq has no PROPFIND builder, so the listing request is built as an
//! `http::Request` and run through the agent. Listings walk collections
//! recursively (PROPFIND Depth: 1 per collection), because WebDAV has no
//! recursive list. The response parser is namespace-agnostic on purpose:
//! servers differ in prefixes (`d:`, `D:`, none), and only `href` plus
//! "is this a collection" matter here. It is not a general XML parser and
//! does not try to be one.
//!
//! Live-server verification is an honest gap on this VM (no WebDAV server
//! here): the parser and URL mapping are tested against canned responses.
//! Point it at a real server before trusting it with data you care about.

use std::time::Duration;

use fg_core::{Error, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::sync_backend::{SyncBackend, check_key, check_prefix};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;

/// WebDAV collection holding a vault. `base_url` is the collection URL with or
/// without a trailing slash. Credentials are never printed.
#[derive(Clone)]
pub struct WebDavConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavConfig")
            .field("base_url", &self.base_url)
            .field("username", &"[redacted]")
            .field("password", &"[redacted]")
            .finish()
    }
}

pub struct WebDavBackend {
    config: WebDavConfig,
    agent: ureq::Agent,
}

impl WebDavBackend {
    pub fn new(config: WebDavConfig) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent();
        Self { config, agent }
    }

    fn url(&self, key: &str) -> String {
        let base = self.config.base_url.trim_end_matches('/');
        if key.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{key}")
        }
    }

    fn auth(&self) -> String {
        use base64::Engine;
        let credentials = format!("{}:{}", self.config.username, self.config.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes())
        )
    }

    /// The base collection's path ("/dav/sync" for
    /// "https://host/dav/sync/"), used to turn response hrefs back into keys.
    fn base_path(&self) -> String {
        url_path(&self.config.base_url)
            .trim_end_matches('/')
            .to_string()
    }

    /// Href to key, or `None` for the collection itself and for hrefs outside
    /// the base collection.
    fn href_to_key(&self, href: &str) -> Option<String> {
        let path = url_path(href);
        let base = self.base_path();
        let rest = path.strip_prefix(base.as_str())?;
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            return None;
        }
        Some(pct_decode(rest))
    }

    /// One PROPFIND Depth: 1. Returns (href, is_collection) pairs, the target
    /// itself first. A missing target lists as empty, not an error.
    fn propfind(&self, url: &str) -> Result<Vec<(String, bool)>> {
        let request = http::Request::builder()
            .method("PROPFIND")
            .uri(url)
            .header("Authorization", self.auth())
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(PROPFIND_BODY.as_bytes().to_vec())
            .map_err(|e| Error::Storage(format!("cannot build PROPFIND request: {e}")))?;
        let mut response = match self.agent.run(request) {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(Vec::new()),
            Err(ureq::Error::StatusCode(code)) => {
                return Err(Error::Storage(format!(
                    "WebDAV PROPFIND {url} returned HTTP {code}"
                )));
            }
            Err(other) => return Err(Error::Storage(other.to_string())),
        };
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Storage(e.to_string()))?;
        parse_propfind(&body)
    }

    fn walk(&self, prefix: &str, out: &mut Vec<String>) -> Result<()> {
        let entries = self.propfind(&self.url(prefix))?;
        let single_file = entries.len() == 1 && !entries[0].1;
        for (href, is_collection) in &entries {
            let Some(key) = self.href_to_key(href) else {
                continue;
            };
            if key == prefix {
                // The target itself: only an answer when it is a lone file.
                if single_file {
                    out.push(key);
                }
                continue;
            }
            if *is_collection {
                self.walk(&key, out)?;
            } else {
                out.push(key);
            }
        }
        Ok(())
    }
}

impl SyncBackend for WebDavBackend {
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        check_prefix(prefix)?;
        let mut keys = Vec::new();
        self.walk(prefix, &mut keys)?;
        keys.sort();
        Ok(keys)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        match self
            .agent
            .get(self.url(key))
            .header("Authorization", self.auth())
            .call()
        {
            Ok(mut response) => response
                .body_mut()
                .read_to_vec()
                .map(Some)
                .map_err(|e| Error::Storage(e.to_string())),
            Err(ureq::Error::StatusCode(404)) => Ok(None),
            Err(ureq::Error::StatusCode(code)) => Err(Error::Storage(format!(
                "WebDAV GET {} returned HTTP {code}",
                self.url(key)
            ))),
            Err(other) => Err(Error::Storage(other.to_string())),
        }
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<bool> {
        check_key(key)?;
        let result = self
            .agent
            .put(self.url(key))
            .header("Authorization", self.auth())
            .header("If-None-Match", "*")
            .send(data);
        match result {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(412)) => Ok(false),
            Err(ureq::Error::StatusCode(code)) => Err(Error::Storage(format!(
                "WebDAV PUT {} returned HTTP {code}",
                self.url(key)
            ))),
            Err(other) => Err(Error::Storage(other.to_string())),
        }
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        check_key(key)?;
        match self
            .agent
            .put(self.url(key))
            .header("Authorization", self.auth())
            .send(data)
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(code)) => Err(Error::Storage(format!(
                "WebDAV PUT {} returned HTTP {code}",
                self.url(key)
            ))),
            Err(other) => Err(Error::Storage(other.to_string())),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match self
            .agent
            .delete(self.url(key))
            .header("Authorization", self.auth())
            .call()
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::StatusCode(404)) => Ok(()),
            Err(ureq::Error::StatusCode(code)) => Err(Error::Storage(format!(
                "WebDAV DELETE {} returned HTTP {code}",
                self.url(key)
            ))),
            Err(other) => Err(Error::Storage(other.to_string())),
        }
    }
}

/// Path part of a URL: absolute URLs lose scheme + host, plain paths pass
/// through, query strings are dropped.
fn url_path(url: &str) -> String {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let path_start = without_scheme.find('/').unwrap_or(without_scheme.len());
    let path = &without_scheme[path_start..];
    path.split('?').next().unwrap_or(path).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn pct_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a PROPFIND multistatus body into (href, is_collection) pairs, in
/// document order. Element prefixes are ignored; a response counts as a
/// collection when its resourcetype contains a collection element.
fn parse_propfind(body: &str) -> Result<Vec<(String, bool)>> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut entries = Vec::new();
    let mut in_response = false;
    let mut in_href = false;
    let mut href = String::new();
    let mut is_collection = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                let name = String::from_utf8_lossy(event.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "response" => {
                        in_response = true;
                        href.clear();
                        is_collection = false;
                    }
                    "href" if in_response => in_href = true,
                    "collection" if in_response => is_collection = true,
                    _ => {}
                }
            }
            Ok(Event::Text(event)) => {
                if in_href {
                    let raw = String::from_utf8_lossy(&event);
                    let text = quick_xml::escape::unescape(&raw)
                        .map_err(|e| Error::Storage(format!("bad PROPFIND XML: {e}")))?;
                    href.push_str(&text);
                }
            }
            Ok(Event::End(event)) => {
                let name = String::from_utf8_lossy(event.local_name().as_ref()).into_owned();
                match name.as_str() {
                    "href" => in_href = false,
                    "response" => {
                        in_response = false;
                        entries.push((std::mem::take(&mut href), is_collection));
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::Storage(format!("bad PROPFIND XML: {e}"))),
            _ => {}
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> WebDavBackend {
        WebDavBackend::new(WebDavConfig {
            base_url: "https://host.example/dav/sync/".into(),
            username: "user".into(),
            password: "pw".into(),
        })
    }

    #[test]
    fn href_mapping() {
        let backend = backend();
        assert_eq!(
            backend.href_to_key("https://host.example/dav/sync/devices/a/seg-000001"),
            Some("devices/a/seg-000001".to_string())
        );
        assert_eq!(
            backend.href_to_key("/dav/sync/devices/a/seg-000001"),
            Some("devices/a/seg-000001".to_string())
        );
        // The collection itself maps to nothing.
        assert_eq!(backend.href_to_key("/dav/sync/"), None);
        assert_eq!(backend.href_to_key("/dav/sync"), None);
        // Outside the base is ignored, not treated as a key.
        assert_eq!(backend.href_to_key("/other/path/file"), None);
        // Percent-decoding applies to the remainder.
        assert_eq!(
            backend.href_to_key("/dav/sync/devices/a%2Db/seg-000001"),
            Some("devices/a-b/seg-000001".to_string())
        );
    }

    #[test]
    fn propfind_parses_namespaces_and_collections() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
            <d:multistatus xmlns:d="DAV:">
              <d:response>
                <d:href>/dav/sync/devices/</d:href>
                <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
              </d:response>
              <d:response>
                <d:href>/dav/sync/devices/a/seg-000001</d:href>
                <d:propstat><d:prop><d:resourcetype/></d:prop></d:propstat>
              </d:response>
              <d:response>
                <d:href>/dav/sync/devices/b/</d:href>
                <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
              </d:response>
            </d:multistatus>"#;
        let entries = parse_propfind(body).unwrap();
        assert_eq!(
            entries,
            vec![
                ("/dav/sync/devices/".to_string(), true),
                ("/dav/sync/devices/a/seg-000001".to_string(), false),
                ("/dav/sync/devices/b/".to_string(), true),
            ]
        );
    }

    #[test]
    fn propfind_without_prefixes_parses() {
        let body = r#"<multistatus><response><href>/dav/sync/manifest</href>
            <propstat><prop><resourcetype/></prop></propstat></response></multistatus>"#;
        let entries = parse_propfind(body).unwrap();
        assert_eq!(entries, vec![("/dav/sync/manifest".to_string(), false)]);
    }
}
