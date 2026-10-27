//! SQL Server catalogue queries: databases, schemas, objects, table detail, DDL.
//!
//! Everything reads the `sys.*` catalogues with parameterless batch queries and
//! decodes the result sets with [`query_rows`]. Three shapes of the catalogues
//! need care:
//!
//! * `sys.default_constraints.definition` comes back with the expression wrapped
//!   in extra parentheses (`((0))`), so the outer pair is stripped to get the
//!   text a user wrote.
//! * `sys.types` reports `max_length` in *bytes*: `nvarchar(50)` is 100 there.
//!   Character types are halved on the way out, and `-1` means `(max)`.
//! * A default constraint is unnamed by the engine (`DF__t__c__4A1B2C3D`), so its
//!   real name is kept in `ColumnInfo::extra["default_constraint"]`, which is
//!   what lets `MsSqlDialect::alter_column_sqls` drop it later.
//!
//! `ddl()` renders a `CREATE TABLE` from the described table rather than
//! replaying a script, because SQL Server keeps no stored `CREATE` text for a
//! table (unlike MySQL's `SHOW CREATE TABLE`).

use fg_core::{
    CheckConstraint, ColumnInfo, Error, ForeignKeyInfo, GeneratedColumn, IndexColumn, IndexInfo,
    ObjectInfo, ObjectKind, ObjectPath, QueryError, ReferentialAction, Relation, Result,
    TableDetail, TriggerInfo,
};
use tiberius::Row;

use super::run::{Conn, map_err};
use super::types::data_type_from_native;

