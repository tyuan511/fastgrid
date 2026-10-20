//! The object tree in the sidebar, and the candidate list for Quick Open.

use std::collections::BTreeMap;

use fg_core::{ObjectInfo, ObjectKind, ObjectPath};

use super::fuzzy::{FuzzyMatch, fuzzy_match, rank};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Namespace,
    /// A heading such as TABLES (18).
    Group(ObjectKind),
    Object(ObjectKind),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TreeNode {
    /// Stable across rebuilds, so expansion state survives a refresh.
    pub id: String,
    pub label: String,
    pub kind: NodeKind,
    /// Set on object nodes.
    pub path: Option<ObjectPath>,
    /// Number of children (groups and namespaces) after filtering.
    pub count: Option<usize>,
    pub row_estimate: Option<u64>,
    /// Matched character positions in `label` for highlighting.
    pub matched: Vec<usize>,
    pub children: Vec<TreeNode>,
}

/// Display order of the groups.
const GROUPS: &[(ObjectKind, &str)] = &[
    (ObjectKind::Table, "Tables"),
    (ObjectKind::View, "Views"),
    (ObjectKind::MaterializedView, "Materialized views"),
    (ObjectKind::Function, "Functions"),
    (ObjectKind::Procedure, "Procedures"),
    (ObjectKind::Sequence, "Sequences"),
    (ObjectKind::Type, "Types"),
];

fn group_label(kind: ObjectKind) -> Option<&'static str> {
    GROUPS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l)
}

/// Build the tree from what the service listed.
///
/// With `show_namespaces` off (SQLite, or a single schema) the groups sit at the top level;
/// otherwise each namespace is a node containing its groups. `filter` keeps only objects whose
/// name fuzzy-matches, and drops groups and namespaces left empty. Objects sort by name,
/// case-insensitively, unless a filter is active, in which case the best match comes first.
pub fn build_tree(
    namespaces: &[String],
    objects: &BTreeMap<String, Vec<ObjectInfo>>,
    filter: &str,
    show_namespaces: bool,
) -> Vec<TreeNode> {
    let mut roots = Vec::new();
    for ns in namespaces {
        let Some(list) = objects.get(ns) else {
            continue;
        };
        let groups = groups_for(ns, list, filter);
        if groups.is_empty() && !filter.is_empty() {
            continue;
        }
        if show_namespaces {
            roots.push(TreeNode {
                id: format!("ns:{ns}"),
                label: ns.clone(),
                kind: NodeKind::Namespace,
                path: None,
                count: Some(groups.iter().filter_map(|g| g.count).sum()),
                row_estimate: None,
                matched: Vec::new(),
                children: groups,
            });
        } else {
            roots.extend(groups);
        }
    }
    roots
}

fn groups_for(ns: &str, list: &[ObjectInfo], filter: &str) -> Vec<TreeNode> {
    let mut out = Vec::new();
    for (kind, label) in GROUPS {
        let in_group: Vec<&ObjectInfo> = list.iter().filter(|o| o.kind == *kind).collect();
        let names: Vec<&str> = in_group.iter().map(|o| o.path.name.as_str()).collect();
        let ranked: Vec<(usize, FuzzyMatch)> = if filter.is_empty() {
            let mut all: Vec<(usize, FuzzyMatch)> = (0..in_group.len())
                .map(|i| {
                    (
                        i,
                        FuzzyMatch {
                            score: 0,
                            positions: Vec::new(),
                        },
                    )
                })
                .collect();
            all.sort_by_key(|(i, _)| names[*i].to_lowercase());
            all
        } else {
            rank(filter, &names)
        };
        if ranked.is_empty() {
            continue;
        }
        let children: Vec<TreeNode> = ranked
            .into_iter()
            .map(|(i, m)| {
                let o = in_group[i];
                TreeNode {
                    id: format!("obj:{ns}:{}:{}", kind_key(*kind), o.path.name),
                    label: o.path.name.clone(),
                    kind: NodeKind::Object(o.kind),
                    path: Some(o.path.clone()),
                    count: None,
                    row_estimate: o.row_estimate,
                    matched: m.positions,
                    children: Vec::new(),
                }
            })
            .collect();
        out.push(TreeNode {
            id: format!("grp:{ns}:{}", kind_key(*kind)),
            label: (*label).to_owned(),
            kind: NodeKind::Group(*kind),
            path: None,
            count: Some(children.len()),
            row_estimate: None,
            matched: Vec::new(),
            children,
        });
    }
    out
}

