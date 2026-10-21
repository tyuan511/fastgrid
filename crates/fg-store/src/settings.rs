use fg_core::{Result, SettingGroup};

use crate::record::Loaded;
use crate::{Store, kinds};

impl Store {
    /// The stored settings group, or its defaults when absent or unreadable
    /// (a bad settings row must never stop the app from starting).
    pub fn setting<T: SettingGroup>(&self) -> Result<T> {
        Ok(self
            .get_json::<T>(kinds::SETTING, T::KEY)?
            .and_then(Loaded::ok)
            .unwrap_or_default())
    }

    pub fn set_setting<T: SettingGroup>(&self, value: &T) -> Result<()> {
        self.put_json(kinds::SETTING, T::KEY, None, None, value)
    }

    /// Like [`Self::setting`], but a row that decrypts and does not deserialize is an error.
    ///
    /// A missing row is still the default: the group was never saved. An unreadable row is not
    /// the default. Security-relevant groups (`ExecSettings`) must use this so a newer payload
    /// cannot silently widen the row cap. Appearance and grid settings stay on [`Self::setting`];
    /// a bad appearance row must not stop the app from starting.
    pub fn setting_strict<T: SettingGroup>(&self) -> Result<T> {
        match self.get_json::<T>(kinds::SETTING, T::KEY)? {
            None => Ok(T::default()),
            Some(Loaded::Ok(value)) => Ok(value),
            Some(Loaded::Unreadable { reason, .. }) => Err(fg_core::Error::Storage(format!(
                "settings group {} cannot be read ({reason})",
                T::KEY
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{AppearanceSettings, GridSettings, KeybindingOverrides, ThemeMode};

    use super::*;
    use crate::testutil::store;

    #[test]
    fn defaults_when_missing_then_roundtrip() {
        let s = store();
        assert_eq!(
            s.setting::<GridSettings>().unwrap(),
            GridSettings::default()
        );
        let g = GridSettings {
            page_size: 1000,
            ..GridSettings::default()
        };
        s.set_setting(&g).unwrap();
        assert_eq!(s.setting::<GridSettings>().unwrap(), g);

        let mut keys = KeybindingOverrides::default();
        keys.0.insert("grid.commit".into(), "cmd-s".into());
        s.set_setting(&keys).unwrap();
        assert_eq!(s.setting::<KeybindingOverrides>().unwrap(), keys);
    }

    #[test]
    fn unreadable_settings_fall_back_to_defaults() {
        let s = store();
        s.put_json(kinds::SETTING, "appearance", None, None, &"not an object")
            .unwrap();
        assert_eq!(
            s.setting::<AppearanceSettings>().unwrap().theme,
            ThemeMode::System
        );
    }

    #[test]
    fn strict_settings_error_when_the_payload_does_not_deserialize() {
        use fg_core::ExecSettings;

        let s = store();
        assert_eq!(
            s.setting_strict::<ExecSettings>().unwrap(),
            ExecSettings::default()
        );
        s.put_json(
            kinds::SETTING,
            ExecSettings::KEY,
            None,
            None,
            &"not an object",
        )
        .unwrap();
        let err = s.setting_strict::<ExecSettings>().unwrap_err();
        assert!(matches!(err, fg_core::Error::Storage(_)), "{err:?}");
        // The lenient path is unchanged: a bad appearance row still yields defaults.
        assert_eq!(
            s.setting::<AppearanceSettings>().unwrap().theme,
            ThemeMode::System
        );
    }
}
