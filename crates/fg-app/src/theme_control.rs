//! Light / dark / follow-the-system, applied to GPUI Kit's global theme.

use fg_core::ThemeMode;
use gpui_kit::component::{Theme, ThemeMode as KitMode};
use gpui_kit::{App, Global, Window};

/// The user's current choice, kept as a global so every window can follow it.
pub struct Chosen(pub ThemeMode);

impl Global for Chosen {}

pub fn chosen(cx: &App) -> ThemeMode {
    cx.try_global::<Chosen>().map_or(ThemeMode::System, |c| c.0)
}

/// Remember `mode` and apply it everywhere.
pub fn set_and_apply(mode: ThemeMode, cx: &mut App) {
    cx.set_global(Chosen(mode));
    apply(mode, cx);
}

/// Call once per window: while the choice is "System", follow the OS when it switches between
/// light and dark.
pub fn follow_system(window: &mut Window) {
    window
        .observe_window_appearance(|window, cx| {
            if chosen(cx) == ThemeMode::System {
                Theme::sync_system_appearance(Some(window), cx);
            }
        })
        .detach();
}

/// Make the window theme match `mode`. "System" follows the OS right now; the app re-applies it
/// when the appearance changes (see `controller::watch_system_appearance`).
pub fn apply(mode: ThemeMode, cx: &mut App) {
    match mode {
        ThemeMode::Light => Theme::change(KitMode::Light, None, cx),
        ThemeMode::Dark => Theme::change(KitMode::Dark, None, cx),
        _ => Theme::sync_system_appearance(None, cx),
    }
}

/// The next choice when the toggle is clicked: system, light, dark, then back to system.
pub fn next(mode: ThemeMode) -> ThemeMode {
    match mode {
        ThemeMode::System => ThemeMode::Light,
        ThemeMode::Light => ThemeMode::Dark,
        _ => ThemeMode::System,
    }
}

pub fn label(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Light => "Light",
        ThemeMode::Dark => "Dark",
        _ => "System",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_toggle_cycles_through_all_three() {
        let mut m = ThemeMode::System;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(label(m));
            m = next(m);
        }
        assert_eq!(seen, ["System", "Light", "Dark", "System"]);
        assert_eq!(
            next(ThemeMode::Unknown),
            ThemeMode::System,
            "an unknown saved mode resets"
        );
    }

    #[gpui_kit::test]
    fn applying_a_mode_changes_the_global_theme(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::component::ActiveTheme as _;
        cx.update(gpui_kit::init);
        cx.update(|cx| set_and_apply(ThemeMode::Dark, cx));
        assert!(cx.update(|cx| cx.theme().is_dark()));
        assert_eq!(cx.update(|cx| chosen(cx)), ThemeMode::Dark);
        cx.update(|cx| set_and_apply(ThemeMode::Light, cx));
        assert!(!cx.update(|cx| cx.theme().is_dark()));
    }
}
