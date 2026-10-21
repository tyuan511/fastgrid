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

/// One cell, formatted and coloured by its kind. `NULL` is muted italic rather than the text
/// "NULL", and numbers are right-aligned monospace so columns line up.
pub fn cell_element(value: &Value, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let cell = display(value);
    match cell.kind {
        CellKind::Null => div()
            .text_size(px(13.))
            .italic()
            .text_color(theme.muted_foreground.opacity(0.65))
            .child(cell.text)
            .into_any_element(),
        CellKind::Pill => div()
            .px_1p5()
            .rounded_sm()
            .bg(theme.muted)
            .text_size(px(13.))
            .text_color(theme.muted_foreground)
            .child(cell.text)
            .into_any_element(),
        CellKind::Number => div()
            .w_full()
            .text_right()
            .font_family(theme.mono_font_family.clone())
            .child(cell.text)
            .into_any_element(),
        CellKind::Mono => div()
            .font_family(theme.mono_font_family.clone())
            .child(cell.text)
            .into_any_element(),
        CellKind::Bool => div()
            .font_family(theme.mono_font_family.clone())
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

/// The four paging buttons. `on` is a plain function pointer, which is `Copy`, so each button can
/// own a copy of it instead of capturing a borrow of this call frame.
pub fn pager<T: 'static>(
    cx: &mut Context<T>,
    prefix: &'static str,
    state: PagerState,
    on: fn(&mut T, PageNav, &mut Context<T>),
) -> AnyElement {
    // `prefix` empty keeps the historical ids (`page-next`), which existing tests pin.
    let button = |id: &'static str, label: AnyElement, enabled: bool| {
        let element_id = if prefix.is_empty() {
            SharedString::from(id)
        } else {
            SharedString::from(format!("{prefix}-{id}"))
        };
        Button::new(element_id)
            .child(label)
            .small()
            .disabled(!enabled)
    };
    let handlers = [
        (PageNav::First, state.has_prev),
        (PageNav::Prev, state.has_prev),
        (PageNav::Next, state.has_next),
        (PageNav::Last, state.has_total && state.has_next),
    ];
    let labels = [
        div().child("«").into_any_element(),
        hugeicon(Hugeicon::ArrowLeft).into_any_element(),
        hugeicon(Hugeicon::ArrowRight).into_any_element(),
        div().child("»").into_any_element(),
    ];
    let ids = ["page-first", "page-prev", "page-next", "page-last"];
    h_flex()
        .gap_0p5()
        .items_center()
        .children(
            handlers
                .into_iter()
                .zip(labels)
                .zip(ids)
                .map(|(((nav, enabled), label), id)| {
                    let handle = on;
                    button(id, label, enabled)
                        .on_click(cx.listener(move |this, _, _, cx| handle(this, nav, cx)))
                }),
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

/// Frame for a body cell: its own padding (the columns have none) and a hairline on its left edge,
/// which is what draws the vertical grid lines. A selected row is filled; the selected cell also
/// gets a border all the way round.
pub fn td_frame(child: AnyElement, selection: CellSelection, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let frame = div()
        .size_full()
        .flex()
        .items_center()
        .px(px(8.))
        .overflow_hidden();
    match selection {
        CellSelection::None => frame.border_l_1().border_color(theme.table_row_border),
        CellSelection::Row => frame
            .border_l_1()
            .border_color(theme.table_row_border)
            .bg(theme.primary.opacity(0.10)),
        CellSelection::Cell => frame
            .border_2()
            .border_color(theme.primary)
            .bg(theme.primary.opacity(0.16)),
    }
    .child(child)
    .into_any_element()
}

/// The same for a header cell; it is a flex child next to the sort arrow.
pub fn th_frame(child: AnyElement, cx: &App) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .items_center()
        .pl(px(8.))
        .overflow_hidden()
        .border_l_1()
        .border_color(cx.theme().table_row_border)
        .child(child)
        .into_any_element()
}
