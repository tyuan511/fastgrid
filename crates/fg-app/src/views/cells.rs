//! Cell rendering and paging widgets shared by the table grid and the query result grid.
//!
//! Both grids show the same `Value`s with the same formatting rules, so the rendering lives here
//! once. What differs between them is where the rows come from (a page fetched from the database
//! versus a result set already in memory) and what else the status bar carries; those stay in the
//! views.

use fg_core::Value;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::h_flex;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::icons::{Hugeicon, hugeicon};
use crate::model::{CellKind, display};

/// One cell, formatted and coloured by its kind. The frame sets the monospace face for every kind,
/// as TablePlus does; `NULL` is muted, and numbers are right-aligned so columns line up.
pub fn cell_element(value: &Value, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let cell = display(value);
    match cell.kind {
        CellKind::Null => div()
            .text_color(theme.muted_foreground.opacity(0.65))
            .child(cell.text)
            .into_any_element(),
        CellKind::Pill => div()
            .px_1()
            .rounded_sm()
            .bg(theme.muted)
            .text_color(theme.muted_foreground)
            .child(cell.text)
            .into_any_element(),
        CellKind::Number => div()
            .w_full()
            .text_right()
            .child(cell.text)
            .into_any_element(),
        CellKind::Mono => div().child(cell.text).into_any_element(),
        CellKind::Bool => div()
            .text_color(if cell.text == "true" {
                theme.success
            } else {
                theme.muted_foreground
            })
            .child(cell.text)
            .into_any_element(),
        CellKind::Text => div().child(cell.text).into_any_element(),
    }
}

/// Which paging button was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageNav {
    First,
    Prev,
    Next,
    Last,
}

/// What the pager needs to enable its buttons. Both grids keep this state themselves: the table
/// grid pages the database, the result grid pages rows it already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagerState {
    pub has_prev: bool,
    pub has_next: bool,
    /// `false` disables "last": a query result always knows its row count, a table only after its
    /// background `COUNT(*)` came back.
    pub has_total: bool,
}

impl PagerState {
    fn enabled(&self, nav: PageNav) -> bool {
        match nav {
            PageNav::First | PageNav::Prev => self.has_prev,
            PageNav::Next => self.has_next,
            PageNav::Last => self.has_total && self.has_next,
        }
    }
}

/// One paging button. `on` is a plain function pointer, which is `Copy`, so each button can own a
/// copy of it instead of capturing a borrow of the caller's frame.
pub fn page_button<T: 'static>(
    cx: &mut Context<T>,
    prefix: &'static str,
    state: PagerState,
    nav: PageNav,
    on: fn(&mut T, PageNav, &mut Context<T>),
) -> Button {
    let (id, label) = match nav {
        PageNav::First => ("page-first", div().child("«").into_any_element()),
        PageNav::Prev => (
            "page-prev",
            hugeicon(Hugeicon::ArrowLeft).into_any_element(),
        ),
        PageNav::Next => (
            "page-next",
            hugeicon(Hugeicon::ArrowRight).into_any_element(),
        ),
        PageNav::Last => ("page-last", div().child("»").into_any_element()),
    };
    // `prefix` empty keeps the historical ids (`page-next`), which existing tests pin.
    let element_id = if prefix.is_empty() {
        SharedString::from(id)
    } else {
        SharedString::from(format!("{prefix}-{id}"))
    };
    Button::new(element_id)
        .child(label)
        .small()
        .disabled(!state.enabled(nav))
        .on_click(cx.listener(move |this, _, _, cx| on(this, nav, cx)))
}

/// The four paging buttons in a row.
pub fn pager<T: 'static>(
    cx: &mut Context<T>,
    prefix: &'static str,
    state: PagerState,
    on: fn(&mut T, PageNav, &mut Context<T>),
) -> AnyElement {
    h_flex()
        .gap_0p5()
        .items_center()
        .children(
            [PageNav::First, PageNav::Prev, PageNav::Next, PageNav::Last]
                .map(|nav| page_button(cx, prefix, state, nav, on)),
        )
        .into_any_element()
}

/// The page-size choices as a row of buttons, the active one highlighted.
pub fn page_size_buttons<T: 'static>(
    cx: &mut Context<T>,
    sizes: &[u32],
    current: u32,
    on: fn(&mut T, u32, &mut Context<T>),
) -> AnyElement {
    h_flex()
        .gap_1()
        .children(sizes.iter().map(|&n| {
            Button::new(SharedString::from(format!("page-size-{n}")))
                .label(n.to_string())
                .small()
                .when(n == current, |b| b.primary())
                .on_click(cx.listener(move |this, _, _, cx| on(this, n, cx)))
        }))
        .into_any_element()
}

/// How a body cell relates to the grid's selection: the cell clicked, the rest of its row, or
/// neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellSelection {
    None,
    Row,
    Cell,
}

/// A staged change painted on a cell (roadmap M3): edited, added, or marked for deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellMark {
    None,
    Modified,
    Inserted,
    Deleted,
}

/// The size both grids use for headers and cells, in the monospace face, as TablePlus draws them.
pub const GRID_TEXT_SIZE: Pixels = px(13.);

/// The right padding the table library keeps in a header cell for its sort arrow (the default
/// size's cell padding). Neither grid uses the library's arrow, so the header frame reaches over it
/// and its column line lands on the same x as the body's.
const HEADER_ARROW_SPACE: Pixels = px(8.);

/// Frame for a body cell: its own padding (the columns have none), in the grid's monospace face.
/// Like TablePlus each cell draws a hairline on its right edge, which makes the column lines (rows
/// are striped instead of ruled, and the row-number gutter has no line). A selected row is filled;
/// the selected cell gets an accent border all the way round.
pub fn td_frame(child: AnyElement, selection: CellSelection, cx: &App) -> AnyElement {
    td_frame_marked(child, selection, CellMark::None, cx)
}

/// [`td_frame`] with a staged-change tint underneath. The selection fill is drawn over it, so a
/// selected edited cell still shows both.
pub fn td_frame_marked(
    child: AnyElement,
    selection: CellSelection,
    mark: CellMark,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let frame = div()
        .size_full()
        .flex()
        .items_center()
        .px(px(6.))
        .overflow_hidden()
        .font_family(theme.mono_font_family.clone())
        .text_size(GRID_TEXT_SIZE);
    let (frame, child) = match mark {
        CellMark::None => (frame, child),
        CellMark::Modified => (frame.bg(theme.warning.opacity(0.22)), child),
        CellMark::Inserted => (frame.bg(theme.success.opacity(0.20)), child),
        // Struck through and tinted: still readable, clearly on its way out.
        CellMark::Deleted => (
            frame.bg(theme.danger.opacity(0.18)),
            div().line_through().child(child).into_any_element(),
        ),
    };
    match selection {
        CellSelection::None => frame.border_r_1().border_color(theme.border),
        CellSelection::Row => frame
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.primary.opacity(0.12)),
        CellSelection::Cell => frame
            .border_1()
            .border_color(theme.primary)
            .bg(theme.primary.opacity(0.20)),
    }
    .child(child)
    .into_any_element()
}

/// The same for a header cell: the column name centered with the same right-edge column line, as
/// TablePlus does. It is a flex child next to the sort arrow, which only sorted columns show.
pub fn th_frame(child: AnyElement, cx: &App) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .px(px(6.))
        .mr(-HEADER_ARROW_SPACE)
        .overflow_hidden()
        .border_r_1()
        .border_color(cx.theme().border)
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(GRID_TEXT_SIZE)
        .child(child)
        .into_any_element()
}
