// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The results, one line per (target, question, repetition), and the summary drawn from them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::stats::{constant, spread, Spread};
use crate::tables::{Declared, Kind, Table};

/// One line per (target, question, phase, repetition).
pub const RESULTS: Declared = Declared {
    name: "results",
    columns: &[
        ("target", Kind::Text),
        ("config_id", Kind::Text),
        ("question", Kind::Text),
        ("spelling", Kind::Text),
        ("selection", Kind::Text),
        ("phase", Kind::Text),
        ("rep", Kind::Int),
        ("session", Kind::Int),
        ("position", Kind::Int),
        ("status", Kind::Text),
        ("verdict", Kind::Text),
        ("wall_ms", Kind::Float),
        ("rows", Kind::Int),
        ("digest", Kind::Text),
        ("expected_rows", Kind::Int),
        ("expected_digest", Kind::Text),
        ("peak_rss_kb", Kind::Int),
        ("peak_anon_kb", Kind::Int),
        ("explain_status", Kind::Text),
        ("planning_ms", Kind::Float),
        ("execution_ms", Kind::Float),
        ("explain_rows", Kind::Float),
        ("explain_rows_ok", Kind::Bool),
        ("shared_hit", Kind::Int),
        ("shared_read", Kind::Int),
        ("shared_dirtied", Kind::Int),
        ("shared_written", Kind::Int),
        ("temp_read", Kind::Int),
        ("temp_written", Kind::Int),
        ("planning_shared_hit", Kind::Int),
        ("planning_shared_read", Kind::Int),
        ("planner_memory_used_kb", Kind::Int),
        ("planner_memory_allocated_kb", Kind::Int),
        ("jit_ms", Kind::Float),
        ("workers_launched", Kind::Int),
        ("leaves_planned", Kind::Int),
        ("leaves_executed", Kind::Int),
        ("leaf_scans", Kind::Int),
        ("other_scans", Kind::Int),
        ("subplans_removed", Kind::Int),
        ("leaf_set", Kind::Text),
        ("explain_peak_rss_kb", Kind::Int),
        ("plan_rows", Kind::Float),
        ("explain_leaf_rows_read", Kind::Float),
        ("explain_other_rows_read", Kind::Float),
        ("witness_status", Kind::Text),
        ("witness_leaves", Kind::Int),
        ("witness_leaf_rows_read", Kind::Int),
        ("witness_leaf_blocks", Kind::Int),
        ("witness_other_rows_read", Kind::Int),
        ("witness_other_blocks", Kind::Int),
        ("plan_leaf_rows_read", Kind::Int),
        ("plan_other_rows_read", Kind::Int),
        ("rows_read_agree", Kind::Bool),
        ("leaves_agree", Kind::Bool),
        ("leaves_disagreement", Kind::Text),
        ("witness_other_relations", Kind::Text),
        ("plan_other_relations", Kind::Text),
        ("explain_other_relations", Kind::Text),
        ("error", Kind::Text),
    ],
    key: &["question", "target", "phase", "rep"],
};

/// The results as written before the relations read outside the leaves were named: read, never
/// written.
pub const RESULTS_V2: Declared = Declared {
    name: "results (before the other relations were named)",
    columns: &[
        ("target", Kind::Text),
        ("config_id", Kind::Text),
        ("question", Kind::Text),
        ("spelling", Kind::Text),
        ("selection", Kind::Text),
        ("phase", Kind::Text),
        ("rep", Kind::Int),
        ("session", Kind::Int),
        ("position", Kind::Int),
        ("status", Kind::Text),
        ("verdict", Kind::Text),
        ("wall_ms", Kind::Float),
        ("rows", Kind::Int),
        ("digest", Kind::Text),
        ("expected_rows", Kind::Int),
        ("expected_digest", Kind::Text),
        ("peak_rss_kb", Kind::Int),
        ("peak_anon_kb", Kind::Int),
        ("explain_status", Kind::Text),
        ("planning_ms", Kind::Float),
        ("execution_ms", Kind::Float),
        ("explain_rows", Kind::Float),
        ("explain_rows_ok", Kind::Bool),
        ("shared_hit", Kind::Int),
        ("shared_read", Kind::Int),
        ("shared_dirtied", Kind::Int),
        ("shared_written", Kind::Int),
        ("temp_read", Kind::Int),
        ("temp_written", Kind::Int),
        ("planning_shared_hit", Kind::Int),
        ("planning_shared_read", Kind::Int),
        ("planner_memory_used_kb", Kind::Int),
        ("planner_memory_allocated_kb", Kind::Int),
        ("jit_ms", Kind::Float),
        ("workers_launched", Kind::Int),
        ("leaves_planned", Kind::Int),
        ("leaves_executed", Kind::Int),
        ("leaf_scans", Kind::Int),
        ("other_scans", Kind::Int),
        ("subplans_removed", Kind::Int),
        ("leaf_set", Kind::Text),
        ("explain_peak_rss_kb", Kind::Int),
        ("plan_rows", Kind::Float),
        ("explain_leaf_rows_read", Kind::Float),
        ("explain_other_rows_read", Kind::Float),
        ("witness_status", Kind::Text),
        ("witness_leaves", Kind::Int),
        ("witness_leaf_rows_read", Kind::Int),
        ("witness_leaf_blocks", Kind::Int),
        ("witness_other_rows_read", Kind::Int),
        ("witness_other_blocks", Kind::Int),
        ("plan_leaf_rows_read", Kind::Int),
        ("plan_other_rows_read", Kind::Int),
        ("rows_read_agree", Kind::Bool),
        ("leaves_agree", Kind::Bool),
        ("error", Kind::Text),
    ],
    key: &["question", "target", "phase", "rep"],
};

