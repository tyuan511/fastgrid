//! The screen shown when the local store exists but its device key is not in the system
//! keychain (a reinstall, a keychain reset). The app cannot open the store at all in that state,
//! so this view does not need a service: it explains what happened, what is safe, and offers the
//! only actions that make sense before any data is touched.
//!
//! Two rules the screen exists to enforce: the old store file is never deleted or overwritten
//! (starting fresh moves it aside under a new name), and nothing here writes a new key over the
//! old one — if the keychain entry ever comes back, the moved-aside file can be moved back and
//! opened again.

use std::path::{Path, PathBuf};

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// What the screen asks its owner to do. The owner (the binary's startup path) owns the store
/// file and the service, so it performs the move and the restart; this view only renders.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryEvent {
    /// Try opening the store again: the keychain may have been restored without relaunching.
    Retry,
    /// Move the unreachable store aside and start with a fresh, empty one.
    StartFresh,
    /// Leave everything exactly as it is and quit.
    Quit,
}

/// Move a store file aside so a fresh one can take its place, without deleting anything. The
/// sidecar files (`-wal`, `-shm`) move with it when they exist. Returns the path the main file
/// moved to. A missing main file is an error: starting fresh over nothing would hide a bug.
pub fn move_store_aside(store_path: &Path) -> Result<PathBuf, String> {
    if !store_path.exists() {
        return Err(format!("no store file at {}", store_path.display()));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let aside = store_path.with_file_name(format!(
        "{}.unrecoverable-{stamp}",
        store_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "store.db".to_owned())
    ));
    let move_one = |from: &Path, to: &Path| -> Result<(), String> {
        if !from.exists() {
            return Ok(());
        }
        std::fs::rename(from, to).map_err(|e| format!("could not move {}: {e}", from.display()))
    };
    move_one(store_path, &aside)?;
    for suffix in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{suffix}", store_path.display()));
        let to = PathBuf::from(format!("{}{suffix}", aside.display()));
        // The main file already moved: report, but do not roll back — the caller must see
        // the partial state rather than silently starting over it.
        move_one(&from, &to)?;
    }
    Ok(aside)
}

/// The recovery screen itself.
pub struct Recovery {
    store_path: PathBuf,
    /// The first Start-fresh click only arms the button; the second one acts. Deleting access to
    /// a whole store — even with the file kept aside — must never happen on one stray click.
    confirm_fresh: bool,
    error: Option<String>,
    recovered: bool,
    aside_path: Option<PathBuf>,
}

impl EventEmitter<RecoveryEvent> for Recovery {}

impl Recovery {
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            confirm_fresh: false,
            error: None,
            recovered: false,
            aside_path: None,
        }
    }

    /// Show a failure from the owner's retry or start-fresh attempt.
    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        self.confirm_fresh = false;
        cx.notify();
    }

    /// The owner opened a fresh store: this screen's job is done, apart from its Close button.
    pub fn set_recovered(&mut self, aside_path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.recovered = true;
        self.aside_path = aside_path;
        self.error = None;
        cx.notify();
    }

    /// Whether the start-fresh button is armed, for tests.
    pub fn fresh_armed(&self) -> bool {
        self.confirm_fresh
    }

    /// Whether recovery succeeded, for tests.
    pub fn is_recovered(&self) -> bool {
        self.recovered
    }
}

impl Render for Recovery {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let is_zh = crate::views::style::is_zh_locale();
        if self.recovered {
            return v_flex()
                .id("recovery")
                .test_support()
                .size_full()
                .gap_3()
                .p_4()
                .bg(theme.background)
                .child(
                    div()
                        .text_size(px(16.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(if is_zh {
                            "已恢复，打开了一个新的空数据库"
                        } else {
                            "Recovered with a fresh, empty database"
                        }),
                )
                .when_some(self.aside_path.clone(), |d, aside| {
                    d.child(
                        div()
                            .id("recovery-aside")
                            .test_support()
                            .aria_label(aside.display().to_string())
                            .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .child(if is_zh {
                                format!("旧数据库文件已保留，未删除：{}", aside.display())
                            } else {
                                format!(
                                    "Your old database file was kept, not deleted: {}",
                                    aside.display()
                                )
                            }),
                    )
                })
                .child(
                    Button::new("recovery-close")
                        .label(if is_zh {
                            "关闭此窗口"
                        } else {
                            "Close this window"
                        })
                        .on_click(|_, window, _| {
                            window.remove_window();
                        }),
                )
                .into_any_element();
        }
        let title = if is_zh {
            "打不开本地数据库：系统钥匙串里没有它的密钥"
        } else {
            "Cannot open the local database: its key is not in the system keychain"
        };
        let body = if is_zh {
            "这通常发生在重装系统或钥匙串被重置之后。你的数据库文件本身没有被改动，也没有新密钥覆盖旧密钥：只要钥匙串里的条目恢复（比如从系统备份还原），原来的数据就能重新打开。"
        } else {
            "This usually follows a system reinstall or a keychain reset. Your database file itself was not modified, and no new key overwrote the old one: if the keychain entry ever comes back — for example from a system backup — the original data can be opened again."
        };
        let fresh_label = if self.confirm_fresh {
            if is_zh {
                "确认：放弃旧数据库，重新开始（旧文件会保留）"
            } else {
                "Confirm: abandon the old database and start fresh (the old file is kept)"
            }
        } else if is_zh {
            "重新开始（旧文件会保留，不删除）"
        } else {
            "Start fresh (the old file is kept, not deleted)"
        };
        v_flex()
            .id("recovery")
            .test_support()
            .size_full()
            .gap_3()
            .p_4()
            .bg(theme.background)
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(body),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(if is_zh {
                        format!("数据库文件：{}", self.store_path.display())
                    } else {
                        format!("Database file: {}", self.store_path.display())
                    }),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .id("recovery-error")
                        .test_support()
                        .aria_label(error.clone())
                        .text_size(px(13.))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(
                        Button::new("recovery-retry")
                            .label(if is_zh { "再试一次" } else { "Try again" })
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(RecoveryEvent::Retry);
                            })),
                    )
                    .child(
                        Button::new("recovery-fresh")
                            .label(fresh_label)
                            .danger()
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.confirm_fresh {
                                    cx.emit(RecoveryEvent::StartFresh);
                                } else {
                                    this.confirm_fresh = true;
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("recovery-quit")
                            .label(if is_zh {
                                "退出，什么都不动"
                            } else {
                                "Quit, change nothing"
                            })
                            .ghost()
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(RecoveryEvent::Quit);
                            })),
                    ),
            )
            .into_any_element()
    }
}
