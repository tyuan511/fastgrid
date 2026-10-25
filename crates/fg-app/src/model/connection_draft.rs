//! The connection form's fields, and their conversion to and from a saved connection.
//!
//! Only SQLite is editable for now (a file path); network engines get host / port / user / TLS /
//! SSH fields together with their drivers (M4, M5). Everything the form does not show is carried
//! over untouched, so editing a connection never drops settings it cannot display.

use fg_core::{
    ConnectionConfig, ConnectionId, ConnectionParams, DbKind, Environment, SafeMode, TagColor,
};

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

#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionDraft {
    /// `Some` when editing an existing connection.
    pub id: Option<ConnectionId>,
    pub name: String,
    pub environment: Environment,
    pub color: Option<TagColor>,
    pub path: String,
    /// Create the file if it does not exist (otherwise a typo'd path fails instead of making an
    /// empty database).
    pub create_if_missing: bool,
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
            path: String::new(),
            create_if_missing: false,
            group_id: None,
            safe_mode: None,
        }
    }

    pub fn from_config(c: &ConnectionConfig) -> Self {
        Self {
            id: Some(c.id.clone()),
            name: c.name.clone(),
            environment: c.environment,
            color: c.color,
            path: match &c.params {
                ConnectionParams::File { path } => path.clone(),
                ConnectionParams::Network { .. } => String::new(),
            },
            create_if_missing: c.option_enabled("create"),
            group_id: c.group_id.clone(),
            safe_mode: c.safe_mode,
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
    /// are kept); `None` makes a new one.
    pub fn to_config(&self, base: Option<&ConnectionConfig>) -> Result<ConnectionConfig, String> {
        let name = self.name.trim();
        let path = self.path.trim();
        if path.is_empty() {
            return Err("Choose a database file.".into());
        }
        let name = if name.is_empty() {
            Self::default_name_for(path)
        } else {
            name.to_owned()
        };

        let mut config = match base {
            Some(b) => b.clone(),
            None => ConnectionConfig::new(
                name.clone(),
                DbKind::Sqlite,
                ConnectionParams::File {
                    path: path.to_owned(),
                },
            ),
        };
        if let Some(id) = &self.id {
            config.id = id.clone();
        }
        config.name = name;
        config.environment = self.environment;
        config.color = self.color;
        config.group_id = self.group_id.clone();
        config.safe_mode = self.safe_mode;
        config.params = ConnectionParams::File {
            path: path.to_owned(),
        };
        if self.create_if_missing {
            config.options.insert("create".into(), "true".into());
        } else {
            config.options.remove("create");
        }
        Ok(config)
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
