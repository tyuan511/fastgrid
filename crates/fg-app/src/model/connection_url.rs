//! Parse a database connection URL into form fields.
//!
//! Accepts `postgres://` and `postgresql://` URLs the way libpq spells them
//! (`postgres://user:password@host:port/database?sslmode=verify-full`) and
//! `mysql://` URLs (`mysql://user:password@host:port/database?sslMode=VERIFY_CA`).
//! Only the parts the connection form shows are extracted; anything else in
//! the query is ignored. Percent-decoding applies to the user, password and
//! database (a `+` stays a `+`: it is not a space outside form encoding).

use fg_core::TlsMode;

/// What a URL filled in: everything the form can take from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedNetworkUrl {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password: Option<String>,
    pub database: Option<String>,
    pub tls_mode: Option<TlsMode>,
}

/// Parse `input` or explain why it is not a usable PostgreSQL URL.
pub fn parse_postgres_url(input: &str) -> Result<ParsedNetworkUrl, String> {
    parse_network_url(input, &["postgres://", "postgresql://"], "postgres://")
}

/// Parse `input` or explain why it is not a usable MySQL URL.
pub fn parse_mysql_url(input: &str) -> Result<ParsedNetworkUrl, String> {
    parse_network_url(input, &["mysql://"], "mysql://")
}

fn parse_network_url(
    input: &str,
    schemes: &[&str],
    expected: &str,
) -> Result<ParsedNetworkUrl, String> {
    let trimmed = input.trim();
    let rest = schemes
        .iter()
        .find_map(|scheme| trimmed.strip_prefix(scheme))
        .ok_or_else(|| format!("expected a URL starting with {expected}"))?;
    let (authority_and_path, query) = match rest.split_once('?') {
        Some((head, query)) => (head, Some(query)),
        None => (rest, None),
    };
    let (authority, path) = match authority_and_path.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (authority_and_path, None),
    };
    if authority.is_empty() {
        return Err("the URL has no host".into());
    }
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(info) => {
            let (user, password) = match info.split_once(':') {
                Some((user, password)) => (user, Some(password)),
                None => (info, None),
            };
            (
                percent_decode(user)?,
                password.map(percent_decode).transpose()?,
            )
        }
        None => (String::new(), None),
    };
    let (host, port) = parse_host_port(hostport)?;
    let database = match path {
        Some("") | None => None,
        Some(db) => {
            let decoded = percent_decode(db)?;
            if decoded.is_empty() {
                None
            } else {
                Some(decoded)
            }
        }
    };
    let mut tls_mode = None;
    if let Some(query) = query {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            // libpq spells it `sslmode=verify-full`; MySQL spells it
            // `sslMode=VERIFY_IDENTITY`. Both are accepted for either
            // scheme: the form does not care which dialect wrote the URL.
            if key == "sslmode" {
                tls_mode = Some(match value {
                    "disable" => TlsMode::Disable,
                    "prefer" => TlsMode::Prefer,
                    "require" => TlsMode::Require,
                    "verify-ca" => TlsMode::VerifyCa,
                    "verify-full" => TlsMode::VerifyFull,
                    other => return Err(format!("unknown sslmode '{other}'")),
                });
            } else if key == "sslMode" {
                tls_mode = Some(match value {
                    "DISABLED" => TlsMode::Disable,
                    "PREFERRED" => TlsMode::Prefer,
                    "REQUIRED" => TlsMode::Require,
                    "VERIFY_CA" => TlsMode::VerifyCa,
                    "VERIFY_IDENTITY" => TlsMode::VerifyFull,
                    other => return Err(format!("unknown sslMode '{other}'")),
                });
            }
        }
    }
    Ok(ParsedNetworkUrl {
        host,
        port,
        user,
        password,
        database,
        tls_mode,
    })
}

