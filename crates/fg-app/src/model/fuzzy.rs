//! Fuzzy matching for Quick Open and the sidebar filter: the query's characters must appear in
//! order in the candidate (case-insensitively), and better matches rank first.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i32,
    /// Character indices (not bytes) of the matched characters, for bold highlighting.
    pub positions: Vec<usize>,
}

const START: i32 = 12;
const BOUNDARY: i32 = 10;
const CONSECUTIVE: i32 = 8;
const MATCH: i32 = 1;
const GAP: i32 = -1;
const LENGTH: i32 = -1;

fn is_separator(c: char) -> bool {
    matches!(c, '_' | '-' | '.' | ' ' | '/' | ':')
}

/// Greedy-with-lookahead subsequence match. Returns `None` if the query is not a subsequence.
/// An empty query matches everything with score 0.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let c: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = c
        .iter()
        .map(|ch| ch.to_lowercase().next().unwrap_or(*ch))
        .collect();

    // Dynamic programming over (query index, candidate index): best score ending a match of
    // q[..=i] exactly at c[j]. Small inputs (names), so O(n*m) is fine.
    let n = c.len();
    let m = q.len();
    if m > n {
        return None;
    }
    const NEG: i32 = i32::MIN / 2;
    let mut best = vec![vec![NEG; n]; m];
    let mut from = vec![vec![usize::MAX; n]; m];

    let bonus = |j: usize| -> i32 {
        if j == 0 {
            START
        } else if is_separator(c[j - 1]) || (c[j].is_uppercase() && c[j - 1].is_lowercase()) {
            BOUNDARY
        } else {
            0
        }
    };

    for j in 0..n {
        if lower[j] == q[0] {
            best[0][j] = MATCH + bonus(j) + GAP * j.min(8) as i32;
        }
    }
    for i in 1..m {
        for j in i..n {
            if lower[j] != q[i] {
                continue;
            }
            for k in (i - 1)..j {
                if best[i - 1][k] == NEG {
                    continue;
                }
                let consecutive = if k + 1 == j {
                    CONSECUTIVE
                } else {
                    GAP * (j - k - 1).min(8) as i32
                };
                let s = best[i - 1][k] + MATCH + bonus(j) + consecutive;
                if s > best[i][j] {
                    best[i][j] = s;
                    from[i][j] = k;
                }
            }
        }
    }
    let (end, score) = (0..n)
        .map(|j| (j, best[m - 1][j]))
        .filter(|(_, s)| *s != NEG)
        .max_by_key(|(j, s)| (*s, std::cmp::Reverse(*j)))?;

    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = j;
        if i > 0 {
            j = from[i][j];
        }
    }
    Some(FuzzyMatch {
        score: score + LENGTH * (n as i32 - m as i32).min(30),
        positions,
    })
}

/// Rank `names` against `query`: best first, ties by shorter then alphabetical. Returns indices
/// into `names` with their matches. An empty query keeps the original order.
pub fn rank<S: AsRef<str>>(query: &str, names: &[S]) -> Vec<(usize, FuzzyMatch)> {
    let mut hits: Vec<(usize, FuzzyMatch)> = names
        .iter()
        .enumerate()
        .filter_map(|(i, n)| fuzzy_match(query, n.as_ref()).map(|m| (i, m)))
        .collect();
    if !query.is_empty() {
        hits.sort_by(|(ia, a), (ib, b)| {
            b.score
                .cmp(&a.score)
                .then_with(|| names[*ia].as_ref().len().cmp(&names[*ib].as_ref().len()))
                .then_with(|| names[*ia].as_ref().cmp(names[*ib].as_ref()))
        });
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(q: &str, c: &str) -> Option<Vec<usize>> {
        fuzzy_match(q, c).map(|m| m.positions)
    }

    #[test]
    fn subsequence_rules() {
        assert_eq!(pos("usr", "users"), Some(vec![0, 1, 3]));
        assert_eq!(pos("USR", "users"), Some(vec![0, 1, 3]), "case-insensitive");
        assert_eq!(pos("xyz", "users"), None);
        assert_eq!(pos("sru", "users"), None, "order matters");
        assert_eq!(pos("userss", "users"), None, "longer than the candidate");
        assert_eq!(pos("", "anything"), Some(vec![]));
    }

    #[test]
    fn prefers_prefixes_boundaries_and_runs() {
        let order = |q: &str, names: &[&str]| -> Vec<String> {
            rank(q, names)
                .into_iter()
                .map(|(i, _)| names[i].to_owned())
                .collect()
        };
        assert_eq!(
            order("user", &["audit_users", "users", "superuser"]),
            ["users", "audit_users", "superuser"]
        );
        assert_eq!(
            order("oi", &["order_items", "orders_index", "omnibus"])
                .first()
                .map(String::as_str),
            Some("order_items")
        );
        // Word boundaries beat scattered letters.
        assert_eq!(
            order("ui", &["user_items", "quiet"])
                .first()
                .map(String::as_str),
            Some("user_items")
        );
    }

    #[test]
    fn camel_case_humps_count_as_boundaries() {
        let a = fuzzy_match("ou", "OrderUser").unwrap();
        let b = fuzzy_match("ou", "scoured").unwrap();
        assert!(a.score > b.score);
        assert_eq!(a.positions, vec![0, 5]);
    }

    #[test]
    fn unicode_positions_are_character_indices() {
        assert_eq!(pos("名字", "用户名字表"), Some(vec![2, 3]));
        assert_eq!(pos("é", "café"), Some(vec![3]));
    }

    #[test]
    fn ties_break_on_length_then_name_and_empty_query_keeps_order() {
        let names = ["bb", "ab", "aab"];
        let ranked = rank("b", &names);
        assert_eq!(
            ranked.iter().map(|(i, _)| names[*i]).collect::<Vec<_>>(),
            ["bb", "ab", "aab"]
        );
        let all = rank("", &["z", "a", "m"]);
        assert_eq!(
            all.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}
