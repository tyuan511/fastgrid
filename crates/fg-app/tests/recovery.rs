//! Headless tests for the lost-keychain recovery screen and the store move-aside helper.
//!
//! The recovery view itself needs no service, which is the point: it is shown precisely when
//! no service exists.
#![recursion_limit = "256"]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use fg_app::views::{Recovery, RecoveryEvent, bind_keys, move_store_aside};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

struct Fixture {
    window: AnyWindowHandle,
    view: Entity<Recovery>,
    events: Rc<RefCell<Vec<RecoveryEvent>>>,
}

fn open(cx: &mut TestAppContext, store_path: PathBuf) -> Fixture {
    cx.update(|cx| {
        gpui_kit::init(cx);
        bind_keys(cx);
    });
    let events = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&events);
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(680.), px(480.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |_, cx| cx.new(|_| Recovery::new(store_path.clone())),
        )
        .unwrap()
    });
    cx.update(|cx| {
        cx.subscribe(&view, move |_, e: &RecoveryEvent, _| {
            log.borrow_mut().push(e.clone())
        })
        .detach();
    });
    Fixture {
        window,
        view,
        events,
    }
}

fn click(cx: &mut TestAppContext, f: &Fixture, id: &str) {
    let id = id.to_owned();
    cx.update_window(f.window, |_, window, cx| {
        window.click(id, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn exists(cx: &mut TestAppContext, f: &Fixture, id: &str) -> bool {
    let id = id.to_owned();
    cx.update_window(f.window, |_, window, _| window.try_find(id).is_some())
        .unwrap()
}

#[gpui_kit::test]
fn moving_a_store_aside_keeps_the_file_and_its_sidecars(cx: &mut TestAppContext) {
    let _ = cx;
    let dir = std::env::temp_dir().join(format!(
        "fastgrid-recovery-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let main = dir.join("store.db");
    let wal = dir.join("store.db-wal");
    std::fs::write(&main, b"data").unwrap();
    std::fs::write(&wal, b"wal").unwrap();
    let aside = move_store_aside(&main).unwrap();
    assert!(
        !main.exists(),
        "the original path is free for a fresh store"
    );
    assert!(aside.exists(), "the old file is kept, not deleted");
    assert_eq!(std::fs::read(&aside).unwrap(), b"data");
    assert!(
        PathBuf::from(format!("{}-wal", aside.display())).exists(),
        "the wal sidecar moves too"
    );
    assert!(
        move_store_aside(&main).is_err(),
        "a second move finds nothing"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[gpui_kit::test]
fn start_fresh_needs_two_clicks_and_quit_emits_once(cx: &mut TestAppContext) {
    let f = open(cx, PathBuf::from("/tmp/fastgrid-store.db"));
    // First click on start-fresh only arms it.
    click(cx, &f, "recovery-fresh");
    assert!(
        f.events.borrow().is_empty(),
        "one click must not start fresh"
    );
    cx.read_entity(&f.view, |v, _| {
        assert!(v.fresh_armed(), "the button arms on first click")
    });
    // Second click acts.
    click(cx, &f, "recovery-fresh");
    assert_eq!(*f.events.borrow(), vec![RecoveryEvent::StartFresh]);
    click(cx, &f, "recovery-quit");
    assert_eq!(
        *f.events.borrow(),
        vec![RecoveryEvent::StartFresh, RecoveryEvent::Quit]
    );
}

#[gpui_kit::test]
fn retry_and_error_and_recovered_states_render(cx: &mut TestAppContext) {
    let f = open(cx, PathBuf::from("/tmp/fastgrid-store.db"));
    click(cx, &f, "recovery-retry");
    assert_eq!(*f.events.borrow(), vec![RecoveryEvent::Retry]);
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| {
            v.set_error("keychain still empty".to_owned(), cx)
        });
    })
    .unwrap();
    assert!(exists(cx, &f, "recovery-error"), "the error is shown");
    cx.update_window(f.window, |_, _, cx| {
        f.view.update(cx, |v, cx| {
            v.set_recovered(Some(PathBuf::from("/tmp/store.db.unrecoverable-1")), cx)
        });
    })
    .unwrap();
    assert!(exists(cx, &f, "recovery-aside"));
    assert!(exists(cx, &f, "recovery-close"));
}