/// The results as written before the rows witness: read, never written.
pub const RESULTS_V1: Declared = Declared {
    name: "results (before the rows witness)",
    columns: &[
        ("target", Kind::Text),
        ("config_id", Kind::Text),
        ("question", Kind::Text),
        ("spelling", Kind::Text),
        ("selection", Kind::Text),
        ("phase", Kind::Text),
        ("rep", Kind::Int),
        ("session", Kind::Int),
        ("position", Kind::Int),
        ("status", Kind::Text),
        ("verdict", Kind::Text),
        ("wall_ms", Kind::Float),
        ("rows", Kind::Int),
        ("digest", Kind::Text),
        ("expected_rows", Kind::Int),
        ("expected_digest", Kind::Text),
        ("peak_rss_kb", Kind::Int),
        ("peak_anon_kb", Kind::Int),
        ("explain_status", Kind::Text),
        ("planning_ms", Kind::Float),
        ("execution_ms", Kind::Float),
        ("explain_rows", Kind::Float),
        ("explain_rows_ok", Kind::Bool),
        ("shared_hit", Kind::Int),
        ("shared_read", Kind::Int),
        ("shared_dirtied", Kind::Int),
        ("shared_written", Kind::Int),
        ("temp_read", Kind::Int),
        ("temp_written", Kind::Int),
        ("planning_shared_hit", Kind::Int),
        ("planning_shared_read", Kind::Int),
        ("planner_memory_used_kb", Kind::Int),
        ("planner_memory_allocated_kb", Kind::Int),
        ("jit_ms", Kind::Float),
        ("workers_launched", Kind::Int),
        ("leaves_planned", Kind::Int),
        ("leaves_executed", Kind::Int),
        ("leaf_scans", Kind::Int),
        ("other_scans", Kind::Int),
        ("subplans_removed", Kind::Int),
        ("leaf_set", Kind::Text),
        ("explain_peak_rss_kb", Kind::Int),
        ("error", Kind::Text),
    ],
    key: &["question", "target", "phase", "rep"],
};

fn get<'a>(r: &'a BTreeMap<String, String>, k: &str) -> &'a str {
    r.get(k).map_or("", String::as_str)
}

fn num(r: &BTreeMap<String, String>, k: &str) -> Option<f64> {
    get(r, k).parse().ok()
}

/// One (question, target, phase): what every repetition came to.
#[derive(Clone, Debug)]
pub struct Cell {
    pub question: String,
    pub target: String,
    pub phase: String,
    pub reps: usize,
    pub right: usize,
    pub wrong: usize,
    pub timeout: usize,
    pub cancelled: usize,
    pub error: usize,
    pub not_run: usize,
    pub explain_failed: usize,
    pub wall: Option<Spread>,
    pub planning: Option<Spread>,
    pub execution: Option<Spread>,
    pub planner_kb: Option<Spread>,
    pub peak_rss_kb: Option<Spread>,
    pub shared_hit: Option<Spread>,
    pub shared_read: Option<Spread>,
    pub temp_written: Option<Spread>,
    pub rows: String,
    pub leaves_planned: String,
    pub leaves_executed: String,
    pub leaf_scans: String,
    pub errors: BTreeSet<String>,
}

impl Cell {
    /// RIGHT only when every repetition ran and answered right. Otherwise the first of WRONG,
    /// TIMEOUT, CANCELLED, ERROR and NOT RUN that occurred, with its count.
    pub fn verdict(&self) -> String {
        let n = self.reps;
        for (count, word) in [
            (self.wrong, "WRONG"),
            (self.timeout, "TIMEOUT"),
            (self.cancelled, "CANCELLED"),
            (self.error, "ERROR"),
            (self.not_run, "NOT RUN"),
        ] {
            if count > 0 {
                return format!("{word} {count}/{n}");
            }
        }
        if self.explain_failed > 0 {
            return format!("EXPLAIN FAILED {}/{n}", self.explain_failed);
        }
        if self.right == n && n > 0 {
            "RIGHT".into()
        } else {
            format!("RIGHT {}/{n}", self.right)
        }
    }