/// Identifier quoting for interpolation into catalogue text. `SHOW`-style
/// statements and `OBJECT_ID('...')` take no parameters, so names are quoted
/// and escaped here instead.
pub fn quote(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

/// A string literal for interpolation: doubled quotes, `N` prefix so the
/// server does not lose non-ASCII names.
fn literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// Run one query and hand back the raw rows.
pub async fn query_rows(conn: &mut Conn, sql: &str) -> Result<Vec<Row>> {
    use futures::TryStreamExt;
    let mut stream = conn.simple_query(sql).await.map_err(map_err)?;
    let mut sets: Vec<Vec<Row>> = Vec::new();
    loop {
        match stream.try_next().await.map_err(map_err)? {
            Some(tiberius::QueryItem::Metadata(_)) => sets.push(Vec::new()),
            Some(tiberius::QueryItem::Row(row)) => match sets.last_mut() {
                Some(set) => set.push(row),
                None => sets.push(vec![row]),
            },
            None => break,
        }
    }
    Ok(sets.into_iter().flatten().collect())
}

fn text(row: &Row, index: usize) -> Option<String> {
    row.try_get::<&str, _>(index)
        .ok()
        .flatten()
        .map(str::to_owned)
}

fn int(row: &Row, index: usize) -> Option<i64> {
    if let Ok(v) = row.try_get::<i32, _>(index) {
        return v.map(i64::from);
    }
    if let Ok(v) = row.try_get::<i16, _>(index) {
        return v.map(i64::from);
    }
    if let Ok(v) = row.try_get::<i64, _>(index) {
        return v;
    }
    row.try_get::<u8, _>(index).ok().flatten().map(i64::from)
}

fn boolean(row: &Row, index: usize) -> bool {
    row.try_get::<bool, _>(index)
        .ok()
        .flatten()
        .unwrap_or(false)
        || int(row, index).is_some_and(|v| v != 0)
}

fn uint(row: &Row, index: usize) -> Option<u64> {
    int(row, index).and_then(|v| u64::try_from(v).ok())
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

/// Strip the extra parentheses the catalogue wraps default expressions in.
fn strip_parens(text: &str) -> String {
    let mut current = text.trim();
    while current.starts_with('(') && current.ends_with(')') {
        let inner = &current[1..current.len() - 1];
        // Only strip when the outer pair actually matches: `(a) + (b)` must
        // keep both halves.
        if !balanced(inner) {
            break;
        }
        current = inner.trim();
    }
    current.to_owned()
}

fn balanced(text: &str) -> bool {
    let mut depth = 0i32;
    for ch in text.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Databases the login can see.
pub async fn list_databases(conn: &mut Conn) -> Result<Vec<String>> {
    let rows = query_rows(conn, "SELECT name FROM sys.databases ORDER BY name").await?;
    Ok(rows.iter().filter_map(|r| text(r, 0)).collect())
}

/// Schemas in the current database.
pub async fn list_namespaces(conn: &mut Conn) -> Result<Vec<String>> {
    let rows = query_rows(
        conn,
        "SELECT name FROM sys.schemas \
         WHERE name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') \
           AND name NOT LIKE 'db\\_%' ESCAPE '\\' \
         ORDER BY name",
    )
    .await?;
    Ok(rows.iter().filter_map(|r| text(r, 0)).collect())
}

/// Tables, views and procedures in one schema.
pub async fn list_objects(conn: &mut Conn, namespace: Option<&str>) -> Result<Vec<ObjectInfo>> {
    let schema = namespace.unwrap_or("dbo");
    // `sys.partitions` gives the row count of the heap or clustered index only;
    // summing index_id IN (0,1) is the standard estimate for a table.
    let sql = format!(
        "SELECT o.name, o.type, \
                CAST(ep.value AS nvarchar(max)) AS comment, \
                SUM(CASE WHEN p.index_id IN (0, 1) THEN p.rows ELSE 0 END) AS row_count, \
                SUM(a.total_pages) * 8 * 1024 AS size_bytes \
         FROM sys.objects o \
         LEFT JOIN sys.partitions p ON p.object_id = o.object_id \
         LEFT JOIN sys.allocation_units a ON a.container_id = p.partition_id \
         LEFT JOIN sys.extended_properties ep \
                ON ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description' \
         WHERE SCHEMA_NAME(o.schema_id) = {schema} AND o.type IN ('U', 'V', 'P') \
         GROUP BY o.name, o.type, ep.value \
         ORDER BY o.name",
        schema = literal(schema)
    );
    let rows = query_rows(conn, &sql).await?;
    let mut out = Vec::new();
    for row in &rows {
        let Some(name) = text(row, 0) else { continue };
        // `sys.objects.type` is `char(2)`, so a single-letter type arrives padded
        // with a trailing space (`"V "`); compare trimmed.
        let kind = match text(row, 1).as_deref().map(str::trim) {
            Some("U") => ObjectKind::Table,
            Some("V") => ObjectKind::View,
            Some("P") => ObjectKind::Procedure,
            _ => ObjectKind::Other,
        };
        let mut info = ObjectInfo::new(ObjectPath::new(Some(schema), name), kind);
        info.comment = non_empty(text(row, 2));
        info.row_estimate = uint(row, 3);
        info.size_bytes = uint(row, 4);
        out.push(info);
    }
    Ok(out)
}

/// Full detail for one table or view.
pub async fn describe(conn: &mut Conn, path: &ObjectPath) -> Result<TableDetail> {
    let schema = path.namespace.as_deref().unwrap_or("dbo");
    let object_id = object_id(conn, schema, &path.name).await?;
    let kind = object_kind(conn, object_id).await?;
    let mut detail = TableDetail {
        path: ObjectPath::new(Some(schema), path.name.clone()),
        kind,
        comment: object_comment(conn, object_id).await?,
        columns: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        triggers: Vec::new(),
        row_estimate: row_estimate(conn, object_id).await?,
        size_bytes: None,
        options: Default::default(),
    };
    detail.columns = columns(conn, object_id).await?;
    detail.indexes = indexes(conn, object_id).await?;
    // `sys.columns` has no "part of the primary key" flag; the primary index
    // carries the positions, and `TableDetail::primary_key()` reads them off
    // the columns.
    if let Some(primary) = detail.indexes.iter().find(|i| i.primary) {
        for (position, column) in primary.columns.iter().enumerate() {
            if let Some(info) = detail.columns.iter_mut().find(|c| c.name == column.name) {
                info.primary_key_position = Some(position as u32 + 1);
            }
        }
    }
    detail.foreign_keys = foreign_keys(conn, object_id, schema).await?;
    detail.checks = checks(conn, object_id).await?;
    detail.triggers = triggers(conn, object_id).await?;
    Ok(detail)
}

async fn object_id(conn: &mut Conn, schema: &str, name: &str) -> Result<i32> {
    let qualified = format!("{}.{}", quote(schema), quote(name));
    let sql = format!("SELECT OBJECT_ID({}) AS id", literal(&qualified));
    let rows = query_rows(conn, &sql).await?;
    match rows.first().and_then(|r| int(r, 0)) {
        Some(id) if id != 0 => {
            i32::try_from(id).map_err(|_| Error::Query(QueryError::new("object id out of range")))
        }
        _ => Err(Error::Query(
            QueryError::new(format!("no object named `{}` in schema `{schema}`", name))
                .with_code("208".to_owned()),
        )),
    }
}

async fn object_kind(conn: &mut Conn, object_id: i32) -> Result<ObjectKind> {
    let sql = format!("SELECT type FROM sys.objects WHERE object_id = {object_id}");
    let rows = query_rows(conn, &sql).await?;
    Ok(
        match rows
            .first()
            .and_then(|r| text(r, 0))
            .as_deref()
            .map(str::trim)
        {
            Some("V") => ObjectKind::View,
            Some("P") => ObjectKind::Procedure,
            Some("FN") | Some("IF") | Some("TF") => ObjectKind::Function,
            Some("SO") => ObjectKind::Sequence,
            _ => ObjectKind::Table,
        },
    )
}

async fn object_comment(conn: &mut Conn, object_id: i32) -> Result<Option<String>> {
    let sql = format!(
        "SELECT CAST(ep.value AS nvarchar(max)) FROM sys.extended_properties ep \
         WHERE ep.major_id = {object_id} AND ep.minor_id = 0 AND ep.name = 'MS_Description'"
    );
    let rows = query_rows(conn, &sql).await?;
    Ok(non_empty(rows.first().and_then(|r| text(r, 0))))
}

async fn row_estimate(conn: &mut Conn, object_id: i32) -> Result<Option<u64>> {
    let sql = format!(
        "SELECT SUM(p.rows) FROM sys.partitions p \
         WHERE p.object_id = {object_id} AND p.index_id IN (0, 1)"
    );
    let rows = query_rows(conn, &sql).await?;
    Ok(rows.first().and_then(|r| uint(r, 0)))
}

async fn columns(conn: &mut Conn, object_id: i32) -> Result<Vec<ColumnInfo>> {
    // `max_length` is in bytes: character types need halving, `-1` is `(max)`.
    // `is_identity` marks IDENTITY columns, `is_computed` computed ones.
    let sql = format!(
        "SELECT c.name, c.column_id, t.name AS type_name, c.max_length, \
                c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed, \
                dc.definition, dc.name AS default_name, c.collation_name, \
                CAST(ep.value AS nvarchar(max)) AS comment, cc.definition AS computed_def \
         FROM sys.columns c \
         JOIN sys.types t ON t.user_type_id = c.user_type_id \
         LEFT JOIN sys.default_constraints dc \
                ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
         LEFT JOIN sys.computed_columns cc \
                ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN sys.extended_properties ep \
                ON ep.major_id = c.object_id AND ep.minor_id = c.column_id \
               AND ep.name = 'MS_Description' \
         WHERE c.object_id = {object_id} \
         ORDER BY c.column_id"
    );
    let rows = query_rows(conn, &sql).await?;
    let mut out = Vec::new();
    for row in &rows {
        let Some(name) = text(row, 0) else { continue };
        let ordinal = uint(row, 1).unwrap_or(0) as u32;
        let type_name = text(row, 2).unwrap_or_else(|| "sql_variant".to_owned());
        let max_length = int(row, 3).unwrap_or(-1);
        let precision = int(row, 4).unwrap_or(0);
        let scale = int(row, 5).unwrap_or(0);
        let native = render_type(&type_name, max_length, precision, scale);
        let mut info = ColumnInfo::new(name, ordinal, data_type_from_native(&native));
        info.nullable = boolean(row, 6);
        info.auto_increment = boolean(row, 7);
        if boolean(row, 8) {
            info.generated = Some(GeneratedColumn {
                expression: text(row, 13).unwrap_or_default(),
                // SQL Server only has persisted computed columns.
                stored: true,
            });
        }
        if let Some(def) = non_empty(text(row, 9)) {
            info.default = Some(strip_parens(&def));
        }
        if let Some(name) = non_empty(text(row, 10)) {
            info.extra
                .insert(fg_core::DEFAULT_CONSTRAINT_KEY.to_owned(), name);
        }
        info.collation = non_empty(text(row, 11));
        info.comment = non_empty(text(row, 12));
        out.push(info);
    }
    Ok(out)
}

/// Rebuild a declared type from the catalogue's byte lengths.
fn render_type(type_name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let lower = type_name.to_ascii_lowercase();
    let wide = matches!(lower.as_str(), "nvarchar" | "nchar");
    match lower.as_str() {
        "varchar" | "char" | "varbinary" | "binary" | "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{type_name}(max)")
            } else {
                let length = if wide { max_length / 2 } else { max_length };
                format!("{type_name}({length})")
            }
        }
        "decimal" | "numeric" => format!("{type_name}({precision},{scale})"),
        "datetime2" | "datetimeoffset" | "time" => format!("{type_name}({scale})"),
        _ => type_name.to_owned(),
    }
}

