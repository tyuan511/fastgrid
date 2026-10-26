//! The connection form's fields, and their conversion to and from a saved connection.
//!
//! SQLite shows a file path; network engines (PostgreSQL, MySQL) show host / port / user /
//! database / TLS / SSH fields. Passwords are form-only: they are never read back from a saved
//! connection, and a blank password field means "keep the saved secret". Everything the form
//! does not show is carried over untouched, so editing a connection never drops settings it
//! cannot display.

use fg_core::{
    ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Environment, SafeMode, SshAuth,
    SshConfig, TagColor, TlsConfig, TlsMode,
};

use super::connection_url::ParsedNetworkUrl;

/// Colour choices in menu order; `None` = follow the environment.
pub const TAG_COLORS: &[(Option<TagColor>, &str)] = &[
    (None, "Default"),
    (Some(TagColor::Red), "Red"),
    (Some(TagColor::Orange), "Orange"),
    (Some(TagColor::Yellow), "Yellow"),
    (Some(TagColor::Green), "Green"),
    (Some(TagColor::Blue), "Blue"),
    (Some(TagColor::Purple), "Purple"),
    (Some(TagColor::Gray), "Gray"),
];

pub const ENVIRONMENTS: &[(Environment, &str)] = &[
    (Environment::Development, "Development"),
    (Environment::Staging, "Staging"),
    (Environment::Production, "Production"),
];

pub fn color_label(c: Option<TagColor>) -> &'static str {
    TAG_COLORS
        .iter()
        .find(|(v, _)| *v == c)
        .map_or("Default", |(_, l)| l)
}

pub fn color_from_label(label: &str) -> Option<TagColor> {
    TAG_COLORS
        .iter()
        .find(|(_, l)| *l == label)
        .and_then(|(c, _)| *c)
}

pub fn environment_label(e: Environment) -> &'static str {
    ENVIRONMENTS
        .iter()
        .find(|(v, _)| *v == e)
        .map_or("Development", |(_, l)| l)
}

pub fn environment_from_label(label: &str) -> Environment {
    ENVIRONMENTS
        .iter()
        .find(|(_, l)| *l == label)
        .map_or(Environment::Development, |(e, _)| *e)
}

/// Safe-mode choices in menu order; `None` = follow the environment.
pub const SAFE_MODES: &[(Option<SafeMode>, &str)] = &[
    (None, "Default"),
    (Some(SafeMode::Normal), "Normal"),
    (Some(SafeMode::Alert), "Alert"),
    (Some(SafeMode::Strict), "Strict"),
    (Some(SafeMode::ReadOnly), "Read-only"),
];

pub fn safe_mode_label(m: Option<SafeMode>) -> &'static str {
    SAFE_MODES
        .iter()
        .find(|(v, _)| *v == m)
        .map_or("Default", |(_, l)| l)
}

pub fn safe_mode_from_label(label: &str) -> Option<SafeMode> {
    SAFE_MODES
        .iter()
        .find(|(_, l)| *l == label)
        .and_then(|(m, _)| *m)
}

/// TLS choices in menu order.
pub const TLS_MODES: &[(TlsMode, &str)] = &[
    (TlsMode::Prefer, "Prefer"),
    (TlsMode::Disable, "Disable"),
    (TlsMode::Require, "Require"),
    (TlsMode::VerifyCa, "Verify CA"),
    (TlsMode::VerifyFull, "Verify full"),
];

pub fn tls_mode_label(m: TlsMode) -> &'static str {
    TLS_MODES
        .iter()
        .find(|(v, _)| *v == m)
        .map_or("Prefer", |(_, l)| l)
}

pub fn tls_mode_from_label(label: &str) -> TlsMode {
    TLS_MODES
        .iter()
        .find(|(_, l)| *l == label)
        .map_or(TlsMode::Prefer, |(m, _)| *m)
}

/// SSH authentication choices in menu order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthChoice {
    Password,
    PrivateKey,
    Agent,
}

pub const SSH_AUTHS: &[(SshAuthChoice, &str)] = &[
    (SshAuthChoice::Password, "Password"),
    (SshAuthChoice::PrivateKey, "Private key"),
    (SshAuthChoice::Agent, "SSH agent"),
];

