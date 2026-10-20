use fg_core::{ConnectionId, ObjectPath, Result, TableViewState, WorkspaceState};
use serde::{Deserialize, Serialize};

use crate::record::Loaded;
use crate::{Store, kinds};

/// A table view is looked up by (namespace, name), but record ids are plaintext, so they are
/// random and the names stay inside the sealed payload. A connection has few views; reading them
/// all is cheap.
#[derive(Serialize, Deserialize)]
struct TableViewRecord {
    namespace: Option<String>,
    name: String,
    state: TableViewState,
}

impl Store {
    /// Window layout of this connection **on this device**: devices have different screens, so
    /// each keeps its own (they still sync, so a device could offer to adopt another's layout).
    pub fn load_workspace(&self, connection: &ConnectionId) -> Result<Option<WorkspaceState>> {
        let id = format!("{connection}:{}", self.device_id());
        Ok(self
            .get_json::<WorkspaceState>(kinds::WORKSPACE, &id)?
            .and_then(Loaded::ok))
    }

    pub fn save_workspace(&self, connection: &ConnectionId, state: &WorkspaceState) -> Result<()> {
        let id = format!("{connection}:{}", self.device_id());
        self.put_json(
            kinds::WORKSPACE,
            &id,
            Some(connection.as_str()),
            None,
            state,
        )
    }

    fn find_table_view(
        &self,
        connection: &ConnectionId,
        path: &ObjectPath,
    ) -> Result<Option<(String, TableViewState)>> {
        let rows = self.fetch(
            "kind = ?1 AND parent_id = ?2",
            "ORDER BY updated_at DESC",
            &[&kinds::TABLE_VIEW, &connection.as_str()],
        )?;
        for row in rows {
            if let Loaded::Ok(rec) = self.decode::<TableViewRecord>(&row)
                && rec.namespace == path.namespace
                && rec.name == path.name
            {
                return Ok(Some((row.id, rec.state)));
            }
        }
        Ok(None)
    }

    pub fn load_table_view(
        &self,
        connection: &ConnectionId,
        path: &ObjectPath,
    ) -> Result<Option<TableViewState>> {
        Ok(self
            .find_table_view(connection, path)?
            .map(|(_, state)| state))
    }

    pub fn save_table_view(
        &self,
        connection: &ConnectionId,
        path: &ObjectPath,
        state: &TableViewState,
    ) -> Result<()> {
        let id = match self.find_table_view(connection, path)? {
            Some((id, _)) => id,
            None => fg_core::TabId::new().to_string(),
        };
        let record = TableViewRecord {
            namespace: path.namespace.clone(),
            name: path.name.clone(),
            state: state.clone(),
        };
        self.put_json(
            kinds::TABLE_VIEW,
            &id,
            Some(connection.as_str()),
            None,
            &record,
        )
    }
}

#[cfg(test)]
mod tests {
    use fg_core::{DocumentId, Filter, FilterOp, SortKey, TabId, TabKind, TabState, Value};

    use super::*;
    use crate::testutil::store;

    #[test]
    fn workspace_roundtrip_is_per_connection() {
        let s = store();
        let (a, b) = (ConnectionId::new(), ConnectionId::new());
        assert_eq!(s.load_workspace(&a).unwrap(), None);

        let tab = TabState {
            id: TabId::new(),
            kind: TabKind::Query {
                document_id: DocumentId::new(),
            },
            pinned: false,
        };
        let ws = WorkspaceState {
            tabs: vec![tab.clone()],
            active_tab: Some(tab.id),
            ..WorkspaceState::default()
        };
        s.save_workspace(&a, &ws).unwrap();
        assert_eq!(s.load_workspace(&a).unwrap(), Some(ws));
        assert_eq!(s.load_workspace(&b).unwrap(), None);
    }

    #[test]
    fn table_views_are_keyed_by_namespace_and_name() {
        let s = store();
        let id = ConnectionId::new();
        let main = ObjectPath::new(Some("main"), "t");
        let none = ObjectPath::new(None, "t");
        let view = TableViewState {
            filters: vec![Filter {
                column: "a".into(),
                op: FilterOp::Gt,
                values: vec![Value::Int(3)],
                enabled: true,
            }],
            sort: vec![SortKey {
                column: "a".into(),
                descending: true,
                nulls: None,
            }],
            pinned_columns: 1,
            ..TableViewState::default()
        };
        s.save_table_view(&id, &main, &view).unwrap();
        s.save_table_view(&id, &main, &view).unwrap();
        assert_eq!(s.load_table_view(&id, &main).unwrap(), Some(view));
        assert_eq!(s.load_table_view(&id, &none).unwrap(), None);

        let rows = s.fetch("kind = ?1", "", &[&kinds::TABLE_VIEW]).unwrap();
        assert_eq!(rows.len(), 1, "saving again updates the same record");
    }
}