async fn indexes(conn: &mut Conn, object_id: i32) -> Result<Vec<IndexInfo>> {
    let sql = format!(
        "SELECT i.name, i.type_desc, i.is_unique, i.is_primary_key, i.is_unique_constraint, \
                i.filter_definition, \
                STUFF((SELECT ', ' + c2.name + CASE WHEN ic2.is_descending_key = 1 THEN ' DESC' ELSE '' END \
                       FROM sys.index_columns ic2 \
                       JOIN sys.columns c2 ON c2.object_id = ic2.object_id AND c2.column_id = ic2.column_id \
                       WHERE ic2.object_id = i.object_id AND ic2.index_id = i.index_id \
                         AND ic2.is_included_column = 0 \
                       ORDER BY ic2.key_ordinal \
                       FOR XML PATH('')), 1, 2, '') AS key_columns, \
                STUFF((SELECT ', ' + c3.name \
                       FROM sys.index_columns ic3 \
                       JOIN sys.columns c3 ON c3.object_id = ic3.object_id AND c3.column_id = ic3.column_id \
                       WHERE ic3.object_id = i.object_id AND ic3.index_id = i.index_id \
                         AND ic3.is_included_column = 1 \
                       FOR XML PATH('')), 1, 2, '') AS included_columns \
         FROM sys.indexes i \
         WHERE i.object_id = {object_id} AND i.name IS NOT NULL \
         ORDER BY i.is_primary_key DESC, i.name"
    );
    let rows = query_rows(conn, &sql).await?;
    let mut out = Vec::new();
    for row in &rows {
        let Some(name) = text(row, 0) else { continue };
        let keys = text(row, 6).unwrap_or_default();
        let columns: Vec<IndexColumn> = keys
            .split(", ")
            .filter(|s| !s.is_empty())
            .map(|entry| match entry.strip_suffix(" DESC") {
                Some(column) => {
                    let mut c = IndexColumn::column(column);
                    c.descending = true;
                    c
                }
                None => IndexColumn::column(entry),
            })
            .collect();
        // Included (non-key) columns are not part of the key, so they cannot
        // ride in `columns` — `create_index_sql` would put them in the key
        // list. They are recorded as text instead.
        let mut extra = std::collections::BTreeMap::new();
        if let Some(included) = non_empty(text(row, 7)) {
            extra.insert("included".to_owned(), included);
        }
        out.push(IndexInfo {
            name,
            columns,
            unique: boolean(row, 2),
            primary: boolean(row, 3),
            method: non_empty(text(row, 1)),
            predicate: non_empty(text(row, 5)),
            comment: None,
            extra,
        });
    }
    Ok(out)
}

