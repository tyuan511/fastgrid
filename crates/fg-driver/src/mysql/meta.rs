//! MySQL / MariaDB metadata from `information_schema` plus `SHOW CREATE`.
//!
//! Databases are the namespaces: `list_namespaces` is `SHOW DATABASES` and
//! every object path carries its database. No system database is hidden —
//! unlike PostgreSQL's per-session temp schemas, `mysql`,
//! `information_schema`, `performance_schema`, and `sys` are real, browsable
//! databases, and TablePlus-style clients show them.
//!
//! Two judgment calls:
//!
//! * A column whose `COLUMN_DEFAULT` is NULL reads as "no default", even
//!   though `DEFAULT NULL` reports the same way. The distinction is
//!   unobservable through `information_schema` alone, and for INSERT
//!   purposes the two behave identically (omit the column, get NULL).
//! * `CHECK_CONSTRAINTS` does not exist before MariaDB 10.2 / MySQL 8.0.16.
//!   A missing-table error (1146) there is swallowed and checks come back
//!   empty; any other error propagates.

use std::collections::BTreeMap;

use fg_core::{
    CheckConstraint, ColumnInfo, Error, ForeignKeyInfo, GeneratedColumn, IndexColumn, IndexInfo,
    ObjectInfo, ObjectKind, ObjectPath, ReferentialAction, Relation, Result, TableDetail,
    TriggerInfo,
};
use mysql_async::prelude::*;
use mysql_async::{Conn, Params};
use mysql_common::Value as MysqlValue;

use super::types::data_type_from_native;

fn quote(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

fn text(v: &MysqlValue) -> Option<String> {
    match v {
        MysqlValue::NULL => None,
        MysqlValue::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        MysqlValue::Int(v) => Some(v.to_string()),
        MysqlValue::UInt(v) => Some(v.to_string()),
        MysqlValue::Float(v) => Some(v.to_string()),
        MysqlValue::Double(v) => Some(v.to_string()),
        MysqlValue::Date(..) | MysqlValue::Time(..) => None,
    }
}

fn cell(row: &[MysqlValue], i: usize) -> Option<String> {
    row.get(i).and_then(text)
}

fn uint(row: &[MysqlValue], i: usize) -> Option<u64> {
    match row.get(i) {
        Some(MysqlValue::UInt(v)) => Some(*v),
        Some(MysqlValue::Int(v)) if *v >= 0 => Some(*v as u64),
        Some(MysqlValue::Bytes(b)) => String::from_utf8_lossy(b).parse().ok(),
        _ => None,
    }
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty())
}

/// One STATISTICS row, normalized across the MySQL 8.0 shape (with
/// EXPRESSION) and the MariaDB shape (without it).
struct IndexRow {
    name: Option<String>,
    column: Option<String>,
    expression: Option<String>,
    descending: bool,
    unique: bool,
    method: Option<String>,
}

/// Run a query and collect every row. Parameters use the binary protocol;
/// pass `Params::Empty` for plain statements.
pub async fn query_rows(
    conn: &mut Conn,
    sql: &str,
    params: Params,
) -> Result<Vec<Vec<MysqlValue>>> {
    let mut result = conn.exec_iter(sql, params).await.map_err(map_err)?;
    let mut out = Vec::new();
    while let Some(row) = result.next().await.map_err(map_err)? {
        out.push(row.unwrap());
    }
    Ok(out)
}

pub fn map_err(e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(server) => {
            // KILL QUERY surfaces as error 1317; that is a cancellation,
            // not a failed statement.
            if server.code == 1317 {
                return Error::Cancelled;
            }
            // SQLSTATE when the server reports one, else the numeric code.
            // A failed statement is a query error either way.
            let code = if server.state.is_empty() {
                server.code.to_string()
            } else {
                server.state
            };
            Error::Query(fg_core::QueryError {
                message: server.message,
                code: Some(code),
                position: None,
            })
        }
        other => {
            if other.is_fatal() {
                Error::Connection(other.to_string())
            } else {
                Error::Query(fg_core::QueryError::new(other.to_string()))
            }
        }
    }
}

fn is_missing_table(e: &Error) -> bool {
    matches!(e, Error::Query(q) if q.code.as_deref() == Some("42S02"))
}

pub async fn list_namespaces(conn: &mut Conn) -> Result<Vec<String>> {
    let rows = query_rows(conn, "SHOW DATABASES", Params::Empty).await?;
    Ok(rows.into_iter().filter_map(|r| cell(&r, 0)).collect())
}

