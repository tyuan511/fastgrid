//! Open tabs of one connection window: opening (reusing a tab that already shows the table),
//! closing (activating a neighbour), cycling, reordering, and saving/restoring with the session.

use fg_core::{DocumentId, ObjectPath, TabId, TabKind, TabState, TableViewMode, WorkspaceState};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TabSet {
    tabs: Vec<TabState>,
    active: Option<TabId>,
}

impl TabSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restore from a saved workspace. A saved active tab that no longer exists falls back to the
    /// first tab.
    pub fn from_workspace(ws: &WorkspaceState) -> Self {
        let active = ws
            .active_tab
            .clone()
            .filter(|id| ws.tabs.iter().any(|t| &t.id == id))
            .or_else(|| ws.tabs.first().map(|t| t.id.clone()));
        Self {
            tabs: ws.tabs.clone(),
            active,
        }
    }

    /// Write tabs into `base` (which carries the layout fields the tab set knows nothing about).
    pub fn to_workspace(&self, base: WorkspaceState) -> WorkspaceState {
        WorkspaceState {
            tabs: self.tabs.clone(),
            active_tab: self.active.clone(),
            ..base
        }
    }

    pub fn tabs(&self) -> &[TabState] {
        &self.tabs
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_id(&self) -> Option<&TabId> {
        self.active.as_ref()
    }

    pub fn active(&self) -> Option<&TabState> {
        let id = self.active.as_ref()?;
        self.tabs.iter().find(|t| &t.id == id)
    }

    pub fn active_index(&self) -> Option<usize> {
        let id = self.active.as_ref()?;
        self.tabs.iter().position(|t| &t.id == id)
    }

    pub fn find_table(&self, path: &ObjectPath) -> Option<&TabState> {
        self.tabs
            .iter()
            .find(|t| matches!(&t.kind, TabKind::Table { path: p, .. } if p == path))
    }

    /// Show a table: an existing tab for it is activated (and switched to `mode`), otherwise a
    /// new one opens right after the active tab. Returns the tab's id.
    pub fn open_table(&mut self, path: &ObjectPath, mode: TableViewMode) -> TabId {
        if let Some(existing) = self
            .tabs
            .iter_mut()
            .find(|t| matches!(&t.kind, TabKind::Table { path: p, .. } if p == path))
        {
            if let TabKind::Table { mode: m, .. } = &mut existing.kind {
                *m = mode;
            }
            let id = existing.id.clone();
            self.active = Some(id.clone());
            return id;
        }
        self.insert(TabKind::Table {
            path: path.clone(),
            mode,
        })
    }

    /// A new tab for a query document (always a new tab).
    pub fn open_query(&mut self, document_id: DocumentId) -> TabId {
        self.insert(TabKind::Query { document_id })
    }

    fn insert(&mut self, kind: TabKind) -> TabId {
        let tab = TabState {
            id: TabId::new(),
            kind,
            pinned: false,
        };
        let id = tab.id.clone();
        let at = self.active_index().map_or(self.tabs.len(), |i| i + 1);
        self.tabs.insert(at, tab);
        self.active = Some(id.clone());
        id
    }

    /// Close a tab. If it was active, its right neighbour (else left) becomes active. Returns the
    /// newly active tab, if any.
    pub fn close(&mut self, id: &TabId) -> Option<TabId> {
        let index = self.tabs.iter().position(|t| &t.id == id)?;
        let was_active = self.active.as_ref() == Some(id);
        self.tabs.remove(index);
        if was_active {
            self.active = self
                .tabs
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| self.tabs.get(i)))
                .map(|t| t.id.clone());
        }
        self.active.clone()
    }

    pub fn activate(&mut self, id: &TabId) -> bool {
        let known = self.tabs.iter().any(|t| &t.id == id);
        if known {
            self.active = Some(id.clone());
        }
        known
    }

    /// `n` is 0-based (`⌘1` = 0).
    pub fn activate_index(&mut self, n: usize) -> bool {
        match self.tabs.get(n) {
            Some(t) => {
                self.active = Some(t.id.clone());
                true
            }
            None => false,
        }
    }

    pub fn next(&mut self) {
        self.step(1);
    }

    pub fn prev(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        let len = self.tabs.len() as isize;
        if len == 0 {
            return;
        }
        let current = self.active_index().map_or(0, |i| i as isize);
        let target = (current + delta).rem_euclid(len) as usize;
        self.active = Some(self.tabs[target].id.clone());
    }

    /// Drag a tab to a new position.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        if from < self.tabs.len() && to < self.tabs.len() && from != to {
            let tab = self.tabs.remove(from);
            self.tabs.insert(to, tab);
        }
    }

    /// Drop tabs for tables that no longer exist (after restoring a session). Query tabs stay.
    pub fn retain_tables(&mut self, exists: impl Fn(&ObjectPath) -> bool) {
        let active = self.active.clone();
        self.tabs.retain(|t| match &t.kind {
            TabKind::Table { path, .. } => exists(path),
            _ => true,
        });
        let still_there = active
            .as_ref()
            .is_some_and(|id| self.tabs.iter().any(|t| &t.id == id));
        if !still_there {
            self.active = self.tabs.first().map(|t| t.id.clone());
        }
    }
}

