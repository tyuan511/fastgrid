//! The state behind one table tab: which page, how sorted, which filters. It turns into a
//! [`TableQuery`] for the service and into a [`TableViewState`] for saving between sessions.

use fg_core::{Filter, ObjectPath, SortKey, TableQuery, TableViewState};

use super::format::group_digits;

pub const PAGE_SIZES: [u32; 3] = [100, 300, 1000];
pub const DEFAULT_PAGE_SIZE: u32 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageState {
    page: u64,
    size: u32,
    /// Exact count, once the background `COUNT(*)` has answered.
    total: Option<u64>,
    /// Rows in the page currently shown.
    loaded: usize,
}

impl PageState {
    pub fn new(size: u32) -> Self {
        Self {
            page: 0,
            size: size.max(1),
            total: None,
            loaded: 0,
        }
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn offset(&self) -> u64 {
        self.page * u64::from(self.size)
    }

    pub fn total(&self) -> Option<u64> {
        self.total
    }

    pub fn set_total(&mut self, total: Option<u64>) {
        self.total = total;
    }

    pub fn set_loaded(&mut self, rows: usize) {
        self.loaded = rows;
    }

    pub fn has_prev(&self) -> bool {
        self.page > 0
    }

    /// With a known total this is exact; without one, a full page suggests there is more.
    pub fn has_next(&self) -> bool {
        match self.total {
            Some(total) => self.offset() + u64::from(self.size) < total,
            None => self.loaded == self.size as usize,
        }
    }

    pub fn next_page(&mut self) -> bool {
        let ok = self.has_next();
        if ok {
            self.page += 1;
        }
        ok
    }

    pub fn prev_page(&mut self) -> bool {
        let ok = self.has_prev();
        if ok {
            self.page -= 1;
        }
        ok
    }

    pub fn first(&mut self) {
        self.page = 0;
    }

    /// Needs a known total.
    pub fn last(&mut self) -> bool {
        match self.total {
            Some(total) if total > 0 => {
                self.page = (total - 1) / u64::from(self.size);
                true
            }
            _ => false,
        }
    }

    /// Change the page size, staying on the page that contains the current first row.
    pub fn set_size(&mut self, size: u32) {
        let first_row = self.offset();
        self.size = size.max(1);
        self.page = first_row / u64::from(self.size);
    }

    /// `Showing 1–300 of 14,200`, `Showing 301–420` (total not known yet) or `No rows`.
    pub fn label(&self) -> String {
        if self.loaded == 0 {
            return "No rows".into();
        }
        let from = self.offset() + 1;
        let to = self.offset() + self.loaded as u64;
        match self.total {
            Some(total) => format!(
                "Showing {}–{} of {}",
                group_digits(from),
                group_digits(to),
                group_digits(total)
            ),
            None => format!("Showing {}–{}", group_digits(from), group_digits(to)),
        }
    }
}

/// Clicking a column header cycles ascending, descending, off. Shift-click adds the column to
/// the existing sort (earlier keys take priority) instead of replacing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SortState {
    keys: Vec<SortKey>,
}

impl SortState {
    pub fn from_keys(keys: Vec<SortKey>) -> Self {
        Self { keys }
    }

    pub fn keys(&self) -> &[SortKey] {
        &self.keys
    }

    pub fn toggle(&mut self, column: &str, additive: bool) {
        let current = self.keys.iter().position(|k| k.column == column);
        let next = match current.map(|i| self.keys[i].descending) {
            None => Some(false),       // off -> ascending
            Some(false) => Some(true), // ascending -> descending
            Some(true) => None,        // descending -> off
        };
        if !additive {
            self.keys.retain(|k| k.column == column);
        }
        let position = self.keys.iter().position(|k| k.column == column);
        match (position, next) {
            (Some(i), Some(desc)) => self.keys[i].descending = desc,
            (Some(i), None) => {
                self.keys.remove(i);
            }
            (None, Some(desc)) => self.keys.push(SortKey {
                column: column.to_owned(),
                descending: desc,
                nulls: None,
            }),
            (None, None) => {}
        }
    }

    /// `(priority starting at 1, descending)`; priority is only worth showing with several keys.
    pub fn indicator(&self, column: &str) -> Option<(usize, bool)> {
        self.keys
            .iter()
            .position(|k| k.column == column)
            .map(|i| (i + 1, self.keys[i].descending))
    }
}

