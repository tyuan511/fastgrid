//! S3-compatible backend: SigV4-signed requests over ureq.
//!
//! Path-style addressing only (the bucket stays in the path), which covers
//! MinIO, Garage and other self-hosted S3-compatible stores. Objects are the
//! same `/`-separated keys the other backends use; sync keys only contain
//! URL-safe characters, so path encoding is a formality, not a hope.
//!
//! Live-server verification is an honest gap on this VM (no MinIO here): the
//! signing code is verified against AWS's published canonical-request example
//! and the XML listing parser against a canned response. Point it at a real
//! bucket before trusting it with data you care about.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fg_core::{Error, Result};
use hmac::{Hmac, Mac};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use sha2::{Digest, Sha256};

use crate::sync_backend::{SyncBackend, check_key, check_prefix};

/// One request timeout. A hung request must end: sync runs off the UI thread,
/// but a stuck backend must never wedge a push or pull forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_KEYS_PER_PAGE: usize = 1000;

/// S3-compatible target. `endpoint` is scheme + host + optional port, no path
/// and no trailing slash (for example `http://127.0.0.1:9000`). Credentials
/// are never printed: the `Debug` impl redacts them.
#[derive(Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("access_key", &"[redacted]")
            .field("secret_key", &"[redacted]")
            .finish()
    }
}

pub struct S3Backend {
    config: S3Config,
    agent: ureq::Agent,
}

impl S3Backend {
    pub fn new(config: S3Config) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent();
        Self { config, agent }
    }

    fn object_url(&self, key: &str) -> String {
        format!(
            "{}/{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            self.config.bucket,
            pct_encode(key, true)
        )
    }

    fn bucket_url(&self) -> String {
        format!(
            "{}/{}",
            self.config.endpoint.trim_end_matches('/'),
            self.config.bucket
        )
    }

    /// One signed request. `query` is already-encoded `name=value` pairs;
    /// `extra_headers` are (name, value) pairs beyond the signing headers.
    fn request(
        &self,
        method: &str,
        url: &str,
        query: &[(String, String)],
        extra_headers: &[(String, String)],
        body: &[u8],
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let (uri_path, query_string) = split_url(url);
        let mut all_query: Vec<(String, String)> = query.to_vec();
        if !query_string.is_empty() {
            for pair in query_string.split('&') {
                if let Some((name, value)) = pair.split_once('=') {
                    all_query.push((name.to_string(), value.to_string()));
                }
            }
        }
        let now = SystemTime::now();
        let dates = amz_dates(now);
        let host = url_host(url).ok_or_else(|| {
            Error::InvalidInput(format!("cannot parse host from S3 endpoint: {url}"))
        })?;
        let payload_hash = sha256_hex(body);
        let mut headers: Vec<(String, String)> = vec![
            ("host".into(), host),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("x-amz-date".into(), dates.datetime.clone()),
        ];
        headers.extend(extra_headers.iter().cloned());
        let canonical = canonical_request(method, &uri_path, &all_query, &headers, &payload_hash);
        let signature = signature(
            &self.config.secret_key,
            &dates.date,
            &dates.datetime,
            &self.config.region,
            &canonical,
        )?;
        let signed_headers = signed_header_names(&headers);
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}/{}/s3/aws4_request, \
             SignedHeaders={signed_headers}, Signature={signature}",
            self.config.access_key, dates.date, self.config.region
        );
        // One code path for every method: ureq's typed builders differ by
        // method, but `run` takes a plain http::Request.
        let mut full_url = url.to_string();
        if !all_query.is_empty() {
            full_url.push('?');
            full_url.push_str(
                &all_query
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
        let mut builder = http::Request::builder()
            .method(method)
            .uri(full_url.as_str());
        for (name, value) in &headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder = builder.header("Authorization", authorization.as_str());
        let request = builder
            .body(body.to_vec())
            .map_err(|e| Error::Storage(format!("cannot build S3 request: {e}")))?;
        self.agent.run(request).map_err(|e| match e {
            ureq::Error::StatusCode(code) => {
                Error::Storage(format!("S3 request to {url} returned HTTP {code}"))
            }
            other => Error::Storage(other.to_string()),
        })
    }
}