pub async fn list_objects(conn: &mut Conn, namespace: Option<&str>) -> Result<Vec<ObjectInfo>> {
    let Some(ns) = namespace else {
        return Err(Error::InvalidInput(
            "MySQL objects live in a database; no default database is set".into(),
        ));
    };
    let rows = query_rows(
        conn,
        "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, DATA_LENGTH, \
         INDEX_LENGTH, TABLE_COMMENT FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
        Params::Positional(vec![MysqlValue::Bytes(ns.as_bytes().to_vec())]),
    )
    .await?;
    let mut out = Vec::new();
    for row in rows {
        let Some(name) = cell(&row, 0) else { continue };
        let kind = match cell(&row, 1).as_deref() {
            Some("BASE TABLE") => ObjectKind::Table,
            Some("VIEW") | Some("SYSTEM VIEW") => ObjectKind::View,
            _ => ObjectKind::Other,
        };
        let mut info = ObjectInfo::new(ObjectPath::new(Some(ns), name), kind);
        info.row_estimate = uint(&row, 2);
        let size = match (uint(&row, 3), uint(&row, 4)) {
            (Some(d), Some(i)) => Some(d + i),
            (Some(d), None) => Some(d),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        };
        info.size_bytes = size;
        info.comment = non_empty(cell(&row, 5));
        out.push(info);
    }
    Ok(out)
}

