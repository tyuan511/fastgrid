//! A small read-only grid for the structure and index views: the same table widget, header and
//! cell frames as the data grid, over rows of plain text (a column's name and type, an index's
//! columns, a foreign key's target). The data grid's delegate carries paging, sorting and
//! foreign-key state these lists have no use for, so they get their own.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::*;

use super::cells::{CellSelection, td_frame, th_frame};

pub struct InfoDelegate {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    empty: &'static str,
}

impl InfoDelegate {
    /// `headers` pairs each column's title with its starting width.
    pub fn new(headers: &[(&'static str, f32)], empty: &'static str) -> Self {
        Self {
            columns: headers
                .iter()
                .map(|(name, width)| Column::new(*name, *name).width(px(*width)).p_0())
                .collect(),
            rows: Vec::new(),
            empty,
        }
    }

    pub fn replace(&mut self, rows: Vec<Vec<String>>) {
        self.rows = rows;
    }
}

impl TableDelegate for InfoDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = self.columns[col_ix].name.clone();
        th_frame(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(name)
                .into_any_element(),
            cx,
        )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = self
            .rows
            .get(row_ix)
            .and_then(|r| r.get(col_ix))
            .cloned()
            .unwrap_or_default();
        let cell = if text.is_empty() {
            div()
                .text_color(cx.theme().muted_foreground.opacity(0.65))
                .child("—")
        } else {
            div().child(text)
        };
        td_frame(cell.into_any_element(), CellSelection::None, cx)
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child(self.empty)
    }
}