impl SyncBackend for S3Backend {
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        check_prefix(prefix)?;
        let mut keys = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_string(), "2".to_string()),
                ("max-keys".to_string(), MAX_KEYS_PER_PAGE.to_string()),
            ];
            if !prefix.is_empty() {
                query.push(("prefix".to_string(), pct_encode(prefix, false)));
            }
            if let Some(token) = &continuation {
                query.push(("continuation-token".to_string(), pct_encode(token, false)));
            }
            let mut response = self.request("GET", &self.bucket_url(), &query, &[], &[])?;
            let body = response
                .body_mut()
                .read_to_string()
                .map_err(|e| Error::Storage(e.to_string()))?;
            let (page, truncated, next) = parse_list_response(&body)?;
            keys.extend(page);
            if !truncated {
                break;
            }
            match next {
                Some(token) => continuation = Some(token),
                None => break,
            }
        }
        keys.sort();
        Ok(keys)
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        let url = self.object_url(key);
        match self.request("GET", &url, &[], &[], &[]) {
            Ok(mut response) => response
                .body_mut()
                .read_to_vec()
                .map(Some)
                .map_err(|e| Error::Storage(e.to_string())),
            Err(error) => match &error {
                Error::Storage(message) if message.contains("returned HTTP 404") => Ok(None),
                _ => Err(error),
            },
        }
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<bool> {
        check_key(key)?;
        let url = self.object_url(key);
        let headers = vec![("If-None-Match".to_string(), "*".to_string())];
        match self.request("PUT", &url, &[], &headers, data) {
            Ok(_) => Ok(true),
            Err(error) => match &error {
                Error::Storage(message) if message.contains("returned HTTP 412") => Ok(false),
                _ => Err(error),
            },
        }
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        check_key(key)?;
        let url = self.object_url(key);
        self.request("PUT", &url, &[], &[], data)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        let url = self.object_url(key);
        match self.request("DELETE", &url, &[], &[], &[]) {
            Ok(_) => Ok(()),
            Err(error) => match &error {
                Error::Storage(message) if message.contains("returned HTTP 404") => Ok(()),
                _ => Err(error),
            },
        }
    }
}

/// Percent-encode per RFC 3986, keeping `/` when it separates path segments.
fn pct_encode(input: &str, keep_slash: bool) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn split_url(url: &str) -> (String, String) {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let path_start = without_scheme.find('/').unwrap_or(without_scheme.len());
    let path_and_query = &without_scheme[path_start..];
    match path_and_query.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (path_and_query.to_string(), String::new()),
    }
}

fn url_host(url: &str) -> Option<String> {
    let without_scheme = url.split("://").nth(1)?;
    let host_port = without_scheme.split('/').next()?;
    Some(host_port.to_string())
}

struct AmzDates {
    date: String,
    datetime: String,
}

fn amz_dates(now: SystemTime) -> AmzDates {
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let day_secs = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    AmzDates {
        date: format!("{year:04}{month:02}{day:02}"),
        datetime: format!(
            "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
            day_secs / 3600,
            (day_secs / 60) % 60,
            day_secs % 60
        ),
    }
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's civil-from-days
/// algorithm; proleptic Gregorian, correct across leap years and centuries.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(data);
    let digest = hash.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Result<Vec<u8>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|e| Error::Storage(format!("cannot init HMAC: {e}")))?;
    mac.update(message);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn signed_header_names(headers: &[(String, String)]) -> String {
    let mut names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    names.sort();
    names.dedup();
    names.join(";")
}

fn canonical_request(
    method: &str,
    uri_path: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_hash: &str,
) -> String {
    let mut sorted_query = query.to_vec();
    sorted_query.sort();
    let query_string = sorted_query
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut sorted_headers = headers.to_vec();
    sorted_headers.sort_by(|a, b| a.0.cmp(&b.0));
    let header_block = sorted_headers
        .iter()
        .map(|(name, value)| format!("{}:{}\n", name.to_lowercase(), value.trim()))
        .collect::<String>();
    format!(
        "{method}\n{uri_path}\n{query_string}\n{header_block}\n{}\n{payload_hash}",
        signed_header_names(headers)
    )
}

fn string_to_sign(date: &str, datetime: &str, region: &str, canonical: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{datetime}\n{date}/{region}/s3/aws4_request\n{}",
        sha256_hex(canonical.as_bytes())
    )
}