fn parse_host_port(hostport: &str) -> Result<(String, Option<u16>), String> {
    if let Some(bracketed) = hostport.strip_prefix('[') {
        let (host, rest) = bracketed
            .split_once(']')
            .ok_or_else(|| "the IPv6 host is missing its closing ']'".to_owned())?;
        if host.is_empty() {
            return Err("the URL has no host".into());
        }
        let port = match rest.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if rest.is_empty() => None,
            None => return Err(format!("unexpected characters after the host: '{rest}'")),
        };
        return Ok((host.to_owned(), port));
    }
    match hostport.split_once(':') {
        Some((host, port)) => {
            if hostport.matches(':').count() > 1 {
                return Err("an IPv6 host must be written bracketed, like [::1]".into());
            }
            if host.is_empty() {
                return Err("the URL has no host".into());
            }
            if port.is_empty() {
                return Err("the port after ':' is empty".into());
            }
            Ok((host.to_owned(), Some(parse_port(port)?)))
        }
        None => {
            if hostport.is_empty() {
                return Err("the URL has no host".into());
            }
            Ok((hostport.to_owned(), None))
        }
    }
}

fn parse_port(port: &str) -> Result<u16, String> {
    port.parse::<u16>()
        .map_err(|_| format!("invalid port '{port}'"))
}

fn percent_decode(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = input
                .get(i + 1..i + 3)
                .ok_or_else(|| format!("incomplete percent-escape at the end of '{input}'"))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|_| format!("invalid percent-escape '%{hex}'"))?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("'{input}' is not valid UTF-8 after decoding"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_url_with_sslmode() {
        let parsed =
            parse_postgres_url("postgres://alice:s3cret@db.example.com:5433/app?sslmode=require")
                .unwrap();
        assert_eq!(parsed.host, "db.example.com");
        assert_eq!(parsed.port, Some(5433));
        assert_eq!(parsed.user, "alice");
        assert_eq!(parsed.password.as_deref(), Some("s3cret"));
        assert_eq!(parsed.database.as_deref(), Some("app"));
        assert_eq!(parsed.tls_mode, Some(TlsMode::Require));
    }

    #[test]
    fn minimal_url_keeps_defaults_empty() {
        let parsed = parse_postgres_url("postgresql://localhost").unwrap();
        assert_eq!(parsed.host, "localhost");
        assert_eq!(parsed.port, None);
        assert_eq!(parsed.user, "");
        assert_eq!(parsed.password, None);
        assert_eq!(parsed.database, None);
        assert_eq!(parsed.tls_mode, None);
    }

    #[test]
    fn ipv6_and_percent_encoding() {
        let parsed = parse_postgres_url("postgres://bob:p%40ss%3Aw@[::1]:5432/my%20db").unwrap();
        assert_eq!(parsed.host, "::1");
        assert_eq!(parsed.port, Some(5432));
        assert_eq!(parsed.password.as_deref(), Some("p@ss:w"));
        assert_eq!(parsed.database.as_deref(), Some("my db"));
    }

    #[test]
    fn mysql_url_with_ssl_mode() {
        let parsed =
            parse_mysql_url("mysql://fgtest:fgtest@127.0.0.1:53306/app?sslMode=VERIFY_CA").unwrap();
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, Some(53306));
        assert_eq!(parsed.user, "fgtest");
        assert_eq!(parsed.password.as_deref(), Some("fgtest"));
        assert_eq!(parsed.database.as_deref(), Some("app"));
        assert_eq!(parsed.tls_mode, Some(TlsMode::VerifyCa));
    }

    #[test]
    fn mysql_url_rejects_other_schemes() {
        assert!(parse_mysql_url("postgres://host/db").is_err());
        assert!(parse_mysql_url("mysql://host/db?sslMode=BOGUS").is_err());
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(parse_postgres_url("mysql://localhost/db").is_err());
        assert!(parse_postgres_url("postgres://").is_err());
        assert!(parse_postgres_url("postgres://host:notaport/db").is_err());
        assert!(parse_postgres_url("postgres://host/db?sslmode=bogus").is_err());
        assert!(parse_postgres_url("postgres://host/db?sslmode=").is_err());
    }
}