/// Everything that decides which rows the grid shows.
#[derive(Debug, Clone, PartialEq)]
pub struct GridQuery {
    path: ObjectPath,
    pub page: PageState,
    pub sort: SortState,
    pub filters: Vec<Filter>,
    pub raw_where: Option<String>,
}

impl GridQuery {
    pub fn new(path: ObjectPath, page_size: u32) -> Self {
        Self {
            path,
            page: PageState::new(page_size),
            sort: SortState::default(),
            filters: Vec::new(),
            raw_where: None,
        }
    }

    /// Restore sorting and filters saved from an earlier session.
    pub fn restore(path: ObjectPath, default_page_size: u32, saved: &TableViewState) -> Self {
        let mut q = Self::new(path, saved.page_size.unwrap_or(default_page_size));
        q.sort = SortState::from_keys(saved.sort.clone());
        q.filters = saved.filters.clone();
        q.raw_where = saved.raw_where.clone();
        q
    }

    pub fn path(&self) -> &ObjectPath {
        &self.path
    }

    pub fn to_table_query(&self) -> TableQuery {
        let mut q = TableQuery::new(self.path.clone(), u64::from(self.page.size()));
        q.offset = self.page.offset();
        q.sort = self.sort.keys().to_vec();
        q.filters = self.filters.clone();
        q.raw_where = self.raw_where.clone().filter(|s| !s.trim().is_empty());
        q
    }

    /// What to remember for next time. Column layout fields are filled in by the grid view.
    pub fn to_view_state(&self, base: TableViewState) -> TableViewState {
        TableViewState {
            filters: self.filters.clone(),
            raw_where: self.raw_where.clone(),
            sort: self.sort.keys().to_vec(),
            page_size: Some(self.page.size()),
            ..base
        }
    }

    /// Sorting or filtering changes what page 1 is: go back to it, forget the old total.
    pub fn toggle_sort(&mut self, column: &str, additive: bool) {
        self.sort.toggle(column, additive);
        self.reset_paging();
    }

    pub fn set_filters(&mut self, filters: Vec<Filter>, raw_where: Option<String>) {
        self.filters = filters;
        self.raw_where = raw_where;
        self.reset_paging();
    }

    fn reset_paging(&mut self) {
        self.page.first();
        self.page.set_total(None);
    }

    /// Number shown on the Filters button: enabled rules plus a raw WHERE.
    pub fn active_filter_count(&self) -> usize {
        self.filters.iter().filter(|f| f.enabled).count()
            + usize::from(
                self.raw_where
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty()),
            )
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{FilterOp, Value};

    use super::*;

    fn path() -> ObjectPath {
        ObjectPath::new(Some("main"), "t")
    }

    #[test]
    fn paging_with_a_known_total() {
        let mut p = PageState::new(100);
        p.set_total(Some(250));
        p.set_loaded(100);
        assert!(!p.has_prev() && p.has_next());
        assert_eq!(p.label(), "Showing 1–100 of 250");
        assert!(p.next_page());
        assert_eq!(p.offset(), 100);
        assert!(p.next_page());
        p.set_loaded(50);
        assert!(!p.has_next(), "page 3 holds the last 50");
        assert!(!p.next_page());
        assert_eq!(p.label(), "Showing 201–250 of 250");
        p.first();
        assert!(p.last());
        assert_eq!(p.offset(), 200);
    }

    #[test]
    fn paging_before_the_total_is_known() {
        let mut p = PageState::new(100);
        p.set_loaded(100);
        assert!(p.has_next(), "a full page suggests more");
        assert_eq!(p.label(), "Showing 1–100");
        p.next_page();
        p.set_loaded(30);
        assert!(!p.has_next(), "a short page is the end");
        assert!(!p.clone().last(), "last page needs the total");
    }

    #[test]
    fn exact_multiple_of_the_page_size_has_no_extra_page() {
        let mut p = PageState::new(100);
        p.set_total(Some(200));
        p.next_page();
        p.set_loaded(100);
        assert!(!p.has_next());
        p.first();
        assert!(p.last());
        assert_eq!(p.offset(), 100);
    }

    #[test]
    fn empty_results_and_large_numbers() {
        let mut p = PageState::new(300);
        assert_eq!(p.label(), "No rows");
        p.set_total(Some(1_234_567));
        p.set_loaded(300);
        assert_eq!(p.label(), "Showing 1–300 of 1,234,567");
        p.set_total(Some(0));
        assert!(!p.clone().last());
    }