fn signature(
    secret: &str,
    date: &str,
    datetime: &str,
    region: &str,
    canonical: &str,
) -> Result<String> {
    let to_sign = string_to_sign(date, datetime, region, canonical);
    let key_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let key_region = hmac_sha256(&key_date, region.as_bytes())?;
    let key_service = hmac_sha256(&key_region, b"s3")?;
    let key_signing = hmac_sha256(&key_service, b"aws4_request")?;
    let signed = hmac_sha256(&key_signing, to_sign.as_bytes())?;
    Ok(signed.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parse a ListBucketResult page: (keys, is_truncated, next_continuation_token).
fn parse_list_response(body: &str) -> Result<(Vec<String>, bool, Option<String>)> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut keys = Vec::new();
    let mut truncated = false;
    let mut token = None;
    let mut current: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                current = Some(String::from_utf8_lossy(event.local_name().as_ref()).into_owned());
            }
            Ok(Event::Text(event)) => {
                if let Some(name) = &current {
                    let raw = String::from_utf8_lossy(&event);
                    let text = quick_xml::escape::unescape(&raw)
                        .map_err(|e| Error::Storage(format!("bad S3 listing XML: {e}")))?
                        .into_owned();
                    match name.as_str() {
                        "Key" => keys.push(text),
                        "IsTruncated" => truncated = text == "true",
                        "NextContinuationToken" => token = Some(text),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => current = None,
            Ok(Event::Eof) => break,
            Err(e) => return Err(Error::Storage(format!("bad S3 listing XML: {e}"))),
            _ => {}
        }
    }
    Ok((keys, truncated, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_right() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(16_880), (2016, 3, 20));
        assert_eq!(civil_from_days(11_000), (2000, 2, 13));
        // Leap day and the century rule: 2000 was a leap year, 1900 was not.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(-22_025), (1909, 9, 13));
    }

    #[test]
    fn amz_dates_format() {
        let dates = amz_dates(UNIX_EPOCH + Duration::from_secs(1_440_892_800));
        assert_eq!(dates.date, "20150830");
        assert_eq!(dates.datetime, "20150830T000000Z");
    }

    #[test]
    fn canonical_request_matches_the_aws_example() {
        // AWS's published IAM example: the canonical request text is fixed, so
        // reproducing it exactly validates the canonicalization rules.
        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
            ("x-amz-date".to_string(), "20150830T000000Z".to_string()),
        ];
        let query = vec![
            ("Action".to_string(), "ListUsers".to_string()),
            ("Version".to_string(), "2010-05-15".to_string()),
        ];
        let canonical = canonical_request(
            "GET",
            "/",
            &query,
            &headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            canonical,
            "GET\n/\nAction=ListUsers&Version=2010-05-15\n\
             content-type:application/x-www-form-urlencoded; charset=utf-8\n\
             host:iam.amazonaws.com\n\
             x-amz-date:20150830T000000Z\n\
             \n\
             content-type;host;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn signing_is_deterministic_and_key_sensitive() {
        let canonical = "GET\n/\n\n\nhost:example.com\n\nhost\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let first = signature(
            "secret-a",
            "20150830",
            "20150830T000000Z",
            "us-east-1",
            canonical,
        )
        .unwrap();
        let again = signature(
            "secret-a",
            "20150830",
            "20150830T000000Z",
            "us-east-1",
            canonical,
        )
        .unwrap();
        let other_key = signature(
            "secret-b",
            "20150830",
            "20150830T000000Z",
            "us-east-1",
            canonical,
        )
        .unwrap();
        let other_day = signature(
            "secret-a",
            "20150831",
            "20150831T000000Z",
            "us-east-1",
            canonical,
        )
        .unwrap();
        assert_eq!(first, again);
        assert_eq!(first.len(), 64);
        assert_ne!(first, other_key);
        assert_ne!(first, other_day);
    }

    #[test]
    fn list_response_parses_pages() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>bucket</Name>
              <Prefix>devices/</Prefix>
              <KeyCount>2</KeyCount>
              <MaxKeys>1000</MaxKeys>
              <IsTruncated>true</IsTruncated>
              <Contents><Key>devices/a/seg-000001</Key></Contents>
              <Contents><Key>devices/a/seg-000002</Key></Contents>
              <NextContinuationToken>token-123</NextContinuationToken>
            </ListBucketResult>"#;
        let (keys, truncated, token) = parse_list_response(body).unwrap();
        assert_eq!(keys, vec!["devices/a/seg-000001", "devices/a/seg-000002"]);
        assert!(truncated);
        assert_eq!(token.as_deref(), Some("token-123"));
    }
}