pub fn ssh_auth_label(a: SshAuthChoice) -> &'static str {
    SSH_AUTHS
        .iter()
        .find(|(v, _)| *v == a)
        .map_or("Password", |(_, l)| l)
}

pub fn ssh_auth_from_label(label: &str) -> SshAuthChoice {
    SSH_AUTHS
        .iter()
        .find(|(_, l)| *l == label)
        .map_or(SshAuthChoice::Password, |(a, _)| *a)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionDraft {
    /// `Some` when editing an existing connection.
    pub id: Option<ConnectionId>,
    pub name: String,
    pub environment: Environment,
    pub color: Option<TagColor>,
    pub kind: DbKind,
    /// The URL box content; form-only, never saved. Kept so parsing a URL
    /// does not clear the box the URL came from.
    pub url: String,
    pub path: String,
    /// Create the file if it does not exist (otherwise a typo'd path fails instead of making an
    /// empty database).
    pub create_if_missing: bool,
    /// Network host; empty means the form has not filled it yet.
    pub host: String,
    /// Network port as typed; empty means the engine default.
    pub port: String,
    pub user: String,
    /// Database to open; empty means the server default (usually the user name).
    pub database: String,
    /// Typed database password; blank keeps the saved one.
    pub password: String,
    pub tls_mode: TlsMode,
    pub tls_ca_path: String,
    pub tls_cert_path: String,
    pub tls_key_path: String,
    pub tls_server_name: String,
    pub use_ssh: bool,
    pub ssh_host: String,
    /// SSH port as typed; empty means 22.
    pub ssh_port: String,
    pub ssh_user: String,
    pub ssh_auth: SshAuthChoice,
    pub ssh_key_path: String,
    /// Typed SSH password or key passphrase; blank keeps the saved one.
    pub ssh_secret: String,
    /// The group the connection belongs to, if any.
    pub group_id: Option<fg_core::GroupId>,
    /// Explicit safe mode; `None` follows the environment's default.
    pub safe_mode: Option<SafeMode>,
}

impl ConnectionDraft {
    pub fn blank() -> Self {
        Self {
            id: None,
            name: String::new(),
            environment: Environment::Development,
            color: None,
            kind: DbKind::Sqlite,
            url: String::new(),
            path: String::new(),
            create_if_missing: false,
            host: String::new(),
            port: String::new(),
            user: String::new(),
            database: String::new(),
            password: String::new(),
            tls_mode: TlsMode::Prefer,
            tls_ca_path: String::new(),
            tls_cert_path: String::new(),
            tls_key_path: String::new(),
            tls_server_name: String::new(),
            use_ssh: false,
            ssh_host: String::new(),
            ssh_port: String::new(),
            ssh_user: String::new(),
            ssh_auth: SshAuthChoice::Password,
            ssh_key_path: String::new(),
            ssh_secret: String::new(),
            group_id: None,
            safe_mode: None,
        }
    }

    pub fn from_config(c: &ConnectionConfig) -> Self {
        let mut draft = Self::blank();
        draft.id = Some(c.id.clone());
        draft.name = c.name.clone();
        draft.environment = c.environment;
        draft.color = c.color;
        draft.kind = c.kind;
        draft.create_if_missing = c.option_enabled("create");
        // Passwords are never read back; the URL box starts empty.
        draft.group_id = c.group_id.clone();
        draft.safe_mode = c.safe_mode;
        match &c.params {
            ConnectionParams::File { path } => {
                draft.path = path.clone();
            }
            ConnectionParams::Network {
                host,
                port,
                user,
                database,
                ..
            } => {
                draft.host = host.clone();
                draft.port = port.to_string();
                draft.user = user.clone();
                draft.database = database.clone().unwrap_or_default();
            }
        }
        if let Some(tls) = &c.tls {
            draft.tls_mode = tls.mode;
            draft.tls_ca_path = tls.ca_cert_path.clone().unwrap_or_default();
            draft.tls_cert_path = tls.client_cert_path.clone().unwrap_or_default();
            draft.tls_key_path = tls.client_key_path.clone().unwrap_or_default();
            draft.tls_server_name = tls.server_name.clone().unwrap_or_default();
        }
        if let Some(ssh) = &c.ssh {
            draft.use_ssh = true;
            draft.ssh_host = ssh.host.clone();
            draft.ssh_port = ssh.port.to_string();
            draft.ssh_user = ssh.user.clone();
            match &ssh.auth {
                SshAuth::Password => draft.ssh_auth = SshAuthChoice::Password,
                SshAuth::PrivateKey { key_path } => {
                    draft.ssh_auth = SshAuthChoice::PrivateKey;
                    draft.ssh_key_path = key_path.clone();
                }
                SshAuth::Agent => draft.ssh_auth = SshAuthChoice::Agent,
            }
        }
        draft
    }

    /// Fill the network fields from a parsed URL. Fields the URL does not
    /// carry (an absent port, an absent sslmode) are left alone.
    pub fn apply_url(&mut self, kind: DbKind, parsed: &ParsedNetworkUrl) {
        self.kind = kind;
        self.host = parsed.host.clone();
        if let Some(port) = parsed.port {
            self.port = port.to_string();
        }
        self.user = parsed.user.clone();
        if let Some(password) = &parsed.password {
            self.password = password.clone();
        }
        if let Some(database) = &parsed.database {
            self.database = database.clone();
        }
        if let Some(mode) = parsed.tls_mode {
            self.tls_mode = mode;
        }
    }

    /// A suggestion for the name when the user picks a file and has typed none.
    pub fn default_name_for(path: &str) -> String {
        std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "SQLite".into())
    }

    /// Build the connection to save. `base` is the connection being edited (its other settings
    /// are kept); `None` makes a new one. Passwords are not part of the config: the caller
    /// saves them separately from [`ConnectionDraft::password`] and
    /// [`ConnectionDraft::ssh_secret`].
    pub fn to_config(&self, base: Option<&ConnectionConfig>) -> Result<ConnectionConfig, String> {
        let name = self.name.trim().to_owned();
        let (params, default_name) = match self.kind {
            DbKind::Sqlite => {
                let path = self.path.trim();
                if path.is_empty() {
                    return Err("Choose a database file.".into());
                }
                (
                    ConnectionParams::File {
                        path: path.to_owned(),
                    },
                    Self::default_name_for(path),
                )
            }
            DbKind::Postgres | DbKind::MySql => {
                let host = self.host.trim();
                if host.is_empty() {
                    return Err("Enter a host.".into());
                }
                let default_port = match self.kind {
                    DbKind::MySql => 3306,
                    _ => 5432,
                };
                let port = parse_port_field(&self.port, default_port)?;
                let user = self.user.trim();
                if user.is_empty() {
                    return Err("Enter a user.".into());
                }
                let database = self.database.trim();
                (
                    ConnectionParams::Network {
                        host: host.to_owned(),
                        port,
                        user: user.to_owned(),
                        database: if database.is_empty() {
                            None
                        } else {
                            Some(database.to_owned())
                        },
                        socket: None,
                    },
                    match database {
                        "" => host.to_owned(),
                        db => format!("{host}/{db}"),
                    },
                )
            }
            // The form only offers SQLite, PostgreSQL and MySQL; anything else keeps
            // the base params untouched (this arm is unreachable from the UI).
            _ => match base {
                Some(b) => (b.params.clone(), b.name.clone()),
                None => return Err("Choose a database engine.".into()),
            },
        };
        let name = if name.is_empty() { default_name } else { name };

        let mut config = match base {
            Some(b) => b.clone(),
            None => ConnectionConfig::new(name.clone(), self.kind, params.clone()),
        };
        if let Some(id) = &self.id {
            config.id = id.clone();
        }
        config.name = name;
        config.kind = self.kind;
        config.environment = self.environment;
        config.color = self.color;
        config.group_id = self.group_id.clone();
        config.safe_mode = self.safe_mode;
        config.params = params;
        match self.kind {
            DbKind::Sqlite => {
                config.ssh = None;
                config.tls = None;
                if self.create_if_missing {
                    config.options.insert("create".into(), "true".into());
                } else {
                    config.options.remove("create");
                }
            }
            DbKind::Postgres | DbKind::MySql => {
                config.options.remove("create");
                let tls = TlsConfig {
                    mode: self.tls_mode,
                    ca_cert_path: some_trimmed(&self.tls_ca_path),
                    client_cert_path: some_trimmed(&self.tls_cert_path),
                    client_key_path: some_trimmed(&self.tls_key_path),
                    server_name: some_trimmed(&self.tls_server_name),
                };
                // Prefer with no paths is the driver's default: store no TLS
                // block at all so an edited connection can drop back to it.
                config.tls = if tls.mode == TlsMode::Prefer
                    && tls.ca_cert_path.is_none()
                    && tls.client_cert_path.is_none()
                    && tls.client_key_path.is_none()
                    && tls.server_name.is_none()
                {
                    None
                } else {
                    Some(tls)
                };
                config.ssh = if self.use_ssh {
                    let ssh_host = self.ssh_host.trim();
                    if ssh_host.is_empty() {
                        return Err("Enter an SSH host.".into());
                    }
                    let ssh_port = parse_port_field(&self.ssh_port, 22)?;
                    let ssh_user = self.ssh_user.trim();
                    if ssh_user.is_empty() {
                        return Err("Enter an SSH user.".into());
                    }
                    let auth = match self.ssh_auth {
                        SshAuthChoice::Password => SshAuth::Password,
                        SshAuthChoice::Agent => SshAuth::Agent,
                        SshAuthChoice::PrivateKey => {
                            let key_path = self.ssh_key_path.trim();
                            if key_path.is_empty() {
                                return Err("Choose an SSH private key.".into());
                            }
                            SshAuth::PrivateKey {
                                key_path: key_path.to_owned(),
                            }
                        }
                    };
                    Some(SshConfig {
                        host: ssh_host.to_owned(),
                        port: ssh_port,
                        user: ssh_user.to_owned(),
                        auth,
                        keepalive_secs: None,
                    })
                } else {
                    None
                };
            }
            _ => {}
        }
        Ok(config)
    }
}