async fn foreign_keys(
    conn: &mut Conn,
    object_id: i32,
    schema: &str,
) -> Result<Vec<ForeignKeyInfo>> {
    let sql = format!(
        "SELECT fk.name, \
                STUFF((SELECT ', ' + pc.name \
                       FROM sys.foreign_key_columns fkc \
                       JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
                                          AND pc.column_id = fkc.parent_column_id \
                       WHERE fkc.constraint_object_id = fk.object_id \
                       ORDER BY fkc.constraint_column_id \
                       FOR XML PATH('')), 1, 2, '') AS child_columns, \
                SCHEMA_NAME(rt.schema_id) AS ref_schema, rt.name AS ref_table, \
                STUFF((SELECT ', ' + rc.name \
                       FROM sys.foreign_key_columns fkc \
                       JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
                                          AND rc.column_id = fkc.referenced_column_id \
                       WHERE fkc.constraint_object_id = fk.object_id \
                       ORDER BY fkc.constraint_column_id \
                       FOR XML PATH('')), 1, 2, '') AS ref_columns, \
                fk.update_referential_action_desc, fk.delete_referential_action_desc, \
                fk.is_disabled \
         FROM sys.foreign_keys fk \
         JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
         WHERE fk.parent_object_id = {object_id} \
         ORDER BY fk.name"
    );
    let rows = query_rows(conn, &sql).await?;
    let mut out = Vec::new();
    for row in &rows {
        let Some(name) = text(row, 0) else { continue };
        let child = split_list(&text(row, 1).unwrap_or_default());
        let ref_schema = text(row, 2).unwrap_or_else(|| schema.to_owned());
        let ref_table = text(row, 3).unwrap_or_default();
        let ref_columns = split_list(&text(row, 4).unwrap_or_default());
        out.push(ForeignKeyInfo {
            name: Some(name),
            columns: child,
            referenced: ObjectPath::new(Some(&ref_schema), ref_table),
            referenced_columns: ref_columns,
            on_update: action(text(row, 5)),
            on_delete: action(text(row, 6)),
            extra: Default::default(),
        });
    }
    Ok(out)
}

