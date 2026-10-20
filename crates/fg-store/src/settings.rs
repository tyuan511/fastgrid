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
}
