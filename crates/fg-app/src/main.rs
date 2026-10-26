use std::path::PathBuf;

use fg_app::app_state::AppState;
use fg_core::Error;
use fg_service::{Service, ServiceOptions, default_key_provider};
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

fn start_service(data_dir: &std::path::Path) -> Result<Service, Error> {
    let options = ServiceOptions::new(data_dir.join("store.db"));
    let keys = default_key_provider(data_dir);
    Service::start(options, keys.as_ref())
}

fn main() {
    let data_dir = data_dir();
    // Logging must never fail startup: without it, tracing events are
    // dropped exactly as before, and the diagnostics export says so.
    if let Err(e) = fg_service::init_file_logging(&data_dir) {
        eprintln!("fastgrid: file logging unavailable: {e}");
    }
    let started = start_service(&data_dir);

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
                Err(Error::KeyUnavailable(_)) => {
                    // Do not quit: the store file is untouched, and the user may restore the
                    // keychain entry, or choose to start fresh with the old file kept aside.
                    // Any other startup failure still quits below.
                    open_recovery(cx, data_dir);
                }
                Err(e) => {
                    eprintln!("fastgrid could not start: {e}");
                    cx.quit();
                }
            }
        });
}

/// The recovery window: retries the start, or moves the unreachable store aside and starts
/// fresh. On success the launcher opens and this window reports itself recovered.
fn open_recovery(cx: &mut App, data_dir: PathBuf) {
    let store_path = data_dir.join("store.db");
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
        let view = cx.new(|_| fg_app::views::Recovery::new(store_path.clone()));
        let events_data_dir = data_dir.clone();
        let events_store_path = store_path.clone();
        cx.subscribe(
            &view,
            move |view, event: &fg_app::views::RecoveryEvent, cx| {
                use fg_app::views::RecoveryEvent;
                match event {
                    RecoveryEvent::Quit => cx.quit(),
                    RecoveryEvent::Retry => match start_service(&events_data_dir) {
                        Ok(service) => {
                            cx.set_global(AppState {
                                service: service.clone(),
                            });
                            fg_app::controller::apply_saved_appearance(service.clone(), cx);
                            open_launcher(cx, service);
                            view.update(cx, |v, cx| v.set_recovered(None, cx));
                        }
                        Err(e) => view.update(cx, |v, cx| v.set_error(e.to_string(), cx)),
                    },
                    RecoveryEvent::StartFresh => {
                        match fg_app::views::move_store_aside(&events_store_path) {
                            Ok(aside) => match start_service(&events_data_dir) {
                                Ok(service) => {
                                    cx.set_global(AppState {
                                        service: service.clone(),
                                    });
                                    fg_app::controller::apply_saved_appearance(service.clone(), cx);
                                    open_launcher(cx, service);
                                    view.update(cx, |v, cx| v.set_recovered(Some(aside), cx));
                                }
                                Err(e) => view.update(cx, |v, cx| v.set_error(e.to_string(), cx)),
                            },
                            Err(e) => view.update(cx, |v, cx| v.set_error(e, cx)),
                        }
                    }
                }
            },
        )
        .detach();
        view
    }) {
        eprintln!("failed to open the recovery window: {e}");
        cx.quit();
    }
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