/// A port field as typed: blank means `default`, anything else must be a port number.
fn parse_port_field(field: &str, default: u16) -> Result<u16, String> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    trimmed
        .parse::<u16>()
        .map_err(|_| format!("'{trimmed}' is not a valid port"))
}

/// A trimmed field, or `None` when blank.
fn some_trimmed(field: &str) -> Option<String> {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// A copy of `source` that can be saved next to it: new id, "name copy", no usage history.
pub fn duplicate(source: &ConnectionConfig) -> ConnectionConfig {
    let mut copy = source.clone();
    copy.id = ConnectionId::new();
    copy.name = format!("{} copy", source.name);
    copy.created_at = None;
    copy.updated_at = None;
    copy.last_used_at = None;
    copy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str, path: &str) -> ConnectionDraft {
        ConnectionDraft {
            name: name.into(),
            path: path.into(),
            ..ConnectionDraft::blank()
        }
    }

    #[test]
    fn the_safe_mode_survives_a_draft_round_trip() {
        let mut d = draft("x", "/data/x.db");
        d.safe_mode = Some(SafeMode::Strict);
        let c = d.to_config(None).unwrap();
        assert_eq!(c.safe_mode, Some(SafeMode::Strict));
        let back = ConnectionDraft::from_config(&c);
        assert_eq!(back.safe_mode, Some(SafeMode::Strict));
        let saved = back.to_config(Some(&c)).unwrap();
        assert_eq!(saved.safe_mode, Some(SafeMode::Strict));

        let d = draft("y", "/data/y.db");
        let c = d.to_config(None).unwrap();
        assert_eq!(c.safe_mode, None, "a new draft follows the environment");
        assert_eq!(ConnectionDraft::from_config(&c).safe_mode, None);
        assert_eq!(safe_mode_label(None), "Default");
        assert_eq!(safe_mode_from_label("Strict"), Some(SafeMode::Strict));
        assert_eq!(safe_mode_from_label("Default"), None);
    }

    #[test]
    fn the_group_survives_a_draft_round_trip() {
        let mut c = draft("x", "/data/x.db").to_config(None).unwrap();
        assert_eq!(c.group_id, None);
        c.group_id = Some(fg_core::GroupId::from_string("g1"));
        let back = ConnectionDraft::from_config(&c);
        assert_eq!(back.group_id, c.group_id);
        let saved = back.to_config(Some(&c)).unwrap();
        assert_eq!(saved.group_id, c.group_id);
    }

    #[test]
    fn a_new_connection_needs_a_file_and_gets_a_default_name() {
        assert!(draft("x", "  ").to_config(None).is_err());
        let c = draft("", "/data/shop.db").to_config(None).unwrap();
        assert_eq!(c.name, "shop");
        assert_eq!(c.kind, DbKind::Sqlite);
        assert!(matches!(&c.params, ConnectionParams::File { path } if path == "/data/shop.db"));
        assert!(
            !c.option_enabled("create"),
            "a typo'd path must not silently create a file"
        );
    }

    #[test]
    fn the_create_option_follows_the_checkbox() {
        let mut d = draft("n", "a.db");
        d.create_if_missing = true;
        assert!(d.to_config(None).unwrap().option_enabled("create"));
        d.create_if_missing = false;
        let base = {
            let mut b = ConnectionConfig::new(
                "n",
                DbKind::Sqlite,
                ConnectionParams::File {
                    path: "a.db".into(),
                },
            );
            b.options.insert("create".into(), "true".into());
            b
        };
        assert!(!d.to_config(Some(&base)).unwrap().option_enabled("create"));
    }

    #[test]
    fn editing_keeps_everything_the_form_does_not_show() {
        let mut base = ConnectionConfig::new(
            "old",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: "a.db".into(),
            },
        );
        base.startup_sql = vec!["PRAGMA foreign_keys = ON".into()];
        base.notes = Some("keep me".into());
        base.sort_order = 7;
        base.options.insert("readonly".into(), "true".into());

        let mut d = ConnectionDraft::from_config(&base);
        d.name = "renamed".into();
        d.environment = Environment::Production;
        d.color = Some(TagColor::Red);
        let edited = d.to_config(Some(&base)).unwrap();

        assert_eq!(edited.id, base.id, "same connection");
        assert_eq!(edited.name, "renamed");
        assert_eq!(edited.environment, Environment::Production);
        assert_eq!(edited.color, Some(TagColor::Red));
        assert_eq!(edited.startup_sql, base.startup_sql);
        assert_eq!(edited.notes.as_deref(), Some("keep me"));
        assert_eq!(edited.sort_order, 7);
        assert!(edited.option_enabled("readonly"));
    }

    #[test]
    fn the_draft_reads_back_a_saved_connection() {
        let mut c = ConnectionConfig::new(
            "shop",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: "/x/shop.db".into(),
            },
        );
        c.environment = Environment::Staging;
        c.color = Some(TagColor::Blue);
        c.options.insert("create".into(), "true".into());
        let d = ConnectionDraft::from_config(&c);
        assert_eq!((d.name.as_str(), d.path.as_str()), ("shop", "/x/shop.db"));
        assert_eq!(
            (d.environment, d.color, d.create_if_missing),
            (Environment::Staging, Some(TagColor::Blue), true)
        );
        assert_eq!(d.id.as_ref(), Some(&c.id));
    }

    #[test]
    fn duplicates_are_new_connections_with_a_clear_name() {
        let mut src = ConnectionConfig::new(
            "shop",
            DbKind::Sqlite,
            ConnectionParams::File {
                path: "a.db".into(),
            },
        );
        src.last_used_at = Some(fg_core::UnixMillis(5));
        let copy = duplicate(&src);
        assert_ne!(copy.id, src.id);
        assert_eq!(copy.name, "shop copy");
        assert_eq!(copy.params, src.params);
        assert!(copy.last_used_at.is_none() && copy.created_at.is_none());
    }

    fn network_draft() -> ConnectionDraft {
        ConnectionDraft {
            kind: DbKind::Postgres,
            host: "db.example.com".into(),
            port: "5433".into(),
            user: "alice".into(),
            database: "app".into(),
            ..ConnectionDraft::blank()
        }
    }

    #[test]
    fn network_draft_builds_network_params_with_defaults() {
        let mut d = network_draft();
        d.port = String::new();
        let c = d.to_config(None).unwrap();
        assert_eq!(c.kind, DbKind::Postgres);
        assert_eq!(
            c.params,
            ConnectionParams::Network {
                host: "db.example.com".into(),
                port: 5432,
                user: "alice".into(),
                database: Some("app".into()),
                socket: None,
            }
        );
        // Prefer with no paths stores no TLS block.
        assert!(c.tls.is_none());
        assert!(c.ssh.is_none());
        assert_eq!(c.name, "db.example.com/app");
    }

    #[test]
    fn network_draft_validates_its_fields() {
        let mut d = network_draft();
        d.host = "  ".into();
        assert!(d.to_config(None).is_err());
        let mut d = network_draft();
        d.port = "nope".into();
        assert!(d.to_config(None).is_err());
        let mut d = network_draft();
        d.user = String::new();
        assert!(d.to_config(None).is_err());
    }

    #[test]
    fn network_draft_round_trips_tls_and_ssh() {
        let mut d = network_draft();
        d.tls_mode = TlsMode::VerifyFull;
        d.tls_server_name = "db.example.com".into();
        d.use_ssh = true;
        d.ssh_host = "bastion.example.com".into();
        d.ssh_user = "ops".into();
        d.ssh_auth = SshAuthChoice::PrivateKey;
        d.ssh_key_path = "~/.ssh/id_ed25519".into();
        let c = d.to_config(None).unwrap();
        let back = ConnectionDraft::from_config(&c);
        assert_eq!(back.tls_mode, TlsMode::VerifyFull);
        assert_eq!(back.tls_server_name, "db.example.com");
        assert!(back.use_ssh);
        assert_eq!(back.ssh_host, "bastion.example.com");
        assert_eq!(back.ssh_port, "22");
        assert_eq!(back.ssh_auth, SshAuthChoice::PrivateKey);
        assert_eq!(back.ssh_key_path, "~/.ssh/id_ed25519");
        // Passwords are never read back.
        assert!(back.password.is_empty() && back.ssh_secret.is_empty());
    }

    #[test]
    fn url_fills_network_fields() {
        let mut d = ConnectionDraft::blank();
        let parsed =
            crate::model::parse_postgres_url("postgres://bob:pw@h:5432/db?sslmode=disable")
                .unwrap();
        d.apply_url(DbKind::Postgres, &parsed);
        assert_eq!(d.kind, DbKind::Postgres);
        assert_eq!(d.host, "h");
        assert_eq!(d.port, "5432");
        assert_eq!(d.user, "bob");
        assert_eq!(d.password, "pw");
        assert_eq!(d.database, "db");
        assert_eq!(d.tls_mode, TlsMode::Disable);
    }

    #[test]
    fn mysql_draft_defaults_to_port_3306() {
        let mut d = ConnectionDraft::blank();
        let parsed = crate::model::parse_mysql_url("mysql://fgtest:fgtest@127.0.0.1/app").unwrap();
        d.apply_url(DbKind::MySql, &parsed);
        assert_eq!(d.kind, DbKind::MySql);
        d.port = String::new();
        let c = d.to_config(None).unwrap();
        assert_eq!(
            c.params,
            ConnectionParams::Network {
                host: "127.0.0.1".into(),
                port: 3306,
                user: "fgtest".into(),
                database: Some("app".into()),
                socket: None,
            }
        );
    }

    #[test]
    fn labels_roundtrip() {
        for (c, label) in TAG_COLORS {
            assert_eq!(color_from_label(label), *c);
            assert_eq!(color_label(*c), *label);
        }
        for (e, label) in ENVIRONMENTS {
            assert_eq!(environment_from_label(label), *e);
            assert_eq!(environment_label(*e), *label);
        }
    }
}