fn split_list(text: &str) -> Vec<String> {
    text.split(", ")
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `NO_ACTION` / `CASCADE` / `SET_NULL` / `SET_DEFAULT` → `ReferentialAction`.
fn action(text: Option<String>) -> Option<ReferentialAction> {
    let text = text?;
    let normalised = text.replace('_', " ");
    Some(ReferentialAction::parse(&normalised))
}

async fn checks(conn: &mut Conn, object_id: i32) -> Result<Vec<CheckConstraint>> {
    let sql = format!(
        "SELECT cc.name, cc.definition FROM sys.check_constraints cc \
         WHERE cc.parent_object_id = {object_id} ORDER BY cc.name"
    );
    let rows = query_rows(conn, &sql).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let expression = text(row, 1)?;
            Some(CheckConstraint {
                name: non_empty(text(row, 0)),
                expression: strip_parens(&expression),
            })
        })
        .collect())
}

async fn triggers(conn: &mut Conn, object_id: i32) -> Result<Vec<TriggerInfo>> {
    let sql = format!(
        "SELECT tr.name, tr.is_instead_of_trigger, tr.is_disabled, \
                OBJECT_DEFINITION(tr.object_id) AS body, \
                STUFF((SELECT ', ' + e.type_desc \
                       FROM sys.trigger_events e \
                       WHERE e.object_id = tr.object_id \
                       FOR XML PATH('')), 1, 2, '') AS events \
         FROM sys.triggers tr \
         WHERE tr.parent_id = {object_id} ORDER BY tr.name"
    );
    let rows = query_rows(conn, &sql).await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let name = text(row, 0)?;
            let timing = if boolean(row, 1) {
                "INSTEAD OF".to_owned()
            } else {
                "AFTER".to_owned()
            };
            Some(TriggerInfo {
                name,
                timing,
                event: text(row, 4).unwrap_or_default().replace('_', " "),
                definition: non_empty(text(row, 3)),
                enabled: !boolean(row, 2),
            })
        })
        .collect())
}

