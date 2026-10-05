// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The rows of a table read under constant conditions, measured while planning.
//!
//! For a table surveyed for the question, read alone or as an inheritance child, its B-trees, GIN
//! and GiST indexes measure the conditions they hold: the index holding the most conditions not yet
//! counted first, of those the one stepping through the fewest key columns, each condition counted
//! once. Which conditions an index holds is read from the conditions alone; an index is read only
//! where it holds a condition no index read before it counted. Where two or more B-trees and GIN
//! indexes counted conditions, the rows in all of their conditions are read together (`overlap`):
//! on one B-tree's own entries where they carry the others' columns, else by the addresses of the
//! rows each holds. The relation's rows are the table's rows, times each group's share together,
//! times each other index's share, times the planner's own share of the conditions no index holds.
//! The relation, every path made for it (a parallel path's rows divided among its workers as the
//! planner divides them) and each of its parameterized sizes take that count, a parameterized size
//! at most the relation's, and at most one row where its conditions fix every column of a unique
//! key by equality, each compared under the collation its key column keeps: an equality under
//! another collation, one that ignores case among them, can match several rows the key keeps apart.
//!
//! The relation is measured once in a round of planning, and every user takes that one measure: its
//! size, and the price of every path through a B-tree holding the conditions it counted. What each
//! index measured is kept until the planning that made the relation returns.
//!
//! A table read with its inheritance children takes the sum of its live children's rows, where one
//! of them is surveyed; its paths already add their children's.
//!
//! A table is surveyed where it carries a valid surveyor over the whole table, or a partial one
//! whose predicate the planner proved from the question's own conditions.

use crate::conditions::{self, Held};
use crate::overlap::{self, Together};
use crate::query::{cells, surveyed, surveyor_am};
use crate::round;
use crate::{gin, gist};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr::null_mut;

static mut NEXT_PATHLIST: pg_sys::set_rel_pathlist_hook_type = None;

/// What one index measured of a relation's conditions in this round.
#[derive(Clone)]
struct Kept {
    /// The depth of the planning that made the relation.
    depth: u32,
    rel: usize,
    index: pg_sys::Oid,
    /// The conditions left out.
    left: Vec<usize>,
    held: Option<Held>,
    /// The leaves a B-tree's blocks lie on.
    leaves: Option<f64>,
}

thread_local! {
    /// What each index measured of a relation's conditions in this round.
    static MEASURES: RefCell<Vec<Kept>> = const { RefCell::new(Vec::new()) };
    /// Each relation's measure in this round: the depth of the planning, the relation, and the
    /// measure.
    static RELATIONS: RefCell<Vec<(u32, usize, Option<Read>)>> = const { RefCell::new(Vec::new()) };
}

/// Puts the hook in place, after any hook already there.
pub fn init() {
    unsafe {
        NEXT_PATHLIST = pg_sys::set_rel_pathlist_hook;
        pg_sys::set_rel_pathlist_hook = Some(pathlist);
    }
}

/// Forgets what was measured by plannings at depth `depth` and deeper.
pub(crate) fn forget_from(depth: u32) {
    MEASURES.with(|m| m.borrow_mut().retain(|e| e.depth < depth));
    RELATIONS.with(|r| r.borrow_mut().retain(|e| e.0 < depth));
}

/// What was measured in the round so far, to be put back by `put_back`.
pub(crate) struct Saved(Vec<Kept>, Vec<(u32, usize, Option<Read>)>);

/// What was measured in the round so far.
pub(crate) fn saved() -> Saved {
    Saved(
        MEASURES.with(|m| m.borrow().clone()),
        RELATIONS.with(|r| r.borrow().clone()),
    )
}

/// Puts back what `saved` holds as what was measured in the round.
pub(crate) fn put_back(saved: Saved) {
    MEASURES.with(|m| *m.borrow_mut() = saved.0);
    RELATIONS.with(|r| *r.borrow_mut() = saved.1);
}