fn kind_key(kind: ObjectKind) -> &'static str {
    group_label(kind).unwrap_or("other")
}

/// `999` -> `999`, `14_200` -> `14.2k`, `999_999` -> `1M`, `1_500_000` -> `1.5M`: the row-count
/// hint beside a table. Rounding never produces `1000k`: it moves up to the next unit instead.
pub fn compact_count(n: u64) -> String {
    const UNITS: [(f64, &str); 3] = [(1e3, "k"), (1e6, "M"), (1e9, "B")];
    if n < 1_000 {
        return n.to_string();
    }
    for (i, (scale, suffix)) in UNITS.iter().enumerate() {
        let tenths = (n as f64 / scale * 10.0).round() as u64; // value in tenths of the unit
        let last = i == UNITS.len() - 1;
        if tenths < 10_000 || last {
            let (whole, frac) = (tenths / 10, tenths % 10);
            return if frac == 0 {
                format!("{whole}{suffix}")
            } else {
                format!("{whole}.{frac}{suffix}")
            };
        }
    }
    n.to_string()
}

/// One row of the Quick Open list.
#[derive(Debug, Clone, PartialEq)]
pub struct QuickItem {
    pub path: ObjectPath,
    pub kind: ObjectKind,
}

/// Everything Quick Open can jump to: browsable objects only (tables and views).
pub fn quick_items(objects: &BTreeMap<String, Vec<ObjectInfo>>) -> Vec<QuickItem> {
    objects
        .values()
        .flatten()
        .filter(|o| o.kind.is_relation())
        .map(|o| QuickItem {
            path: o.path.clone(),
            kind: o.kind,
        })
        .collect()
}

/// Best matches first, at most `limit`. An empty query lists the first items alphabetically.
pub fn quick_search(
    items: &[QuickItem],
    query: &str,
    limit: usize,
) -> Vec<(QuickItem, Vec<usize>)> {
    if query.is_empty() {
        let mut all: Vec<&QuickItem> = items.iter().collect();
        all.sort_by_key(|i| i.path.name.to_lowercase());
        return all
            .into_iter()
            .take(limit)
            .map(|i| (i.clone(), Vec::new()))
            .collect();
    }
    let names: Vec<&str> = items.iter().map(|i| i.path.name.as_str()).collect();
    rank(query, &names)
        .into_iter()
        .take(limit)
        .map(|(i, m)| (items[i].clone(), m.positions))
        .collect()
}

