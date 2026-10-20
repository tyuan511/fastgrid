use std::collections::BTreeMap;

use fg_core::{
    ColumnInfo, Error, ForeignKeyInfo, GeneratedColumn, IndexColumn, IndexInfo, ObjectInfo,
    ObjectKind, ObjectPath, QueryError, ReferentialAction, Relation, Result, TableDetail,
    TriggerInfo,
};
use rusqlite::{Connection, Row};

use super::quote;
use super::run::map_err;
use super::types::data_type;

fn query_rows<T>(
    conn: &Connection,
    sql: &str,
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql).map_err(map_err)?;
    stmt.query_map([], f)
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)
}

pub fn list_namespaces(conn: &Connection) -> Result<Vec<String>> {
    query_rows(conn, "PRAGMA database_list", |r| r.get::<_, String>(1))
}

fn kind_of(type_name: &str) -> ObjectKind {
    if type_name == "view" {
        ObjectKind::View
    } else {
        ObjectKind::Table
    }
}

pub fn list_objects(conn: &Connection, ns: &str) -> Result<Vec<ObjectInfo>> {
    let sql = format!(
        "SELECT name, type FROM {}.sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
        quote(ns)
    );
    query_rows(conn, &sql, |r| {
        let name: String = r.get(0)?;
        let ty: String = r.get(1)?;
        Ok(ObjectInfo::new(
            ObjectPath::new(Some(ns), name),
            kind_of(&ty),
        ))
    })
}

fn namespace_of(path: &ObjectPath) -> &str {
    path.namespace.as_deref().unwrap_or("main")
}

pub fn describe(conn: &Connection, path: &ObjectPath) -> Result<TableDetail> {
    let ns = namespace_of(path);
    let (q_ns, q_name) = (quote(ns), quote(&path.name));

    let kind_sql = format!(
        "SELECT type FROM {q_ns}.sqlite_master WHERE name = ?1 AND type IN ('table', 'view')"
    );
    let kind = conn
        .query_row(&kind_sql, [&path.name], |r| r.get::<_, String>(0))
        .map(|t| kind_of(&t))
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::Query(QueryError::new(format!(
                "no such table or view: {}.{}",
                ns, path.name
            ))),
            other => map_err(other),
        })?;

    // table_list: schema, name, type, ncol, wr (WITHOUT ROWID), strict
    let mut options = BTreeMap::new();
    if let Ok(flags) = conn.query_row(&format!("PRAGMA {q_ns}.table_list({q_name})"), [], |r| {
        Ok((r.get::<_, i64>(4)? != 0, r.get::<_, i64>(5)? != 0))
    }) {
        if flags.0 {
            options.insert("without_rowid".into(), "true".into());
        }
        if flags.1 {
            options.insert("strict".into(), "true".into());
        }
    }
    let without_rowid = options.contains_key("without_rowid");

    // table_xinfo: cid, name, type, notnull, dflt_value, pk, hidden
    let raw_columns = query_rows(conn, &format!("PRAGMA {q_ns}.table_xinfo({q_name})"), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)? != 0,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    let pk_count = raw_columns.iter().filter(|c| c.5 > 0).count();
    let mut columns = Vec::new();
    for (cid, name, ty, not_null, default, pk, hidden) in raw_columns {
        // hidden = 1 are the internal columns of virtual tables.
        if hidden == 1 {
            continue;
        }
        let mut col = ColumnInfo::new(
            name,
            u32::try_from(cid).unwrap_or(0) + 1,
            data_type(ty.as_deref()),
        );
        col.nullable = !not_null && pk == 0;
        col.default = default;
        col.primary_key_position = u32::try_from(pk).ok().filter(|p| *p > 0);
        // A lone INTEGER PRIMARY KEY is the rowid alias: the engine assigns it.
        col.auto_increment = pk_count == 1
            && pk == 1
            && !without_rowid
            && ty
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("integer"));
        if hidden >= 2 {
            // 2 = VIRTUAL, 3 = STORED. The expression itself is only in the CREATE statement.
            col.generated = Some(GeneratedColumn {
                expression: String::new(),
                stored: hidden == 3,
            });
        }
        columns.push(col);
    }

    // index_list: seq, name, unique, origin, partial
    let index_heads = query_rows(conn, &format!("PRAGMA {q_ns}.index_list({q_name})"), |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)? != 0,
        ))
    })?;
    let mut indexes = Vec::with_capacity(index_heads.len());
    for (name, unique, origin, partial) in index_heads {
        // index_xinfo: seqno, cid, name (NULL for expressions), desc, coll, key (1 = key column)
        let cols = query_rows(
            conn,
            &format!("PRAGMA {q_ns}.index_xinfo({})", quote(&name)),
            |r| {
                let col_name: Option<String> = r.get(2)?;
                Ok((
                    r.get::<_, i64>(5)? != 0,
                    IndexColumn {
                        expression: col_name.is_none(),
                        name: col_name.unwrap_or_else(|| "<expr>".into()),
                        descending: r.get::<_, i64>(3)? != 0,
                    },
                ))
            },
        )?
        .into_iter()
        .filter_map(|(is_key, col)| is_key.then_some(col))
        .collect();
        let mut extra = BTreeMap::new();
        if partial {
            extra.insert("partial".into(), "true".into());
        }
        indexes.push(IndexInfo {
            name,
            columns: cols,
            unique,
            primary: origin == "pk",
            method: Some("btree".into()),
            predicate: None,
            comment: None,
            extra,
        });
    }

    Ok(TableDetail {
        path: ObjectPath::new(Some(ns), path.name.clone()),
        kind,
        comment: None,
        columns,
        indexes,
        foreign_keys: foreign_keys(conn, ns, &path.name)?,
        checks: Vec::new(),
        triggers: triggers(conn, ns, &path.name)?,
        row_estimate: None,
        size_bytes: None,
        options,
    })
}