/// Title shown on a tab. Query tabs are titled by their document, which this model does not own.
pub fn table_title(path: &ObjectPath) -> String {
    path.name.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> ObjectPath {
        ObjectPath::new(Some("main"), name)
    }

    fn names(set: &TabSet) -> Vec<String> {
        set.tabs()
            .iter()
            .map(|t| match &t.kind {
                TabKind::Table { path, .. } => path.name.clone(),
                TabKind::Query { .. } => "<query>".into(),
            })
            .collect()
    }

    #[test]
    fn opening_the_same_table_reuses_its_tab() {
        let mut s = TabSet::new();
        let a = s.open_table(&t("a"), TableViewMode::Data);
        s.open_table(&t("b"), TableViewMode::Data);
        let again = s.open_table(&t("a"), TableViewMode::Structure);
        assert_eq!(a, again);
        assert_eq!(s.tabs().len(), 2);
        assert_eq!(s.active_id(), Some(&a));
        assert!(matches!(
            &s.active().unwrap().kind,
            TabKind::Table {
                mode: TableViewMode::Structure,
                ..
            }
        ));
    }

    #[test]
    fn new_tabs_open_to_the_right_of_the_active_one() {
        let mut s = TabSet::new();
        let a = s.open_table(&t("a"), TableViewMode::Data);
        s.open_table(&t("b"), TableViewMode::Data);
        s.open_table(&t("c"), TableViewMode::Data);
        assert_eq!(names(&s), ["a", "b", "c"]);
        s.activate(&a);
        s.open_table(&t("d"), TableViewMode::Data);
        assert_eq!(names(&s), ["a", "d", "b", "c"]);
        s.open_query(DocumentId::new());
        assert_eq!(names(&s), ["a", "d", "<query>", "b", "c"]);
    }

    #[test]
    fn closing_activates_the_right_neighbour_then_the_left() {
        let mut s = TabSet::new();
        let ids: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|n| s.open_table(&t(n), TableViewMode::Data))
            .collect();
        s.activate(&ids[1]);
        assert_eq!(s.close(&ids[1]), Some(ids[2].clone()), "right neighbour");
        assert_eq!(
            s.close(&ids[2]),
            Some(ids[0].clone()),
            "no right neighbour: left"
        );
        assert_eq!(s.close(&ids[0]), None);
        assert!(s.is_empty());
        assert_eq!(s.close(&ids[0]), None, "closing an unknown tab is a no-op");
    }

    #[test]
    fn closing_an_inactive_tab_keeps_the_active_one() {
        let mut s = TabSet::new();
        let a = s.open_table(&t("a"), TableViewMode::Data);
        let b = s.open_table(&t("b"), TableViewMode::Data);
        assert_eq!(s.close(&a), Some(b.clone()));
        assert_eq!(s.active_id(), Some(&b));
    }

    #[test]
    fn cycling_wraps_around() {
        let mut s = TabSet::new();
        let ids: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|n| s.open_table(&t(n), TableViewMode::Data))
            .collect();
        assert_eq!(s.active_id(), Some(&ids[2]));
        s.next();
        assert_eq!(s.active_id(), Some(&ids[0]));
        s.prev();
        assert_eq!(s.active_id(), Some(&ids[2]));
        assert!(s.activate_index(1));
        assert_eq!(s.active_id(), Some(&ids[1]));
        assert!(!s.activate_index(9));
        TabSet::new().next(); // no panic on an empty set
    }

    #[test]
    fn reordering() {
        let mut s = TabSet::new();
        for n in ["a", "b", "c"] {
            s.open_table(&t(n), TableViewMode::Data);
        }
        s.move_tab(0, 2);
        assert_eq!(names(&s), ["b", "c", "a"]);
        s.move_tab(5, 0);
        s.move_tab(1, 1);
        assert_eq!(names(&s), ["b", "c", "a"]);
    }

    #[test]
    fn session_roundtrip_and_stale_state() {
        let mut s = TabSet::new();
        let a = s.open_table(&t("a"), TableViewMode::Data);
        s.open_table(&t("gone"), TableViewMode::Data);
        s.open_query(DocumentId::new());
        s.activate(&a);

        let base = WorkspaceState {
            sidebar_width: Some(250.0),
            ..Default::default()
        };
        let ws = s.to_workspace(base);
        assert_eq!(ws.sidebar_width, Some(250.0), "layout fields survive");
        let mut restored = TabSet::from_workspace(&ws);
        assert_eq!(restored, s);

        restored.retain_tables(|p| p.name != "gone");
        assert_eq!(names(&restored), ["a", "<query>"]);
        assert_eq!(restored.active_id(), Some(&a));

        // A saved active tab that is not in the list falls back to the first tab.
        let mut broken = ws.clone();
        broken.active_tab = Some(TabId::new());
        assert_eq!(
            TabSet::from_workspace(&broken).active_id(),
            Some(&broken.tabs[0].id)
        );
    }

    #[test]
    fn dropping_the_active_table_on_restore_picks_another_tab() {
        let mut s = TabSet::new();
        s.open_table(&t("a"), TableViewMode::Data);
        s.open_table(&t("b"), TableViewMode::Data); // active
        s.retain_tables(|p| p.name != "b");
        assert_eq!(names(&s), ["a"]);
        assert!(s.active().is_some());
    }
}
