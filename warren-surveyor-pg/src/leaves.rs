// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The statistics the planner asks for a column of a table that carries a surveyor over the whole
//! table. A partial surveyor does not turn them on: the statistics are asked for without the
//! question's conditions, so nothing proves its predicate.
//!
//! Where the planner asks for a column's statistics, through the table or through any of the
//! table's indexes that is not partial, and a valid B-tree over the whole table leads with that
//! column, what the B-tree measures from its pages stands in for ANALYZE's:
//!
//! - the share of NULLs: the B-tree's block of NULLs, measured, counted on its leaves where it
//!   lies inside one page or two; where the read cannot reach them, ANALYZE's share, held to
//!   those pages';
//! - the distinct values, where the column is an integer or a date: every value between the
//!   B-tree's first and last, where they are no more than the column's rows other than NULL;
//! - the most common values, where those values are no more than the statistics target: each
//!   value with its share of the rows, measured, a value inside one leaf or two counted on those
//!   leaves; a value ANALYZE's most common values name keeps the share they give it where ANALYZE
//!   read every row of the table; a value inside one leaf or two that the read cannot reach within
//!   its pages takes the share ANALYZE's statistics give it, held between none and those pages'.
//!
//! Where ANALYZE read every row of the table (`query::every_row_analyzed`), and its most common
//! values and its NULLs hold every row, its statistics are the table's own counts: they are handed
//! as they are, and the B-tree is not read for them.
//!
//! The rest (a histogram, the order against the table's) is ANALYZE's, where it gathered one. Where
//! every value between the first and the last holds rows, the common values' shares are scaled so
//! that with the share of NULLs they sum to 1. Otherwise, where they would sum past 1, the common
//! values' shares are scaled to fit. A share ANALYZE's most common values give stays as it is in
//! both, and the measured shares are scaled around it; only where ANALYZE's shares alone pass what
//! the NULLs leave is every share scaled.
//!
//! PostgreSQL keeps no statistics for a column of a subquery or WITH query that groups by it among
//! other columns, by GROUP BY or DISTINCT. Where that column is a table's column, carried up
//! through subqueries and WITH queries as it is, its distinct values are the same count, and
//! nothing else is said of it; where ANALYZE counted the column from every row, its count.
//!
//! A table read with its inheritance children or its partitions is handed its members' counts
//! added place by place, for rows add over places: the share of NULLs, the most common values
//! where every member counts every value it holds, and the distinct values the members hold
//! between them, leaving out the members the planner ruled out by their constraints or pruned.
//! Where a member holding rows carries no surveyor, has no B-tree its column leads, or cannot be
//! read within its pages, the hierarchy keeps ANALYZE's statistics for it. A security-barrier
//! subquery, a set operation and grouping sets are left to the planner. What is measured is kept
//! until the statement is planned, and read again for the next.
//!
//! These reads draw at most half of the statement's planning-read budget (`budget`), so that the
//! read of the conditions, which the planner asks for after the statistics, keeps the rest. A
//! B-tree's read stops once its pages would pass the pages of the table, or what is left of that
//! half: the block of NULLs first, then the first and last values, then each value between them.
//! What it read before stands, and the rest is ANALYZE's. Then, in the key's order while the pages
//! read stay within both, each value spanning whole leaves that a few of its own leaves cannot
//! stand for (the index stores a column of more than one width, or those leaves hold different
//! rows) is read whole, every entry on every one of its leaves counted; past them, it keeps the
//! count from its own leaves.

use crate::measure::{self, End};
use crate::query::{cells, every_row_analyzed, members, surveyed, surveyor_am};
use crate::round;
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::IntoDatum;
use std::cell::RefCell;
use std::ffi::{c_char, CStr};
use std::ptr::null_mut;

static mut NEXT_RELATION: pg_sys::get_relation_stats_hook_type = None;
static mut NEXT_INDEX: pg_sys::get_index_stats_hook_type = None;

thread_local! {
    /// What each B-tree measured in this round of planning.
    static KEPT: RefCell<Vec<(pg_sys::Oid, Option<Vectors>)>> = const { RefCell::new(Vec::new()) };
}

/// Puts the hooks in place, after any hooks already there.
pub fn init() {
    unsafe {
        NEXT_RELATION = pg_sys::get_relation_stats_hook;
        pg_sys::get_relation_stats_hook = Some(relation_stats);
        NEXT_INDEX = pg_sys::get_index_stats_hook;
        pg_sys::get_index_stats_hook = Some(index_stats);
    }
}

/// Forgets what was measured in the round.
pub(crate) fn forget() {
    KEPT.with(|k| k.borrow_mut().clear());
}

/// What was measured in the round so far, to be put back by `put_back`.
pub(crate) struct Saved(Vec<(pg_sys::Oid, Option<Vectors>)>);

/// What was measured in the round so far.
pub(crate) fn saved() -> Saved {
    Saved(KEPT.with(|k| k.borrow().clone()))
}

/// Puts back what `saved` holds as what was measured in the round.
pub(crate) fn put_back(saved: Saved) {
    KEPT.with(|k| *k.borrow_mut() = saved.0);
}

/// What a B-tree leading with a column measured of it, for a table of `tuples` rows.
#[derive(Clone, Debug)]
struct Vectors {
    tuples: f64,
    /// The B-tree's block of NULLs.
    nulls: measure::Measured,
    /// Every value between the first and the last, where they are counted and no more than the
    /// rows other than NULL.
    distinct: Option<f64>,
    /// Each value between the first and the last as the B-tree measured it, where they are no more
    /// than the statistics target.
    places: Option<Places>,
    /// The first and last values, where every value between them is taken to hold rows.
    ends: Option<(i64, i64)>,
    /// Whether ANALYZE read every row of the table, so that the counts its most common values give
    /// are the table's own.
    exact: bool,
}

#[derive(Clone, Debug)]
struct Places {
    measured: Vec<(i64, measure::Measured)>,
    kind: pg_sys::Oid,
    equality: pg_sys::Oid,
    collation: pg_sys::Oid,
}

/// The most common values handed to the planner: each value holding rows with its share, and
/// whether the share is the one ANALYZE's most common values give it, the largest first.
struct Common {
    values: Vec<(i64, f64, bool)>,
    kind: pg_sys::Oid,
    equality: pg_sys::Oid,
    collation: pg_sys::Oid,
}

/// What a column is to an index: a table's column, or an expression.
enum Lead {
    Column(pg_sys::AttrNumber),
    Expression(*mut pg_sys::Node),
}