pub async fn describe(conn: &mut Conn, path: &ObjectPath) -> Result<TableDetail> {
    let Some(ns) = path.namespace.as_deref() else {
        return Err(Error::InvalidInput(
            "MySQL objects live in a database; no default database is set".into(),
        ));
    };
    let head = query_rows(
        conn,
        "SELECT TABLE_TYPE, TABLE_ROWS, DATA_LENGTH, INDEX_LENGTH, \
         TABLE_COMMENT, ENGINE, TABLE_COLLATION FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await?;
    let Some(head) = head.into_iter().next() else {
        return Err(Error::Connection(format!(
            "no such object {}.{}",
            ns, path.name
        )));
    };
    let kind = match cell(&head, 0).as_deref() {
        Some("BASE TABLE") => ObjectKind::Table,
        Some("VIEW") | Some("SYSTEM VIEW") => ObjectKind::View,
        _ => ObjectKind::Other,
    };
    let mut detail = TableDetail {
        path: path.clone(),
        kind,
        comment: non_empty(cell(&head, 4)),
        columns: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        triggers: Vec::new(),
        row_estimate: uint(&head, 1),
        size_bytes: match (uint(&head, 2), uint(&head, 3)) {
            (Some(d), Some(i)) => Some(d + i),
            (Some(d), None) => Some(d),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        },
        options: BTreeMap::new(),
    };
    if let Some(engine) = non_empty(cell(&head, 5)) {
        detail.options.insert("engine".into(), engine);
    }
    if let Some(collation) = non_empty(cell(&head, 6)) {
        detail.options.insert("collation".into(), collation);
    }

    // Primary key positions first: column rows reference them.
    let pk_rows = query_rows(
        conn,
        "SELECT COLUMN_NAME, SEQ_IN_INDEX FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = 'PRIMARY' \
         ORDER BY SEQ_IN_INDEX",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await?;
    let mut pk: BTreeMap<String, u32> = BTreeMap::new();
    for row in pk_rows {
        if let (Some(name), Some(seq)) = (cell(&row, 0), uint(&row, 1)) {
            pk.insert(name, seq as u32);
        }
    }

    let col_rows = query_rows(
        conn,
        "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_TYPE, IS_NULLABLE, \
         COLUMN_DEFAULT, EXTRA, GENERATION_EXPRESSION, COLLATION_NAME, \
         COLUMN_COMMENT FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await?;
    for row in col_rows {
        let Some(name) = cell(&row, 0) else { continue };
        let native = cell(&row, 2).unwrap_or_default();
        let ordinal = uint(&row, 1).unwrap_or(0) as u32;
        let mut col = ColumnInfo::new(name.clone(), ordinal, data_type_from_native(&native));
        col.nullable = cell(&row, 3).as_deref() == Some("YES");
        col.default = cell(&row, 4);
        col.primary_key_position = pk.get(&name).copied();
        let extra = cell(&row, 5).unwrap_or_default();
        col.auto_increment = extra.contains("auto_increment");
        if let Some(on_update) = extra.strip_prefix("on update ") {
            col.extra.insert("on_update".into(), on_update.to_owned());
        }
        if let Some(expr) = non_empty(cell(&row, 6)) {
            col.generated = Some(GeneratedColumn {
                expression: expr,
                stored: extra.contains("STORED GENERATED"),
            });
        }
        col.collation = non_empty(cell(&row, 7));
        col.comment = non_empty(cell(&row, 8));
        detail.columns.push(col);
    }

    // MySQL 8.0's STATISTICS carries EXPRESSION for functional indexes;
    // MariaDB's does not (unknown column 42S22). Try the full shape first,
    // then fall back to the shared columns.
    let idx_rows: Vec<IndexRow> = match query_rows(
        conn,
        "SELECT INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME, EXPRESSION, COLLATION, \
         NON_UNIQUE, INDEX_TYPE FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
         ORDER BY INDEX_NAME, SEQ_IN_INDEX",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| IndexRow {
                name: cell(&r, 0),
                column: cell(&r, 2),
                expression: non_empty(cell(&r, 3)),
                descending: cell(&r, 4).as_deref() == Some("D"),
                unique: uint(&r, 5) == Some(0),
                method: non_empty(cell(&r, 6)),
            })
            .collect(),
        Err(e) if matches!(&e, Error::Query(q) if q.code.as_deref() == Some("42S22")) => {
            query_rows(
                conn,
                "SELECT INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME, COLLATION, \
                 NON_UNIQUE, INDEX_TYPE FROM information_schema.STATISTICS \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                 ORDER BY INDEX_NAME, SEQ_IN_INDEX",
                Params::Positional(vec![
                    MysqlValue::Bytes(ns.as_bytes().to_vec()),
                    MysqlValue::Bytes(path.name.as_bytes().to_vec()),
                ]),
            )
            .await?
            .into_iter()
            .map(|r| IndexRow {
                name: cell(&r, 0),
                column: cell(&r, 2),
                expression: None,
                descending: cell(&r, 3).as_deref() == Some("D"),
                unique: uint(&r, 4) == Some(0),
                method: non_empty(cell(&r, 5)),
            })
            .collect()
        }
        Err(e) => return Err(e),
    };
    let mut indexes: BTreeMap<String, IndexInfo> = BTreeMap::new();
    for row in idx_rows {
        let Some(name) = row.name else { continue };
        let entry = indexes.entry(name.clone()).or_insert_with(|| IndexInfo {
            name: name.clone(),
            columns: Vec::new(),
            unique: row.unique,
            primary: name == "PRIMARY",
            method: row.method.clone(),
            predicate: None,
            comment: None,
            extra: BTreeMap::new(),
        });
        if let Some(expr) = row.expression {
            entry.columns.push(IndexColumn {
                name: expr,
                expression: true,
                descending: row.descending,
            });
        } else if let Some(col) = row.column {
            entry.columns.push(IndexColumn {
                name: col,
                expression: false,
                descending: row.descending,
            });
        }
    }
    detail.indexes = indexes.into_values().collect();

    detail.foreign_keys = query_foreign_keys(conn, ns, Some(&path.name)).await?;

    // CHECK_CONSTRAINTS is absent on old servers; a missing table there
    // just means "no checks to report".
    match query_rows(
        conn,
        "SELECT CONSTRAINT_NAME, CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS \
         WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ?",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await
    {
        Ok(rows) => {
            for row in rows {
                if let Some(expr) = cell(&row, 1) {
                    detail.checks.push(CheckConstraint {
                        name: non_empty(cell(&row, 0)),
                        expression: expr,
                    });
                }
            }
        }
        Err(e) if is_missing_table(&e) => {}
        Err(e) => return Err(e),
    }

    let trg_rows = query_rows(
        conn,
        "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT \
         FROM information_schema.TRIGGERS \
         WHERE TRIGGER_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await?;
    for row in trg_rows {
        if let Some(name) = cell(&row, 0) {
            detail.triggers.push(TriggerInfo {
                name,
                timing: cell(&row, 1).unwrap_or_default(),
                event: cell(&row, 2).unwrap_or_default(),
                definition: cell(&row, 3),
                enabled: true,
            });
        }
    }
    Ok(detail)
}

async fn query_foreign_keys(
    conn: &mut Conn,
    namespace: &str,
    table: Option<&str>,
) -> Result<Vec<ForeignKeyInfo>> {
    let (sql, params) = match table {
        Some(table) => (
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.ORDINAL_POSITION, \
             k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
             k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k \
             JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
               ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
               AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
             WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? \
               AND k.REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            Params::Positional(vec![
                MysqlValue::Bytes(namespace.as_bytes().to_vec()),
                MysqlValue::Bytes(table.as_bytes().to_vec()),
            ]),
        ),
        None => (
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.ORDINAL_POSITION, \
             k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
             k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k \
             JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
               ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
               AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
             WHERE k.TABLE_SCHEMA = ? \
               AND k.REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
            Params::Positional(vec![MysqlValue::Bytes(namespace.as_bytes().to_vec())]),
        ),
    };
    let rows = query_rows(conn, sql, params).await?;
    let mut out: Vec<ForeignKeyInfo> = Vec::new();
    for row in rows {
        let name = non_empty(cell(&row, 0));
        let Some(col) = cell(&row, 1) else { continue };
        let ref_ns = cell(&row, 3);
        let Some(ref_table) = cell(&row, 4) else {
            continue;
        };
        let ref_col = cell(&row, 5);
        // Consecutive rows of one constraint share its name; a new name
        // starts a new key. (Two constraints could share a name only across
        // tables, and list_relations groups per table by ordering.)
        let same = out.last().is_some_and(|last: &ForeignKeyInfo| {
            last.name == name && last.referenced.name == ref_table
        });
        if same {
            if let Some(last) = out.last_mut() {
                last.columns.push(col);
                if let Some(rc) = ref_col {
                    last.referenced_columns.push(rc);
                }
            }
        } else {
            out.push(ForeignKeyInfo {
                name,
                columns: vec![col],
                referenced: ObjectPath::new(ref_ns.as_deref(), ref_table),
                referenced_columns: ref_col.into_iter().collect(),
                on_update: cell(&row, 6).map(|s| ReferentialAction::parse(&s)),
                on_delete: cell(&row, 7).map(|s| ReferentialAction::parse(&s)),
                extra: BTreeMap::new(),
            });
        }
    }
    Ok(out)
}

pub async fn list_relations(conn: &mut Conn, namespace: Option<&str>) -> Result<Vec<Relation>> {
    let Some(ns) = namespace else {
        return Err(Error::InvalidInput(
            "MySQL relations live in a database; no default database is set".into(),
        ));
    };
    // The bulk query does not carry the owning table, so re-derive it:
    // group by (table, constraint) via a second column in the query.
    let rows = query_rows(
        conn,
        "SELECT k.TABLE_NAME, k.CONSTRAINT_NAME, k.COLUMN_NAME, k.ORDINAL_POSITION, \
         k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
         k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
         FROM information_schema.KEY_COLUMN_USAGE k \
         JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
           ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
           AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
         WHERE k.TABLE_SCHEMA = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
         ORDER BY k.TABLE_NAME, k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
        Params::Positional(vec![MysqlValue::Bytes(ns.as_bytes().to_vec())]),
    )
    .await?;
    let mut out: Vec<Relation> = Vec::new();
    for row in rows {
        let Some(from_table) = cell(&row, 0) else {
            continue;
        };
        let name = non_empty(cell(&row, 1));
        let Some(col) = cell(&row, 2) else { continue };
        let ref_ns = cell(&row, 4);
        let Some(ref_table) = cell(&row, 5) else {
            continue;
        };
        let ref_col = cell(&row, 6);
        let same = out.last().is_some_and(|last: &Relation| {
            last.from.name == from_table
                && last.key.name == name
                && last.key.referenced.name == ref_table
        });
        if same {
            if let Some(last) = out.last_mut() {
                last.key.columns.push(col);
                if let Some(rc) = ref_col {
                    last.key.referenced_columns.push(rc);
                }
            }
        } else {
            out.push(Relation {
                from: ObjectPath::new(Some(ns), from_table),
                key: ForeignKeyInfo {
                    name,
                    columns: vec![col],
                    referenced: ObjectPath::new(ref_ns.as_deref(), ref_table),
                    referenced_columns: ref_col.into_iter().collect(),
                    on_update: cell(&row, 7).map(|s| ReferentialAction::parse(&s)),
                    on_delete: cell(&row, 8).map(|s| ReferentialAction::parse(&s)),
                    extra: BTreeMap::new(),
                },
            });
        }
    }
    Ok(out)
}

pub async fn ddl(conn: &mut Conn, path: &ObjectPath) -> Result<Option<String>> {
    let Some(ns) = path.namespace.as_deref() else {
        return Err(Error::InvalidInput(
            "MySQL objects live in a database; no default database is set".into(),
        ));
    };
    let kind_rows = query_rows(
        conn,
        "SELECT TABLE_TYPE FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
        Params::Positional(vec![
            MysqlValue::Bytes(ns.as_bytes().to_vec()),
            MysqlValue::Bytes(path.name.as_bytes().to_vec()),
        ]),
    )
    .await?;
    let kind = kind_rows
        .first()
        .and_then(|r| cell(r, 0))
        .unwrap_or_default();
    // SHOW CREATE does not accept parameters; the names are backtick-quoted
    // with doubling, which is the standard safe interpolation for MySQL.
    let sql = match kind.as_str() {
        "VIEW" | "SYSTEM VIEW" => {
            format!("SHOW CREATE VIEW {}.{}", quote(ns), quote(&path.name))
        }
        _ => format!("SHOW CREATE TABLE {}.{}", quote(ns), quote(&path.name)),
    };
    let rows = query_rows(conn, &sql, Params::Empty).await?;
    Ok(rows
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().nth(1))
        .and_then(|v| text(&v)))
}