#[pg_guard]
unsafe extern "C-unwind" fn pathlist(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rti: pg_sys::Index,
    rte: *mut pg_sys::RangeTblEntry,
) {
    if let Some(next) = NEXT_PATHLIST {
        next(root, rel, rti, rte);
    }
    if root.is_null()
        || rel.is_null()
        || rte.is_null()
        || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
        || !(*rte).tablesample.is_null()
        || !matches!(
            (*rel).reloptkind,
            pg_sys::RelOptKind::RELOPT_BASEREL | pg_sys::RelOptKind::RELOPT_OTHER_MEMBER_REL
        )
        || pg_sys::is_dummy_rel(rel)
    {
        return;
    }
    let am = surveyor_am();
    if am == pg_sys::InvalidOid {
        return;
    }
    if (*rte).inh {
        children_summed(root, rel, rti);
        return;
    }
    if !surveyed(rel, true) {
        return;
    }
    let _planning = crate::budget::planning(root);
    let before = (*rel).rows;
    if let Some(read) = relation(root, rel, am) {
        let rows = read.rows(root, rel);
        rewrite(root, rel, rows);
        if pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
            let together: String = read
                .together
                .iter()
                .map(|t| {
                    format!(
                        ", {} of them counted together {} at {:.0} rows ({} pages)",
                        t.parts.len(),
                        if t.on_entries && t.every_leaf {
                            "on every leaf of one index's block"
                        } else if t.on_entries {
                            "on five leaves of one index's block"
                        } else {
                            "by their rows' addresses"
                        },
                        t.share * (*rel).tuples,
                        t.pages
                    )
                })
                .collect();
            debug1!(
                "surveyor: {} measured at {:.0} rows of {:.0} by {}{} (pages read {}); the planner had {:.0}",
                name((*rte).relid),
                rows,
                (*rel).tuples,
                read.indexes.join(", "),
                together,
                read.pages,
                before
            );
        }
    }
}

unsafe fn name(relid: pg_sys::Oid) -> String {
    let n = pg_sys::get_rel_name(relid);
    if n.is_null() {
        relid.to_u32().to_string()
    } else {
        CStr::from_ptr(n).to_string_lossy().into_owned()
    }
}

/// What the indexes of a relation measured of its conditions: each index's conditions and its share
/// of the table's rows, the rows in all of the conditions of each group of indexes read together,
/// the pages read, and the indexes.
#[derive(Clone)]
pub(crate) struct Read {
    parts: Vec<(Vec<*mut pg_sys::RestrictInfo>, f64)>,
    together: Vec<Together>,
    pages: u32,
    indexes: Vec<String>,
}

impl Read {
    /// The conditions the indexes counted.
    fn counted(&self) -> Vec<*mut pg_sys::RestrictInfo> {
        self.parts
            .iter()
            .flat_map(|p| p.0.iter().copied())
            .collect()
    }

    /// The share of the table's rows the indexes measured for the conditions they counted: the
    /// share in all of the conditions of each group of indexes read together, times each other
    /// index's share.
    fn share(&self) -> f64 {
        let together = |at: usize| self.together.iter().any(|t| t.parts.contains(&at));
        let apart: f64 = (0..self.parts.len())
            .filter(|&at| !together(at))
            .map(|at| self.parts[at].1)
            .product();
        apart * self.together.iter().map(|t| t.share).product::<f64>()
    }

    /// The relation's rows: the table's, times the indexes' share, times the planner's own share of
    /// the conditions no index counted.
    unsafe fn rows(&self, root: *mut pg_sys::PlannerInfo, rel: *mut pg_sys::RelOptInfo) -> f64 {
        let counted = self.counted();
        let rest: Vec<*mut pg_sys::RestrictInfo> = cells((*rel).baserestrictinfo)
            .into_iter()
            .map(|ri| ri as *mut pg_sys::RestrictInfo)
            .filter(|ri| !counted.contains(ri))
            .collect();
        pg_sys::clamp_row_est((*rel).tuples * self.share() * selectivity(root, &rest))
    }
}