    pub fn all_right(&self) -> bool {
        self.verdict() == "RIGHT"
    }
}

fn range_text(xs: &[String]) -> String {
    match constant(xs) {
        Some(v) => v,
        None => {
            let nums: Vec<f64> = xs.iter().filter_map(|x| x.parse().ok()).collect();
            if nums.len() == xs.len() && !nums.is_empty() {
                let lo = nums.iter().copied().fold(f64::INFINITY, f64::min);
                let hi = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                format!("{lo}..{hi}")
            } else {
                xs.join("|")
            }
        }
    }
}

/// Groups the results by (question, target, phase). Times are summarised over the repetitions that
/// answered RIGHT only; every other outcome is counted, never averaged in.
pub fn summarise(t: &Table) -> Vec<Cell> {
    type Key = (String, String, String);
    let mut groups: BTreeMap<Key, Vec<&BTreeMap<String, String>>> = BTreeMap::new();
    for r in &t.rows {
        groups
            .entry((
                get(r, "question").into(),
                get(r, "target").into(),
                get(r, "phase").into(),
            ))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    for ((question, target, phase), rs) in groups {
        let status = |s: &str| rs.iter().filter(|r| get(r, "status") == s).count();
        let right: Vec<&&BTreeMap<String, String>> = rs
            .iter()
            .filter(|r| get(r, "status") == "OK" && get(r, "verdict") == "RIGHT")
            .collect();
        let explained: Vec<&&BTreeMap<String, String>> = right
            .iter()
            .copied()
            .filter(|r| get(r, "explain_status") == "OK")
            .collect();
        // Null unless every row holds the figure: a figure the server did not report (planner
        // memory before version 17) has no median, and no median is taken over some rows only.
        let series = |rows: &[&&BTreeMap<String, String>], k: &str| -> Option<Spread> {
            let v: Option<Vec<f64>> = rows.iter().map(|r| num(r, k)).collect();
            spread(&v?)
        };
        let texts = |k: &str| -> String {
            let v: Vec<String> = explained.iter().map(|r| get(r, k).to_string()).collect();
            range_text(&v)
        };
        out.push(Cell {
            reps: rs.len(),
            right: right.len(),
            wrong: rs.iter().filter(|r| get(r, "verdict") == "WRONG").count(),
            timeout: status("TIMEOUT"),
            cancelled: status("CANCELLED"),
            error: status("ERROR"),
            not_run: status("NOT_RUN"),
            explain_failed: right.len() - explained.len(),
            wall: series(&right, "wall_ms"),
            planning: series(&explained, "planning_ms"),
            execution: series(&explained, "execution_ms"),
            planner_kb: series(&explained, "planner_memory_used_kb"),
            peak_rss_kb: series(&right, "peak_rss_kb"),
            shared_hit: series(&explained, "shared_hit"),
            shared_read: series(&explained, "shared_read"),
            temp_written: series(&explained, "temp_written"),
            rows: range_text(
                &right
                    .iter()
                    .map(|r| get(r, "rows").to_string())
                    .collect::<Vec<_>>(),
            ),
            leaves_planned: texts("leaves_planned"),
            leaves_executed: texts("leaves_executed"),
            leaf_scans: texts("leaf_scans"),
            errors: rs
                .iter()
                .map(|r| get(r, "error").to_string())
                .filter(|e| !e.is_empty())
                .collect(),
            question,
            target,
            phase,
        });
    }
    out
}

/// One line per (question, target, phase). `rows` and the leaf counts are one value, or the range
/// `low..high` when the repetitions differed.
pub const SUMMARY: Declared = Declared {
    name: "summary",
    columns: &[
        ("question", Kind::Text),
        ("target", Kind::Text),
        ("phase", Kind::Text),
        ("verdict", Kind::Text),
        ("reps", Kind::Int),
        ("right", Kind::Int),
        ("wrong", Kind::Int),
        ("timeout", Kind::Int),
        ("cancelled", Kind::Int),
        ("error", Kind::Int),
        ("not_run", Kind::Int),
        ("rows", Kind::Text),
        ("wall_ms_median", Kind::Float),
        ("wall_ms_min", Kind::Float),
        ("wall_ms_max", Kind::Float),
        ("wall_ms_mad", Kind::Float),
        ("planning_ms_median", Kind::Float),
        ("planning_ms_min", Kind::Float),
        ("planning_ms_max", Kind::Float),
        ("execution_ms_median", Kind::Float),
        ("execution_ms_min", Kind::Float),
        ("execution_ms_max", Kind::Float),
        ("planner_memory_kb_median", Kind::Float),
        ("peak_rss_kb_median", Kind::Float),
        ("shared_hit_median", Kind::Float),
        ("shared_read_median", Kind::Float),
        ("leaves_planned", Kind::Text),
        ("leaves_executed", Kind::Text),
        ("leaf_scans", Kind::Text),
        ("errors", Kind::Text),
    ],
    key: &["question", "target", "phase"],
};

fn f3(x: f64) -> String {
    format!("{x:.3}")
}

/// The summary's rows, one per (question, target, phase), as text for `tables::write`.
pub fn summary_rows(cells: &[Cell]) -> Vec<Vec<String>> {
    let o = |x: &Option<Spread>, f: fn(&Spread) -> f64| {
        x.as_ref().map(|s| f3(f(s))).unwrap_or_default()
    };
    cells
        .iter()
        .map(|c| {
            vec![
                c.question.clone(),
                c.target.clone(),
                c.phase.clone(),
                c.verdict(),
                c.reps.to_string(),
                c.right.to_string(),
                c.wrong.to_string(),
                c.timeout.to_string(),
                c.cancelled.to_string(),
                c.error.to_string(),
                c.not_run.to_string(),
                c.rows.clone(),
                o(&c.wall, |s| s.median),
                o(&c.wall, |s| s.min),
                o(&c.wall, |s| s.max),
                o(&c.wall, |s| s.mad),
                o(&c.planning, |s| s.median),
                o(&c.planning, |s| s.min),
                o(&c.planning, |s| s.max),
                o(&c.execution, |s| s.median),
                o(&c.execution, |s| s.min),
                o(&c.execution, |s| s.max),
                o(&c.planner_kb, |s| s.median),
                o(&c.peak_rss_kb, |s| s.median),
                o(&c.shared_hit, |s| s.median),
                o(&c.shared_read, |s| s.median),
                c.leaves_planned.clone(),
                c.leaves_executed.clone(),
                c.leaf_scans.clone(),
                c.errors.iter().cloned().collect::<Vec<_>>().join(" | "),
            ]
        })
        .collect()
}

fn grid(
    title: &str,
    rows: &[String],
    cols: &[String],
    cell: impl Fn(&str, &str) -> String,
) -> String {
    let mut body: Vec<Vec<String>> = Vec::new();
    let mut head = vec!["question".to_string()];
    head.extend(cols.iter().cloned());
    body.push(head);
    for r in rows {
        let mut line = vec![r.clone()];
        for c in cols {
            line.push(cell(r, c));
        }
        body.push(line);
    }
    let widths: Vec<usize> = (0..body[0].len())
        .map(|i| body.iter().map(|l| l[i].chars().count()).max().unwrap_or(0))
        .collect();
    let mut s = format!("\n{title}\n");
    for (n, l) in body.iter().enumerate() {
        let cells: Vec<String> = l
            .iter()
            .enumerate()
            .map(|(i, v)| {
                if i == 0 {
                    format!("{v:<w$}", w = widths[i])
                } else {
                    format!("{v:>w$}", w = widths[i])
                }
            })
            .collect();
        let _ = writeln!(s, "{}", cells.join("  "));
        if n == 0 {
            let _ = writeln!(
                s,
                "{}",
                widths
                    .iter()
                    .map(|w| "-".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("  ")
            );
        }
    }
    s
}

/// The printed summary: one grid per measure, questions down, targets across, the blocks read and
/// written before any time. A cell whose repetitions were not all RIGHT shows what they were
/// instead of a figure.
pub fn summary_text(cells: &[Cell], targets: &[String]) -> String {
    let mut by: BTreeMap<(&str, &str, &str), &Cell> = BTreeMap::new();
    let mut questions: Vec<String> = Vec::new();
    for c in cells {
        by.insert((&c.question, &c.target, &c.phase), c);
        if !questions.contains(&c.question) {
            questions.push(c.question.clone());
        }
    }
    let both = |q: &str, t: &str| -> Vec<&Cell> {
        ["cold", "warm"]
            .iter()
            .filter_map(|p| by.get(&(q, t, *p)).copied())
            .collect()
    };
    let figure = |phase: &'static str, f: fn(&Cell) -> Option<String>| {
        let by = &by;
        move |q: &str, t: &str| -> String {
            match by.get(&(q, t, phase)) {
                None => "-".into(),
                Some(c) if !c.all_right() => c.verdict(),
                Some(c) => f(c).unwrap_or_else(|| "-".into()),
            }
        }
    };
    fn med(s: &Option<Spread>) -> Option<String> {
        s.as_ref().map(|s| format!("{:.2}", s.median))
    }
    let mut out = String::new();
    out.push_str(&grid(
        "Answers (every repetition, cold and warm)",
        &questions,
        targets,
        |q, t| {
            let cs = both(q, t);
            if cs.is_empty() {
                return "-".into();
            }
            let bad: Vec<String> = cs
                .iter()
                .filter(|c| !c.all_right())
                .map(|c| format!("{} {}", c.phase, c.verdict()))
                .collect();
            if bad.is_empty() {
                "RIGHT".into()
            } else {
                bad.join("; ")
            }
        },
    ));
    out.push_str(&grid(
        "Shared blocks read + hit, warm, median",
        &questions,
        targets,
        figure("warm", |c| match (&c.shared_read, &c.shared_hit) {
            (Some(r), Some(h)) => Some(format!("{:.0} + {:.0}", r.median, h.median)),
            _ => None,
        }),
    ));
    out.push_str(&grid(
        "Temp blocks written, warm, median",
        &questions,
        targets,
        figure("warm", |c| {
            c.temp_written.as_ref().map(|s| format!("{:.0}", s.median))
        }),
    ));
    out.push_str(&grid(
        "Leaves planned / executed / scan nodes",
        &questions,
        targets,
        figure("warm", |c| {
            Some(format!(
                "{} / {} / {}",
                c.leaves_planned, c.leaves_executed, c.leaf_scans
            ))
        }),
    ));
    out.push_str(&grid(
        "Client wall time, warm, median ms",
        &questions,
        targets,
        figure("warm", |c| med(&c.wall)),
    ));
    out.push_str(&grid(
        "Client wall time, warm, min..max ms",
        &questions,
        targets,
        figure("warm", |c| {
            c.wall
                .as_ref()
                .map(|s| format!("{:.2}..{:.2}", s.min, s.max))
        }),
    ));
    out.push_str(&grid(
        "Client wall time, cold, median ms",
        &questions,
        targets,
        figure("cold", |c| med(&c.wall)),
    ));
    out.push_str(&grid(
        "Planning time, warm, median ms",
        &questions,
        targets,
        figure("warm", |c| med(&c.planning)),
    ));
    out.push_str(&grid(
        "Planning time, cold, median ms",
        &questions,
        targets,
        figure("cold", |c| med(&c.planning)),
    ));
    out.push_str(&grid(
        "Execution time, warm, median ms",
        &questions,
        targets,
        figure("warm", |c| med(&c.execution)),
    ));
    out.push_str(&grid(
        "Planner memory used, warm, median kB",
        &questions,
        targets,
        figure("warm", |c| {
            c.planner_kb.as_ref().map(|s| format!("{:.0}", s.median))
        }),
    ));
    out.push_str(&grid(
        "Rows returned",
        &questions,
        targets,
        figure("warm", |c| Some(c.rows.clone())),
    ));

    // Totals per target.
    let mut totals = String::from("\nPer target\n");
    for t in targets {
        let mine: Vec<&Cell> = cells.iter().filter(|c| &c.target == t).collect();
        let qs: BTreeSet<&str> = mine.iter().map(|c| c.question.as_str()).collect();
        let all_right = qs
            .iter()
            .filter(|q| both(q, t).iter().all(|c| c.all_right()))
            .count();
        let warm_sum: f64 = mine
            .iter()
            .filter(|c| c.phase == "warm")
            .filter_map(|c| c.wall.as_ref().map(|s| s.median))
            .sum();
        let warm_missing = mine
            .iter()
            .filter(|c| c.phase == "warm" && c.wall.is_none())
            .count();
        let _ = writeln!(
            totals,
            "  {t}: questions {}, all repetitions RIGHT {}, not all RIGHT {}; sum of warm wall medians {:.1} ms{}",
            qs.len(),
            all_right,
            qs.len() - all_right,
            warm_sum,
            if warm_missing > 0 {
                format!(" (leaving out {warm_missing} question(s) with no RIGHT warm repetition)")
            } else {
                String::new()
            }
        );
    }
    out.push_str(&totals);
    out
}

/// The repetitions where the timed execution's own counters and its EXPLAIN disagree on the rows
/// read or on the leaves scanned, one line each. Empty is the law holding.
pub fn disagreements(t: &Table) -> Vec<String> {
    t.rows
        .iter()
        .filter(|r| get(r, "rows_read_agree") == "false" || get(r, "leaves_agree") == "false")
        .map(|r| {
            let mut line = format!(
                "{} {} {} rep {}: the statement read {} rows on the leaves and {} elsewhere, planning {} and {} of them, and execution scanned {} leaves; its EXPLAIN read {} and {} on {} leaves",
                get(r, "question"),
                get(r, "target"),
                get(r, "phase"),
                get(r, "rep"),
                get(r, "witness_leaf_rows_read"),
                get(r, "witness_other_rows_read"),
                get(r, "plan_leaf_rows_read"),
                get(r, "plan_other_rows_read"),
                get(r, "witness_leaves"),
                get(r, "explain_leaf_rows_read"),
                get(r, "explain_other_rows_read"),
                get(r, "leaves_executed"),
            );
            for (label, column) in [
                ("elsewhere, by execution's counters", "witness_other_relations"),
                ("elsewhere, by planning's counters", "plan_other_relations"),
                ("elsewhere, by its EXPLAIN", "explain_other_relations"),
                ("leaves", "leaves_disagreement"),
            ] {
                let v = get(r, column);
                if !v.is_empty() {
                    line.push_str(&format!(" | {label}: {v}"));
                }
            }
            line
        })
        .collect()
}

/// How many finished repetitions had a witness that was not OK, by its status. The law is not asked
/// of those.
pub fn unwitnessed(t: &Table) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for r in &t.rows {
        let w = get(r, "witness_status");
        if get(r, "status") == "OK" && w != "OK" {
            *out.entry(if w.is_empty() {
                "none".to_string()
            } else {
                w.to_string()
            })
            .or_insert(0) += 1;
        }
    }
    out
}

