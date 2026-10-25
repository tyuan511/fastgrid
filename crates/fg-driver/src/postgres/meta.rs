//! PostgreSQL metadata from `pg_catalog`: namespaces, objects, table detail, DDL,
//! and bulk foreign keys. Everything is read-only catalog SQL; no `pg_dump` binary.

use std::collections::BTreeMap;

use fg_core::{
    CheckConstraint, ColumnInfo, Error, ForeignKeyInfo, GeneratedColumn, IndexColumn, IndexInfo,
    ObjectInfo, ObjectKind, ObjectPath, ReferentialAction, Relation, Result, TableDetail,
    TriggerInfo,
};
use tokio_postgres::Client;

use super::run::map_err;
use super::types::data_type_from_native;

/// Schemas that are not user data: temporary schemas and TOAST tables. `pg_catalog`
/// and `information_schema` stay listed — they are real, browsable schemas.
pub async fn list_namespaces(client: &Client) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT LIKE 'pg_temp%' AND nspname NOT LIKE 'pg_toast%' \
             ORDER BY nspname",
            &[],
        )
        .await
        .map_err(map_err)?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

fn kind_of(relkind: &str) -> Option<ObjectKind> {
    match relkind {
        "r" | "p" => Some(ObjectKind::Table),
        "v" => Some(ObjectKind::View),
        "m" => Some(ObjectKind::MaterializedView),
        _ => None,
    }
}

pub async fn list_objects(client: &Client, namespace: &str) -> Result<Vec<ObjectInfo>> {
    let rows = client
        .query(
            "SELECT c.relname, c.relkind::text, c.reltuples, pg_total_relation_size(c.oid), \
             obj_description(c.oid, 'pg_class') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'v', 'm', 'p') \
             ORDER BY c.relname",
            &[&namespace],
        )
        .await
        .map_err(map_err)?;
    let mut out = Vec::new();
    for row in rows {
        let name: String = row.get(0);
        let relkind: String = row.get(1);
        let Some(kind) = kind_of(&relkind) else {
            continue;
        };
        let reltuples: f32 = row.get(2);
        let size: i64 = row.get(3);
        let mut info = ObjectInfo::new(ObjectPath::new(Some(namespace), name), kind);
        if reltuples >= 0.0 {
            info.row_estimate = Some(reltuples as u64);
        }
        if size >= 0 {
            info.size_bytes = Some(size as u64);
        }
        info.comment = row.get(4);
        out.push(info);
    }
    Ok(out)
}

/// Resolve `(namespace, name)` to a class oid, or fail closed when it does not exist.
async fn class_oid(client: &Client, namespace: &str, name: &str) -> Result<i32> {
    let row = client
        .query_opt(
            "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&namespace, &name],
        )
        .await
        .map_err(map_err)?;
    row.map(|r| r.get(0))
        .ok_or_else(|| Error::Connection(format!("no such object {namespace}.{name}")))
}

fn ref_action(code: &str) -> ReferentialAction {
    match code {
        "a" => ReferentialAction::NoAction,
        "r" => ReferentialAction::Restrict,
        "c" => ReferentialAction::Cascade,
        "n" => ReferentialAction::SetNull,
        "d" => ReferentialAction::SetDefault,
        other => ReferentialAction::Other(other.to_owned()),
    }
}

/// Column names for a `conkey`/`confkey` int2 array, in key order.
async fn key_columns(client: &Client, relid: i32, keys: &[i16]) -> Result<Vec<String>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let key_list: Vec<i16> = keys.to_vec();
    let rows = client
        .query(
            "SELECT a.attname FROM pg_attribute a \
             WHERE a.attrelid = $1 AND a.attnum = ANY($2) \
             ORDER BY array_position($2, a.attnum)",
            &[&relid, &key_list],
        )
        .await
        .map_err(map_err)?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

