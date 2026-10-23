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

/// fastgrid's own look: a calm cool-grey surface, one indigo accent, 13px type (the macOS and
/// TablePlus size), 8px radii.
/// Applied on top of the kit theme (light or dark) every time the theme changes.
pub fn compact_type(cx: &mut App) {
    use gpui_kit::{hsla, px};
    let theme = Theme::global_mut(cx);
    theme.font_size = px(13.);
    theme.mono_font_size = px(13.);
    theme.radius = px(8.);
    theme.radius_lg = px(12.);

    let dark = theme.is_dark();
    // (hue 0-360, saturation, lightness) shorthand.
    let c = |h: f32, s: f32, l: f32| hsla(h / 360., s, l, 1.);
    let accent = if dark {
        c(239., 0.90, 0.72)
    } else {
        c(239., 0.80, 0.58)
    };
    theme.primary = accent;
    theme.primary_hover = accent.opacity(0.88);
    theme.primary_active = accent.opacity(0.76);
    theme.primary_foreground = hsla(0., 0., 1., 1.);
    theme.ring = accent.opacity(0.5);
    theme.selection = accent.opacity(0.25);
    theme.list_active = accent.opacity(if dark { 0.20 } else { 0.11 });
    theme.list_active_border = accent.opacity(0.0);
    theme.tokens.table_active = hsla(0., 0., 0., 0.).into();

    if dark {
        theme.background = c(222., 0.16, 0.085);
        theme.foreground = c(220., 0.14, 0.92);
        theme.sidebar = c(222., 0.16, 0.105);
        theme.tab_bar = c(222., 0.16, 0.085);
        theme.border = c(222., 0.12, 0.17);
        theme.muted = c(222., 0.13, 0.15);
        theme.muted_foreground = c(220., 0.09, 0.62);
        // Headers sit on the grid's own surface, as in TablePlus.
        theme.table_head = theme.background;
        theme.tokens.table_head = theme.background.into();
        theme.table_even = c(222., 0.14, 0.105);
        theme.list_hover = c(222., 0.13, 0.14);
    } else {
        theme.background = c(0., 0., 1.);
        theme.foreground = c(222., 0.25, 0.12);
        theme.sidebar = c(220., 0.20, 0.975);
        theme.tab_bar = c(0., 0., 1.);
        theme.border = c(220., 0.16, 0.91);
        theme.muted = c(220., 0.18, 0.95);
        theme.muted_foreground = c(220., 0.10, 0.44);
        theme.table_head = theme.background;
        theme.tokens.table_head = theme.background.into();
        theme.table_even = c(220., 0.12, 0.97);
        theme.list_hover = c(220., 0.18, 0.95);
    }
    // TablePlus stripes alternate rows and draws no lines between them; the column lines come
    // from the cells themselves (see `views::cells`), since this token also draws row borders.
    theme.tokens.table_even = theme.table_even.into();
    theme.table_row_border = hsla(0., 0., 0., 0.);
    theme.title_bar = theme.background;
    theme.title_bar_border = theme.border;
    theme.tab_active = theme.background;
    theme.tab_active_foreground = theme.foreground;
    theme.tab_foreground = theme.muted_foreground;
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
                compact_type(cx);
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
    compact_type(cx);
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
