use std::path::PathBuf;

use fg_app::app_state::AppState;
use fg_core::Error;
use fg_service::{KeychainKeyProvider, MemoryBackend, Service, ServiceOptions};
use gpui_kit::component::TitleBar;
use gpui_kit::*;

/// `~/Library/Application Support/fastgrid` on macOS, `%APPDATA%\fastgrid` on Windows,
/// `~/.local/share/fastgrid` on Linux. `FASTGRID_DATA_DIR` overrides it (development, tests).
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FASTGRID_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("fastgrid")
}

fn main() {
    let options = ServiceOptions::new(data_dir().join("store.db"));
    // Development only: keep the device key in memory instead of the OS keychain, so a throwaway
    // data directory leaves nothing behind. The store cannot be reopened afterwards.
    let started = if std::env::var_os("FASTGRID_MEMORY_KEYCHAIN").is_some() {
        Service::start(
            options,
            &KeychainKeyProvider::with_backend(MemoryBackend::default()),
        )
    } else {
        Service::start(options, &KeychainKeyProvider::os())
    };

    gpui_kit::application()
        .with_assets(fg_app::views::FastgridAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            fg_app::views::bind_keys(cx);
            cx.activate(true);
            match started {
                Ok(service) => {
                    cx.set_global(AppState {
                        service: service.clone(),
                    });
                    fg_app::controller::apply_saved_appearance(service.clone(), cx);
                    open_launcher(cx, service);
                }
                Err(e) => {
                    eprintln!("fastgrid could not start: {e}");
                    if matches!(e, Error::KeyUnavailable(_)) {
                        eprintln!(
                            "The data store exists but its encryption key is not in the system \
                         keychain. Restore it by importing an encrypted backup or syncing."
                        );
                    }
                    cx.quit();
                }
            }
        });
}

fn open_launcher(cx: &mut App, service: Service) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(680.), px(480.)),
            cx,
        ))),
        window_min_size: Some(size(px(680.), px(480.))),
        ..TitleBar::window_options()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
        fg_app::theme_control::follow_system(window);
        fg_app::controller::launcher(service, window, cx)
    }) {
        eprintln!("failed to open the window: {e}");
        cx.quit();
    }
}