/// Per (question, target), over the repetitions that answered RIGHT: the answer's rows, the
/// planner's estimate of them, the rows the execution read by its own counters and by its EXPLAIN,
/// how many times the answer that is, the leaves it touched against those planned, and the median
/// warm and cold wall time. Time is the verdict; the rows say how far the planner's estimate and
/// the rows the execution read stand from the answer.
pub fn rows_text(t: &Table) -> String {
    type Key = (String, String);
    let mut groups: BTreeMap<Key, Vec<&BTreeMap<String, String>>> = BTreeMap::new();
    for r in t.rows.iter().filter(|r| get(r, "verdict") == "RIGHT") {
        groups
            .entry((get(r, "question").into(), get(r, "target").into()))
            .or_default()
            .push(r);
    }
    let med = |rs: &[&BTreeMap<String, String>], k: &str| -> Option<f64> {
        let xs: Vec<f64> = rs.iter().filter_map(|r| num(r, k)).collect();
        if xs.len() == rs.len() {
            crate::stats::median(&xs)
        } else {
            None
        }
    };
    let show = |x: Option<f64>| x.map_or("-".to_string(), |v| format!("{v:.0}"));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "question\ttarget\tanswer\testimate\tread\tplanner_read\texplain_read\tread_per_answer\tleaves_touched\tleaves_planned\twarm_ms\tcold_ms"
    );
    for ((q, target), rs) in &groups {
        let answer = med(rs, "expected_rows");
        let read = med(rs, "witness_leaf_rows_read");
        let per = match (read, answer) {
            (Some(r), Some(a)) if a > 0.0 => format!("{:.2}", r / a),
            (Some(_), Some(_)) => "no answer".to_string(),
            _ => "-".to_string(),
        };
        let phase = |p: &str| {
            let ps: Vec<&BTreeMap<String, String>> = rs
                .iter()
                .copied()
                .filter(|r| get(r, "phase") == p)
                .collect();
            if ps.is_empty() {
                "-".to_string()
            } else {
                med(&ps, "wall_ms").map_or("-".to_string(), |v| format!("{v:.3}"))
            }
        };
        let _ = writeln!(
            out,
            "{q}\t{target}\t{}\t{}\t{}\t{}\t{}\t{per}\t{}\t{}\t{}\t{}",
            show(answer),
            show(med(rs, "plan_rows")),
            show(read),
            show(
                med(rs, "plan_leaf_rows_read")
                    .zip(med(rs, "plan_other_rows_read"))
                    .map(|(a, b)| a + b)
            ),
            show(med(rs, "explain_leaf_rows_read")),
            show(med(rs, "witness_leaves")),
            show(med(rs, "leaves_planned")),
            phase("warm"),
            phase("cold"),
        );
    }
    out
}