/// Relations (foreign keys) for every table in one schema.
pub async fn list_relations(conn: &mut Conn, namespace: Option<&str>) -> Result<Vec<Relation>> {
    let schema = namespace.unwrap_or("dbo");
    let sql = format!(
        "SELECT SCHEMA_NAME(t.schema_id) AS child_schema, t.name AS child_table, \
                fk.name, \
                STUFF((SELECT ', ' + pc.name \
                       FROM sys.foreign_key_columns fkc \
                       JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id \
                                          AND pc.column_id = fkc.parent_column_id \
                       WHERE fkc.constraint_object_id = fk.object_id \
                       ORDER BY fkc.constraint_column_id \
                       FOR XML PATH('')), 1, 2, '') AS child_columns, \
                SCHEMA_NAME(rt.schema_id) AS ref_schema, rt.name AS ref_table, \
                STUFF((SELECT ', ' + rc.name \
                       FROM sys.foreign_key_columns fkc \
                       JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id \
                                          AND rc.column_id = fkc.referenced_column_id \
                       WHERE fkc.constraint_object_id = fk.object_id \
                       ORDER BY fkc.constraint_column_id \
                       FOR XML PATH('')), 1, 2, '') AS ref_columns, \
                fk.update_referential_action_desc, fk.delete_referential_action_desc \
         FROM sys.foreign_keys fk \
         JOIN sys.tables t ON t.object_id = fk.parent_object_id \
         JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
         WHERE SCHEMA_NAME(t.schema_id) = {schema} \
         ORDER BY t.name, fk.name",
        schema = literal(schema)
    );
    let rows = query_rows(conn, &sql).await?;
    let mut out = Vec::new();
    for row in &rows {
        let child_schema = text(row, 0).unwrap_or_else(|| schema.to_owned());
        let Some(child_table) = text(row, 1) else {
            continue;
        };
        let Some(name) = text(row, 2) else { continue };
        let ref_schema = text(row, 4).unwrap_or_else(|| schema.to_owned());
        let ref_table = text(row, 5).unwrap_or_default();
        out.push(Relation {
            from: ObjectPath::new(Some(&child_schema), child_table),
            key: ForeignKeyInfo {
                name: Some(name),
                columns: split_list(&text(row, 3).unwrap_or_default()),
                referenced: ObjectPath::new(Some(&ref_schema), ref_table),
                referenced_columns: split_list(&text(row, 6).unwrap_or_default()),
                on_update: action(text(row, 7)),
                on_delete: action(text(row, 8)),
                extra: Default::default(),
            },
        });
    }
    Ok(out)
}

/// A `CREATE TABLE` / `CREATE VIEW` script for one object.
pub async fn ddl(conn: &mut Conn, path: &ObjectPath) -> Result<Option<String>> {
    let schema = path.namespace.as_deref().unwrap_or("dbo");
    let object_id = match object_id(conn, schema, &path.name).await {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let kind = object_kind(conn, object_id).await?;
    if kind == ObjectKind::View {
        // Views keep their definition in the catalogue.
        let sql = format!("SELECT OBJECT_DEFINITION({object_id})");
        let rows = query_rows(conn, &sql).await?;
        return Ok(rows.first().and_then(|r| text(r, 0)));
    }
    let detail = describe(conn, path).await?;
    let dialect: &dyn fg_core::Dialect = &fg_core::MsSqlDialect;
    let mut script = fg_sql::render_create_table_as(&detail, &detail.path, dialect);
    let table = dialect.qualified_name(&detail.path);
    for index in &detail.indexes {
        if index.primary {
            continue;
        }
        script.push_str(";\n\n");
        script.push_str(&dialect.create_index_sql(&table, index));
    }
    Ok(Some(script))
}

/// Server version string for the connection banner.
pub async fn server_version(conn: &mut Conn) -> Result<String> {
    let rows = query_rows(conn, "SELECT @@VERSION").await?;
    Ok(rows
        .first()
        .and_then(|r| text(r, 0))
        .unwrap_or_else(|| "SQL Server".to_owned()))
}