/// For tests and callers that only need to know whether a name passes the filter.
pub fn matches_filter(filter: &str, name: &str) -> bool {
    fuzzy_match(filter, name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(ns: &str, name: &str, kind: ObjectKind, rows: Option<u64>) -> ObjectInfo {
        let mut o = ObjectInfo::new(ObjectPath::new(Some(ns), name), kind);
        o.row_estimate = rows;
        o
    }

    fn sample() -> (Vec<String>, BTreeMap<String, Vec<ObjectInfo>>) {
        let main = vec![
            obj("main", "Users", ObjectKind::Table, Some(14_200)),
            obj("main", "orders", ObjectKind::Table, Some(890)),
            obj("main", "order_items", ObjectKind::Table, None),
            obj("main", "v_active_users", ObjectKind::View, None),
        ];
        (
            vec!["main".into()],
            BTreeMap::from([("main".to_string(), main)]),
        )
    }

    fn labels(nodes: &[TreeNode]) -> Vec<String> {
        nodes.iter().map(|n| n.label.clone()).collect()
    }

    #[test]
    fn groups_objects_by_kind_sorted_case_insensitively() {
        let (ns, objs) = sample();
        let tree = build_tree(&ns, &objs, "", false);
        assert_eq!(labels(&tree), ["Tables", "Views"]);
        assert_eq!(tree[0].count, Some(3));
        assert_eq!(
            labels(&tree[0].children),
            ["order_items", "orders", "Users"]
        );
        assert_eq!(tree[0].children[2].row_estimate, Some(14_200));
        assert_eq!(
            tree[0].children[0].path,
            Some(ObjectPath::new(Some("main"), "order_items"))
        );
    }

    #[test]
    fn namespaces_are_a_level_only_when_asked_for() {
        let (ns, mut objs) = sample();
        objs.insert(
            "other".into(),
            vec![obj("other", "t", ObjectKind::Table, None)],
        );
        let ns = vec![ns[0].clone(), "other".into()];
        let flat = build_tree(&ns, &objs, "", false);
        assert_eq!(flat.len(), 3, "main: Tables+Views, other: Tables");
        let nested = build_tree(&ns, &objs, "", true);
        assert_eq!(labels(&nested), ["main", "other"]);
        assert_eq!(nested[0].count, Some(4));
        assert_eq!(labels(&nested[0].children), ["Tables", "Views"]);
    }

    #[test]
    fn filtering_ranks_highlights_and_drops_empty_groups() {
        let (ns, objs) = sample();
        let tree = build_tree(&ns, &objs, "ord", false);
        assert_eq!(labels(&tree), ["Tables"], "no views match");
        assert_eq!(
            labels(&tree[0].children),
            ["orders", "order_items"],
            "shorter first on a tie"
        );
        assert_eq!(tree[0].children[0].matched, vec![0, 1, 2]);
        assert_eq!(tree[0].count, Some(2));
        assert!(build_tree(&ns, &objs, "zzz", false).is_empty());
    }

    #[test]
    fn node_ids_are_stable_across_rebuilds() {
        let (ns, objs) = sample();
        let a = build_tree(&ns, &objs, "", false);
        let b = build_tree(&ns, &objs, "", false);
        assert_eq!(a[0].children[0].id, b[0].children[0].id);
        assert_eq!(a[0].id, "grp:main:Tables");
        // A filtered tree keeps the same ids for the same objects.
        let filtered = build_tree(&ns, &objs, "orders", false);
        assert_eq!(filtered[0].children[0].id, "obj:main:Tables:orders");
    }

    #[test]
    fn other_kinds_get_their_own_groups() {
        let objs = BTreeMap::from([(
            "public".to_string(),
            vec![
                obj("public", "f", ObjectKind::Function, None),
                obj("public", "t", ObjectKind::Table, None),
                obj("public", "mv", ObjectKind::MaterializedView, None),
            ],
        )]);
        let tree = build_tree(&["public".into()], &objs, "", false);
        assert_eq!(labels(&tree), ["Tables", "Materialized views", "Functions"]);
    }

    #[test]
    fn row_counts_are_compact() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1k");
        assert_eq!(compact_count(14_200), "14.2k");
        assert_eq!(
            compact_count(999_999),
            "1M",
            "rounds up into the next unit, never 1000k"
        );
        assert_eq!(compact_count(999_949), "999.9k");
        assert_eq!(compact_count(1_049), "1k");
        assert_eq!(compact_count(1_500_000), "1.5M");
        assert_eq!(compact_count(2_000_000_000), "2B");
    }

    #[test]
    fn quick_open_covers_tables_and_views_only() {
        let mut objs = sample().1;
        objs.get_mut("main")
            .unwrap()
            .push(obj("main", "do_thing", ObjectKind::Function, None));
        let items = quick_items(&objs);
        assert_eq!(items.len(), 4);
        let hits = quick_search(&items, "usr", 5);
        assert_eq!(hits[0].0.path.name, "Users");
        assert_eq!(hits[0].1, vec![0, 1, 3]);
        assert!(quick_search(&items, "usr", 1).len() == 1);
        let empty = quick_search(&items, "", 2);
        assert_eq!(
            empty
                .iter()
                .map(|(i, _)| i.path.name.as_str())
                .collect::<Vec<_>>(),
            ["order_items", "orders"]
        );
        assert!(matches_filter("od", "orders") && !matches_filter("zz", "orders"));
    }
}