/// The rows of one loop: `rows` over `loops` loops, whole where they divide, else to two decimals;
/// `-` for a scan that never ran.
fn per_loop(rows: &str, loops: &str) -> String {
    match (rows.parse::<f64>(), loops.parse::<f64>()) {
        (Ok(r), Ok(l)) if l > 0.0 => {
            let v = r / l;
            if v.fract() == 0.0 {
                format!("{v:.0}")
            } else {
                format!("{v:.2}")
            }
        }
        _ => "-".to_string(),
    }
}

/// From the leaves (one line per scan node of each target's first EXPLAIN of each question): per
/// question and relation, each target's scans of it, in plan order: the scan's type and index, the
/// planner's estimate for one loop against the actual rows of one loop, its loops and its heap
/// fetches. Under index sets the targets are `<target>@<set>`, so each relation's scans under every
/// set stand together.
pub fn scans_text(leaves: &Table) -> String {
    let mut lines: Vec<(String, String, String, i64, String)> = leaves
        .rows
        .iter()
        .map(|r| {
            let relation = match get(r, "schema") {
                "" => get(r, "relation").to_string(),
                s => format!("{s}.{}", get(r, "relation")),
            };
            let dash = |k: &str| match get(r, k) {
                "" => "-".to_string(),
                v => v.to_string(),
            };
            let line = format!(
                "{}\t{relation}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                get(r, "question"),
                get(r, "target"),
                get(r, "node"),
                get(r, "node_type"),
                dash("index"),
                dash("plan_rows"),
                per_loop(get(r, "rows"), get(r, "loops")),
                get(r, "loops"),
                dash("heap_fetches"),
            );
            (
                get(r, "question").to_string(),
                relation,
                get(r, "target").to_string(),
                get(r, "node").parse().unwrap_or(0),
                line,
            )
        })
        .collect();
    lines.sort();
    let mut out = String::from(
        "question\trelation\ttarget\tnode\tscan\tindex\testimate\tactual\tloops\theap_fetches\n",
    );
    for (.., line) in lines {
        let _ = writeln!(out, "{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_stand_by_question_and_relation_with_the_estimate_against_the_actual_per_loop() {
        let row = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let scan = |target: &str,
                    node: &str,
                    kind: &str,
                    relation: &str,
                    est: &str,
                    rows: &str,
                    loops: &str| {
            row(&[
                ("target", target),
                ("question", "q"),
                ("node", node),
                ("node_type", kind),
                ("schema", "lego"),
                ("relation", relation),
                ("index", ""),
                ("loops", loops),
                ("rows", rows),
                ("plan_rows", est),
                ("heap_fetches", ""),
            ])
        };
        let t = Table {
            rows: vec![
                scan(
                    "flat@dba",
                    "2",
                    "Seq Scan",
                    "lego_purchases",
                    "35",
                    "760",
                    "1",
                ),
                scan("flat@all", "1", "Index Scan", "lego_sets", "1", "2", "3"),
                scan(
                    "flat@all",
                    "2",
                    "Index Scan",
                    "lego_purchases",
                    "740",
                    "760",
                    "1",
                ),
                scan("flat@dba", "1", "Seq Scan", "lego_sets", "", "0", "0"),
            ],
        };
        let text = scans_text(&t);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "question\trelation\ttarget\tnode\tscan\tindex\testimate\tactual\tloops\theap_fetches",
                "q\tlego.lego_purchases\tflat@all\t2\tIndex Scan\t-\t740\t760\t1\t-",
                "q\tlego.lego_purchases\tflat@dba\t2\tSeq Scan\t-\t35\t760\t1\t-",
                "q\tlego.lego_sets\tflat@all\t1\tIndex Scan\t-\t1\t0.67\t3\t-",
                "q\tlego.lego_sets\tflat@dba\t1\tSeq Scan\t-\t-\t-\t0\t-",
            ]
        );
    }

    fn results(lines: &[&str]) -> Table {
        let head = "question\ttarget\tphase\tstatus\tverdict\twall_ms\texplain_status\tplanning_ms\texecution_ms\trows\tleaves_planned\tleaves_executed\tleaf_scans\terror\tplanner_memory_used_kb";
        Table {
            rows: lines
                .iter()
                .map(|l| {
                    head.split('\t')
                        .zip(l.split('\t'))
                        .map(|(h, v)| (h.to_string(), v.to_string()))
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn a_disagreement_names_the_relations_read_outside_the_leaves() {
        let row = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let t = Table {
            rows: vec![
                row(&[
                    ("question", "q"),
                    ("target", "t"),
                    ("phase", "warm"),
                    ("rep", "1"),
                    ("rows_read_agree", "false"),
                    ("leaves_agree", "true"),
                    ("witness_other_relations", "pg_catalog.pg_class 16/1"),
                    ("explain_other_relations", ""),
                ]),
                row(&[("rows_read_agree", "true"), ("leaves_agree", "true")]),
            ],
        };
        let d = disagreements(&t);
        assert_eq!(d.len(), 1);
        assert!(
            d[0].ends_with(" | elsewhere, by execution's counters: pg_catalog.pg_class 16/1"),
            "{}",
            d[0]
        );
    }

    #[test]
    fn a_wrong_answer_is_reported_and_not_timed() {
        let t = results(&[
            "q\tt\twarm\tOK\tRIGHT\t10\tOK\t1\t2\t5\t3\t3\t3\t",
            "q\tt\twarm\tOK\tWRONG\t1\tOK\t1\t2\t4\t3\t3\t3\t",
            "q\tt\twarm\tOK\tRIGHT\t12\tOK\t1\t2\t5\t3\t3\t3\t",
        ]);
        let c = &summarise(&t)[0];
        assert_eq!(c.verdict(), "WRONG 1/3");
        assert!(!c.all_right());
        // the fast wrong repetition is not in the median
        assert_eq!(c.wall.unwrap().median, 11.0);
        assert_eq!(c.wall.unwrap().n, 2);
    }

    #[test]
    fn a_timeout_is_reported_and_not_averaged() {
        let t = results(&[
            "q\tt\tcold\tTIMEOUT\t-\t120000\tNOT_RUN\t\t\t\t\t\t\tcanceling statement due to statement timeout",
            "q\tt\tcold\tNOT_RUN\t-\t\tNOT_RUN\t\t\t\t\t\t\tan earlier repetition did not finish",
        ]);
        let c = &summarise(&t)[0];
        assert_eq!(c.verdict(), "TIMEOUT 1/2");
        assert!(c.wall.is_none());
        assert_eq!(c.errors.len(), 2);
    }

    #[test]
    fn all_right_is_right() {
        let t = results(&[
            "q\tt\twarm\tOK\tRIGHT\t10\tOK\t1\t2\t5\t3\t3\t3\t",
            "q\tt\twarm\tOK\tRIGHT\t30\tOK\t3\t2\t5\t3\t3\t4\t",
        ]);
        let c = &summarise(&t)[0];
        assert_eq!(c.verdict(), "RIGHT");
        assert_eq!(c.planning.unwrap().median, 2.0);
        assert_eq!(c.leaves_planned, "3");
        assert_eq!(c.leaf_scans, "3..4");
    }

    fn summary_value(cells: &[Cell], column: &str) -> crate::tables::Value {
        let dir = std::env::temp_dir().join(format!(
            "warren-bench-summary-{column}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("summary.parquet");
        crate::tables::write(&p, &SUMMARY, &summary_rows(cells)).unwrap();
        let rows = crate::tables::read_values(&p, &SUMMARY).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let i = SUMMARY.names().iter().position(|c| *c == column).unwrap();
        rows[0][i].clone()
    }

    #[test]
    fn a_median_over_a_figure_the_server_did_not_report_is_null() {
        let t = results(&[
            "q\tt\twarm\tOK\tRIGHT\t10\tOK\t1\t2\t5\t3\t3\t3\t\t",
            "q\tt\twarm\tOK\tRIGHT\t12\tOK\t1\t2\t5\t3\t3\t3\t\t",
        ]);
        let cells = summarise(&t);
        assert_eq!(cells[0].verdict(), "RIGHT");
        assert_eq!(cells[0].planner_kb, None);
        assert_eq!(
            summary_value(&cells, "planner_memory_kb_median"),
            crate::tables::Value::Null
        );
        assert_eq!(
            summary_value(&cells, "planning_ms_median"),
            crate::tables::Value::Float(1.0)
        );
        let text = summary_text(&cells, &["t".into()]);
        let grid = text
            .split("Planner memory used, warm, median kB")
            .nth(1)
            .unwrap();
        let line = grid.lines().find(|l| l.starts_with("q ")).unwrap();
        assert!(!line.contains('0'), "{line}");
    }

    #[test]
    fn a_median_over_a_figure_some_repetitions_lack_is_null() {
        let t = results(&[
            "q\tt\twarm\tOK\tRIGHT\t10\tOK\t1\t2\t5\t3\t3\t3\t\t160",
            "q\tt\twarm\tOK\tRIGHT\t12\tOK\t1\t2\t5\t3\t3\t3\t\t",
            "q\tt\twarm\tOK\tRIGHT\t11\tOK\t1\t2\t5\t3\t3\t3\t\t170",
        ]);
        let cells = summarise(&t);
        assert_eq!(cells[0].planner_kb, None);
        let t = results(&[
            "q\tt\twarm\tOK\tRIGHT\t10\tOK\t1\t2\t5\t3\t3\t3\t\t160",
            "q\tt\twarm\tOK\tRIGHT\t11\tOK\t1\t2\t5\t3\t3\t3\t\t170",
        ]);
        assert_eq!(summarise(&t)[0].planner_kb.unwrap().median, 165.0);
    }

    #[test]
    fn the_printed_grid_shows_the_verdict_in_place_of_a_time() {
        let t = results(&[
            "q\tfast\twarm\tOK\tWRONG\t1\tOK\t1\t2\t4\t3\t3\t3\t",
            "q\tslow\twarm\tOK\tRIGHT\t50\tOK\t1\t2\t5\t3\t3\t3\t",
        ]);
        let cells = summarise(&t);
        let text = summary_text(&cells, &["fast".into(), "slow".into()]);
        let wall = text
            .split("Client wall time, warm, median ms")
            .nth(1)
            .unwrap();
        let line = wall.lines().find(|l| l.starts_with("q ")).unwrap();
        assert!(line.contains("WRONG 1/1"));
        assert!(line.contains("50.00"));
        assert!(!line.contains("1.00"));
    }
}
