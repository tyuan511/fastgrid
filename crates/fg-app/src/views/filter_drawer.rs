//! The filter drawer above the status bar: one row per rule (on/off, column, operator, value),
//! or a single raw `WHERE` box. The drawer edits a [`FilterDraft`]; applying converts it to typed
//! filters and tells the grid.

use fg_core::{ColumnMeta, FilterOp};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IndexPath, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::icons::{Hugeicon, hugeicon};

use crate::model::{
    Applied, DraftRow, FilterDraft, OPERATORS, RowError, op_from_label, value_slots,
};

type Choice = Entity<SelectState<SearchableVec<SharedString>>>;

struct RowUi {
    enabled: bool,
    column: Choice,
    op: Choice,
    value: Entity<InputState>,
    value2: Entity<InputState>,
}

pub enum FilterDrawerEvent {
    Apply(Applied),
}

pub struct FilterDrawer {
    columns: Vec<ColumnMeta>,
    rows: Vec<RowUi>,
    raw_mode: bool,
    raw_input: Entity<InputState>,
    errors: Vec<RowError>,
    /// Columns and rules handed in without a window; materialised on the next render.
    pending_columns: Option<Vec<ColumnMeta>>,
    pending_draft: Option<FilterDraft>,
    subscriptions: Vec<Subscription>,
}

impl EventEmitter<FilterDrawerEvent> for FilterDrawer {}

fn choice(
    items: Vec<SharedString>,
    selected: Option<usize>,
    window: &mut Window,
    cx: &mut Context<FilterDrawer>,
) -> Choice {
    cx.new(|cx| {
        SelectState::new(
            SearchableVec::new(items),
            selected.map(IndexPath::new),
            window,
            cx,
        )
    })
}

impl FilterDrawer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let raw_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("status = 'active' AND score > 90"));
        let on_raw = cx.subscribe(&raw_input, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.apply(cx);
            }
        });
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            raw_mode: false,
            raw_input,
            errors: Vec::new(),
            pending_columns: None,
            pending_draft: None,
            subscriptions: vec![on_raw],
        }
    }

    pub fn is_raw(&self) -> bool {
        self.raw_mode
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn errors(&self) -> &[RowError] {
        &self.errors
    }

    /// The columns rules can use (called whenever a new result arrives).
    pub fn set_columns(&mut self, columns: Vec<ColumnMeta>, cx: &mut Context<Self>) {
        self.pending_columns = Some(columns);
        cx.notify();
    }

    /// Show these rules (restoring a saved view).
    pub fn load(&mut self, draft: FilterDraft, cx: &mut Context<Self>) {
        self.pending_draft = Some(draft);
        cx.notify();
    }

    fn column_names(&self) -> Vec<SharedString> {
        self.columns
            .iter()
            .map(|c| SharedString::from(c.name.clone()))
            .collect()
    }

    fn make_row(&mut self, row: &DraftRow, window: &mut Window, cx: &mut Context<Self>) -> RowUi {
        let names = self.column_names();
        let col_ix = names.iter().position(|n| n.as_ref() == row.column);
        let ops: Vec<SharedString> = OPERATORS
            .iter()
            .map(|(_, l)| SharedString::from(*l))
            .collect();
        let op_ix = OPERATORS.iter().position(|(o, _)| *o == row.op);
        let column = choice(names, col_ix.or(Some(0)), window, cx);
        let op = choice(ops, op_ix, window, cx);
        let value = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("value");
            s.set_value(row.value.clone(), window, cx);
            s
        });
        let value2 = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("and");
            s.set_value(row.value2.clone(), window, cx);
            s
        });
        // Changing the operator changes how many boxes show; Enter in a box applies.
        let on_op = cx.subscribe(
            &op,
            |_, _, _: &SelectEvent<SearchableVec<SharedString>>, cx| cx.notify(),
        );
        let on_value = cx.subscribe(&value, |this, _, e: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = e {
                this.apply(cx);
            }
        });
        let on_value2 = cx.subscribe(&value2, |this, _, e: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = e {
                this.apply(cx);
            }
        });
        self.subscriptions.extend([on_op, on_value, on_value2]);
        RowUi {
            enabled: row.enabled,
            column,
            op,
            value,
            value2,
        }
    }

    /// Apply anything handed in while no window was available.
    fn materialise(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(columns) = self.pending_columns.take() {
            self.columns = columns;
            // Rows that name a column that is gone fall back to the first column.
            let draft = self.draft(cx);
            self.rebuild(draft, window, cx);
        }
        if let Some(draft) = self.pending_draft.take() {
            self.rebuild(draft, window, cx);
        }
    }

    fn rebuild(&mut self, draft: FilterDraft, window: &mut Window, cx: &mut Context<Self>) {
        self.rows.clear();
        let rows = draft.rows.clone();
        for row in &rows {
            let ui = self.make_row(row, window, cx);
            self.rows.push(ui);
        }
        self.raw_mode = draft.raw_mode;
        self.raw_input
            .update(cx, |s, cx| s.set_value(draft.raw_where.clone(), window, cx));
        self.errors.clear();
        cx.notify();
    }

    /// Read the boxes back into a draft.
    pub fn draft(&self, cx: &App) -> FilterDraft {
        let rows = self
            .rows
            .iter()
            .map(|r| DraftRow {
                enabled: r.enabled,
                column: r
                    .column
                    .read(cx)
                    .selected_value()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                op: r
                    .op
                    .read(cx)
                    .selected_value()
                    .and_then(|l| op_from_label(l))
                    .unwrap_or(FilterOp::Eq),
                value: r.value.read(cx).value().to_string(),
                value2: r.value2.read(cx).value().to_string(),
            })
            .collect();
        FilterDraft {
            rows,
            raw_mode: self.raw_mode,
            raw_where: self.raw_input.read(cx).value().to_string(),
        }
    }

    pub fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first = self
            .columns
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let ui = self.make_row(&DraftRow::new(first), window, cx);
        self.rows.push(ui);
        cx.notify();
    }

    pub fn remove_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.rows.len() {
            self.rows.remove(index);
            self.errors.clear();
            cx.notify();
        }
    }

    pub fn toggle_row(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.rows.get_mut(index) {
            r.enabled = !r.enabled;
            cx.notify();
        }
    }

    pub fn set_raw_mode(&mut self, raw: bool, cx: &mut Context<Self>) {
        self.raw_mode = raw;
        self.errors.clear();
        cx.notify();
    }

    /// Validate and hand the result to the grid; problems are shown beside the rows instead.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        match self.draft(cx).to_applied(&self.columns) {
            Ok(applied) => {
                self.errors.clear();
                cx.emit(FilterDrawerEvent::Apply(applied));
            }
            Err(errors) => self.errors = errors,
        }
        cx.notify();
    }

    /// Remove every rule and apply the empty set.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.rows.clear();
        self.raw_mode = false;
        self.errors.clear();
        cx.emit(FilterDrawerEvent::Apply(Applied {
            filters: Vec::new(),
            raw_where: None,
        }));
        cx.notify();
    }
}