fn foreign_keys(conn: &Connection, ns: &str, table: &str) -> Result<Vec<ForeignKeyInfo>> {
    // foreign_key_list: id, seq, table, from, to, on_update, on_delete, match
    let rows = query_rows(
        conn,
        &format!("PRAGMA {}.foreign_key_list({})", quote(ns), quote(table)),
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        },
    )?;
    let mut keys: Vec<(i64, ForeignKeyInfo)> = Vec::new();
    for (id, target, from, to, on_update, on_delete) in rows {
        match keys.iter_mut().find(|(fid, _)| *fid == id) {
            Some((_, fk)) => {
                fk.columns.push(from);
                fk.referenced_columns.extend(to);
            }
            None => keys.push((
                id,
                ForeignKeyInfo {
                    name: None,
                    columns: vec![from],
                    referenced: ObjectPath::new(Some(ns), target),
                    referenced_columns: to.into_iter().collect(),
                    on_update: on_update.as_deref().map(ReferentialAction::parse),
                    on_delete: on_delete.as_deref().map(ReferentialAction::parse),
                    extra: BTreeMap::new(),
                },
            )),
        }
    }
    Ok(keys.into_iter().map(|(_, fk)| fk).collect())
}

pub fn list_relations(conn: &Connection, ns: &str) -> Result<Vec<Relation>> {
    let mut out = Vec::new();
    for object in list_objects(conn, ns)? {
        if object.kind != ObjectKind::Table {
            continue;
        }
        for key in foreign_keys(conn, ns, &object.path.name)? {
            out.push(Relation {
                from: object.path.clone(),
                key,
            });
        }
    }
    Ok(out)
}

fn triggers(conn: &Connection, ns: &str, table: &str) -> Result<Vec<TriggerInfo>> {
    let sql = format!(
        "SELECT name, sql FROM {}.sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 ORDER BY name",
        quote(ns)
    );
    let mut stmt = conn.prepare(&sql).map_err(map_err)?;
    let rows = stmt
        .query_map([table], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    Ok(rows
        .into_iter()
        .map(|(name, definition)| {
            let upper = definition.as_deref().unwrap_or("").to_ascii_uppercase();
            let timing = ["INSTEAD OF", "BEFORE", "AFTER"]
                .into_iter()
                .find(|t| upper.contains(t))
                .unwrap_or("")
                .to_owned();
            let event = ["INSERT", "UPDATE", "DELETE"]
                .into_iter()
                .filter_map(|e| upper.find(e).map(|pos| (pos, e)))
                .min()
                .map(|(_, e)| e.to_owned())
                .unwrap_or_default();
            TriggerInfo {
                name,
                timing,
                event,
                definition,
                enabled: true,
            }
        })
        .collect())
}

pub fn ddl(conn: &Connection, path: &ObjectPath) -> Result<Option<String>> {
    let sql = format!(
        "SELECT sql FROM {}.sqlite_master WHERE name = ?1",
        quote(namespace_of(path))
    );
    match conn.query_row(&sql, [&path.name], |r| r.get::<_, Option<String>>(0)) {
        Ok(ddl) => Ok(ddl),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(map_err(e)),
    }
}
