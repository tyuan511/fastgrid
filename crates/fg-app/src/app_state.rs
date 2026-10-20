use fg_service::Service;
use gpui_kit::Global;

/// Process-wide handles every window needs.
pub struct AppState {
    pub service: Service,
}

impl Global for AppState {}
