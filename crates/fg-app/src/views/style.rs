//! Colours come from the theme tokens, never hard-coded, so light and dark both work.

use fg_core::{Environment, TagColor};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{App, Hsla};

/// The colour of an environment marker: the user's tag colour if they chose one, otherwise what
/// the environment implies (production red, staging amber, development green).
pub fn environment_color(cx: &App, environment: Environment, tag: Option<TagColor>) -> Hsla {
    let theme = cx.theme();
    match tag {
        Some(TagColor::Red) => theme.danger,
        Some(TagColor::Orange | TagColor::Yellow) => theme.warning,
        Some(TagColor::Green) => theme.success,
        Some(TagColor::Blue) => theme.info,
        Some(TagColor::Purple) => theme.chart_4,
        Some(TagColor::Gray) => theme.muted_foreground,
        Some(_) | None => match environment {
            Environment::Staging => theme.warning,
            Environment::Development => theme.success,
            // Production, and anything newer this build does not know: treat as production.
            _ => theme.danger,
        },
    }
}

/// Detects if the current system environment is set to Chinese.
pub fn is_zh_locale() -> bool {
    static IS_ZH: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *IS_ZH.get_or_init(|| {
        if let Ok(lang) = std::env::var("LANG")
            && (lang.starts_with("zh") || lang.starts_with("ZH"))
        {
            return true;
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(output) = std::process::Command::new("defaults")
                .args(["read", "-g", "AppleLanguages"])
                .output()
            {
                let s = String::from_utf8_lossy(&output.stdout);
                if s.contains("zh-Hans") || s.contains("zh-Hant") || s.contains("zh_CN") {
                    return true;
                }
            }
        }
        false
    })
}