pub async fn describe(client: &Client, path: &ObjectPath) -> Result<TableDetail> {
    let namespace = path.namespace.as_deref().unwrap_or("public");
    let oid = class_oid(client, namespace, &path.name).await?;
    let meta_rows = client
        .query(
            "SELECT c.relkind::text, obj_description(c.oid, 'pg_class'), c.reltuples, \
             pg_total_relation_size(c.oid) \
             FROM pg_class c WHERE c.oid = $1",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let meta = meta_rows
        .first()
        .ok_or_else(|| Error::Connection("object vanished".into()))?;
    let relkind: String = meta.get(0);
    let kind = kind_of(&relkind).unwrap_or(ObjectKind::Table);

    let col_rows = client
        .query(
            "SELECT a.attname, a.attnum, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
             pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text, \
             col_description(a.attrelid, a.attnum), t.typtype::text, co.collname, a.atttypid \
             FROM pg_attribute a \
             JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             LEFT JOIN pg_collation co ON co.oid = a.attcollation \
             WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let pk_rows = client
        .query(
            "SELECT conkey FROM pg_constraint \
             WHERE conrelid = $1 AND contype = 'p'",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let mut pk_position: BTreeMap<i16, u32> = BTreeMap::new();
    if let Some(row) = pk_rows.first() {
        let keys: Vec<i16> = row.get(0);
        for (i, attnum) in keys.iter().enumerate() {
            pk_position.insert(*attnum, (i as u32) + 1);
        }
    }
    // Enum variants for enum-typed columns, in declaration order, keyed by type OID
    // (type names are not unique across schemas).
    let mut enum_variants: BTreeMap<i32, Vec<String>> = BTreeMap::new();
    let mut wanted: Vec<i32> = Vec::new();
    for row in &col_rows {
        let typtype: String = row.get(8);
        if typtype == "e" {
            let typid: i32 = row.get(10);
            if !wanted.contains(&typid) {
                wanted.push(typid);
            }
        }
    }
    for typid in wanted {
        let variants = client
            .query(
                "SELECT enumlabel FROM pg_enum \
                 WHERE enumtypid = $1 ORDER BY enumsortorder",
                &[&typid],
            )
            .await
            .map_err(map_err)?;
        enum_variants.insert(typid, variants.iter().map(|r| r.get(0)).collect());
    }

    let mut columns = Vec::new();
    for row in &col_rows {
        let name: String = row.get(0);
        let attnum: i16 = row.get(1);
        let native: String = row.get(2);
        let not_null: bool = row.get(3);
        let default_expr: Option<String> = row.get(4);
        let identity: String = row.get(5);
        let generated: String = row.get(6);
        let comment: Option<String> = row.get(7);
        let typtype: String = row.get(8);
        let collation: Option<String> = row.get(9);
        let typid: i32 = row.get(10);
        let mut data_type = data_type_from_native(&native);
        if typtype == "e"
            && let Some(variants) = enum_variants.get(&typid)
        {
            data_type.category = fg_core::TypeCategory::Enum;
            data_type.variants = variants.clone();
        }
        let (default, generated_col) = if generated == "s" {
            (
                None,
                default_expr.map(|expression| GeneratedColumn {
                    expression,
                    stored: true,
                }),
            )
        } else {
            (default_expr, None)
        };
        let mut extra = BTreeMap::new();
        if identity == "a" {
            extra.insert("identity".to_owned(), "always".to_owned());
        } else if identity == "d" {
            extra.insert("identity".to_owned(), "by default".to_owned());
        }
        columns.push(ColumnInfo {
            name,
            ordinal: attnum as u32,
            data_type,
            nullable: !not_null,
            default,
            primary_key_position: pk_position.get(&attnum).copied(),
            auto_increment: !identity.is_empty(),
            generated: generated_col,
            collation,
            comment,
            extra,
        });
    }

    let fk_rows = client
        .query(
            "SELECT conname, conkey, confrelid, confkey, confupdtype::text, confdeltype::text \
             FROM pg_constraint WHERE conrelid = $1 AND contype = 'f'",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let mut foreign_keys = Vec::new();
    for row in &fk_rows {
        let conname: Option<String> = row.get(0);
        let conkey: Vec<i16> = row.get(1);
        let confrelid: i32 = row.get(2);
        let confkey: Vec<i16> = row.get(3);
        let confupdtype: String = row.get(4);
        let confdeltype: String = row.get(5);
        let ref_meta = client
            .query_one(
                "SELECT n.nspname, c.relname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
                &[&confrelid],
            )
            .await
            .map_err(map_err)?;
        let ref_namespace: String = ref_meta.get(0);
        let ref_table: String = ref_meta.get(1);
        foreign_keys.push(ForeignKeyInfo {
            name: conname,
            columns: key_columns(client, oid, &conkey).await?,
            referenced: ObjectPath::new(Some(ref_namespace.as_str()), ref_table),
            referenced_columns: key_columns(client, confrelid, &confkey).await?,
            on_update: Some(ref_action(&confupdtype)),
            on_delete: Some(ref_action(&confdeltype)),
            extra: BTreeMap::new(),
        });
    }

    let index_rows = client
        .query(
            "SELECT i.relname, ix.indisunique, ix.indisprimary, am.amname, \
             pg_get_expr(ix.indpred, ix.indrelid), ix.indkey::text, ix.indoption, \
             pg_get_expr(ix.indexprs, ix.indrelid), obj_description(i.oid, 'pg_class') \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_am am ON am.oid = i.relam \
             WHERE ix.indrelid = $1",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let mut indexes = Vec::new();
    for row in &index_rows {
        let name: String = row.get(0);
        let unique: bool = row.get(1);
        let primary: bool = row.get(2);
        let method: String = row.get(3);
        let predicate: Option<String> = row.get(4);
        let indkey_text: String = row.get(5);
        let indkey: Vec<i16> = indkey_text
            .split_whitespace()
            .filter_map(|n| n.parse::<i16>().ok())
            .collect();
        let indoption: Vec<i16> = row.get(6);
        let indexprs: Option<String> = row.get(7);
        let comment: Option<String> = row.get(8);
        let mut cols = Vec::new();
        for (i, attnum) in indkey.iter().enumerate() {
            if *attnum == 0 {
                cols.push(IndexColumn {
                    name: indexprs.clone().unwrap_or_default(),
                    expression: true,
                    descending: false,
                });
            } else {
                let names = key_columns(client, oid, &[*attnum]).await?;
                let descending = indoption.get(i).map(|opt| opt & 1 == 1).unwrap_or(false);
                cols.push(IndexColumn {
                    name: names.first().cloned().unwrap_or_default(),
                    expression: false,
                    descending,
                });
            }
        }
        indexes.push(IndexInfo {
            name,
            columns: cols,
            unique,
            primary,
            method: Some(method),
            predicate: predicate.filter(|p| !p.is_empty()),
            comment,
            extra: BTreeMap::new(),
        });
    }

    let check_rows = client
        .query(
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = $1 AND contype = 'c'",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let mut checks = Vec::new();
    for row in &check_rows {
        let name: Option<String> = row.get(0);
        let definition: String = row.get(1);
        // `pg_get_constraintdef` returns `CHECK (...)` with the name; keep the
        // expression part for display.
        let expression = definition
            .strip_prefix("CHECK ")
            .unwrap_or(&definition)
            .to_owned();
        checks.push(CheckConstraint { name, expression });
    }

    let trigger_rows = client
        .query(
            "SELECT tgname, tgenabled <> 'D', \
             CASE WHEN tgtype & 2 <> 0 THEN 'BEFORE' WHEN tgtype & 64 <> 0 THEN 'INSTEAD OF' ELSE 'AFTER' END, \
             NULLIF(CONCAT_WS(' OR ', \
               CASE WHEN tgtype & 4 <> 0 THEN 'INSERT' END, \
               CASE WHEN tgtype & 8 <> 0 THEN 'DELETE' END, \
               CASE WHEN tgtype & 16 <> 0 THEN 'UPDATE' END, \
               CASE WHEN tgtype & 32 <> 0 THEN 'TRUNCATE' END), ''), \
             pg_get_triggerdef(oid) \
             FROM pg_trigger WHERE tgrelid = $1 AND NOT tgisinternal",
            &[&oid],
        )
        .await
        .map_err(map_err)?;
    let mut triggers = Vec::new();
    for row in &trigger_rows {
        let name: String = row.get(0);
        let enabled: bool = row.get(1);
        let timing: String = row.get(2);
        let event: String = row.get(3);
        let definition: String = row.get(4);
        triggers.push(TriggerInfo {
            name,
            timing,
            event,
            definition: Some(definition),
            enabled,
        });
    }

    let reltuples: f32 = meta.get(2);
    let size: i64 = meta.get(3);
    Ok(TableDetail {
        path: path.clone(),
        kind,
        comment: meta.get(1),
        columns,
        indexes,
        foreign_keys,
        checks,
        triggers,
        row_estimate: if reltuples >= 0.0 {
            Some(reltuples as u64)
        } else {
            None
        },
        size_bytes: if size >= 0 { Some(size as u64) } else { None },
        options: BTreeMap::new(),
    })
}

/// `CREATE` statement for a table, view or materialized view. Tables are assembled
/// from their catalog rows (columns, constraints, partition key); views use
/// `pg_get_viewdef`. Indexes follow as separate statements, as `pg_dump` emits them.
pub async fn ddl(client: &Client, path: &ObjectPath) -> Result<Option<String>> {
    let namespace = path.namespace.as_deref().unwrap_or("public");
    let oid = class_oid(client, namespace, &path.name).await?;
    let kind_row = client
        .query_one("SELECT relkind::text FROM pg_class WHERE oid = $1", &[&oid])
        .await
        .map_err(map_err)?;
    let relkind: String = kind_row.get(0);
    let qualified = format!("\"{namespace}\".\"{}\"", path.name);
    match relkind.as_str() {
        "v" | "m" => {
            let row = client
                .query_one("SELECT pg_get_viewdef($1, true)", &[&oid])
                .await
                .map_err(map_err)?;
            let viewdef: String = row.get(0);
            let keyword = if relkind == "m" {
                "MATERIALIZED VIEW"
            } else {
                "VIEW"
            };
            Ok(Some(format!("CREATE {keyword} {qualified} AS {viewdef}")))
        }
        "r" | "p" => {
            let detail = describe(client, path).await?;
            let mut lines = Vec::new();
            for column in &detail.columns {
                let mut line = format!("    \"{}\" {}", column.name, column.data_type.native);
                if let Some(generated) = &column.generated {
                    line.push_str(&format!(
                        " GENERATED ALWAYS AS ({}) STORED",
                        generated.expression
                    ));
                } else {
                    if let Some(default) = &column.default {
                        line.push_str(&format!(" DEFAULT {default}"));
                    }
                    if !column.nullable {
                        line.push_str(" NOT NULL");
                    }
                }
                if let Some(identity) = column.extra.get("identity") {
                    line.push_str(&format!(
                        " GENERATED {} AS IDENTITY",
                        identity.to_uppercase()
                    ));
                }
                lines.push(line);
            }
            let pk = detail.primary_key();
            if !pk.is_empty() {
                let cols = pk
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("    PRIMARY KEY ({cols})"));
            }
            for fk in &detail.foreign_keys {
                let cols = fk
                    .columns
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let ref_cols = fk
                    .referenced_columns
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut line = format!(
                    "    FOREIGN KEY ({cols}) REFERENCES \"{}\".\"{}\" ({ref_cols})",
                    fk.referenced.namespace.as_deref().unwrap_or("public"),
                    fk.referenced.name,
                );
                if let Some(action) = &fk.on_update {
                    line.push_str(&format!(" ON UPDATE {}", action_text(action)));
                }
                if let Some(action) = &fk.on_delete {
                    line.push_str(&format!(" ON DELETE {}", action_text(action)));
                }
                lines.push(line);
            }
            for check in &detail.checks {
                let name = check
                    .name
                    .as_ref()
                    .map(|n| format!("CONSTRAINT \"{n}\" "))
                    .unwrap_or_default();
                lines.push(format!("    {name}CHECK {}", check.expression));
            }
            let mut ddl = format!("CREATE TABLE {qualified} (\n{}\n)", lines.join(",\n"));
            if relkind == "p" {
                let part_row = client
                    .query_opt(
                        "SELECT partstrat::text, partattrs::text FROM pg_partitioned_table WHERE partrelid = $1",
                        &[&oid],
                    )
                    .await
                    .map_err(map_err)?;
                if let Some(part_row) = part_row {
                    let strat: String = part_row.get(0);
                    let attrs_text: String = part_row.get(1);
                    let attrs: Vec<i16> = attrs_text
                        .split_whitespace()
                        .filter_map(|n| n.parse::<i16>().ok())
                        .collect();
                    let cols = key_columns(client, oid, &attrs).await?;
                    let cols = cols
                        .iter()
                        .map(|c| format!("\"{c}\""))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let method = match strat.as_str() {
                        "l" => "LIST",
                        "r" => "RANGE",
                        "h" => "HASH",
                        _ => "",
                    };
                    if !method.is_empty() && !cols.is_empty() {
                        ddl.push_str(&format!(" PARTITION BY {method} ({cols})"));
                    }
                }
            }
            ddl.push(';');
            // Indexes as separate statements.
            let index_rows = client
                .query(
                    "SELECT pg_get_indexdef(ix.indexrelid) FROM pg_index ix \
                     WHERE ix.indrelid = $1 AND NOT ix.indisprimary",
                    &[&oid],
                )
                .await
                .map_err(map_err)?;
            for row in &index_rows {
                let def: String = row.get(0);
                ddl.push_str(&format!("\n{def};"));
            }
            if let Some(comment) = &detail.comment {
                ddl.push_str(&format!(
                    "\nCOMMENT ON TABLE {qualified} IS '{}';",
                    comment.replace('\'', "''")
                ));
            }
            Ok(Some(ddl))
        }
        _ => Ok(None),
    }
}

fn action_text(action: &ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::NoAction => "NO ACTION",
        ReferentialAction::Restrict => "RESTRICT",
        ReferentialAction::Cascade => "CASCADE",
        ReferentialAction::SetNull => "SET NULL",
        ReferentialAction::SetDefault => "SET DEFAULT",
        ReferentialAction::Other(_) => "NO ACTION",
        // `ReferentialAction` is non_exhaustive: future variants degrade to NO ACTION.
        _ => "NO ACTION",
    }
}

/// Every foreign key in a namespace in one round trip, for reverse references and
/// ER diagrams. Column order follows the key definition.
pub async fn list_relations(client: &Client, namespace: &str) -> Result<Vec<Relation>> {
    let rows = client
        .query(
            "SELECT c.relname, con.conname, con.conkey, fn.nspname, fc.relname, con.confkey, \
             con.confupdtype::text, con.confdeltype::text, con.conrelid, con.confrelid \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_class fc ON fc.oid = con.confrelid \
             JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
             WHERE n.nspname = $1 AND con.contype = 'f' \
             ORDER BY c.relname, con.conname",
            &[&namespace],
        )
        .await
        .map_err(map_err)?;
    let mut out = Vec::new();
    for row in &rows {
        let from_table: String = row.get(0);
        let conname: Option<String> = row.get(1);
        let conkey: Vec<i16> = row.get(2);
        let ref_namespace: String = row.get(3);
        let ref_table: String = row.get(4);
        let confkey: Vec<i16> = row.get(5);
        let confupdtype: String = row.get(6);
        let confdeltype: String = row.get(7);
        let owner_oid: i32 = row.get(8);
        let ref_oid: i32 = row.get(9);
        out.push(Relation {
            from: ObjectPath::new(Some(namespace), from_table),
            key: ForeignKeyInfo {
                name: conname,
                columns: key_columns(client, owner_oid, &conkey).await?,
                referenced: ObjectPath::new(Some(ref_namespace.as_str()), ref_table),
                referenced_columns: key_columns(client, ref_oid, &confkey).await?,
                on_update: Some(ref_action(&confupdtype)),
                on_delete: Some(ref_action(&confdeltype)),
                extra: BTreeMap::new(),
            },
        });
    }
    Ok(out)
}