/// The conditions `index` of `rel` would hold, other than those in `counted`, read from the
/// conditions alone, and the key columns its read would step through.
unsafe fn holds(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> (Vec<*mut pg_sys::RestrictInfo>, usize) {
    match (*index).relam {
        pg_sys::BTREE_AM_OID => {
            conditions::holding(root, rel, index, counted).map_or((Vec::new(), 0), |h| {
                let stepped = h.stepped();
                (h.clauses, stepped)
            })
        }
        pg_sys::GIN_AM_OID => (gin::holds(root, rel, index, counted), 0),
        pg_sys::GIST_AM_OID => (gist::holds(root, rel, index, counted), 0),
        _ => (Vec::new(), 0),
    }
}

/// What `index` of `rel` measures of `rel`'s conditions, other than those in `counted`: a B-tree's,
/// a GIN's or a GiST's, measured once in the round.
pub(crate) unsafe fn measure(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<Held> {
    measure_on_leaves(root, rel, index, counted).0
}

/// As `measure`, with the leaves a B-tree's blocks lie on.
pub(crate) unsafe fn measure_on_leaves(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> (Option<Held>, Option<f64>) {
    let depth = round::depth();
    let oid = (*index).indexoid;
    let mut left: Vec<usize> = counted.iter().map(|&c| c as usize).collect();
    left.sort_unstable();
    left.dedup();
    if depth > 0 {
        let kept = MEASURES.with(|m| {
            m.borrow()
                .iter()
                .find(|e| e.rel == rel as usize && e.index == oid && e.left == left)
                .map(|e| (e.held.clone(), e.leaves))
        });
        if let Some(kept) = kept {
            return kept;
        }
    }
    let (held, leaves) = match (*index).relam {
        pg_sys::BTREE_AM_OID => conditions::held_on_leaves(root, rel, index, counted)
            .map_or((None, None), |(h, l)| (Some(h), l)),
        pg_sys::GIN_AM_OID => (gin::held_except(root, rel, index, counted), None),
        pg_sys::GIST_AM_OID => (gist::held_except(root, rel, index, counted), None),
        _ => (None, None),
    };
    if depth > 0 {
        MEASURES.with(|m| {
            m.borrow_mut().push(Kept {
                depth,
                rel: rel as usize,
                index: oid,
                left,
                held: held.clone(),
                leaves,
            })
        });
    }
    (held, leaves)
}

/// What the indexes of `rel` measured of its constant conditions, measured once in the round; none
/// where no index holds a condition.
pub(crate) unsafe fn relation(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    am: pg_sys::Oid,
) -> Option<Read> {
    let depth = round::depth();
    if depth > 0 {
        let kept = RELATIONS.with(|r| {
            r.borrow()
                .iter()
                .find(|e| e.1 == rel as usize)
                .map(|e| e.2.clone())
        });
        if let Some(kept) = kept {
            return kept;
        }
    }
    let read = measured(root, rel, am);
    if depth > 0 {
        RELATIONS.with(|r| r.borrow_mut().push((depth, rel as usize, read.clone())));
    }
    read
}

/// The rows of `rel` under its constant conditions, measured by its indexes; none where no index
/// holds a condition.
unsafe fn measured(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    am: pg_sys::Oid,
) -> Option<Read> {
    let tuples = (*rel).tuples;
    if tuples <= 0.0 || (*rel).baserestrictinfo.is_null() {
        return None;
    }
    let mut candidates: Vec<*mut pg_sys::IndexOptInfo> = cells((*rel).indexlist)
        .into_iter()
        .map(|i| i as *mut pg_sys::IndexOptInfo)
        .filter(|&i| (*i).relam != am)
        .collect();
    let mut counted: Vec<*mut pg_sys::RestrictInfo> = Vec::new();
    let mut read = Read {
        parts: Vec::new(),
        together: Vec::new(),
        pages: 0,
        indexes: Vec::new(),
    };
    let mut from = Vec::new();
    loop {
        // the index holding the most conditions not yet counted, of those the one stepping through
        // the fewest key columns
        let best = candidates
            .iter()
            .enumerate()
            .map(|(at, &i)| {
                let (clauses, stepped) = holds(root, rel, i, &counted);
                (at, clauses.len(), stepped)
            })
            .filter(|&(_, n, _)| n > 0)
            .fold(
                None,
                |best: Option<(usize, usize, usize)>, (at, n, s)| match best {
                    Some((_, m, t)) if m > n || (m == n && t <= s) => best,
                    _ => Some((at, n, s)),
                },
            );
        let Some((at, _, _)) = best else { break };
        let index = candidates.remove(at);
        let Some(h) = measure(root, rel, index, &counted) else {
            continue;
        };
        if h.clauses.is_empty() {
            continue;
        }
        read.parts
            .push((h.clauses.clone(), (h.rows / tuples).clamp(0.0, 1.0)));
        read.pages += h.pages;
        read.indexes.push(name((*index).indexoid));
        from.push(index);
        counted.extend(h.clauses.iter().copied());
    }
    if read.parts.is_empty() {
        return None;
    }
    let parts: Vec<overlap::Part> = from
        .iter()
        .zip(&read.parts)
        .map(|(&index, (clauses, share))| (index, clauses.as_slice(), *share))
        .collect();
    read.together = overlap::together(root, rel, &parts);
    read.pages += read.together.iter().map(|t| t.pages).sum::<u32>();
    Some(read)
}

/// The planner's own share of the rows `clauses` leave.
unsafe fn selectivity(
    root: *mut pg_sys::PlannerInfo,
    clauses: &[*mut pg_sys::RestrictInfo],
) -> f64 {
    if clauses.is_empty() {
        return 1.0;
    }
    let mut list: *mut pg_sys::List = null_mut();
    for &ri in clauses {
        list = pg_sys::lappend(list, ri as *mut std::ffi::c_void);
    }
    pg_sys::clauselist_selectivity(root, list, 0, pg_sys::JoinType::JOIN_INNER, null_mut())
}

/// The workers' share of a parallel path's rows, as the planner divides them.
unsafe fn parallel_divisor(path: *mut pg_sys::Path) -> f64 {
    let workers = (*path).parallel_workers as f64;
    let leader = 1.0 - 0.3 * workers;
    if pg_sys::parallel_leader_participation && leader > 0.0 {
        workers + leader
    } else {
        workers
    }
}

/// Whether the condition `ri` fixes key column `at` of the index `index` of `rel` by an equality of
/// the column's operator family, compared under the collation the column keeps, against a value
/// that reads nothing of `rel`.
unsafe fn fixes(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    at: usize,
    ri: *mut pg_sys::RestrictInfo,
) -> bool {
    let clause = (*ri).clause as *mut pg_sys::Node;
    if clause.is_null() || (*clause).type_ != pg_sys::NodeTag::T_OpExpr {
        return false;
    }
    let op = clause as *mut pg_sys::OpExpr;
    let args = cells((*op).args);
    if args.len() != 2 {
        return false;
    }
    let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
    let (other, opno) = if conditions::column_of(rel, index, l) == Some(at) {
        (r, (*op).opno)
    } else if conditions::column_of(rel, index, r) == Some(at) {
        (l, pg_sys::get_commutator((*op).opno))
    } else {
        return false;
    };
    opno != pg_sys::InvalidOid
        && matches!(
            conditions::strategy(opno, *(*index).opfamily.add(at)),
            Some((n, _)) if n == pg_sys::BTEqualStrategyNumber as i32
        )
        && conditions::compares_as(index, at, (*op).inputcollid)
        && !pg_sys::bms_is_member((*rel).relid as i32, pg_sys::pull_varnos(root, other))
        && !pg_sys::contain_volatile_functions(other)
}

/// The most rows a lookup of `rel` under `ppi` can find: one, where every column of one of its
/// unique keys is fixed by an equality, from the lookup's own conditions or the relation's.
unsafe fn allowed(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    ppi: *mut pg_sys::ParamPathInfo,
) -> Option<f64> {
    let mut clauses = cells((*ppi).ppi_clauses);
    clauses.extend(cells((*rel).baserestrictinfo));
    cells((*rel).indexlist)
        .into_iter()
        .map(|i| i as *mut pg_sys::IndexOptInfo)
        .any(|index| {
            (*index).unique
                && (*index).immediate
                && ((*index).indpred.is_null() || (*index).predOK)
                && (0..(*index).nkeycolumns as usize).all(|at| {
                    clauses
                        .iter()
                        .any(|&ri| fixes(root, rel, index, at, ri as *mut pg_sys::RestrictInfo))
                })
        })
        .then_some(1.0)
}

/// Sets `rel`'s rows, and every path's and parameterized size's to match; a parameterized size at
/// most what its unique keys allow.
unsafe fn rewrite(root: *mut pg_sys::PlannerInfo, rel: *mut pg_sys::RelOptInfo, rows: f64) {
    let before = (*rel).rows;
    (*rel).rows = rows;
    let ratio = if before > 0.0 { rows / before } else { 1.0 };
    for ppi in cells((*rel).ppilist) {
        let ppi = ppi as *mut pg_sys::ParamPathInfo;
        let most = allowed(root, rel, ppi).unwrap_or(rows).min(rows);
        (*ppi).ppi_rows = pg_sys::clamp_row_est(((*ppi).ppi_rows * ratio).min(most));
    }
    let rows_of = |path: *mut pg_sys::Path| {
        if (*path).param_info.is_null() {
            rows
        } else {
            (*(*path).param_info).ppi_rows
        }
    };
    for path in cells((*rel).pathlist) {
        let path = path as *mut pg_sys::Path;
        (*path).rows = rows_of(path);
    }
    for path in cells((*rel).partial_pathlist) {
        let path = path as *mut pg_sys::Path;
        let divisor = parallel_divisor(path);
        (*path).rows = pg_sys::clamp_row_est(if divisor > 0.0 {
            rows_of(path) / divisor
        } else {
            rows_of(path)
        });
    }
}

/// Sets the rows of the inheritance parent `rel`, at `rti`, to its live children's, where one of
/// them is surveyed.
unsafe fn children_summed(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rti: pg_sys::Index,
) {
    let mut sum = 0.0;
    let mut any = false;
    for info in cells((*root).append_rel_list) {
        let info = info as *mut pg_sys::AppendRelInfo;
        if (*info).parent_relid != rti {
            continue;
        }
        let at = (*info).child_relid as usize;
        if (*root).simple_rel_array.is_null() || at >= (*root).simple_rel_array_size as usize {
            continue;
        }
        let child = *(*root).simple_rel_array.add(at);
        if child.is_null() || pg_sys::is_dummy_rel(child) {
            continue;
        }
        sum += (*child).rows;
        any |= surveyed(child, true);
    }
    if any && sum > 0.0 {
        (*rel).rows = sum;
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;
    use std::cell::RefCell;
    use std::ffi::CStr;

    thread_local! {
        static SEEN: RefCell<Vec<(String, bool, f64)>> = const { RefCell::new(Vec::new()) };
    }
    static mut NEXT: pg_sys::set_rel_pathlist_hook_type = None;

    /// Records each table relation's rows once the hooks before it have run: its name, whether it
    /// is read with its children, and its rows.
    #[pg_guard]
    unsafe extern "C-unwind" fn record(
        root: *mut pg_sys::PlannerInfo,
        rel: *mut pg_sys::RelOptInfo,
        rti: pg_sys::Index,
        rte: *mut pg_sys::RangeTblEntry,
    ) {
        if let Some(next) = NEXT {
            next(root, rel, rti, rte);
        }
        if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION {
            return;
        }
        let name = CStr::from_ptr(pg_sys::get_rel_name((*rte).relid))
            .to_string_lossy()
            .into_owned();
        SEEN.with(|s| s.borrow_mut().push((name, (*rte).inh, (*rel).rows)));
    }

    /// Each table relation's rows while `query` was planned.
    fn sizes(query: &str) -> Vec<(String, bool, f64)> {
        SEEN.with(|s| s.borrow_mut().clear());
        unsafe {
            NEXT = pg_sys::set_rel_pathlist_hook;
            pg_sys::set_rel_pathlist_hook = Some(record);
        }
        let planned = Spi::run(&format!("EXPLAIN {query}"));
        unsafe { pg_sys::set_rel_pathlist_hook = NEXT };
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        SEEN.with(|s| s.borrow().clone())
    }

    fn texts(sql: &str) -> Vec<String> {
        crate::tests::texts(sql)
    }

    fn count(sql: &str) -> f64 {
        Spi::get_one::<i64>(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .unwrap() as f64
    }

    /// The rows EXPLAIN gives each line of `query`'s plan that names `node`.
    fn estimates(query: &str, node: &str) -> Vec<f64> {
        texts(&format!("EXPLAIN {query}"))
            .iter()
            .filter(|l| l.contains(node))
            .map(|l| {
                l.split(" rows=")
                    .nth(1)
                    .and_then(|r| r.split_whitespace().next())
                    .and_then(|r| r.parse().ok())
                    .unwrap_or_else(|| panic!("no estimate in {l}"))
            })
            .collect()
    }

    /// 60,000 purchases every 17 minutes from 2020, each in a lane (5) and a slot (10) whose lane
    /// it decides, and on one of 600 days, with a key on the month and the instant, a key on the
    /// lane and the slot, and a key on the day.
    fn bought() {
        Spi::run(
            "CREATE TABLE bought AS \
             SELECT g AS id, timestamp '2020-01-01' + g * interval '17 minutes' AS at, \
                    (g % 5)::smallint AS lane, (g % 10)::smallint AS slot, g / 100 AS day \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX bought_month_at ON bought ((extract(month FROM at)::smallint), at); \
             CREATE INDEX bought_lane_slot ON bought (lane, slot); \
             CREATE INDEX bought_day ON bought (day); \
             ANALYZE bought",
        )
        .unwrap();
    }

    /// The rows under one leaf of `index`, on average.
    fn under_a_leaf(index: &str) -> f64 {
        Spi::get_one::<f64>(&format!(
            "SELECT 60000.0::float8 / greatest(relpages - 2, 1) FROM pg_class WHERE relname = '{index}'"
        ))
        .unwrap()
        .unwrap()
    }

    const MONTH: &str = "extract(month FROM at)::smallint";

    fn surveyed() {
        Spi::run("CREATE INDEX bought_order ON bought USING surveyor (id)").unwrap();
    }

    #[pg_test]
    fn a_tables_rows_are_what_its_indexes_measure_and_every_path_takes_them() {
        bought();
        let base = "SELECT id FROM bought WHERE";
        let count_of = "SELECT count(*) FROM bought WHERE";
        let conditions = [
            (
                format!("{MONTH} = 12 AND at >= '2020-12-01' AND at < '2020-12-04'"),
                "bought_month_at",
                1.0,
            ),
            // the month's 12 values stepped through
            (
                "at >= '2021-03-01' AND at < '2021-03-02'".to_string(),
                "bought_month_at",
                12.0,
            ),
            // a pair the planner multiplies, which one key reads at once
            ("lane = 1 AND slot = 6".to_string(), "bought_lane_slot", 1.0),
            (
                "lane = 1 AND slot IN (1, 6)".to_string(),
                "bought_lane_slot",
                2.0,
            ),
        ];
        let before: Vec<f64> = conditions
            .iter()
            .map(|(c, _, _)| estimates(&format!("{base} {c}"), "on bought")[0])
            .collect();
        surveyed();
        for ((c, index, blocks), planned) in conditions.iter().zip(before) {
            let query = format!("{base} {c}");
            let counted = count(&format!("{count_of} {c}"));
            let rows = estimates(&query, "on bought")[0];
            // one leaf at each end of each block
            assert!(
                (rows - counted).abs() <= 2.0 * blocks * under_a_leaf(index) + 1.0,
                "{c}: {rows} measured, {counted} counted, {planned} planned"
            );
            // every path of the relation takes the count
            Spi::run("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off").unwrap();
            let through_index = estimates(&query, "on bought")[0];
            Spi::run("RESET enable_seqscan; RESET enable_bitmapscan").unwrap();
            assert_eq!(through_index, rows, "{c}");
        }
        // the planner's product of the lane's and the slot's shares, a fifth of the pair's rows
        let pair = estimates(&format!("{base} lane = 1 AND slot = 6"), "on bought")[0];
        assert!(pair > 4000.0, "{pair}");
    }

    /// 20,000 spots, one in every 500 holding the word "rare" and the rest "plain", each at a
    /// random place in a box 100 by 200, with a GIN on the words and a GiST on the places. The
    /// words keep no statistics.
    fn spots() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; SELECT setseed(0.29); \
             CREATE TABLE spots AS SELECT g AS id, \
                 to_tsvector('simple', CASE WHEN g % 500 = 0 THEN 'rare' ELSE 'plain' END) AS words, \
                 cube(ARRAY[random() * 100, random() * 200]) AS place \
             FROM generate_series(1, 20000) g; \
             ALTER TABLE spots ALTER words SET STATISTICS 0; \
             CREATE INDEX spots_words ON spots USING gin (words); \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX spots_place ON spots USING gist (place); \
             ANALYZE spots",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_word_and_a_box_are_measured_by_the_gin_and_the_gist() {
        spots();
        let base = "SELECT id FROM spots WHERE";
        let word = "words @@ 'rare'::tsquery";
        let place = "place <@ cube(ARRAY[0, 0]::float8[], ARRAY[10, 20]::float8[])";
        let planned: Vec<f64> = [word, place]
            .iter()
            .map(|c| estimates(&format!("{base} {c}"), "on spots")[0])
            .collect();
        Spi::run("CREATE INDEX spots_order ON spots USING surveyor (id)").unwrap();
        // the word's rows, counted by the GIN
        let counted = count(&format!("SELECT count(*) FROM spots WHERE {word}"));
        let rows = estimates(&format!("{base} {word}"), "on spots")[0];
        assert_eq!(rows, counted, "planned {}", planned[0]);
        assert!(
            (planned[0] - counted).abs() > 10.0,
            "planned {}",
            planned[0]
        );
        // the box's rows, measured by the GiST
        let counted = count(&format!("SELECT count(*) FROM spots WHERE {place}"));
        let rows = estimates(&format!("{base} {place}"), "on spots")[0];
        assert!(
            rows >= 0.25 * counted && rows <= 4.0 * counted,
            "{rows} against {counted}, planned {}",
            planned[1]
        );
        assert!(planned[1] < 0.2 * counted, "planned {}", planned[1]);
    }

    #[pg_test]
    fn a_partial_surveyor_turns_the_size_on_only_for_questions_inside_its_where() {
        bought();
        let base = "SELECT id FROM bought WHERE";
        // each pair's rows are a tenth of the table, where the planner expects a fiftieth
        let (inside, outside) = ("lane = 1 AND slot = 6", "lane = 2 AND slot = 7");
        let planned = |cond: &str| estimates(&format!("{base} {cond}"), "on bought")[0];
        let before = (planned(inside), planned(outside), planned("slot = 6"));
        Spi::run("CREATE INDEX bought_lane_one ON bought USING surveyor (id) WHERE lane = 1")
            .unwrap();
        let counted = count(&format!("SELECT count(*) FROM bought WHERE {inside}"));
        let rows = planned(inside);
        assert!(
            (rows - counted).abs() <= 2.0 * under_a_leaf("bought_lane_slot") + 1.0,
            "{rows} measured, {counted} counted"
        );
        assert!(before.0 < counted / 2.0, "{before:?}");
        // outside its WHERE, the planner's own estimate
        assert_eq!(planned(outside), before.1);
        assert_eq!(planned("slot = 6"), before.2);
    }

    #[pg_test]
    fn two_surveyors_on_one_table_act_as_one() {
        bought();
        let base = "SELECT id FROM bought WHERE";
        let conditions = [
            "lane = 1 AND slot = 6",
            "lane = 3 AND slot IN (3, 8)",
            "at >= '2021-03-01' AND at < '2021-03-02'",
        ];
        let rows = || -> Vec<f64> {
            conditions
                .iter()
                .map(|c| estimates(&format!("{base} {c}"), "on bought")[0])
                .collect()
        };
        surveyed();
        let one = rows();
        Spi::run(
            "CREATE INDEX bought_order_again ON bought USING surveyor (lane, slot); \
             CREATE INDEX bought_lane_one ON bought USING surveyor (id) WHERE lane = 1",
        )
        .unwrap();
        assert_eq!(rows(), one);
        Spi::run("DROP INDEX bought_order").unwrap();
        assert_eq!(rows(), one);
    }

    #[pg_test]
    fn a_parallel_path_and_a_parameterized_size_take_the_measured_count() {
        bought();
        surveyed();
        Spi::run(
            "SET LOCAL parallel_setup_cost = 0; SET LOCAL parallel_tuple_cost = 0; \
             SET LOCAL min_parallel_table_scan_size = 0; SET LOCAL max_parallel_workers_per_gather = 2; \
             SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off",
        )
        .unwrap();
        let query = "SELECT id FROM bought WHERE lane = 1 AND slot = 6";
        let gathered = estimates(query, "Gather");
        let scanned = estimates(query, "Parallel Seq Scan on bought");
        assert_eq!(
            gathered.len(),
            1,
            "{:?}",
            texts(&format!("EXPLAIN {query}"))
        );
        // two workers and the leader's share
        assert!(
            (scanned[0] * 2.4 - gathered[0]).abs() <= 1.0,
            "{scanned:?} {gathered:?}"
        );
        Spi::run(
            "RESET enable_indexscan; RESET enable_bitmapscan; RESET max_parallel_workers_per_gather; \
             CREATE TABLE picked AS SELECT g * 7 AS day FROM generate_series(1, 5) g; ANALYZE picked; \
             SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; SET LOCAL enable_material = off",
        )
        .unwrap();
        // each lookup of a day takes the day's share of the relation's measured rows
        let joined = "SELECT b.id FROM picked p JOIN bought b ON b.day = p.day WHERE b.lane = 1 AND b.slot = 6";
        let relation = sizes(joined)
            .iter()
            .find(|(n, _, _)| n == "bought")
            .unwrap()
            .2;
        let plan = texts(&format!("EXPLAIN {joined}")).join("\n");
        let inner = estimates(joined, "using bought_day on bought");
        assert_eq!(inner.len(), 1, "{plan}");
        assert!(
            (inner[0] - relation / 600.0).abs() <= 1.0,
            "{} against {relation} over 600 days\n{plan}",
            inner[0]
        );
    }

    #[pg_test]
    fn a_lookup_by_every_column_of_a_unique_key_is_planned_at_one_row_at_most() {
        bought();
        Spi::run("CREATE UNIQUE INDEX bought_id ON bought (id); ANALYZE bought").unwrap();
        surveyed();
        Spi::run(
            "CREATE TABLE picked AS SELECT g * 7 AS id FROM generate_series(1, 5) g; ANALYZE picked; \
             SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; \
             SET LOCAL enable_material = off; SET LOCAL enable_bitmapscan = off",
        )
        .unwrap();
        let joined = "SELECT b.id FROM picked p JOIN bought b ON b.id = p.id \
                      WHERE b.lane = 1 AND b.slot = 6";
        // the relation measured at several times the rows the planner expected
        let relation = sizes(joined)
            .iter()
            .find(|(n, _, _)| n == "bought")
            .unwrap()
            .2;
        assert!(relation > 4000.0, "{relation}");
        let plan = texts(&format!("EXPLAIN {joined}")).join("\n");
        let inner = estimates(joined, "using bought_id on bought");
        assert_eq!(inner, vec![1.0], "{plan}");
    }

    #[pg_test]
    fn a_lookup_compared_under_another_collation_than_its_unique_key_is_not_held_to_one_row() {
        // each name three times over in its case, one row each under the default collation and
        // three under a collation that ignores case
        Spi::run(
            "CREATE COLLATION ignoring_case \
                 (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
             CREATE TABLE named AS SELECT g AS id, (g % 5)::smallint AS lane, \
                    (g % 10)::smallint AS slot, \
                    (ARRAY['brick', 'Brick', 'BRICK'])[1 + g % 3] || (g / 3) AS name \
             FROM generate_series(1, 60000) g; \
             CREATE UNIQUE INDEX named_name ON named (name); \
             CREATE INDEX named_name_any_case ON named (name COLLATE ignoring_case); \
             CREATE INDEX named_lane_slot ON named (lane, slot); \
             CREATE INDEX named_order ON named USING surveyor (id); \
             CREATE TABLE asked AS SELECT 'brick' || (g * 7) AS name FROM generate_series(1, 5) g; \
             ANALYZE named; ANALYZE asked; \
             SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off; \
             SET LOCAL enable_material = off; SET LOCAL enable_bitmapscan = off",
        )
        .unwrap();
        let joined =
            "SELECT n.id FROM asked a JOIN named n ON n.name = a.name COLLATE ignoring_case \
                      WHERE n.lane = 1 AND n.slot = 6";
        assert_eq!(
            count(
                "SELECT count(*) FROM named \
                 WHERE name = 'brick7' COLLATE ignoring_case"
            ),
            3.0
        );
        // the relation measured at several times the rows the planner expected, and each lookup
        // taking its share of them: the unique key, under the default collation, fixes none
        let relation = sizes(joined)
            .iter()
            .find(|(n, _, _)| n == "named")
            .unwrap()
            .2;
        assert!(relation > 4000.0, "{relation}");
        let plan = texts(&format!("EXPLAIN {joined}")).join("\n");
        let inner = estimates(joined, "using named_name_any_case on named");
        assert_eq!(inner.len(), 1, "{plan}");
        assert!(inner[0] > 1.0, "{plan}");
    }

    #[pg_test]
    fn conditions_inside_one_page_keep_the_planners_share_at_most_that_pages_rows() {
        // 600 themes, the 110 roots with no parent: the block of NULLs lies inside the last leaf
        Spi::run(
            "CREATE TABLE kinds AS SELECT g AS id, CASE WHEN g > 490 THEN NULL ELSE g % 37 END AS parent_id \
             FROM generate_series(1, 600) g; \
             CREATE INDEX kinds_parent_id ON kinds (parent_id, id); \
             CREATE INDEX kinds_order ON kinds USING surveyor (id); ANALYZE kinds",
        )
        .unwrap();
        let query = "SELECT id FROM kinds WHERE parent_id IS NULL";
        let counted = count("SELECT count(*) FROM kinds WHERE parent_id IS NULL");
        let rows = estimates(query, "on kinds")[0];
        assert!(
            (rows - counted).abs() <= 0.05 * counted,
            "{rows} against {counted}"
        );
    }

    #[pg_test]
    fn an_inheritance_parent_is_the_sum_of_its_childrens_measured_rows() {
        Spi::run(
            "CREATE TABLE kin (id int, lane smallint, slot smallint); \
             CREATE TABLE kin_a () INHERITS (kin); CREATE TABLE kin_b () INHERITS (kin); \
             INSERT INTO kin SELECT g, g % 5, g % 10 FROM generate_series(1, 10000) g; \
             INSERT INTO kin_a SELECT g, g % 5, g % 10 FROM generate_series(1, 30000) g; \
             INSERT INTO kin_b SELECT g, g % 5, g % 10 FROM generate_series(1, 30000) g; \
             CREATE INDEX kin_ls ON kin (lane, slot); CREATE INDEX kin_order ON kin USING surveyor (id); \
             CREATE INDEX kin_a_ls ON kin_a (lane, slot); CREATE INDEX kin_a_order ON kin_a USING surveyor (id); \
             CREATE INDEX kin_b_ls ON kin_b (lane, slot); \
             ANALYZE kin; ANALYZE kin_a; ANALYZE kin_b",
        )
        .unwrap();
        let query = "SELECT id FROM kin WHERE lane = 1 AND slot = 6";
        let seen = sizes(query);
        let of = |name: &str, inh: bool| {
            seen.iter()
                .filter(|(n, i, _)| n == name && *i == inh)
                .map(|(_, _, r)| *r)
                .next_back()
                .unwrap_or_else(|| panic!("{name}: {seen:?}"))
        };
        let parent = of("kin", true);
        let children = of("kin", false) + of("kin_a", false) + of("kin_b", false);
        assert_eq!(parent, children, "{seen:?}");
        // the two tables carrying a surveyor are measured; the third keeps the planner's product
        for (table, measured) in [("kin", true), ("kin_a", true), ("kin_b", false)] {
            let counted = count(&format!(
                "SELECT count(*) FROM ONLY {table} WHERE lane = 1 AND slot = 6"
            ));
            let rows = of(table, false);
            if measured {
                assert!(
                    (rows - counted).abs() <= 0.05 * counted + 800.0,
                    "{table}: {rows} against {counted}"
                );
            } else {
                assert!(
                    (rows - counted).abs() > 100.0,
                    "{table}: {rows} against {counted}"
                );
            }
        }
        // the plan's Append adds the children's paths
        let appended = estimates(query, "Append")[0];
        assert!(
            (appended - parent).abs() <= 3.0,
            "{appended} against {parent}"
        );
    }
}