impl Render for FilterDrawer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.materialise(window, cx);
        let theme = cx.theme().clone();

        let body: AnyElement =
            if self.raw_mode {
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("WHERE"),
                    )
                    .child(Input::new(&self.raw_input).id("filter-raw-input"))
                    .into_any_element()
            } else {
                let rows: Vec<AnyElement> =
                    self.rows
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let op =
                                r.op.read(cx)
                                    .selected_value()
                                    .and_then(|l| op_from_label(l))
                                    .unwrap_or(FilterOp::Eq);
                            let slots = value_slots(op);
                            let error = self
                                .errors
                                .iter()
                                .find(|e| e.row == i)
                                .map(|e| e.message.clone());
                            v_flex()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            Checkbox::new(SharedString::from(format!(
                                                "filter-{i}-enabled"
                                            )))
                                            .checked(r.enabled)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.toggle_row(i, cx);
                                            })),
                                        )
                                        .child(div().w(px(160.)).child(
                                            Select::new(&r.column).id(SharedString::from(format!(
                                                "filter-{i}-column"
                                            ))),
                                        ))
                                        .child(
                                            div().w(px(150.)).child(
                                                Select::new(&r.op).id(SharedString::from(format!(
                                                    "filter-{i}-op"
                                                ))),
                                            ),
                                        )
                                        .when(slots >= 1, |d| {
                                            d.child(div().w(px(200.)).child(
                                                Input::new(&r.value).id(SharedString::from(
                                                    format!("filter-{i}-value"),
                                                )),
                                            ))
                                        })
                                        .when(slots >= 2, |d| {
                                            d.child(div().w(px(200.)).child(
                                                Input::new(&r.value2).id(SharedString::from(
                                                    format!("filter-{i}-value2"),
                                                )),
                                            ))
                                        })
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "filter-{i}-remove"
                                            )))
                                            .icon(hugeicon(Hugeicon::Close))
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.remove_row(i, cx);
                                            })),
                                        ),
                                )
                                .when_some(error, |d, message| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("filter-{i}-error")))
                                            .test_support()
                                            .aria_label(message.clone())
                                            .pl(px(28.))
                                            .text_xs()
                                            .text_color(theme.danger)
                                            .child(message),
                                    )
                                })
                                .into_any_element()
                        })
                        .collect();
                v_flex().gap_1p5().children(rows).into_any_element()
            };

        v_flex()
            .id("filter-drawer")
            .test_support()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(body)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("filter-add")
                            .label("Add rule")
                            .icon(hugeicon(Hugeicon::Plus))
                            .xsmall()
                            .disabled(self.raw_mode || self.columns.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                    )
                    .child(
                        Button::new("filter-raw")
                            .label(if self.raw_mode {
                                "Use rules"
                            } else {
                                "Raw SQL"
                            })
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| {
                                let raw = !this.raw_mode;
                                this.set_raw_mode(raw, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("filter-clear")
                            .label("Clear")
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.clear(cx))),
                    )
                    .child(
                        Button::new("filter-apply")
                            .label("Apply")
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    ),
            )
    }
}