    #[test]
    fn changing_page_size_keeps_the_first_visible_row() {
        let mut p = PageState::new(100);
        p.set_total(Some(10_000));
        p.set_loaded(100);
        p.next_page();
        p.next_page(); // rows 201-300
        p.set_size(1000);
        assert_eq!((p.offset(), p.size()), (0, 1000));
        p.set_size(100);
        let mut q = PageState::new(100);
        q.next_to(5);
        q.set_size(300);
        assert_eq!(q.offset(), 300, "row 501 is on the page starting at 300");
    }

    impl PageState {
        fn next_to(&mut self, page: u64) {
            self.page = page;
        }
    }

    #[test]
    fn sort_cycles_asc_desc_off() {
        let mut s = SortState::default();
        s.toggle("a", false);
        assert_eq!(s.indicator("a"), Some((1, false)));
        s.toggle("a", false);
        assert_eq!(s.indicator("a"), Some((1, true)));
        s.toggle("a", false);
        assert_eq!(s.indicator("a"), None);
        assert!(s.keys().is_empty());
    }

    #[test]
    fn plain_click_replaces_the_sort_shift_click_adds_to_it() {
        let mut s = SortState::default();
        s.toggle("a", false);
        s.toggle("b", false);
        assert_eq!(
            s.indicator("a"),
            None,
            "a plain click on another column replaces"
        );
        assert_eq!(s.indicator("b"), Some((1, false)));

        s.toggle("c", true);
        s.toggle("a", true);
        assert_eq!(s.indicator("b"), Some((1, false)));
        assert_eq!(s.indicator("c"), Some((2, false)));
        assert_eq!(s.indicator("a"), Some((3, false)));

        s.toggle("c", true);
        s.toggle("c", true);
        assert_eq!(s.indicator("c"), None, "cycled off");
        assert_eq!(s.indicator("a"), Some((2, false)), "priorities close up");
    }

    #[test]
    fn sorting_or_filtering_returns_to_the_first_page() {
        let mut q = GridQuery::new(path(), 100);
        q.page.set_total(Some(1000));
        q.page.set_loaded(100);
        q.page.next_page();
        q.toggle_sort("a", false);
        assert_eq!((q.page.offset(), q.page.total()), (0, None));

        q.page.set_loaded(100);
        q.page.next_page();
        q.set_filters(vec![], Some("x > 1".into()));
        assert_eq!(q.page.offset(), 0);
    }

    #[test]
    fn builds_the_service_query() {
        let mut q = GridQuery::new(path(), 100);
        q.toggle_sort("a", false);
        q.set_filters(
            vec![Filter {
                column: "b".into(),
                op: FilterOp::Eq,
                values: vec![Value::Int(1)],
                enabled: true,
            }],
            Some("   ".into()),
        );
        q.page.set_loaded(100);
        q.page.set_total(Some(500));
        q.page.next_page();
        let t = q.to_table_query();
        assert_eq!((t.limit, t.offset), (100, 100));
        assert_eq!(t.sort.len(), 1);
        assert_eq!(t.filters.len(), 1);
        assert_eq!(t.raw_where, None, "a blank raw WHERE is dropped");
    }

    #[test]
    fn active_filter_count_ignores_disabled_rules() {
        let mut q = GridQuery::new(path(), 100);
        let rule = |enabled| Filter {
            column: "a".into(),
            op: FilterOp::IsNull,
            values: vec![],
            enabled,
        };
        q.set_filters(
            vec![rule(true), rule(false), rule(true)],
            Some("x = 1".into()),
        );
        assert_eq!(q.active_filter_count(), 3);
        q.set_filters(vec![rule(false)], None);
        assert_eq!(q.active_filter_count(), 0);
    }

    #[test]
    fn view_state_roundtrip() {
        let mut q = GridQuery::new(path(), 100);
        q.toggle_sort("a", false);
        q.set_filters(vec![], Some("x = 1".into()));
        q.page.set_size(1000);
        let saved = q.to_view_state(TableViewState {
            pinned_columns: 2,
            ..Default::default()
        });
        assert_eq!(saved.pinned_columns, 2, "layout fields are kept");
        let restored = GridQuery::restore(path(), 300, &saved);
        assert_eq!(restored.page.size(), 1000);
        assert_eq!(restored.sort, q.sort);
        assert_eq!(restored.raw_where.as_deref(), Some("x = 1"));
        // Nothing saved: defaults.
        let fresh = GridQuery::restore(path(), 300, &TableViewState::default());
        assert_eq!(fresh.page.size(), 300);
    }
}