#[pg_guard]
unsafe extern "C-unwind" fn relation_stats(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
    attnum: pg_sys::AttrNumber,
    vardata: *mut pg_sys::VariableStatData,
) -> bool {
    if let Some(next) = NEXT_RELATION {
        if next(root, rte, attnum, vardata) {
            return true;
        }
    }
    if root.is_null() || rte.is_null() || attnum <= 0 {
        return false;
    }
    // what is read to answer draws on the statistics' half of the statement's budget
    let _planning = crate::budget::planning(root);
    let _statistics = crate::budget::Statistics::enter();
    match (*rte).rtekind {
        pg_sys::RTEKind::RTE_RELATION if (*rte).inh => parent_column(root, rte, attnum, vardata),
        pg_sys::RTEKind::RTE_RELATION => table_column(root, rte, attnum, vardata),
        pg_sys::RTEKind::RTE_CTE => grouped_column(root, rte, attnum, vardata),
        pg_sys::RTEKind::RTE_SUBQUERY => grouped_column(root, rte, attnum, vardata),
        _ => false,
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn index_stats(
    root: *mut pg_sys::PlannerInfo,
    index: pg_sys::Oid,
    column: pg_sys::AttrNumber,
    vardata: *mut pg_sys::VariableStatData,
) -> bool {
    if let Some(next) = NEXT_INDEX {
        if next(root, index, column, vardata) {
            return true;
        }
    }
    if root.is_null() || column < 1 {
        return false;
    }
    // what is read to answer draws on the statistics' half of the statement's budget
    let _planning = crate::budget::planning(root);
    let _statistics = crate::budget::Statistics::enter();
    let am = surveyor_am();
    if am == pg_sys::InvalidOid {
        return false;
    }
    let Some((rel, asked)) = planner_index(root, index) else {
        return false;
    };
    // a partial index's statistics are not the table's
    if !(*asked).indpred.is_null() || !surveyed(rel, false) || column as i32 > (*asked).ncolumns {
        return false;
    }
    let Some(lead) = lead_of(asked, column as usize - 1) else {
        return false;
    };
    let Some(btree) = leading_btree(rel, &lead) else {
        return false;
    };
    let mut gathered = gathered_statistics(index, column);
    if gathered.is_null() && (*btree).indexoid != index {
        gathered = gathered_statistics((*btree).indexoid, 1);
    }
    // what ANALYZE counted from every row of the table, every value named, stands as it is
    let table = (**(*root).simple_rte_array.add((*rel).relid as usize)).relid;
    let counted = every_row_analyzed(table) && counted_whole(gathered, (*rel).tuples);
    let read = if counted { None } else { vectors(rel, btree) };
    let Some(read) = read else {
        if !gathered.is_null() {
            pg_sys::ReleaseSysCache(gathered);
        }
        return false;
    };
    let readable = pg_sys::all_rows_selectable(root, (*rel).relid, null_mut());
    let tuple = statistics(gathered, rel, index, column, &read, readable);
    (*vardata).acl_ok = if gathered.is_null() && read.places.is_none() {
        // nothing but the counts, which carry no value of the table
        true
    } else {
        readable
    };
    if !gathered.is_null() {
        pg_sys::ReleaseSysCache(gathered);
    }
    (*vardata).statsTuple = tuple;
    (*vardata).freefunc = Some(free_statistics);
    true
}

/// Statistics for column `attnum` of the table `rte` reads, where a B-tree of the table leads with
/// it and the table carries a surveyor over the whole table.
unsafe fn table_column(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
    attnum: pg_sys::AttrNumber,
    vardata: *mut pg_sys::VariableStatData,
) -> bool {
    if (*rte).inh {
        return false;
    }
    let am = surveyor_am();
    if am == pg_sys::InvalidOid {
        return false;
    }
    let Some(varno) = varno_of(root, rte) else {
        return false;
    };
    let Some(rel) = planner_rel(root, varno) else {
        return false;
    };
    if !surveyed(rel, false) {
        return false;
    }
    let Some(btree) = leading_btree(rel, &Lead::Column(attnum)) else {
        return false;
    };
    let gathered = gathered_statistics((*rte).relid, attnum);
    // what ANALYZE counted from every row of the table, every value named, stands as it is
    let counted = every_row_analyzed((*rte).relid) && counted_whole(gathered, (*rel).tuples);
    let read = if counted { None } else { vectors(rel, btree) };
    let Some(read) = read else {
        if !gathered.is_null() {
            pg_sys::ReleaseSysCache(gathered);
        }
        return false;
    };
    let readable = pg_sys::all_rows_selectable(
        root,
        varno,
        pg_sys::bms_make_singleton(attnum as i32 - pg_sys::FirstLowInvalidHeapAttributeNumber),
    );
    let tuple = statistics(gathered, rel, (*rte).relid, attnum, &read, readable);
    (*vardata).acl_ok = if gathered.is_null() && read.places.is_none() {
        // nothing but the counts, which carry no value of the table
        true
    } else {
        readable
    };
    if !gathered.is_null() {
        pg_sys::ReleaseSysCache(gathered);
    }
    (*vardata).statsTuple = tuple;
    (*vardata).freefunc = Some(free_statistics);
    true
}

/// What one table holds of a column, as its own surveyor counts it for the statistics it hands: its
/// rows, its NULLs' rows, each value's rows where every value it holds is counted, with whether the
/// count is ANALYZE's, and its first and last values where every value between them is taken to
/// hold rows.
struct Vector {
    rows: f64,
    nulls: f64,
    values: Option<Vec<(i64, f64, bool)>>,
    ends: Option<(i64, i64)>,
}

/// What the table at `varno` of `root`'s range table holds of its column `attnum`, counted as its
/// own statistics are: a table holding no row holds nothing, and needs no read. None where it
/// carries no surveyor, no B-tree leads with the column, or the read cannot be made within its
/// pages.
unsafe fn vector_of(
    root: *mut pg_sys::PlannerInfo,
    varno: pg_sys::Index,
    rel: *mut pg_sys::RelOptInfo,
    attnum: pg_sys::AttrNumber,
) -> Option<Vector> {
    let tuples = (*rel).tuples;
    if tuples <= 0.0 {
        return Some(Vector {
            rows: 0.0,
            nulls: 0.0,
            values: Some(Vec::new()),
            ends: None,
        });
    }
    if !surveyed(rel, false) {
        return None;
    }
    let btree = leading_btree(rel, &Lead::Column(attnum))?;
    let relid = (**(*root).simple_rte_array.add(varno as usize)).relid;
    let countable = measure::countable(*(*btree).opcintype);
    let gathered = gathered_statistics(relid, attnum);
    let vector = if every_row_analyzed(relid) && counted_whole(gathered, tuples) {
        // ANALYZE's own counts, every value named
        Some(Vector {
            rows: tuples,
            nulls: (*form_of(gathered)).stanullfrac as f64 * tuples,
            values: countable.then(|| {
                listed_values(gathered)
                    .into_iter()
                    .map(|(v, share)| (v, share * tuples, true))
                    .collect()
            }),
            ends: None,
        })
    } else {
        vectors(rel, btree).map(|read| {
            let analyzed = if gathered.is_null() {
                0.0
            } else {
                (*form_of(gathered)).stanullfrac as f64
            };
            let readable = pg_sys::all_rows_selectable(
                root,
                varno,
                pg_sys::bms_make_singleton(
                    attnum as i32 - pg_sys::FirstLowInvalidHeapAttributeNumber,
                ),
            );
            Vector {
                rows: tuples,
                nulls: read
                    .nulls
                    .rows_within(tuples, || Some(analyzed))
                    .unwrap_or(0.0)
                    .clamp(0.0, tuples),
                values: common_values(gathered, rel, &read, readable).map(|common| {
                    common
                        .values
                        .iter()
                        .map(|&(v, share, named)| (v, share * tuples, named))
                        .collect()
                }),
                ends: read.ends,
            }
        })
    };
    if !gathered.is_null() {
        pg_sys::ReleaseSysCache(gathered);
    }
    vector
}

/// The values the vectors `held` hold between them: each value counted, and every value between
/// the first and last of a vector whose values are not counted. None where a vector holding rows
/// gives neither.
fn distinct_of(held: &[Vector]) -> Option<f64> {
    let mut spans: Vec<(i64, i64)> = Vec::new();
    let mut points: Vec<i64> = Vec::new();
    for h in held.iter().filter(|h| h.rows > 0.0) {
        match (&h.values, h.ends) {
            (Some(values), _) => points.extend(values.iter().filter(|v| v.1 > 0.0).map(|v| v.0)),
            (None, Some(ends)) => spans.push(ends),
            (None, None) => return None,
        }
    }
    spans.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (first, last) in spans {
        match merged.last_mut() {
            Some(m) if first <= m.1.saturating_add(1) => m.1 = m.1.max(last),
            _ => merged.push((first, last)),
        }
    }
    points.sort();
    points.dedup();
    let between: i128 = merged
        .iter()
        .map(|&(first, last)| values_from(first, last))
        .sum();
    let apart = points
        .iter()
        .filter(|&&p| !merged.iter().any(|&(first, last)| first <= p && p <= last))
        .count() as i128;
    Some((between + apart) as f64)
}

/// The values from `first` to `last`, both counted, in a type wide enough to count every value of
/// a bigint.
fn values_from(first: i64, last: i64) -> i128 {
    i128::from(last) - i128::from(first) + 1
}

/// Statistics for column `attnum` of the table `rte` reads with its inheritance children or its
/// partitions, where every member it reads carries a surveyor over the whole member and a B-tree
/// led by its own column for it: the members' counts added place by place, for rows add over
/// places. The share of NULLs is the members' NULLs over their rows; the most common values, where
/// every member counts every value it holds, each value's rows added over the members; the
/// distinct values, those the members hold between them. A member the planner ruled out by its
/// constraints, or pruned, is left out. The rest, and anything a member cannot count, is
/// ANALYZE's for the whole hierarchy, where it gathered one.
unsafe fn parent_column(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
    attnum: pg_sys::AttrNumber,
    vardata: *mut pg_sys::VariableStatData,
) -> bool {
    if surveyor_am() == pg_sys::InvalidOid {
        return false;
    }
    let Some(varno) = varno_of(root, rte) else {
        return false;
    };
    let members = members(root, varno);
    if members.is_empty() {
        return false;
    }
    let mut held = Vec::with_capacity(members.len());
    for m in &members {
        let Some(column) = m.column(attnum) else {
            return false;
        };
        let Some(vector) = vector_of(root, m.varno, m.rel, column) else {
            return false;
        };
        held.push(vector);
    }
    let rows: f64 = held.iter().map(|h| h.rows).sum();
    if rows <= 0.0 {
        return false;
    }
    let nulls = (held.iter().map(|h| h.nulls).sum::<f64>() / rows).clamp(0.0, 1.0);
    // each value's rows added over the members, where every member holding rows counts every
    // value it holds; a share stays ANALYZE's only where every member's count of it is
    let values = held
        .iter()
        .filter(|h| h.rows > 0.0)
        .all(|h| h.values.is_some())
        .then(|| {
            let mut added: Vec<(i64, f64, bool)> = Vec::new();
            for &(v, n, named) in held.iter().filter_map(|h| h.values.as_ref()).flatten() {
                match added.iter_mut().find(|a| a.0 == v) {
                    Some(a) => {
                        a.1 += n;
                        a.2 &= named;
                    }
                    None => added.push((v, n, named)),
                }
            }
            added.retain(|a| a.1 > 0.0);
            added.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            added
        });
    let distinct = distinct_of(&held);
    let relid = (*rte).relid;
    let gathered = pg_sys::SearchSysCache3(
        pg_sys::SysCacheIdentifier::STATRELATTINH as i32,
        pg_sys::Datum::from(relid),
        pg_sys::Datum::from(attnum as i32),
        pg_sys::Datum::from(true),
    );
    let mut tuple = if gathered.is_null() {
        formed(relid, attnum)
    } else {
        pg_sys::heap_copytuple(gathered)
    };
    let mut listed = None;
    if let Some(values) = values.filter(|v| !v.is_empty()) {
        let (mut kind, mut typmod, mut collation) = (pg_sys::InvalidOid, -1, pg_sys::InvalidOid);
        pg_sys::get_atttypetypmodcoll(relid, attnum, &mut kind, &mut typmod, &mut collation);
        let equality = (*pg_sys::lookup_type_cache(kind, pg_sys::TYPECACHE_EQ_OPR as i32)).eq_opr;
        if equality != pg_sys::InvalidOid {
            let mut shares: Vec<f64> = values.iter().map(|v| v.1 / rows).collect();
            let named: Vec<bool> = values.iter().map(|v| v.2).collect();
            let places = match (
                values.iter().map(|v| v.0).min(),
                values.iter().map(|v| v.0).max(),
            ) {
                (Some(first), Some(last)) => {
                    usize::try_from(values_from(first, last)).unwrap_or(usize::MAX)
                }
                _ => 0,
            };
            whole(&mut shares, &named, places, nulls);
            let common = Common {
                values: values
                    .iter()
                    .zip(&shares)
                    .map(|(v, &share)| (v.0, share, v.2))
                    .collect(),
                kind,
                equality,
                collation,
            };
            tuple = with_common(tuple, Some(&common), &shares);
            listed = Some(named);
        }
    }
    // the shares of NULLs and of the common values sum to at most 1
    let mut shares = common_shares(tuple);
    let sum: f64 = shares.iter().sum();
    if sum > 0.0 && sum + nulls > 1.0 {
        let named = listed
            .clone()
            .filter(|n| n.len() == shares.len())
            .unwrap_or_else(|| vec![true; shares.len()]);
        fill(&mut shares, &named, (1.0 - nulls).max(0.0));
        tuple = with_common(tuple, None, &shares);
    }
    let form = form_of(tuple);
    (*form).stanullfrac = nulls as f32;
    if let Some(distinct) = distinct {
        (*form).stadistinct = distinct as f32;
    }
    (*vardata).acl_ok = if gathered.is_null() && listed.is_none() {
        // nothing but the counts, which carry no value of the table
        true
    } else {
        pg_sys::all_rows_selectable(
            root,
            varno,
            pg_sys::bms_make_singleton(attnum as i32 - pg_sys::FirstLowInvalidHeapAttributeNumber),
        )
    };
    if !gathered.is_null() {
        pg_sys::ReleaseSysCache(gathered);
    }
    (*vardata).statsTuple = tuple;
    (*vardata).freefunc = Some(free_statistics);
    true
}

/// Statistics for output column `attnum` of the subquery or WITH query `rte` reads, where that
/// query groups by it among other columns (by GROUP BY, or by DISTINCT) and it is a table's column
/// carried up as it is, which a B-tree of a table carrying a surveyor leads with and whose values
/// are counted: that count, and nothing else.
unsafe fn grouped_column(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
    attnum: pg_sys::AttrNumber,
    vardata: *mut pg_sys::VariableStatData,
) -> bool {
    if (*rte).security_barrier {
        return false;
    }
    let am = surveyor_am();
    if am == pg_sys::InvalidOid {
        return false;
    }
    let Some(varno) = varno_of(root, rte) else {
        return false;
    };
    let Some(sub) = sub_root(root, rte, varno) else {
        return false;
    };
    let Some(target) = output(sub, attnum) else {
        return false;
    };
    let q = (*sub).parse;
    let clause = if !(*q).distinctClause.is_null() {
        (*q).distinctClause
    } else {
        (*q).groupClause
    };
    // one grouping column alone is unique, as the planner already reads it
    if clause.is_null() || (*clause).length < 2 || !grouped_by(target, clause) {
        return false;
    }
    let Some((owner, at, relid, column)) =
        table_column_of(sub, (*target).expr as *mut pg_sys::Node)
    else {
        return false;
    };
    let Some(rel) = planner_rel(owner, at) else {
        return false;
    };
    if !surveyed(rel, false) {
        return false;
    }
    let Some(btree) = leading_btree(rel, &Lead::Column(column)) else {
        return false;
    };
    // what ANALYZE counted from every row of the table, every value named: its own count
    let gathered = gathered_statistics(relid, column);
    let rows = (*rel).tuples;
    let analyzed = (every_row_analyzed(relid) && counted_whole(gathered, rows)).then(|| {
        let distinct = (*form_of(gathered)).stadistinct as f64;
        if distinct < 0.0 {
            (-distinct * rows).round()
        } else {
            distinct
        }
    });
    if !gathered.is_null() {
        pg_sys::ReleaseSysCache(gathered);
    }
    let Some(distinct) = analyzed.or_else(|| vectors(rel, btree).and_then(|v| v.distinct)) else {
        return false;
    };
    let tuple = formed(relid, column);
    let form = form_of(tuple);
    (*form).stadistinct = distinct as f32;
    (*vardata).statsTuple = tuple;
    (*vardata).freefunc = Some(free_statistics);
    // nothing but the count, which carries no value of the table
    (*vardata).acl_ok = true;
    true
}

/// Frees a statistics tuple made here, when the planner is done with it.
#[pg_guard]
unsafe extern "C-unwind" fn free_statistics(tuple: pg_sys::HeapTuple) {
    pg_sys::heap_freetuple(tuple);
}

/// The planner's relation at `varno` of `root`'s range table.
unsafe fn planner_rel(
    root: *mut pg_sys::PlannerInfo,
    varno: pg_sys::Index,
) -> Option<*mut pg_sys::RelOptInfo> {
    if (*root).simple_rel_array.is_null() || varno as i32 >= (*root).simple_rel_array_size {
        return None;
    }
    let rel = *(*root).simple_rel_array.add(varno as usize);
    (!rel.is_null()).then_some(rel)
}

/// The planner's relation and its entry for the index `index`, among `root`'s relations.
unsafe fn planner_index(
    root: *mut pg_sys::PlannerInfo,
    index: pg_sys::Oid,
) -> Option<(*mut pg_sys::RelOptInfo, *mut pg_sys::IndexOptInfo)> {
    if (*root).simple_rel_array.is_null() {
        return None;
    }
    for at in 1..(*root).simple_rel_array_size as usize {
        let rel = *(*root).simple_rel_array.add(at);
        if rel.is_null() {
            continue;
        }
        for i in cells((*rel).indexlist) {
            let i = i as *mut pg_sys::IndexOptInfo;
            if (*i).indexoid == index {
                return Some((rel, i));
            }
        }
    }
    None
}

/// What column `at` (from 0) of `index` is.
unsafe fn lead_of(index: *mut pg_sys::IndexOptInfo, at: usize) -> Option<Lead> {
    let key = *(*index).indexkeys.add(at);
    if key != 0 {
        return Some(Lead::Column(key as pg_sys::AttrNumber));
    }
    let before = (0..at).filter(|&i| *(*index).indexkeys.add(i) == 0).count();
    cells((*index).indexprs)
        .get(before)
        .map(|&e| Lead::Expression(crate::query::bare(e as *mut pg_sys::Node)))
}

/// A valid B-tree over the whole of `rel` whose leading column is `lead`, the asked one first.
unsafe fn leading_btree(
    rel: *mut pg_sys::RelOptInfo,
    lead: &Lead,
) -> Option<*mut pg_sys::IndexOptInfo> {
    cells((*rel).indexlist)
        .into_iter()
        .map(|i| i as *mut pg_sys::IndexOptInfo)
        .find(|&i| {
            (*i).relam == pg_sys::BTREE_AM_OID
                && (*i).indpred.is_null()
                && (*i).nkeycolumns >= 1
                && match (lead, lead_of(i, 0)) {
                    (Lead::Column(a), Some(Lead::Column(b))) => *a == b,
                    (Lead::Expression(a), Some(Lead::Expression(b))) => {
                        pg_sys::equal(*a as *const std::ffi::c_void, b as *const std::ffi::c_void)
                    }
                    _ => false,
                }
        })
}

/// What `btree`, an index of `rel`, measures of its leading column, read once while a statement
/// is planned.
unsafe fn vectors(
    rel: *mut pg_sys::RelOptInfo,
    btree: *mut pg_sys::IndexOptInfo,
) -> Option<Vectors> {
    let oid = (*btree).indexoid;
    let keep = round::depth() > 0;
    if keep {
        if let Some(kept) = KEPT.with(|k| {
            k.borrow()
                .iter()
                .find(|(o, _)| *o == oid)
                .map(|(_, v)| v.clone())
        }) {
            return kept;
        }
    }
    let tuples = (*rel).tuples;
    let read = if tuples > 0.0 {
        let index = pg_sys::index_open(oid, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        // where ANALYZE read every row, the values its most common values name are counted
        let table = (*(*index).rd_index).indrelid;
        let exact = every_row_analyzed(table);
        let named = if exact {
            let key = *(*btree).indexkeys;
            let gathered = if key != 0 {
                gathered_statistics(table, key as pg_sys::AttrNumber)
            } else {
                gathered_statistics(oid, 1)
            };
            let values = listed_values(gathered).iter().map(|l| l.0).collect();
            if !gathered.is_null() {
                pg_sys::ReleaseSysCache(gathered);
            }
            values
        } else {
            Vec::new()
        };
        #[cfg(any(test, feature = "pg_test"))]
        tests::note_read(oid);
        let read = read_vectors(index, btree, tuples, (*rel).pages, &named).map(|mut read| {
            read.exact = exact;
            read
        });
        pg_sys::index_close(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        read
    } else {
        None
    };
    if keep {
        KEPT.with(|k| k.borrow_mut().push((oid, read.clone())));
    }
    read
}

/// The measurement of the leading column of the B-tree `index`, for a table of `tuples` rows and
/// `most` pages: the block of NULLs, then the first and last values, then each value between
/// them, each read only while the pages read stay within `most`, and what was read kept. The
/// values in `named`, whose counts ANALYZE has, are not read whole. None where the block of NULLs
/// cannot be read within them.
unsafe fn read_vectors(
    index: pg_sys::Relation,
    btree: *mut pg_sys::IndexOptInfo,
    tuples: f64,
    most: u32,
    named: &[i64],
) -> Option<Vectors> {
    let block = |after| End {
        parts: vec![None],
        after,
    };
    let nulls = measure::rows_at(index, &block(false), &block(true), tuples, true, most)?;
    let mut used = nulls.pages;
    let kind = *(*btree).opcintype;
    let stored = (*pg_sys::TupleDescAttr((*index).rd_att, 0)).atttypid;
    let mut read = Vectors {
        tuples,
        nulls,
        distinct: None,
        places: None,
        ends: None,
        exact: false,
    };
    if !measure::countable(kind) || stored != kind {
        return Some(read);
    }
    let Some((first, last, read_now)) = measure::ends(index, most - used) else {
        return Some(read);
    };
    used += read_now;
    let places = values_from(first, last) as f64;
    let measured_nulls = if nulls.bracketed() {
        0.0
    } else {
        (nulls.rows / tuples).min(1.0)
    };
    if places <= tuples * (1.0 - measured_nulls) {
        read.distinct = Some(places);
        read.ends = Some((first, last));
    }
    if places > pg_sys::default_statistics_target as f64 {
        return Some(read);
    }
    let equality = pg_sys::get_opfamily_member(
        *(*btree).opfamily,
        kind,
        kind,
        pg_sys::BTEqualStrategyNumber as i16,
    );
    if equality == pg_sys::InvalidOid {
        return Some(read);
    }
    let mut measured = Vec::new();
    for v in first..=last {
        let part = Some((measure::datum_of(v, kind), kind));
        let lower = End {
            parts: vec![part],
            after: false,
        };
        let upper = End {
            parts: vec![part],
            after: true,
        };
        let count = !named.contains(&v);
        let Some(m) = measure::rows_at(index, &lower, &upper, tuples, count, most - used) else {
            return Some(read);
        };
        used += m.pages;
        measured.push((v, m));
    }
    // the values whose leaves the leaves read cannot stand for, each read whole while the pages
    // read stay within `most` and what is left of the budget
    for (v, m) in measured.iter_mut() {
        let left = most.saturating_sub(used);
        if !m.uneven
            || m.bracketed()
            || named.contains(v)
            || m.whole_pages() > left as f64
            || !crate::budget::fits((*index).rd_id, m.whole_pages())
        {
            continue;
        }
        let part = Some((measure::datum_of(*v, kind), kind));
        let lower = End {
            parts: vec![part],
            after: false,
        };
        let upper = End {
            parts: vec![part],
            after: true,
        };
        let (counted, read_now) =
            measure::every_leaf(index, &lower, &upper, left, &mut |_, _, _| {});
        used += read_now;
        if let Some((counted, on)) = counted {
            m.rows = counted;
            m.leaves = on;
            m.uneven = false;
            m.counted = true;
        }
    }
    read.places = Some(Places {
        measured,
        kind,
        equality,
        collation: *(*btree).indexcollations,
    });
    Some(read)
}

/// The share of the rows of `rel` the statistics `gathered` give the value `v` of `places`' type,
/// as the planner reads them for an equality; with none gathered, the planner's default.
unsafe fn gathered_share(
    gathered: pg_sys::HeapTuple,
    rel: *mut pg_sys::RelOptInfo,
    places: &Places,
    v: i64,
) -> f64 {
    let mut vardata = pg_sys::VariableStatData {
        rel,
        statsTuple: gathered,
        vartype: places.kind,
        atttype: places.kind,
        atttypmod: -1,
        acl_ok: true,
        ..Default::default()
    };
    pg_sys::var_eq_const(
        &mut vardata,
        places.equality,
        places.collation,
        measure::datum_of(v, places.kind),
        false,
        true,
        false,
    )
}

/// The share of the rows the statistics `gathered` give `value`, where their most common values
/// name it, compared by the operator `equality` under `collation` as the planner compares a
/// constant with them, and read only where the planner would read them: where every row of the
/// column may be read (`readable`), or the operator is leakproof. None where they name no such
/// value.
pub(crate) unsafe fn listed_share(
    gathered: pg_sys::HeapTuple,
    equality: pg_sys::Oid,
    collation: pg_sys::Oid,
    value: pg_sys::Datum,
    readable: bool,
) -> Option<f64> {
    if gathered.is_null() {
        return None;
    }
    let function = pg_sys::get_opcode(equality);
    let mut vardata = pg_sys::VariableStatData {
        statsTuple: gathered,
        acl_ok: readable,
        ..Default::default()
    };
    if !pg_sys::statistic_proc_security_check(&mut vardata, function) {
        return None;
    }
    let mut slot = pg_sys::AttStatsSlot::default();
    if !pg_sys::get_attstatsslot(
        &mut slot,
        gathered,
        pg_sys::STATISTIC_KIND_MCV as i32,
        pg_sys::InvalidOid,
        (pg_sys::ATTSTATSSLOT_VALUES | pg_sys::ATTSTATSSLOT_NUMBERS) as i32,
    ) {
        return None;
    }
    let mut compare: pg_sys::FmgrInfo = std::mem::zeroed();
    pg_sys::fmgr_info(function, &mut compare);
    let mut share = None;
    for i in 0..(slot.nvalues.min(slot.nnumbers)) as usize {
        let equal = pg_sys::FunctionCall2Coll(&mut compare, collation, *slot.values.add(i), value);
        if equal.value() != 0 {
            share = Some(*slot.numbers.add(i) as f64);
            break;
        }
    }
    pg_sys::free_attstatsslot(&mut slot);
    share
}

/// The most common values of what `read` measured: each value's rows; where the statistics
/// `gathered` name a value among their most common values, the share they give it, wherever
/// ANALYZE read every row, and otherwise where the value lies inside one leaf or two that the read
/// could not reach; where such a value is not named, the share they give it, held to those pages'
/// rows. The values holding rows, the largest first. `readable` is whether every row of the column
/// may be read.
unsafe fn common_values(
    gathered: pg_sys::HeapTuple,
    rel: *mut pg_sys::RelOptInfo,
    read: &Vectors,
    readable: bool,
) -> Option<Common> {
    let places = read.places.as_ref()?;
    let mut values = Vec::new();
    for &(v, m) in &places.measured {
        let named = if read.exact || m.only_bracketed() {
            listed_share(
                gathered,
                places.equality,
                places.collation,
                measure::datum_of(v, places.kind),
                readable,
            )
        } else {
            None
        };
        let rows = match named {
            Some(share) => share * read.tuples,
            None => m
                .rows_within(read.tuples, || {
                    Some(gathered_share(gathered, rel, places, v))
                })
                .unwrap_or(0.0),
        };
        if rows > 0.0 {
            values.push((v, rows / read.tuples, named.is_some()));
        }
    }
    values.sort_by(|a, b| b.1.total_cmp(&a.1));
    Some(Common {
        values,
        kind: places.kind,
        equality: places.equality,
        collation: places.collation,
    })
}

/// What ANALYZE gathered for column `attnum` of relation `relid`, alone and not with inheritance
/// children, from the catalog's cache; null when it gathered nothing.
pub(crate) unsafe fn gathered_statistics(
    relid: pg_sys::Oid,
    attnum: pg_sys::AttrNumber,
) -> pg_sys::HeapTuple {
    pg_sys::SearchSysCache3(
        pg_sys::SysCacheIdentifier::STATRELATTINH as i32,
        pg_sys::Datum::from(relid),
        pg_sys::Datum::from(attnum as i32),
        pg_sys::Datum::from(false),
    )
}

/// Whether the statistics `gathered`, for a table of `rows` rows, name every row: their share of
/// NULLs and their most common values' shares hold every row of the table.
unsafe fn counted_whole(gathered: pg_sys::HeapTuple, rows: f64) -> bool {
    if gathered.is_null() || rows <= 0.0 {
        return false;
    }
    let nulls = (*form_of(gathered)).stanullfrac as f64;
    let mut slot = pg_sys::AttStatsSlot::default();
    let listed = if pg_sys::get_attstatsslot(
        &mut slot,
        gathered,
        pg_sys::STATISTIC_KIND_MCV as i32,
        pg_sys::InvalidOid,
        pg_sys::ATTSTATSSLOT_NUMBERS as i32,
    ) {
        let sum = (0..slot.nnumbers as usize)
            .map(|i| *slot.numbers.add(i) as f64)
            .sum::<f64>();
        pg_sys::free_attstatsslot(&mut slot);
        sum
    } else {
        0.0
    };
    nulls + listed >= 1.0 - 0.5 / rows
}

/// The values of a countable type the most common values of the statistics `gathered` name, each
/// with the share they give it.
unsafe fn listed_values(gathered: pg_sys::HeapTuple) -> Vec<(i64, f64)> {
    if gathered.is_null() {
        return Vec::new();
    }
    let mut slot = pg_sys::AttStatsSlot::default();
    if !pg_sys::get_attstatsslot(
        &mut slot,
        gathered,
        pg_sys::STATISTIC_KIND_MCV as i32,
        pg_sys::InvalidOid,
        (pg_sys::ATTSTATSSLOT_VALUES | pg_sys::ATTSTATSSLOT_NUMBERS) as i32,
    ) {
        return Vec::new();
    }
    let values = (0..(slot.nvalues.min(slot.nnumbers)) as usize)
        .filter_map(|i| {
            measure::whole(*slot.values.add(i), slot.valuetype)
                .map(|v| (v, *slot.numbers.add(i) as f64))
        })
        .collect();
    pg_sys::free_attstatsslot(&mut slot);
    values
}

unsafe fn form_of(tuple: pg_sys::HeapTuple) -> *mut pg_sys::FormData_pg_statistic {
    let header = (*tuple).t_data;
    (header as *mut u8).add((*header).t_hoff as usize) as *mut pg_sys::FormData_pg_statistic
}

/// The statistics tuple for column `attnum` of relation `relid`, read as the planner's `rel`: a
/// copy of `gathered` where ANALYZE gathered one, else one holding nothing but what `read`
/// measured, with `read`'s measurements in place of ANALYZE's. `readable` is whether every row of
/// the column may be read.
unsafe fn statistics(
    gathered: pg_sys::HeapTuple,
    rel: *mut pg_sys::RelOptInfo,
    relid: pg_sys::Oid,
    attnum: pg_sys::AttrNumber,
    read: &Vectors,
    readable: bool,
) -> pg_sys::HeapTuple {
    let mut tuple = if gathered.is_null() {
        formed(relid, attnum)
    } else {
        pg_sys::heap_copytuple(gathered)
    };
    let analyzed = (*form_of(tuple)).stanullfrac as f64;
    let nulls = (read
        .nulls
        .rows_within(read.tuples, || Some(analyzed))
        .unwrap_or(0.0)
        / read.tuples)
        .clamp(0.0, 1.0);
    // which common values keep the share ANALYZE's most common values give them: where they are
    // ANALYZE's own, every one
    let mut listed = None;
    if let Some(common) = common_values(gathered, rel, read, readable) {
        let mut shares: Vec<f64> = common.values.iter().map(|v| v.1).collect();
        let named: Vec<bool> = common.values.iter().map(|v| v.2).collect();
        let places = read.places.as_ref().map_or(0, |p| p.measured.len());
        whole(&mut shares, &named, places, nulls);
        tuple = with_common(tuple, Some(&common), &shares);
        listed = Some(named);
    }
    // the shares of NULLs and of the common values sum to at most 1
    let mut shares = common_shares(tuple);
    let sum: f64 = shares.iter().sum();
    if sum > 0.0 && sum + nulls > 1.0 {
        let named = listed
            .filter(|n| n.len() == shares.len())
            .unwrap_or_else(|| vec![true; shares.len()]);
        fill(&mut shares, &named, (1.0 - nulls).max(0.0));
        tuple = with_common(tuple, None, &shares);
    }
    let form = form_of(tuple);
    (*form).stanullfrac = nulls as f32;
    if let Some(distinct) = read.distinct {
        (*form).stadistinct = distinct as f32;
    }
    tuple
}

/// Scales `shares`, the common values' shares, so that with the share of NULLs `nulls` they sum to
/// 1, where they are every one of the `places` values between the first and the last. The shares
/// ANALYZE's most common values gave (`listed`) stay as they are.
fn whole(shares: &mut [f64], listed: &[bool], places: usize, nulls: f64) {
    let sum: f64 = shares.iter().sum();
    if shares.len() == places && sum > 0.0 {
        fill(shares, listed, (1.0 - nulls).max(0.0));
    }
}

/// Scales the shares among `shares` that are not ANALYZE's (`listed`) so that all of them sum to
/// `room`, ANALYZE's as they are. Where ANALYZE's alone pass `room`, every share is scaled to it.
fn fill(shares: &mut [f64], listed: &[bool], room: f64) {
    let (mut kept, mut measured) = (0.0, 0.0);
    for (share, &named) in shares.iter().zip(listed) {
        if named {
            kept += share;
        } else {
            measured += share;
        }
    }
    if kept > room {
        let fit = room / (kept + measured);
        shares.iter_mut().for_each(|s| *s *= fit);
    } else if measured > 0.0 {
        let fit = (room - kept) / measured;
        for (share, &named) in shares.iter_mut().zip(listed) {
            if !named {
                *share *= fit;
            }
        }
    }
}

/// The kinds of statistics the five slots of a statistics tuple hold.
unsafe fn kinds(tuple: pg_sys::HeapTuple) -> [i16; 5] {
    let f = form_of(tuple);
    [
        (*f).stakind1,
        (*f).stakind2,
        (*f).stakind3,
        (*f).stakind4,
        (*f).stakind5,
    ]
}

/// The shares of the most common values a statistics tuple holds.
unsafe fn common_shares(tuple: pg_sys::HeapTuple) -> Vec<f64> {
    let mut slot = pg_sys::AttStatsSlot::default();
    if !pg_sys::get_attstatsslot(
        &mut slot,
        tuple,
        pg_sys::STATISTIC_KIND_MCV as i32,
        pg_sys::InvalidOid,
        pg_sys::ATTSTATSSLOT_NUMBERS as i32,
    ) {
        return Vec::new();
    }
    let shares = (0..slot.nnumbers as usize)
        .map(|i| *slot.numbers.add(i) as f64)
        .collect();
    pg_sys::free_attstatsslot(&mut slot);
    shares
}

/// `tuple` with its most common values' slot holding `shares`, and where `common` is given, its
/// values; the slot is the one already holding them, or the first free one. `tuple` is freed.
unsafe fn with_common(
    tuple: pg_sys::HeapTuple,
    common: Option<&Common>,
    shares: &[f64],
) -> pg_sys::HeapTuple {
    let held = kinds(tuple);
    let mcv = pg_sys::STATISTIC_KIND_MCV as i16;
    let Some(slot) = held
        .iter()
        .position(|&k| k == mcv)
        .or_else(|| held.iter().position(|&k| k == 0))
    else {
        return tuple;
    };
    let catalog = pg_sys::table_open(
        pg_sys::StatisticRelationId,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let desc = (*catalog).rd_att;
    let columns = (*desc).natts as usize;
    let mut values = vec![pg_sys::Datum::from(0); columns];
    let mut nulls = vec![false; columns];
    let mut replace = vec![false; columns];
    let mut set = |attnum: u32, value: pg_sys::Datum| {
        let at = attnum as usize - 1 + slot;
        values[at] = value;
        replace[at] = true;
    };
    let mut numbers: Vec<pg_sys::Datum> = shares
        .iter()
        .map(|&s| (s as f32).into_datum().expect("a share is a value"))
        .collect();
    let numbers = pg_sys::construct_array_builtin(
        numbers.as_mut_ptr(),
        numbers.len() as i32,
        pg_sys::FLOAT4OID,
    );
    set(
        pg_sys::Anum_pg_statistic_stanumbers1,
        pg_sys::Datum::from(numbers),
    );
    if let Some(common) = common {
        let (mut len, mut byval, mut align) = (0i16, false, 0 as c_char);
        pg_sys::get_typlenbyvalalign(common.kind, &mut len, &mut byval, &mut align);
        let mut elems: Vec<pg_sys::Datum> = common
            .values
            .iter()
            .map(|&(v, _, _)| measure::datum_of(v, common.kind))
            .collect();
        let array = pg_sys::construct_array(
            elems.as_mut_ptr(),
            elems.len() as i32,
            common.kind,
            len as i32,
            byval,
            align,
        );
        set(pg_sys::Anum_pg_statistic_stakind1, pg_sys::Datum::from(mcv));
        set(
            pg_sys::Anum_pg_statistic_staop1,
            pg_sys::Datum::from(common.equality),
        );
        set(
            pg_sys::Anum_pg_statistic_stacoll1,
            pg_sys::Datum::from(common.collation),
        );
        set(
            pg_sys::Anum_pg_statistic_stavalues1,
            pg_sys::Datum::from(array),
        );
    }
    let modified = pg_sys::heap_modify_tuple(
        tuple,
        desc,
        values.as_mut_ptr(),
        nulls.as_mut_ptr(),
        replace.as_mut_ptr(),
    );
    pg_sys::table_close(catalog, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    pg_sys::heap_freetuple(tuple);
    modified
}

/// A statistics tuple for column `attnum` of relation `relid` holding no values and no kinds of
/// statistics, formed in the catalog's own shape.
unsafe fn formed(relid: pg_sys::Oid, attnum: pg_sys::AttrNumber) -> pg_sys::HeapTuple {
    let catalog = pg_sys::table_open(
        pg_sys::StatisticRelationId,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let desc = (*catalog).rd_att;
    let columns = (*desc).natts as usize;
    // starelid, staattnum, stainherit, stanullfrac, stawidth and stadistinct, then five each of
    // stakind, staop and stacoll, every one zero; the arrays after them are NULL
    let fixed = 6 + 3 * 5;
    let mut values = vec![pg_sys::Datum::from(0); columns];
    let mut nulls: Vec<bool> = (0..columns).map(|c| c >= fixed).collect();
    values[0] = pg_sys::Datum::from(relid);
    values[1] = pg_sys::Datum::from(attnum as i32);
    let tuple = pg_sys::heap_form_tuple(desc, values.as_mut_ptr(), nulls.as_mut_ptr());
    pg_sys::table_close(catalog, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    tuple
}
/// Where `rte` stands in `root`'s range table.
unsafe fn varno_of(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
) -> Option<pg_sys::Index> {
    if (*root).simple_rte_array.is_null() {
        return None;
    }
    (1..(*root).simple_rel_array_size as usize)
        .find(|&i| *(*root).simple_rte_array.add(i) == rte)
        .map(|i| i as pg_sys::Index)
}

/// The planner's state for the subquery or WITH query `rte`, at `varno` of `root`'s range table,
/// once it has been planned.
unsafe fn sub_root(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
    varno: pg_sys::Index,
) -> Option<*mut pg_sys::PlannerInfo> {
    let sub = match (*rte).rtekind {
        pg_sys::RTEKind::RTE_SUBQUERY => {
            if (*rte).inh || (*root).simple_rel_array.is_null() {
                return None;
            }
            let rel = *(*root).simple_rel_array.add(varno as usize);
            if rel.is_null() {
                return None;
            }
            (*rel).subroot
        }
        pg_sys::RTEKind::RTE_CTE => {
            if (*rte).self_reference {
                return None;
            }
            let mut owner = root;
            for _ in 0..(*rte).ctelevelsup {
                owner = (*owner).parent_root;
                if owner.is_null() {
                    return None;
                }
            }
            let name = CStr::from_ptr((*rte).ctename);
            let at = cells((*(*owner).parse).cteList).iter().position(|&c| {
                CStr::from_ptr((*(c as *mut pg_sys::CommonTableExpr)).ctename) == name
            })?;
            let ids = (*owner).cte_plan_ids;
            if ids.is_null() || at >= (*ids).length as usize {
                return None;
            }
            let plan = (*(*ids).elements.add(at)).int_value;
            let subroots = (*(*root).glob).subroots;
            if plan <= 0 || subroots.is_null() || plan as usize > (*subroots).length as usize {
                return None;
            }
            (*(*subroots).elements.add(plan as usize - 1)).ptr_value as *mut pg_sys::PlannerInfo
        }
        _ => return None,
    };
    (!sub.is_null() && !(*sub).parse.is_null()).then_some(sub)
}

/// Output column `attnum` of the query `root` plans, where the query has no set operation and no
/// grouping sets.
unsafe fn output(
    root: *mut pg_sys::PlannerInfo,
    attnum: pg_sys::AttrNumber,
) -> Option<*mut pg_sys::TargetEntry> {
    let q = (*root).parse;
    if !(*q).setOperations.is_null() || !(*q).groupingSets.is_null() {
        return None;
    }
    let list = if (*q).returningList.is_null() {
        (*q).targetList
    } else {
        (*q).returningList
    };
    let target = pg_sys::get_tle_by_resno(list, attnum);
    (!target.is_null() && !(*target).resjunk).then_some(target)
}

/// Whether the output column `target` is one of the grouping or DISTINCT columns `clause` names.
unsafe fn grouped_by(target: *mut pg_sys::TargetEntry, clause: *mut pg_sys::List) -> bool {
    let reference = (*target).ressortgroupref;
    reference != 0
        && cells(clause)
            .iter()
            .any(|&c| (*(c as *mut pg_sys::SortGroupClause)).tleSortGroupRef == reference)
}

/// The table and the column that `expr`, an output of the query `root` plans, is, where it is a
/// table's column carried up as it is through subqueries and WITH queries, none of them a security
/// barrier, a set operation or grouping sets, and the table is read without inheritance children:
/// the query that reads the table, the table's place in its range table, the table and the column.
unsafe fn table_column_of(
    mut root: *mut pg_sys::PlannerInfo,
    mut expr: *mut pg_sys::Node,
) -> Option<(
    *mut pg_sys::PlannerInfo,
    pg_sys::Index,
    pg_sys::Oid,
    pg_sys::AttrNumber,
)> {
    loop {
        while !expr.is_null() && (*expr).type_ == pg_sys::NodeTag::T_RelabelType {
            expr = (*(expr as *mut pg_sys::RelabelType)).arg as *mut pg_sys::Node;
        }
        if expr.is_null() || (*expr).type_ != pg_sys::NodeTag::T_Var {
            return None;
        }
        let var = expr as *mut pg_sys::Var;
        let varno = (*var).varno as usize;
        if (*var).varlevelsup != 0
            || (*var).varattno <= 0
            || (*root).simple_rte_array.is_null()
            || varno == 0
            || varno >= (*root).simple_rel_array_size as usize
        {
            return None;
        }
        let rte = *(*root).simple_rte_array.add(varno);
        if rte.is_null() {
            return None;
        }
        match (*rte).rtekind {
            pg_sys::RTEKind::RTE_RELATION => {
                return (!(*rte).inh).then_some((
                    root,
                    varno as pg_sys::Index,
                    (*rte).relid,
                    (*var).varattno,
                ));
            }
            pg_sys::RTEKind::RTE_SUBQUERY | pg_sys::RTEKind::RTE_CTE => {
                if (*rte).security_barrier {
                    return None;
                }
                let sub = sub_root(root, rte, varno as pg_sys::Index)?;
                let target = output(sub, (*var).varattno)?;
                root = sub;
                expr = (*target).expr as *mut pg_sys::Node;
            }
            _ => return None,
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;
    use std::cell::RefCell;

    fn texts(sql: &str) -> Vec<String> {
        Spi::connect(|client| {
            let mut out = Vec::new();
            for row in client.select(sql, None, &[])? {
                out.push(row.get::<String>(1)?.unwrap_or_default());
            }
            Ok::<_, pgrx::spi::SpiError>(out)
        })
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn number(sql: &str) -> f64 {
        Spi::get_one::<i64>(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .unwrap_or_else(|| panic!("{sql}: no row")) as f64
    }

    /// The rows the planner estimates for the first line of `query`'s plan that contains `node`, or
    /// for its top node when `node` is empty.
    fn estimated(query: &str, node: &str) -> f64 {
        let lines = texts(&format!("EXPLAIN {query}"));
        let line = lines
            .iter()
            .find(|l| l.contains(node))
            .unwrap_or_else(|| panic!("no {node} in\n{}", lines.join("\n")));
        line.split(" rows=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("no estimate in {line}"))
    }

    /// A statistics tuple the hooks handed the planner: the relation and column it was asked for,
    /// the share of NULLs, the distinct values, and the most common values as whole numbers with
    /// their shares.
    #[derive(Clone, Debug)]
    struct Handed {
        of: (u32, i16),
        nulls: f64,
        distinct: f64,
        common: Vec<(Option<i64>, f64)>,
    }

    thread_local! {
        static HANDED: RefCell<Vec<Handed>> = const { RefCell::new(Vec::new()) };
        /// The B-trees whose leading column was read for its statistics.
        static READ: RefCell<Vec<pg_sys::Oid>> = const { RefCell::new(Vec::new()) };
    }

    /// Notes that the B-tree `index` was read for its leading column's statistics.
    pub(super) fn note_read(index: pg_sys::Oid) {
        READ.with(|r| r.borrow_mut().push(index));
    }

    /// The B-trees read for statistics while `query` was planned.
    fn statistics_reads(query: &str) -> Vec<pg_sys::Oid> {
        READ.with(|r| r.borrow_mut().clear());
        Spi::run(&format!("EXPLAIN {query}")).unwrap_or_else(|e| panic!("{query}: {e}"));
        READ.with(|r| r.borrow().clone())
    }
    static mut NEXT_RELATION: pg_sys::get_relation_stats_hook_type = None;
    static mut NEXT_INDEX: pg_sys::get_index_stats_hook_type = None;

    unsafe fn record(
        of: (pg_sys::Oid, pg_sys::AttrNumber),
        vardata: *mut pg_sys::VariableStatData,
    ) {
        let tuple = (*vardata).statsTuple;
        if tuple.is_null() {
            return;
        }
        let form = super::form_of(tuple);
        let mut slot = pg_sys::AttStatsSlot::default();
        let mut common = Vec::new();
        if pg_sys::get_attstatsslot(
            &mut slot,
            tuple,
            pg_sys::STATISTIC_KIND_MCV as i32,
            pg_sys::InvalidOid,
            (pg_sys::ATTSTATSSLOT_VALUES | pg_sys::ATTSTATSSLOT_NUMBERS) as i32,
        ) {
            for i in 0..slot.nnumbers as usize {
                let value = crate::measure::whole(*slot.values.add(i), slot.valuetype);
                common.push((value, *slot.numbers.add(i) as f64));
            }
            pg_sys::free_attstatsslot(&mut slot);
        }
        HANDED.with(|h| {
            h.borrow_mut().push(Handed {
                of: (of.0.to_u32(), of.1),
                nulls: (*form).stanullfrac as f64,
                distinct: (*form).stadistinct as f64,
                common,
            })
        });
    }

    #[pg_guard]
    unsafe extern "C-unwind" fn relation_recorded(
        root: *mut pg_sys::PlannerInfo,
        rte: *mut pg_sys::RangeTblEntry,
        attnum: pg_sys::AttrNumber,
        vardata: *mut pg_sys::VariableStatData,
    ) -> bool {
        let took = NEXT_RELATION.is_some_and(|next| next(root, rte, attnum, vardata));
        if took {
            record(((*rte).relid, attnum), vardata);
        }
        took
    }

    #[pg_guard]
    unsafe extern "C-unwind" fn index_recorded(
        root: *mut pg_sys::PlannerInfo,
        index: pg_sys::Oid,
        column: pg_sys::AttrNumber,
        vardata: *mut pg_sys::VariableStatData,
    ) -> bool {
        let took = NEXT_INDEX.is_some_and(|next| next(root, index, column, vardata));
        if took {
            record((index, column), vardata);
        }
        took
    }

    /// The statistics tuples the hooks handed the planner while `query` was planned.
    fn handed(query: &str) -> Vec<Handed> {
        HANDED.with(|h| h.borrow_mut().clear());
        unsafe {
            NEXT_RELATION = pg_sys::get_relation_stats_hook;
            NEXT_INDEX = pg_sys::get_index_stats_hook;
            pg_sys::get_relation_stats_hook = Some(relation_recorded);
            pg_sys::get_index_stats_hook = Some(index_recorded);
        }
        let planned = Spi::run(&format!("EXPLAIN {query}"));
        unsafe {
            pg_sys::get_relation_stats_hook = NEXT_RELATION;
            pg_sys::get_index_stats_hook = NEXT_INDEX;
        }
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        HANDED.with(|h| h.borrow().clone())
    }

    /// 60,000 rows, never analyzed: `a` takes 5,003 values and is NULL on every 50th row, `b` takes
    /// 7 values.
    fn leaves_table() {
        Spi::run(
            "CREATE TABLE leaves AS \
             SELECT g AS id, CASE WHEN g % 50 = 0 THEN NULL ELSE (g * 7919) % 5003 END AS a, g % 7 AS b \
             FROM generate_series(1, 60000) g",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_column_no_b_tree_leads_with_keeps_the_planners_statistics() {
        leaves_table();
        Spi::run(
            "CREATE INDEX leaves_part ON leaves (a) WHERE a < 1000; \
             CREATE INDEX leaves_ba ON leaves (b, a); ANALYZE leaves",
        )
        .unwrap();
        let grouped = "SELECT a FROM leaves GROUP BY a";
        let nulls = "SELECT id FROM leaves WHERE a IS NOT NULL";
        let (groups, null_rows) = (estimated(grouped, ""), estimated(nulls, ""));
        // a partial B-tree on the column, and one led by another, measure nothing of it
        Spi::run("CREATE INDEX leaves_ab ON leaves USING surveyor (a, b)").unwrap();
        assert_eq!(estimated(grouped, ""), groups);
        assert_eq!(estimated(nulls, ""), null_rows);
        let table = Spi::get_one::<pg_sys::Oid>("SELECT 'leaves'::regclass::oid")
            .unwrap()
            .unwrap()
            .to_u32();
        let seen = handed(grouped);
        assert!(seen.iter().all(|h| h.of != (table, 2)), "{seen:?}");
    }

    /// Sales over 2,000 days from 2000, the day's rows growing from 1 to 39 along them; the shop
    /// in 9 and the tag in 3 every row; the month a level of the instant. B-trees on the day, the
    /// shop, the tag, and the month with the instant; a surveyor on the sale. Analyzed from a
    /// sample of 3,000 rows.
    fn sales() {
        Spi::run(
            "CREATE TABLE sales AS \
             SELECT row_number() OVER () AS id, date '2000-01-01' + k AS day, \
                    timestamp '2000-01-01' + k * interval '1 day' + n * interval '1 minute' AS at, \
                    (n % 9) AS shop, (ARRAY['a', 'b', 'c'])[1 + n % 3] AS tag \
             FROM generate_series(0, 1999) k, generate_series(1, 1 + k / 52) n; \
             CREATE INDEX sales_day ON sales (day); \
             CREATE INDEX sales_shop ON sales (shop); \
             CREATE INDEX sales_tag ON sales (tag); \
             CREATE INDEX sales_month_at ON sales ((extract(month FROM at)::smallint), at); \
             CREATE INDEX sales_order ON sales USING surveyor (id); \
             SET LOCAL default_statistics_target = 10; ANALYZE sales; RESET default_statistics_target",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_columns_distinct_values_are_every_value_between_the_first_and_last_whichever_b_tree_is_newer(
    ) {
        sales();
        let days = number("SELECT (max(day) - min(day) + 1)::bigint FROM sales");
        let grouped = "SELECT day FROM sales GROUP BY day";
        assert_eq!(estimated(grouped, ""), days);
        let seen = handed(grouped);
        assert!(seen.iter().all(|h| h.distinct == days), "{seen:?}");
        // an expression two B-trees lead with, the newer one asked first
        Spi::run(
            "CREATE INDEX sales_date ON sales ((at::date)); \
             CREATE INDEX sales_date_id ON sales ((at::date), id); \
             SET LOCAL default_statistics_target = 10; ANALYZE sales; RESET default_statistics_target",
        )
        .unwrap();
        let by_expression = "SELECT at::date FROM sales GROUP BY at::date";
        assert_eq!(estimated(by_expression, ""), days);
        // a subquery's grouped column takes the same count
        let grouped_twice =
            "SELECT day FROM (SELECT day, shop FROM sales GROUP BY day, shop) s GROUP BY day";
        assert_eq!(estimated(grouped_twice, ""), days);
        // the sample's guess, which the planner had
        Spi::run("DROP INDEX sales_order").unwrap();
        assert_ne!(estimated(by_expression, ""), days);
    }

    #[pg_test]
    fn the_share_of_nulls_is_the_b_trees_null_block() {
        // a fifth of the rows with no shop, a column ANALYZE leaves alone
        Spi::run(
            "CREATE TABLE shops AS SELECT g AS id, CASE WHEN g % 5 = 0 THEN NULL ELSE g % 9 END AS shop \
             FROM generate_series(1, 60000) g; \
             ALTER TABLE shops ALTER COLUMN shop SET STATISTICS 0; \
             CREATE INDEX shops_shop ON shops (shop); ANALYZE shops",
        )
        .unwrap();
        let rows = number("SELECT count(*) FROM shops");
        let shops = number("SELECT count(shop) FROM shops");
        let query = "SELECT id FROM shops WHERE shop IS NOT NULL";
        let unsurveyed = estimated(query, "");
        assert!((unsurveyed - shops).abs() > 0.1 * shops, "{unsurveyed}");
        Spi::run("CREATE INDEX shops_order ON shops USING surveyor (id)").unwrap();
        let seen = handed(query);
        let nulls = seen
            .first()
            .expect("the shop's statistics were handed")
            .nulls;
        // one leaf at each end of the block
        let leaves = number("SELECT relpages - 2 FROM pg_class WHERE relname = 'shops_shop'");
        let truth = 1.0 - shops / rows;
        assert!(
            (nulls - truth).abs() <= 2.0 / leaves,
            "{nulls} against {truth}, {leaves} leaves"
        );
        let planned = estimated(query, "");
        assert!(
            (planned - shops).abs() <= 2.0 * rows / leaves,
            "{planned} against {shops}"
        );
    }

    #[pg_test]
    fn the_shares_of_nulls_and_common_values_sum_to_at_most_one() {
        sales();
        // a third more rows with no tag, after the sample held every tag as common
        Spi::run(
            "INSERT INTO sales (id, day, at, shop, tag) \
             SELECT 100000 + g, date '2001-01-01', timestamp '2001-01-01', 1, NULL \
             FROM generate_series(1, (SELECT count(*) / 3 FROM sales)) g",
        )
        .unwrap();
        let seen = handed("SELECT id FROM sales WHERE tag IS NOT NULL OR id < 0");
        let tag = seen.first().expect("the tag's statistics were handed");
        let sum: f64 = tag.common.iter().map(|c| c.1).sum();
        assert!(tag.nulls > 0.2, "{tag:?}");
        assert!(tag.nulls + sum <= 1.0 + 1e-6, "{tag:?}");
        assert_eq!(tag.common.len(), 3, "{tag:?}");
    }

    #[pg_test]
    fn a_level_with_few_values_hands_each_values_share_as_measured() {
        sales();
        let month = "extract(month FROM at)::smallint";
        let seen = handed(&format!("SELECT id FROM sales WHERE {month} = 7 OR id < 0"));
        let months = &seen
            .first()
            .expect("the month's statistics were handed")
            .common;
        assert_eq!(months.len(), 12, "{months:?}");
        let rows = number("SELECT count(*) FROM sales");
        let leaf = Spi::get_one::<f64>(
            "SELECT reltuples::float8 / greatest(relpages - 2, 1) FROM pg_class \
             WHERE relname = 'sales_month_at'",
        )
        .unwrap()
        .unwrap();
        for (m, share) in months {
            let m = m.expect("a month is a value");
            let counted = number(&format!("SELECT count(*) FROM sales WHERE {month} = {m}"));
            // one leaf at each end of the month's block
            assert!(
                (share * rows - counted).abs() <= 2.0 * leaf,
                "month {m}: {} measured, {counted} counted",
                share * rows
            );
        }
        // the largest first
        assert!(months.windows(2).all(|w| w[0].1 >= w[1].1), "{months:?}");
    }

    #[pg_test]
    fn a_table_analyze_read_whole_keeps_its_statistics_as_they_are_unread() {
        // 20 values: the first held by 3 rows, inside the first page of the leaves; the others by
        // 500 each; ANALYZE reads every one of the 9,503 rows and names every value
        Spi::run(
            "CREATE TABLE edged AS \
             SELECT g AS id, CASE WHEN g <= 3 THEN 1 ELSE 2 + g % 19 END AS v \
             FROM generate_series(1, 9503) g; \
             CREATE INDEX edged_v ON edged (v) WITH (deduplicate_items = off); \
             CREATE INDEX edged_order ON edged USING surveyor (id); ANALYZE edged",
        )
        .unwrap();
        let query = "SELECT id FROM edged WHERE v = 5 OR id < 0";
        let seen = handed(query);
        assert!(seen.is_empty(), "{seen:?}");
        assert!(statistics_reads(query).is_empty());
        // the first value's 3 rows, as ANALYZE counted them
        assert_eq!(estimated("SELECT id FROM edged WHERE v = 1", ""), 3.0);
    }

    #[pg_test]
    fn a_table_grown_past_what_analyze_read_is_read_for_its_statistics() {
        // the 9,503 rows ANALYZE read whole, then 60,000 more written since
        Spi::run(
            "CREATE TABLE grown AS \
             SELECT g AS id, CASE WHEN g <= 3 THEN 1 ELSE 2 + g % 19 END AS v \
             FROM generate_series(1, 9503) g; \
             CREATE INDEX grown_v ON grown (v) WITH (deduplicate_items = off); \
             CREATE INDEX grown_order ON grown USING surveyor (id); ANALYZE grown",
        )
        .unwrap();
        let query = "SELECT id FROM grown WHERE v = 5 OR id < 0";
        assert!(handed(query).is_empty());
        Spi::run("INSERT INTO grown SELECT 9503 + g, 2 + g % 19 FROM generate_series(1, 60000) g")
            .unwrap();
        let seen = handed(query);
        assert!(seen.iter().any(|h| h.of.1 == 2), "{seen:?}");
        assert!(!statistics_reads(query).is_empty());
    }

    #[pg_test]
    fn a_value_of_a_table_analyze_read_whole_takes_its_count_without_reading_its_leaves() {
        crate::conditions::tests::listed();
        // ANALYZE reads every row and names every category
        Spi::run(
            "ALTER TABLE listed ALTER cat SET STATISTICS -1; ANALYZE listed; \
             CREATE INDEX listed_order ON listed USING surveyor (id); \
             CREATE EXTENSION IF NOT EXISTS pageinspect",
        )
        .unwrap();
        // the statistics are ANALYZE's, unread
        let seen = handed("SELECT id FROM listed WHERE cat = 7 OR id < 0");
        assert!(seen.iter().all(|h| h.of.1 != 2), "{seen:?}");
        assert!(statistics_reads("SELECT id FROM listed WHERE cat = 7 OR id < 0").is_empty());
        // the 7th category spans whole leaves of an index storing text, and takes ANALYZE's count
        // from the read's first pages, its leaves not read whole
        let query = "SELECT id FROM listed WHERE cat = 7";
        assert_eq!(
            estimated(query, ""),
            number("SELECT count(*) FROM listed WHERE cat = 7")
        );
        let leaves = number(
            "SELECT count(DISTINCT s.blkno) \
             FROM bt_multi_page_stats('listed_cat_id', 1, -1) s, \
                  LATERAL bt_page_items('listed_cat_id', s.blkno::int) i \
             WHERE s.type = 'l' AND i.data LIKE '07 00 00 00%' \
               AND NOT (s.btpo_next <> 0 AND i.itemoffset = 1)",
        );
        assert!(leaves >= 6.0, "{leaves} leaves");
        let read = crate::conditions::tests::reads(query);
        let pages = read
            .iter()
            .find(|(name, _)| name == "listed_cat_id")
            .and_then(|(_, pages)| *pages)
            .unwrap_or_else(|| panic!("{read:?}"));
        assert!(pages <= 8, "{pages} pages read, {leaves} leaves");
    }

    #[pg_test]
    fn a_sampled_table_measures_its_named_values_by_the_index() {
        // 60,040 parts in 30 categories, the 15th holding 40 and each other about 2,070, with a key
        // on the category and the part carrying names of 8 to 95 bytes in no order, so that the
        // leaves hold unequal rows; ANALYZE samples 30,000 of the rows, first keeping no statistics
        // on the category and then naming every category; only the 15th lies inside a page or two
        Spi::run(
            "CREATE TABLE varied AS SELECT g AS id, \
                 CASE WHEN g <= 40 THEN 15 WHEN g % 29 < 14 THEN 1 + g % 29 ELSE 2 + g % 29 END AS cat, \
                 left(repeat(md5(g::text), 3), 8 + abs(hashint4(g)) % 88) AS name \
             FROM generate_series(1, 60040) g; \
             CREATE INDEX varied_cat_id ON varied (cat, id) INCLUDE (name); \
             CREATE INDEX varied_order ON varied USING surveyor (id); \
             ALTER TABLE varied ALTER cat SET STATISTICS 0; ANALYZE varied",
        )
        .unwrap();
        assert!(number("SELECT count(*) FROM varied") > 30000.0);
        let inside = "SELECT id FROM varied WHERE cat = 15";
        let alone = estimated(inside, "");
        Spi::run("ALTER TABLE varied ALTER cat SET STATISTICS -1; ANALYZE varied").unwrap();
        // a category spanning whole leaves: the index's own count, not ANALYZE's sampled share
        assert_eq!(
            estimated("SELECT id FROM varied WHERE cat = 7", ""),
            number("SELECT count(*) FROM varied WHERE cat = 7")
        );
        // the 15th, inside a page or two of an integer key: its page's rows over its categories,
        // as with no statistics
        assert_eq!(estimated(inside, ""), alone);
        // every category handed as measured, with the NULLs summing to 1
        let seen = handed("SELECT id FROM varied WHERE cat = 7 OR id < 0");
        let cat = seen
            .iter()
            .find(|h| h.of.1 == 2)
            .unwrap_or_else(|| panic!("{seen:?}"));
        assert_eq!(cat.common.len(), 30, "{cat:?}");
        let sum: f64 = cat.common.iter().map(|c| c.1).sum();
        assert!((sum + cat.nulls - 1.0).abs() <= 1e-5, "{sum}: {cat:?}");
    }

    #[pg_test]
    fn a_value_of_an_index_storing_text_is_read_whole_and_its_share_agrees_with_its_size() {
        crate::conditions::tests::banded();
        let seen = handed("SELECT n FROM banded WHERE a = 1 OR n < 0");
        let a = seen
            .iter()
            .find(|h| h.of.1 == 1)
            .unwrap_or_else(|| panic!("{seen:?}"));
        assert_eq!(a.common.len(), 3, "{a:?}");
        let rows = number("SELECT count(*) FROM banded");
        let second = a
            .common
            .iter()
            .find(|c| c.0 == Some(2))
            .unwrap_or_else(|| panic!("{a:?}"));
        assert!(
            (second.1 * rows - 12000.0).abs() <= 0.5,
            "{} handed, {a:?}",
            second.1 * rows
        );
        // the relation's size for the value is the same count
        assert_eq!(estimated("SELECT n FROM banded WHERE a = 2", ""), 12000.0);
    }

    #[pg_test]
    fn a_statistics_read_stops_once_its_pages_would_pass_the_pages_of_its_table() {
        // 12,000 rows on a few pages, each one of 40 values, and the same rows each beside a note
        // that fills a page with a few of them; ANALYZE keeps a short list from a small sample
        Spi::run(
            "CREATE TABLE forty AS SELECT g AS id, g % 40 AS v FROM generate_series(1, 12000) g; \
             CREATE TABLE forty_padded (id int, v int, note text); \
             ALTER TABLE forty_padded ALTER note SET STORAGE PLAIN; \
             INSERT INTO forty_padded SELECT id, v, repeat('x', 2000) FROM forty; \
             CREATE INDEX forty_v ON forty (v) WITH (deduplicate_items = off); \
             CREATE INDEX forty_padded_v ON forty_padded (v) WITH (deduplicate_items = off); \
             CREATE INDEX forty_order ON forty USING surveyor (id); \
             CREATE INDEX forty_padded_order ON forty_padded USING surveyor (id); \
             SET LOCAL default_statistics_target = 10; ANALYZE forty; ANALYZE forty_padded; \
             RESET default_statistics_target",
        )
        .unwrap();
        let common = |table: &str| {
            let seen = handed(&format!("SELECT id FROM {table} WHERE v = 7 OR id < 0"));
            let v = seen
                .iter()
                .find(|h| h.of.1 == 2)
                .unwrap_or_else(|| panic!("{table}: {seen:?}"))
                .clone();
            (v.common.len(), v.distinct)
        };
        // each of the 40 values read beside the notes
        assert_eq!(common("forty_padded"), (40, 40.0));
        // on the few pages, the first and last values are read, and ANALYZE's list stands
        let (listed, distinct) = common("forty");
        assert!(listed <= 10, "{listed}");
        assert_eq!(distinct, 40.0);
    }

    #[pg_test]
    fn common_values_fewer_than_the_values_between_the_first_and_last_keep_their_shares() {
        let near = |a: f64, b: f64| (a - b).abs() <= 1e-12;
        let measured = [false; 4];
        // every one of the four values: scaled to sum with the NULLs to 1
        let mut shares = [0.3, 0.2, 0.2, 0.2];
        super::whole(&mut shares, &measured, 4, 0.1);
        assert!(near(shares.iter().sum::<f64>(), 0.9), "{shares:?}");
        assert!(near(shares[0] / shares[1], 1.5), "{shares:?}");
        // three of four: as measured
        let mut shares = [0.3, 0.2, 0.2];
        super::whole(&mut shares, &measured, 4, 0.1);
        assert_eq!(shares, [0.3, 0.2, 0.2]);
    }

    #[pg_test]
    fn the_shares_analyzes_list_gives_stay_as_they_are_and_the_measured_shares_fill_the_rest() {
        let near = |a: f64, b: f64| (a - b).abs() <= 1e-12;
        // every one of the four values, one of them ANALYZE's
        let mut shares = [0.3, 0.2, 0.2, 0.001];
        super::whole(&mut shares, &[false, false, false, true], 4, 0.1);
        assert_eq!(shares[3], 0.001, "{shares:?}");
        assert!(near(shares.iter().sum::<f64>(), 0.9), "{shares:?}");
        assert!(near(shares[0] / shares[1], 1.5), "{shares:?}");
        // every share ANALYZE's, short of the whole: as they are
        let mut shares = [0.3, 0.2];
        super::whole(&mut shares, &[true, true], 2, 0.1);
        assert_eq!(shares, [0.3, 0.2]);
        // ANALYZE's alone past the room: every share scaled to it
        let mut shares = [0.5, 0.5, 0.2];
        super::fill(&mut shares, &[true, true, false], 0.9);
        assert!(near(shares.iter().sum::<f64>(), 0.9), "{shares:?}");
        assert!(near(shares[0] / shares[2], 2.5), "{shares:?}");
        // past the room with ANALYZE's within it: only the measured shares give way
        let mut shares = [0.3, 0.5, 0.4];
        super::fill(&mut shares, &[true, false, false], 0.9);
        assert_eq!(shares[0], 0.3, "{shares:?}");
        assert!(near(shares.iter().sum::<f64>(), 0.9), "{shares:?}");
        // ANALYZE's two as they are, the measured two scaled into the 0.3 left
        let mut shares = [0.3, 0.3, 0.3, 0.1];
        super::fill(&mut shares, &[true, false, true, false], 0.9);
        assert_eq!((shares[0], shares[2]), (0.3, 0.3), "{shares:?}");
        assert!(
            near(shares[1], 0.225) && near(shares[3], 0.075),
            "{shares:?}"
        );
        assert!(near(shares.iter().sum::<f64>(), 0.9), "{shares:?}");
    }

    /// One table read three ways: `kinds`, holding no row of its own, over three classes, each of
    /// one lane by its constraint; `parted`, partitioned by the lane; and the rows themselves. The
    /// first lane holds 20,000 rows of 7 categories, the second 30,000 of 13, which ANALYZE only
    /// samples, and the third 10,000 of 5, a tenth of them with no category. Each class and each
    /// partition keys its category and carries a surveyor. The parents are never analyzed.
    fn hierarchy() {
        Spi::run(
            "CREATE TABLE kinds (id int, lane int, cat int); \
             CREATE TABLE kinds_a (CHECK (lane = 1)) INHERITS (kinds); \
             CREATE TABLE kinds_b (CHECK (lane = 2)) INHERITS (kinds); \
             CREATE TABLE kinds_c (CHECK (lane = 3)) INHERITS (kinds); \
             INSERT INTO kinds_a SELECT g, 1, g % 7 FROM generate_series(1, 20000) g; \
             INSERT INTO kinds_b SELECT g, 2, 10 + g % 13 FROM generate_series(1, 30000) g; \
             INSERT INTO kinds_c SELECT g, 3, CASE WHEN g % 10 <> 0 THEN 30 + g % 5 END \
                 FROM generate_series(1, 10000) g; \
             CREATE TABLE parted (id int, lane int, cat int) PARTITION BY LIST (lane); \
             CREATE TABLE parted_a PARTITION OF parted FOR VALUES IN (1); \
             CREATE TABLE parted_b PARTITION OF parted FOR VALUES IN (2); \
             CREATE TABLE parted_c PARTITION OF parted FOR VALUES IN (3); \
             INSERT INTO parted SELECT * FROM kinds; \
             CREATE INDEX ON kinds_a (cat) WITH (deduplicate_items = off); \
             CREATE INDEX ON kinds_b (cat) WITH (deduplicate_items = off); \
             CREATE INDEX ON kinds_c (cat) WITH (deduplicate_items = off); \
             CREATE INDEX ON parted (cat) WITH (deduplicate_items = off); \
             CREATE INDEX kinds_a_order ON kinds_a USING surveyor (id); \
             CREATE INDEX kinds_b_order ON kinds_b USING surveyor (id); \
             CREATE INDEX kinds_c_order ON kinds_c USING surveyor (id); \
             CREATE INDEX parted_order ON parted USING surveyor (id); \
             ALTER TABLE kinds_b ALTER cat SET STATISTICS 10; \
             ALTER TABLE parted_b ALTER cat SET STATISTICS 10; \
             ANALYZE kinds_a; ANALYZE kinds_b; ANALYZE kinds_c; \
             ANALYZE parted_a; ANALYZE parted_b; ANALYZE parted_c",
        )
        .unwrap();
    }

    /// The statistics handed for column `attnum` of `table` while `query` was planned.
    fn handed_column(table: &str, attnum: i16, query: &str) -> Option<Handed> {
        let oid = Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{table}'::regclass::oid"))
            .unwrap()
            .unwrap()
            .to_u32();
        handed(query).into_iter().find(|h| h.of == (oid, attnum))
    }

    /// The statistics handed for the category of `table` while `query` was planned.
    fn handed_category(table: &str, query: &str) -> Option<Handed> {
        handed_column(table, 3, query)
    }

    #[pg_test]
    fn a_table_read_with_its_children_is_handed_its_members_counts_added_place_by_place() {
        hierarchy();
        let rows = number("SELECT count(*) FROM kinds");
        for table in ["kinds", "parted"] {
            let query = format!("SELECT cat FROM {table} GROUP BY cat");
            let cat = handed_category(table, &query)
                .unwrap_or_else(|| panic!("nothing handed for {table}"));
            // the NULLs of the third lane, the 25 categories of the three, each at its rows
            assert!((cat.nulls * rows - 1000.0).abs() <= 0.5, "{table}: {cat:?}");
            assert_eq!(cat.distinct, 25.0, "{table}: {cat:?}");
            assert_eq!(cat.common.len(), 25, "{table}: {cat:?}");
            for &(value, share) in &cat.common {
                let value = value.expect("a category");
                let counted = number(&format!("SELECT count(*) FROM {table} WHERE cat = {value}"));
                assert!(
                    (share * rows - counted).abs() <= 0.01 * counted,
                    "{table}: {value} at {}, {counted} counted",
                    share * rows
                );
            }
            // the first lane alone, the others ruled out by their constraints or pruned
            let first = handed_category(
                table,
                &format!("SELECT cat FROM {table} WHERE lane = 1 GROUP BY cat"),
            )
            .unwrap_or_else(|| panic!("nothing handed for {table} in the first lane"));
            assert_eq!(first.distinct, 7.0, "{table}: {first:?}");
            assert_eq!(first.nulls, 0.0, "{table}: {first:?}");
            assert_eq!(first.common.len(), 7, "{table}: {first:?}");
        }
        // a class carrying no surveyor leaves the hierarchy to ANALYZE's statistics, as a table
        // carrying none is left
        Spi::run("DROP INDEX kinds_c_order").unwrap();
        assert!(handed_category("kinds", "SELECT cat FROM kinds GROUP BY cat").is_none());
    }

    const LEAST: i64 = i64::MIN;
    const GREATEST: i64 = i64::MAX;

    #[pg_test]
    fn a_bigint_column_from_the_least_bigint_up_keeps_analyzes_distinct_values() {
        // 5,000 keys and the least bigint, which ANALYZE reads whole: the values between the first
        // and the last are far more than the rows, and more than a bigint counts
        Spi::run(&format!(
            "CREATE TABLE far_keys AS SELECT g::int8 AS k, g AS id FROM generate_series(1, 5000) g; \
             INSERT INTO far_keys VALUES ({LEAST}, 0); \
             CREATE INDEX far_keys_k ON far_keys (k); \
             CREATE INDEX far_keys_order ON far_keys USING surveyor (id); ANALYZE far_keys"
        ))
        .unwrap();
        let query = "SELECT id FROM far_keys WHERE k > 10";
        let k = handed_column("far_keys", 1, query).expect("the key's statistics were handed");
        // every row a value of its own, as ANALYZE counted them, and no value listed
        assert_eq!(k.distinct, -1.0, "{k:?}");
        assert!(k.common.is_empty(), "{k:?}");
        let planned = estimated(query, "");
        assert!((planned - 4990.0).abs() <= 50.0, "{planned}");
    }

    #[pg_test]
    fn partitions_holding_the_least_and_the_greatest_bigint_hand_those_two_values() {
        Spi::run(&format!(
            "CREATE TABLE far_ends (k int8, id int) PARTITION BY LIST (id); \
             CREATE TABLE far_ends_least PARTITION OF far_ends FOR VALUES IN (1); \
             CREATE TABLE far_ends_greatest PARTITION OF far_ends FOR VALUES IN (2); \
             INSERT INTO far_ends SELECT {LEAST}, 1 FROM generate_series(1, 100); \
             INSERT INTO far_ends SELECT {GREATEST}, 2 FROM generate_series(1, 300); \
             CREATE INDEX ON far_ends (k); \
             CREATE INDEX far_ends_order ON far_ends USING surveyor (id); \
             ANALYZE far_ends_least; ANALYZE far_ends_greatest"
        ))
        .unwrap();
        let k = handed_column("far_ends", 1, "SELECT k FROM far_ends GROUP BY k")
            .expect("the key's statistics were handed");
        assert_eq!(k.distinct, 2.0, "{k:?}");
        assert_eq!(
            k.common,
            vec![(Some(GREATEST), 0.75), (Some(LEAST), 0.25)],
            "{k:?}"
        );
    }

    #[pg_test]
    fn partitions_ending_at_the_greatest_bigint_hand_the_values_between_their_ends() {
        // the greatest 200 bigints in each of two partitions, ten rows each: more values than the
        // statistics target, each partition's first and last read; each row beside a note, so that
        // each partition holds several times the pages its key's reads take
        Spi::run(&format!(
            "CREATE TABLE far_tops (k int8, id int, note text) PARTITION BY LIST (id); \
             CREATE TABLE far_tops_a PARTITION OF far_tops FOR VALUES IN (1); \
             CREATE TABLE far_tops_b PARTITION OF far_tops FOR VALUES IN (2); \
             INSERT INTO far_tops SELECT {GREATEST} - (g / 2) % 200, 1 + g % 2, repeat('x', 200) \
             FROM generate_series(0, 3999) g; \
             CREATE INDEX ON far_tops (k); \
             CREATE INDEX far_tops_order ON far_tops USING surveyor (id); \
             ANALYZE far_tops_a; ANALYZE far_tops_b"
        ))
        .unwrap();
        let k = handed_column("far_tops", 1, "SELECT k FROM far_tops GROUP BY k")
            .expect("the key's statistics were handed");
        assert_eq!(k.distinct, 200.0, "{k:?}");
    }
}
