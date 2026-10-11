// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What one `EXPLAIN (ANALYZE, BUFFERS, SUMMARY, MEMORY, VERBOSE, FORMAT JSON)` reports: times,
//! rows, buffers, planner memory, and every relation a plan node scans, with its schema and the
//! index it used. MEMORY is sent only to a server that has it (see `command`), and a figure the
//! EXPLAIN did not report is read as absent, never as zero. Also the plan the timed session's
//! execution sends as a notice, and whether two plans are the same tree.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// The first server version (`server_version_num`) whose EXPLAIN takes the MEMORY option.
pub const MEMORY_FROM: u32 = 170_000;

/// The first server version whose EXPLAIN prints a node's actual rows per loop to two decimals;
/// before it, to none.
pub const TWO_DECIMALS_FROM: u32 = 180_000;

/// The EXPLAIN command sent to a server of this version: MEMORY only where the server has it.
/// VERBOSE names each scan's schema.
pub fn command(server_version_num: u32) -> &'static str {
    if server_version_num >= MEMORY_FROM {
        "EXPLAIN (ANALYZE, BUFFERS, SUMMARY, MEMORY, VERBOSE, FORMAT JSON)"
    } else {
        "EXPLAIN (ANALYZE, BUFFERS, SUMMARY, VERBOSE, FORMAT JSON)"
    }
}

/// One scan node: the relation it reads, how often it ran, the rows it produced, and the rows it
/// read to produce them: those produced plus those its filter or recheck removed, or, for an
/// index-only scan, its heap fetches. Per-loop averages are multiplied back by the loops, so a
/// parallel scan counts every process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scan {
    /// The scan's place among the plan's scan nodes, from 1, depth first.
    pub node: u64,
    pub node_type: String,
    /// The relation's schema, where the EXPLAIN names it.
    pub schema: Option<String>,
    pub relation: String,
    /// The indexes the scan read: an index scan's one, or a bitmap heap scan's bitmap index
    /// scans' in plan order.
    pub indexes: Vec<String>,
    /// Index searches, where the EXPLAIN reports them: summed over a bitmap heap scan's bitmap
    /// index scans.
    pub index_searches: Option<u64>,
    /// An index scan's condition; a bitmap heap scan's recheck condition.
    pub index_cond: Option<String>,
    /// An index-only scan's heap fetches.
    pub heap_fetches: Option<u64>,
    pub loops: u64,
    pub rows: f64,
    /// The planner's estimate of the rows the scan produces in one loop, as the EXPLAIN prints it;
    /// `None` where it prints none.
    pub plan_rows: Option<f64>,
    pub rows_read: f64,
    /// How far `rows_read` can be from the true count: EXPLAIN prints a scan run in several loops
    /// as per-loop averages, rows to two decimals (to none before `TWO_DECIMALS_FROM`) and removed
    /// rows to none.
    pub slack: f64,
}

impl Scan {
    /// The indexes the scan read, joined by `+`; `None` when it read none.
    pub fn index(&self) -> Option<String> {
        (!self.indexes.is_empty()).then(|| self.indexes.join("+"))
    }
}

/// The figures of one EXPLAIN. Each `Option` is `None` when the EXPLAIN did not report it: planner
/// memory before version 17, for example.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    pub rows: f64,
    /// The planner's estimate of the rows the whole plan returns.
    pub plan_rows: Option<f64>,
    pub shared_hit: Option<u64>,
    pub shared_read: Option<u64>,
    pub shared_dirtied: Option<u64>,
    pub shared_written: Option<u64>,
    pub temp_read: Option<u64>,
    pub temp_written: Option<u64>,
    pub planning_shared_hit: Option<u64>,
    pub planning_shared_read: Option<u64>,
    pub planner_memory_used_kb: Option<u64>,
    pub planner_memory_allocated_kb: Option<u64>,
    pub jit_ms: Option<f64>,
    /// Parallel workers launched, over every node.
    pub workers_launched: Option<u64>,
    /// Subplans removed at executor start, over every node.
    pub subplans_removed: Option<u64>,
    pub scans: Vec<Scan>,
    /// The plan as the EXPLAIN printed it.
    pub plan: Value,
}

