use fg_core::{
    ColumnInfo, Error, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind, ObjectPath, QueryError,
    Result, TableDetail,
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
        Ok(ObjectInfo {
            path: ObjectPath::new(Some(ns), name),
            kind: kind_of(&ty),
            comment: None,
        })
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

    // table_xinfo: cid, name, type, notnull, dflt_value, pk, hidden
    let columns = query_rows(conn, &format!("PRAGMA {q_ns}.table_xinfo({q_name})"), |r| {
        let pk: i64 = r.get(5)?;
        let hidden: i64 = r.get(6)?;
        let ty: Option<String> = r.get(2)?;
        Ok((
            hidden,
            ColumnInfo {
                name: r.get(1)?,
                data_type: data_type(ty.as_deref()),
                nullable: r.get::<_, i64>(3)? == 0,
                default: r.get(4)?,
                primary_key_position: u32::try_from(pk).ok().filter(|p| *p > 0),
                comment: None,
            },
        ))
    })?
    .into_iter()
    // hidden = 1 are the internal columns of virtual tables.
    .filter_map(|(hidden, col)| (hidden != 1).then_some(col))
    .collect();

    // index_list: seq, name, unique, origin, partial
    let index_heads = query_rows(conn, &format!("PRAGMA {q_ns}.index_list({q_name})"), |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut indexes = Vec::with_capacity(index_heads.len());
    for (name, unique, origin) in index_heads {
        // index_info: seqno, cid, name (NULL for expression columns)
        let cols = query_rows(
            conn,
            &format!("PRAGMA {q_ns}.index_info({})", quote(&name)),
            |r| {
                Ok(r.get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "<expr>".into()))
            },
        )?;
        indexes.push(IndexInfo {
            name,
            columns: cols,
            unique,
            primary: origin == "pk",
        });
    }

    // foreign_key_list: id, seq, table, from, to, on_update, on_delete, match
    let fk_rows = query_rows(
        conn,
        &format!("PRAGMA {q_ns}.foreign_key_list({q_name})"),
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
    let mut foreign_keys: Vec<(i64, ForeignKeyInfo)> = Vec::new();
    for (id, table, from, to, on_update, on_delete) in fk_rows {
        match foreign_keys.iter_mut().find(|(fid, _)| *fid == id) {
            Some((_, fk)) => {
                fk.columns.push(from);
                fk.referenced_columns.extend(to);
            }
            None => foreign_keys.push((
                id,
                ForeignKeyInfo {
                    name: None,
                    columns: vec![from],
                    referenced: ObjectPath::new(Some(ns), table),
                    referenced_columns: to.into_iter().collect(),
                    on_update,
                    on_delete,
                },
            )),
        }
    }

    Ok(TableDetail {
        path: ObjectPath::new(Some(ns), path.name.clone()),
        kind,
        columns,
        indexes,
        foreign_keys: foreign_keys.into_iter().map(|(_, fk)| fk).collect(),
    })
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