fn num(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// A per-node count, zero when the node does not show it: a node shows workers launched and
/// subplans removed only where there are some.
fn count(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// A figure that is absent when the EXPLAIN did not report it.
fn reported(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn reported_ms(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// Reads the JSON text EXPLAIN returned by a server of this version.
pub fn parse(text: &str, server_version_num: u32) -> Result<Facts, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("EXPLAIN output: {e}"))?;
    let top = doc
        .as_array()
        .and_then(|a| a.first())
        .ok_or("EXPLAIN output: not a one-element array")?;
    let plan = top.get("Plan").ok_or("EXPLAIN output: no Plan")?;
    let planning = top.get("Planning").cloned().unwrap_or(Value::Null);
    let mut f = Facts {
        planning_ms: reported_ms(top, "Planning Time"),
        execution_ms: reported_ms(top, "Execution Time"),
        rows: num(plan, "Actual Rows") * num(plan, "Actual Loops"),
        plan_rows: reported_ms(plan, "Plan Rows"),
        shared_hit: reported(plan, "Shared Hit Blocks"),
        shared_read: reported(plan, "Shared Read Blocks"),
        shared_dirtied: reported(plan, "Shared Dirtied Blocks"),
        shared_written: reported(plan, "Shared Written Blocks"),
        temp_read: reported(plan, "Temp Read Blocks"),
        temp_written: reported(plan, "Temp Written Blocks"),
        planning_shared_hit: reported(&planning, "Shared Hit Blocks"),
        planning_shared_read: reported(&planning, "Shared Read Blocks"),
        planner_memory_used_kb: reported(&planning, "Memory Used"),
        planner_memory_allocated_kb: reported(&planning, "Memory Allocated"),
        jit_ms: top
            .get("JIT")
            .and_then(|j| j.get("Timing"))
            .and_then(|t| t.get("Total"))
            .and_then(Value::as_f64),
        workers_launched: Some(0),
        subplans_removed: Some(0),
        plan: plan.clone(),
        ..Facts::default()
    };
    walk(plan, server_version_num, &mut f);
    Ok(f)
}

/// A scan's rounding bound: nothing when it ran once, since its counts are then exact.
fn slack(node: &Value, loops: u64, server_version_num: u32) -> f64 {
    if loops <= 1 {
        return 0.0;
    }
    let rows = if server_version_num >= TWO_DECIMALS_FROM {
        0.005
    } else {
        0.5
    };
    let averaged = ["Rows Removed by Filter", "Rows Removed by Index Recheck"]
        .iter()
        .filter(|k| node.get(**k).is_some())
        .count() as f64;
    loops as f64 * (rows + 0.5 * averaged)
}

/// The Bitmap Index Scans under a node (through BitmapAnd and BitmapOr), in plan order: each one's
/// index and its index searches.
fn bitmap_indexes(node: &Value, out: &mut Vec<(String, Option<u64>)>) {
    for c in node
        .get("Plans")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match c.get("Node Type").and_then(Value::as_str) {
            Some("Bitmap Index Scan") => {
                if let Some(ix) = c.get("Index Name").and_then(Value::as_str) {
                    out.push((ix.to_string(), reported(c, "Index Searches")));
                }
            }
            Some("BitmapAnd" | "BitmapOr") => bitmap_indexes(c, out),
            _ => {}
        }
    }
}

fn walk(node: &Value, server_version_num: u32, f: &mut Facts) {
    if let Some(rel) = node.get("Relation Name").and_then(Value::as_str) {
        let loops = count(node, "Actual Loops");
        let node_type = node
            .get("Node Type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let rows_read = if node_type == "Index Only Scan" {
            num(node, "Heap Fetches")
        } else {
            (num(node, "Actual Rows")
                + num(node, "Rows Removed by Filter")
                + num(node, "Rows Removed by Index Recheck"))
                * loops as f64
        };
        let text = |k: &str| node.get(k).and_then(Value::as_str).map(str::to_string);
        let (indexes, index_searches, index_cond) = if node_type == "Bitmap Heap Scan" {
            let mut b = Vec::new();
            bitmap_indexes(node, &mut b);
            let searches: Option<Vec<u64>> = b.iter().map(|(_, s)| *s).collect();
            (
                b.into_iter().map(|(i, _)| i).collect(),
                searches.filter(|s| !s.is_empty()).map(|s| s.iter().sum()),
                text("Recheck Cond"),
            )
        } else {
            (
                text("Index Name").into_iter().collect(),
                reported(node, "Index Searches"),
                text("Index Cond"),
            )
        };
        f.scans.push(Scan {
            node: f.scans.len() as u64 + 1,
            node_type: node_type.to_string(),
            schema: text("Schema"),
            relation: rel.to_string(),
            indexes,
            index_searches,
            index_cond,
            heap_fetches: (node_type == "Index Only Scan")
                .then(|| reported(node, "Heap Fetches"))
                .flatten(),
            loops,
            rows: num(node, "Actual Rows") * loops as f64,
            plan_rows: reported_ms(node, "Plan Rows"),
            rows_read,
            slack: slack(node, loops, server_version_num),
        });
    }
    *f.subplans_removed.get_or_insert(0) += count(node, "Subplans Removed");
    *f.workers_launched.get_or_insert(0) += count(node, "Workers Launched");
    if let Some(children) = node.get("Plans").and_then(Value::as_array) {
        for c in children {
            walk(c, server_version_num, f);
        }
    }
}

/// The scans read against a target's own relations (its leaves, or the table itself).
#[derive(Clone, Debug, PartialEq)]
pub struct Leaves {
    /// Distinct leaves some plan node scans.
    pub planned: BTreeSet<String>,
    /// Distinct leaves some plan node scanned at least once.
    pub executed: BTreeSet<String>,
    /// Scan nodes over the target's leaves; a leaf scanned by two nodes counts twice.
    pub leaf_scans: usize,
    /// Scan nodes over any other relation.
    pub other_scans: usize,
    /// Rows read by the scans over the target's leaves, and over any other relation.
    pub leaf_rows_read: f64,
    pub other_rows_read: f64,
    /// The rounding bounds of those two sums (see `Scan::slack`).
    pub leaf_slack: f64,
    pub other_slack: f64,
    /// The leaves that certainly gave up rows: read more than their rounding bound.
    pub yielded: BTreeSet<String>,
    /// Every other relation read, with the rows read from it.
    pub others: BTreeMap<String, f64>,
}

/// The other relations as one line of text: `relation rows`, comma-separated, in name order.
pub fn others_text(others: &BTreeMap<String, f64>) -> String {
    others
        .iter()
        .map(|(r, rows)| format!("{r} {rows}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn leaves(facts: &Facts, target_leaves: &BTreeSet<String>) -> Leaves {
    let mut l = Leaves {
        planned: BTreeSet::new(),
        executed: BTreeSet::new(),
        leaf_scans: 0,
        other_scans: 0,
        leaf_rows_read: 0.0,
        other_rows_read: 0.0,
        leaf_slack: 0.0,
        other_slack: 0.0,
        yielded: BTreeSet::new(),
        others: BTreeMap::new(),
    };
    let mut per_leaf: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for s in &facts.scans {
        if target_leaves.contains(&s.relation) {
            l.leaf_scans += 1;
            l.leaf_rows_read += s.rows_read;
            l.leaf_slack += s.slack;
            l.planned.insert(s.relation.clone());
            if s.loops > 0 {
                l.executed.insert(s.relation.clone());
            }
            let e = per_leaf.entry(s.relation.as_str()).or_insert((0.0, 0.0));
            *e = (e.0 + s.rows_read, e.1 + s.slack);
        } else {
            l.other_scans += 1;
            l.other_rows_read += s.rows_read;
            l.other_slack += s.slack;
            if s.rows_read > 0.0 {
                *l.others.entry(s.relation.clone()).or_insert(0.0) += s.rows_read;
            }
        }
    }
    l.yielded = per_leaf
        .into_iter()
        .filter(|(_, (read, slack))| *read > *slack)
        .map(|(r, _)| r.to_string())
        .collect();
    l
}

/// Whether a notice carries a plan: a first line, then a JSON object.
fn carries_plan(notice: &str) -> bool {
    notice
        .split_once('\n')
        .is_some_and(|(_, rest)| rest.starts_with('{'))
}

/// The plan of the statement a session ran, from the notices the server sent it while the statement
/// ran. The statement's own plan is sent as it ends, after any plan of a function evaluated while it
/// was planned, so it is the last notice that carries one.
pub fn timed_plan(notices: &[String]) -> Result<Value, String> {
    let last = notices
        .iter()
        .rev()
        .find(|n| carries_plan(n))
        .ok_or_else(|| {
            format!(
                "none of the {} notices the statement brought carries a plan",
                notices.len()
            )
        })?;
    let (_, json) = last.split_once('\n').unwrap_or_default();
    let doc: Value = serde_json::from_str(json)
        .map_err(|e| format!("the statement's plan cannot be read: {e}"))?;
    doc.get("Plan")
        .cloned()
        .ok_or_else(|| "the statement's plan cannot be read: it holds no Plan".to_string())
}

/// What makes two plans the same tree, node by node: what the node is, what it reads and by which
/// index, how it joins, groups or scans, its conditions and keys, and its children in order. Costs,
/// estimates and everything counted while it ran are left out, since they move while the plan stays.
const TREE: &[&str] = &[
    "Node Type",
    "Parent Relationship",
    "Subplan Name",
    "Custom Plan Provider",
    "Relation Name",
    "Schema",
    "Alias",
    "Index Name",
    "Function Name",
    "Table Function Name",
    "CTE Name",
    "Tuplestore Name",
    "Strategy",
    "Partial Mode",
    "Operation",
    "Command",
    "Join Type",
    "Inner Unique",
    "Scan Direction",
    "Parallel Aware",
    "Async Capable",
    "Single Copy",
    "Workers Planned",
    "Cache Mode",
    "Sampling Method",
    "Index Cond",
    "Recheck Cond",
    "TID Cond",
    "Filter",
    "Join Filter",
    "Hash Cond",
    "Merge Cond",
    "One-Time Filter",
    "Order By",
    "Run Condition",
    "Sort Key",
    "Presorted Key",
    "Group Key",
    "Grouping Sets",
    "Cache Key",
    "Window",
];

fn children(node: &Value) -> &[Value] {
    node.get("Plans")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// One node's own part of the tree, its children left out.
fn own(node: &Value) -> serde_json::Map<String, Value> {
    TREE.iter()
        .filter_map(|k| node.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect()
}

/// A plan reduced to its tree.
pub fn tree(plan: &Value) -> Value {
    let mut out = own(plan);
    let kids: Vec<Value> = children(plan).iter().map(tree).collect();
    if !kids.is_empty() {
        out.insert("Plans".into(), Value::Array(kids));
    }
    Value::Object(out)
}

/// Whether two plans are the same tree.
pub fn same_tree(a: &Value, b: &Value) -> bool {
    tree(a) == tree(b)
}

/// A node as EXPLAIN's text names it: its type, its index, and what it reads.
fn label(node: &Value) -> String {
    let s = |k: &str| node.get(k).and_then(Value::as_str);
    let mut l = s("Node Type").unwrap_or("?").to_string();
    if let Some(ix) = s("Index Name") {
        l.push_str(&format!(" using {ix}"));
    }
    if let Some(rel) = s("Relation Name") {
        l.push_str(" on ");
        if let Some(schema) = s("Schema") {
            l.push_str(&format!("{schema}."));
        }
        l.push_str(rel);
        if let Some(alias) = s("Alias").filter(|a| *a != rel) {
            l.push_str(&format!(" {alias}"));
        }
    } else if let Some(name) = s("CTE Name").or(s("Function Name")) {
        l.push_str(&format!(" on {name}"));
    }
    l
}

/// Where the timed execution's plan `timed` and its EXPLAIN's `explain` part: the first node, depth
/// first and counted from 1, that is not the same both ways. `None` when they are the same tree.
pub fn parting(timed: &Value, explain: &Value) -> Option<String> {
    fn walk(a: &Value, b: &Value, n: &mut u64) -> Option<String> {
        *n += 1;
        let (oa, ob) = (own(a), own(b));
        let (ka, kb) = (children(a), children(b));
        if oa != ob || ka.len() != kb.len() {
            let (la, lb) = (label(a), label(b));
            if la != lb {
                return Some(format!(
                    "at node {n}, {la} in the timed execution and {lb} in its EXPLAIN"
                ));
            }
            let mut differ: Vec<&str> = TREE
                .iter()
                .copied()
                .filter(|k| oa.get(*k) != ob.get(*k))
                .collect();
            if ka.len() != kb.len() {
                differ.push("children");
            }
            let which = match differ.as_slice() {
                [one] if *one == "children" => "children differ".to_string(),
                [one] => format!("{one} differs"),
                more => format!("{} differ", more.join(", ")),
            };
            return Some(format!("at node {n}, {la} both ways, whose {which}"));
        }
        ka.iter().zip(kb).find_map(|(x, y)| walk(x, y, n))
    }
    walk(timed, explain, &mut 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V18: u32 = 180_006;

    const SAMPLE: &str = r#"[{"Plan": {"Node Type": "Append", "Actual Rows": 12.00, "Actual Loops": 1,
        "Subplans Removed": 2, "Shared Hit Blocks": 5, "Shared Read Blocks": 7,
        "Plans": [
          {"Node Type": "Seq Scan", "Relation Name": "t_a", "Actual Rows": 12.00, "Actual Loops": 1},
          {"Node Type": "Seq Scan", "Relation Name": "t_b", "Actual Rows": 0.00, "Actual Loops": 0},
          {"Node Type": "Seq Scan", "Relation Name": "t_a", "Actual Rows": 0.00, "Actual Loops": 1},
          {"Node Type": "Seq Scan", "Relation Name": "themes", "Actual Rows": 3.00, "Actual Loops": 1}
        ]},
      "Planning": {"Shared Hit Blocks": 40, "Shared Read Blocks": 1, "Memory Used": 160, "Memory Allocated": 264},
      "Planning Time": 2.5, "Triggers": [], "Execution Time": 1.25}]"#;

    // What a version 13 server returned for EXPLAIN (ANALYZE, BUFFERS, SUMMARY, FORMAT JSON) of a
    // prepared statement: a Planning group with buffers and no memory.
    const WITHOUT_MEMORY: &str = r#"[{"Plan": {"Node Type": "Seq Scan", "Parallel Aware": false,
        "Relation Name": "t_part", "Alias": "t", "Actual Rows": 594, "Actual Loops": 1,
        "Shared Hit Blocks": 0, "Shared Read Blocks": 503, "Shared Dirtied Blocks": 0,
        "Shared Written Blocks": 0, "Temp Read Blocks": 0, "Temp Written Blocks": 0},
      "Planning": {"Shared Hit Blocks": 437, "Shared Read Blocks": 1, "Shared Dirtied Blocks": 0,
        "Shared Written Blocks": 0, "Temp Read Blocks": 0, "Temp Written Blocks": 0},
      "Planning Time": 2.409, "Triggers": [], "Execution Time": 3.318}]"#;

    #[test]
    fn reads_times_rows_buffers_and_memory() {
        let f = parse(SAMPLE, V18).unwrap();
        assert_eq!(f.planning_ms, Some(2.5));
        assert_eq!(f.execution_ms, Some(1.25));
        assert_eq!(f.rows, 12.0);
        assert_eq!((f.shared_hit, f.shared_read), (Some(5), Some(7)));
        assert_eq!(
            (f.planning_shared_hit, f.planning_shared_read),
            (Some(40), Some(1))
        );
        assert_eq!(
            (f.planner_memory_used_kb, f.planner_memory_allocated_kb),
            (Some(160), Some(264))
        );
        assert_eq!(f.subplans_removed, Some(2));
        assert_eq!(f.jit_ms, None);
        assert_eq!(f.scans.len(), 4);
    }

    #[test]
    fn memory_is_asked_for_only_from_version_17() {
        for v in [130_023, 140_024, 150_019, 160_015, 169_999] {
            assert_eq!(
                command(v),
                "EXPLAIN (ANALYZE, BUFFERS, SUMMARY, VERBOSE, FORMAT JSON)",
                "{v}"
            );
        }
        for v in [170_000, 170_006, 180_006, 190_000] {
            assert_eq!(
                command(v),
                "EXPLAIN (ANALYZE, BUFFERS, SUMMARY, MEMORY, VERBOSE, FORMAT JSON)",
                "{v}"
            );
        }
    }

    #[test]
    fn planner_memory_that_was_not_reported_is_absent() {
        let f = parse(WITHOUT_MEMORY, V18).unwrap();
        assert_eq!(f.planner_memory_used_kb, None);
        assert_eq!(f.planner_memory_allocated_kb, None);
        assert_eq!(
            (f.planning_shared_hit, f.planning_shared_read),
            (Some(437), Some(1))
        );
        assert_eq!((f.shared_hit, f.shared_read), (Some(0), Some(503)));
        assert_eq!(f.planning_ms, Some(2.409));
        assert_eq!(f.rows, 594.0);
    }

    #[test]
    fn figures_that_were_not_reported_are_absent_not_zero() {
        let f = parse(
            r#"[{"Plan": {"Node Type": "Result", "Actual Rows": 1, "Actual Loops": 1}}]"#,
            V18,
        )
        .unwrap();
        assert_eq!(f.rows, 1.0);
        assert_eq!((f.planning_ms, f.execution_ms), (None, None));
        assert_eq!((f.shared_hit, f.shared_read), (None, None));
        assert_eq!((f.shared_dirtied, f.shared_written), (None, None));
        assert_eq!((f.temp_read, f.temp_written), (None, None));
        assert_eq!(
            (f.planning_shared_hit, f.planning_shared_read),
            (None, None)
        );
        assert_eq!(
            (f.planner_memory_used_kb, f.planner_memory_allocated_kb),
            (None, None)
        );
        assert_eq!((f.workers_launched, f.subplans_removed), (Some(0), Some(0)));
    }

    #[test]
    fn leaves_separate_planned_executed_and_repeated_scans() {
        let f = parse(SAMPLE, V18).unwrap();
        let target: BTreeSet<String> = ["t_a", "t_b", "t_c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let l = leaves(&f, &target);
        assert_eq!(l.planned.len(), 2);
        assert_eq!(l.executed.len(), 1);
        assert_eq!(l.leaf_scans, 3);
        assert_eq!(l.other_scans, 1);
    }

    // The parallel scan's figures are those PostgreSQL 18.6 printed for a filtered count over a
    // partitioned table: per-loop averages over the leader and two workers.
    const READS: &str = r#"[{"Plan": {"Node Type": "Append", "Plan Rows": 118000, "Actual Rows": 115202, "Actual Loops": 1,
        "Plans": [
          {"Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "t_all", "Actual Rows": 38392.00,
           "Actual Loops": 3, "Rows Removed by Filter": 155025},
          {"Node Type": "Index Scan", "Relation Name": "t_ix", "Actual Rows": 5, "Actual Loops": 2,
           "Rows Removed by Filter": 1},
          {"Node Type": "Bitmap Heap Scan", "Relation Name": "t_bm", "Actual Rows": 10, "Actual Loops": 1,
           "Rows Removed by Index Recheck": 4},
          {"Node Type": "Index Only Scan", "Relation Name": "t_io", "Actual Rows": 50, "Actual Loops": 1,
           "Heap Fetches": 3}
        ]}}]"#;

    #[test]
    fn rows_read_count_what_each_scan_fetched_across_its_loops() {
        let f = parse(READS, V18).unwrap();
        assert_eq!(f.plan_rows, Some(118000.0));
        let read: Vec<(&str, f64)> = f
            .scans
            .iter()
            .map(|s| (s.relation.as_str(), s.rows_read))
            .collect();
        assert_eq!(
            read,
            [
                ("t_all", 580251.0),
                ("t_ix", 12.0),
                ("t_bm", 14.0),
                ("t_io", 3.0)
            ]
        );
        let target: BTreeSet<String> = ["t_all", "t_ix"].iter().map(|s| s.to_string()).collect();
        let l = leaves(&f, &target);
        assert_eq!((l.leaf_rows_read, l.other_rows_read), (580263.0, 17.0));
        // three loops and two loops, each with its removed rows averaged
        assert!((l.leaf_slack - 5.0 * 0.505).abs() < 1e-9);
        assert_eq!(l.other_slack, 0.0);
        assert_eq!(l.yielded.iter().collect::<Vec<_>>(), ["t_all", "t_ix"]);
        assert_eq!(others_text(&l.others), "t_bm 14, t_io 3");
    }

    #[test]
    fn before_18_a_scans_rows_are_rounded_to_whole_rows_per_loop() {
        let text = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "t_a", "Actual Rows": 0,
            "Actual Loops": 4}}]"#;
        let slack_at = |v| parse(text, v).unwrap().scans[0].slack;
        assert!((slack_at(170_006) - 4.0 * 0.5).abs() < 1e-9);
        assert!((slack_at(V18) - 4.0 * 0.005).abs() < 1e-9);
    }

    #[test]
    fn a_leaf_read_within_its_rounding_is_not_certainly_yielded() {
        let text = r#"[{"Plan": {"Node Type": "Append", "Actual Rows": 0, "Actual Loops": 1,
            "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "t_a", "Actual Rows": 0.00, "Actual Loops": 3,
               "Rows Removed by Filter": 0},
              {"Node Type": "Seq Scan", "Relation Name": "t_b", "Actual Rows": 0, "Actual Loops": 1}
            ]}}]"#;
        let f = parse(text, V18).unwrap();
        let target: BTreeSet<String> = ["t_a", "t_b"].iter().map(|s| s.to_string()).collect();
        let l = leaves(&f, &target);
        assert_eq!(l.executed.len(), 2);
        assert!(l.yielded.is_empty());
    }

    // The fields PostgreSQL 18 prints under VERBOSE for an index scan, an index-only scan and a
    // bitmap heap scan over two bitmap index scans, cut to those read.
    const INDEXED: &str = r#"[{"Plan": {"Node Type": "Nested Loop", "Actual Rows": 3, "Actual Loops": 1,
        "Plans": [
          {"Node Type": "Index Scan", "Relation Name": "lego_sets", "Schema": "lego",
           "Index Name": "lego_sets_theme_id_set_num_idx", "Index Cond": "(s.theme_id = 158)",
           "Index Searches": 1, "Plan Rows": 12, "Actual Rows": 3, "Actual Loops": 1},
          {"Node Type": "Index Only Scan", "Relation Name": "lego_purchases", "Schema": "lego",
           "Index Name": "lego_purchases_month_idx", "Index Searches": 12, "Heap Fetches": 0,
           "Plan Rows": 1.5, "Actual Rows": 5, "Actual Loops": 3},
          {"Node Type": "Bitmap Heap Scan", "Relation Name": "lego_parts", "Schema": "lego",
           "Recheck Cond": "((part_cat_id = 27) OR (part_cat_id = 13))", "Actual Rows": 9, "Actual Loops": 1,
           "Plans": [{"Node Type": "BitmapOr", "Plans": [
             {"Node Type": "Bitmap Index Scan", "Index Name": "lego_parts_part_cat_id_part_num_idx", "Index Searches": 1},
             {"Node Type": "Bitmap Index Scan", "Index Name": "lego_parts_pkey", "Index Searches": 2}]}]},
          {"Node Type": "Seq Scan", "Relation Name": "lego_themes", "Schema": "lego", "Actual Rows": 1, "Actual Loops": 1}
        ]}}]"#;

    #[test]
    fn a_scan_names_its_schema_and_the_index_it_read() {
        let f = parse(INDEXED, V18).unwrap();
        let s = &f.scans;
        assert_eq!(s.iter().map(|x| x.node).collect::<Vec<_>>(), [1, 2, 3, 4]);
        assert_eq!(s[0].schema.as_deref(), Some("lego"));
        assert_eq!(
            s[0].index().as_deref(),
            Some("lego_sets_theme_id_set_num_idx")
        );
        // the estimate is per loop, as printed; the actual rows are over every loop
        assert_eq!((s[0].plan_rows, s[0].rows), (Some(12.0), 3.0));
        assert_eq!((s[1].plan_rows, s[1].rows), (Some(1.5), 15.0));
        assert_eq!(s[3].plan_rows, None);
        assert_eq!((s[0].index_searches, s[0].heap_fetches), (Some(1), None));
        assert_eq!(s[0].index_cond.as_deref(), Some("(s.theme_id = 158)"));
        assert_eq!(
            (s[1].node_type.as_str(), s[1].heap_fetches),
            ("Index Only Scan", Some(0))
        );
        assert_eq!(
            s[2].index().as_deref(),
            Some("lego_parts_part_cat_id_part_num_idx+lego_parts_pkey")
        );
        assert_eq!(s[2].index_searches, Some(3));
        assert!(s[2]
            .index_cond
            .as_deref()
            .unwrap()
            .starts_with("((part_cat_id = 27)"));
        assert_eq!((s[3].index(), s[3].index_searches), (None, None));
    }

    #[test]
    fn refuses_output_that_is_not_a_plan() {
        assert!(parse("[]", V18).is_err());
        assert!(parse("not json", V18).is_err());
    }

    // One plan as PostgreSQL 18 prints it under EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON),
    // cut to a few of its fields, and the same plan as the timed session's notice brings it, with
    // nothing counted per node: other costs, no actual figures.
    const EXPLAINED: &str = r#"[{"Plan": {"Node Type": "Hash Join", "Parallel Aware": false,
        "Async Capable": false, "Join Type": "Inner", "Startup Cost": 1020.50, "Total Cost": 1431.13,
        "Plan Rows": 1672, "Plan Width": 8, "Actual Startup Time": 4.1, "Actual Total Time": 6.2,
        "Actual Rows": 1672.00, "Actual Loops": 1, "Output": ["i.set_num"], "Inner Unique": true,
        "Hash Cond": "(s.inventory_id = i.id)", "Shared Hit Blocks": 186, "Shared Read Blocks": 0,
        "Disabled": false,
        "Plans": [
          {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Parallel Aware": false,
           "Async Capable": false, "Relation Name": "lego_inventory_sets", "Schema": "lego",
           "Alias": "s", "Total Cost": 30.00, "Plan Rows": 1672, "Actual Rows": 1672.00,
           "Actual Loops": 1, "Output": ["s.inventory_id"], "Shared Hit Blocks": 12},
          {"Node Type": "Hash", "Parent Relationship": "Inner", "Parallel Aware": false,
           "Async Capable": false, "Total Cost": 431.13, "Plan Rows": 31687, "Actual Rows": 31669.00,
           "Actual Loops": 1, "Hash Buckets": 32768, "Peak Memory Usage": 1641,
           "Output": ["i.set_num", "i.id"],
           "Plans": [
             {"Node Type": "Index Only Scan", "Parent Relationship": "Outer", "Parallel Aware": false,
              "Async Capable": false, "Scan Direction": "Forward",
              "Index Name": "lego_inventories_set_num_version_id_idx",
              "Relation Name": "lego_inventories", "Schema": "lego", "Alias": "i",
              "Total Cost": 431.13, "Plan Rows": 31669, "Actual Rows": 31669.00, "Actual Loops": 1,
              "Output": ["i.set_num", "i.id"], "Filter": "(i.version = 1)",
              "Rows Removed by Filter": 18, "Heap Fetches": 0, "Index Searches": 1,
              "Shared Hit Blocks": 174}]}]},
      "Planning": {"Shared Hit Blocks": 40}, "Planning Time": 0.9, "Execution Time": 6.4}]"#;

    const TIMED: &str = r#"duration: 6.204 ms  plan:
{
  "Query Text": "SELECT i.set_num FROM lego_inventories i JOIN lego_inventory_sets s ON s.inventory_id = i.id WHERE i.version = 1",
  "Plan": {"Node Type": "Hash Join", "Parallel Aware": false, "Async Capable": false,
    "Join Type": "Inner", "Startup Cost": 1163.47, "Total Cost": 1574.10, "Plan Rows": 1672,
    "Plan Width": 8, "Output": ["i.set_num"], "Inner Unique": true,
    "Hash Cond": "(s.inventory_id = i.id)", "Disabled": false,
    "Plans": [
      {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Parallel Aware": false,
       "Async Capable": false, "Relation Name": "lego_inventory_sets", "Schema": "lego", "Alias": "s",
       "Total Cost": 30.00, "Plan Rows": 1672, "Output": ["s.inventory_id"]},
      {"Node Type": "Hash", "Parent Relationship": "Inner", "Parallel Aware": false,
       "Async Capable": false, "Total Cost": 574.10, "Plan Rows": 31687,
       "Output": ["i.set_num", "i.id"],
       "Plans": [
         {"Node Type": "Index Only Scan", "Parent Relationship": "Outer", "Parallel Aware": false,
          "Async Capable": false, "Scan Direction": "Forward",
          "Index Name": "lego_inventories_set_num_version_id_idx",
          "Relation Name": "lego_inventories", "Schema": "lego", "Alias": "i",
          "Total Cost": 574.10, "Plan Rows": 31669, "Output": ["i.set_num", "i.id"],
          "Filter": "(i.version = 1)"}]}]}
}"#;

    /// What an SQL function evaluated while the question was planned brings first.
    const WHILE_PLANNING: &str = "duration: 0.004 ms  plan:\n{\n  \"Query Text\": \"\",\n  \"Plan\": {\"Node Type\": \"Result\", \"Parallel Aware\": false, \"Async Capable\": false}\n}";

    fn explained() -> Value {
        parse(EXPLAINED, V18).unwrap().plan
    }

    fn timed() -> Value {
        timed_plan(&[TIMED.to_string()]).unwrap()
    }

    /// The timed plan with the node at `path` (child indexes from the top) changed by `f`.
    fn changed(path: &[usize], f: impl Fn(&mut serde_json::Map<String, Value>)) -> Value {
        let mut p = timed();
        let mut n = &mut p;
        for i in path {
            n = &mut n["Plans"][*i];
        }
        f(n.as_object_mut().unwrap());
        p
    }

    #[test]
    fn the_timed_plan_is_the_last_plan_its_notices_bring() {
        let notices = [
            WHILE_PLANNING.to_string(),
            "a notice that is no plan".to_string(),
            TIMED.to_string(),
        ];
        let p = timed_plan(&notices).unwrap();
        assert_eq!(p["Node Type"], "Hash Join");
        assert_eq!(p["Plans"][1]["Plans"][0]["Node Type"], "Index Only Scan");
        assert!(timed_plan(&[]).is_err());
        assert!(timed_plan(&["a notice that is no plan".to_string()]).is_err());
        // a last plan that cannot be read is never taken for an earlier one
        let broken = "duration: 6.204 ms  plan:\n{\n  \"Query Text\": \"SELECT";
        let e = timed_plan(&[TIMED.to_string(), broken.to_string()]).unwrap_err();
        assert!(e.contains("cannot be read"), "{e}");
    }

    #[test]
    fn one_plan_read_from_either_session_is_the_same_tree_whatever_its_costs_and_counts() {
        assert!(same_tree(&timed(), &explained()));
        assert_eq!(parting(&timed(), &explained()), None);
        let t = tree(&explained());
        let text = t.to_string();
        for absent in [
            "Cost",
            "Rows",
            "Actual",
            "Blocks",
            "Buckets",
            "Output",
            "Heap Fetches",
        ] {
            assert!(!text.contains(absent), "{absent} in {text}");
        }
        assert_eq!(
            t["Plans"][1]["Plans"][0]["Index Name"],
            "lego_inventories_set_num_version_id_idx"
        );
    }

    #[test]
    fn another_scan_is_another_tree_and_named_where_the_plans_part() {
        let seq = changed(&[1, 0], |n| {
            n.insert("Node Type".into(), "Seq Scan".into());
            n.remove("Index Name");
            n.remove("Scan Direction");
        });
        assert!(!same_tree(&seq, &explained()));
        assert_eq!(
            parting(&seq, &explained()).unwrap(),
            "at node 4, Seq Scan on lego.lego_inventories i in the timed execution and Index Only \
             Scan using lego_inventories_set_num_version_id_idx on lego.lego_inventories i in its \
             EXPLAIN"
        );
    }

    #[test]
    fn another_join_another_condition_or_another_order_is_another_tree() {
        let left = changed(&[], |n| {
            n.insert("Join Type".into(), "Left".into());
        });
        assert!(!same_tree(&left, &explained()));
        assert_eq!(
            parting(&left, &explained()).unwrap(),
            "at node 1, Hash Join both ways, whose Join Type differs"
        );
        let cond = changed(&[], |n| {
            n.insert("Hash Cond".into(), "(s.set_num = i.set_num)".into());
        });
        assert!(!same_tree(&cond, &explained()));
        let filter = changed(&[1, 0], |n| {
            n.remove("Filter");
        });
        assert!(!same_tree(&filter, &explained()));
        let swapped = changed(&[], |n| {
            if let Some(Value::Array(c)) = n.get_mut("Plans") {
                c.reverse();
            }
        });
        assert!(!same_tree(&swapped, &explained()));
        let fewer = changed(&[1], |n| {
            n.remove("Plans");
        });
        assert_eq!(
            parting(&fewer, &explained()).unwrap(),
            "at node 3, Hash both ways, whose children differ"
        );
    }
}
