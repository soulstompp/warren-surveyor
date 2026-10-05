// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The price of the B-trees of a table that carries a surveyor.
//!
//! While such a table is planned for a question it is surveyed for (a surveyor over the whole
//! table, or a partial one whose predicate the planner proved from the question's conditions), each
//! of its B-trees is priced by the surveyor in place of the B-tree's own price function, for that
//! plan only. The surveyor prices a path as the B-tree's own function does, with two changes:
//!
//! - Where every condition of the path is a constant condition the index holds, the share of the
//!   table's rows measured for them is the share of the rows it fetches, and of the entries the scan
//!   walks where every one of them bounds the scan: the round's measure of the table where it
//!   counted exactly those conditions, and the index's own measure of them otherwise. Where every
//!   one of them bounds the scan and the index was read for exactly them, the leaves it measured
//!   them on are the pages the scan touches, in place of the entries' share of the index's pages.
//! - The index's own pages, each index alone, whatever other tables the statement reads. Its share
//!   of shared buffers is all of its pages up to the whole of `shared_buffers`. Each page the scans
//!   touch is fetched first once, and the pages repeated scans fetch again are counted as the
//!   B-tree counts them, with that share in place of the planner's share of the cache. A first
//!   fetch costs `PAGE_IN_MEMORY` where the page is in shared buffers now, whatever the
//!   tablespace's page costs: of the pages the scans touch, the share of the index's pages in
//!   shared buffers when the plan is made, counted once in the round of planning from the buffers'
//!   headers, no more than the index's share of shared buffers. Every other page, a first fetch
//!   from outside shared buffers or a fetch again, is priced as the disk at the tablespace's page
//!   costs: of each scan's first fetches the page its descent reaches at `random_page_cost`, and
//!   the leaves it walks after it at `warren_surveyor_pg.walked_leaf_page_cost` where it is set,
//!   the price of a leaf walked in key order as measured on the drive, which already holds the
//!   steps to a leaf that does not follow on disk; where it is not set, at the tablespace's
//!   `seq_page_cost` where they follow one another on disk and `random_page_cost` where they do
//!   not; a fetch again at `random_page_cost`. Which leaves follow one another on disk is read once
//!   in the round of planning, from the downlinks of one page above the leaves in the middle of the
//!   key: the share of the steps from one leaf to the next in key order that go to the next block.
//!
//! The descent's comparisons and pages, the entries' and conditions' work, and the table's pages
//! are priced as the B-tree prices them.

use crate::conditions;
use crate::measure;
use crate::query::{cells, examine_indexcol, release, surveyed, surveyor_am};
use crate::round;
use crate::size;
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

/// The price of an index page fetched into shared buffers, on the planner's scale of costs, whatever
/// the tablespace's page costs.
pub(crate) const PAGE_IN_MEMORY: f64 = 0.047;

/// `warren_surveyor_pg.walked_leaf_page_cost`: the price of an index leaf read from disk by a scan
/// walking the leaves in key order, every step of the walk counted in it, to the next block or
/// not; below 0, the tablespace's `seq_page_cost` for the leaves that follow one another on disk
/// and `random_page_cost` for the rest.
static WALKED_LEAF_PAGE_COST: GucSetting<f64> = GucSetting::<f64>::new(-1.0);

/// `warren_surveyor_pg.walked_leaf_page_cost` as it stands: below 0 where it is not set.
pub(crate) fn walked_leaf_page_cost() -> f64 {
    WALKED_LEAF_PAGE_COST.get()
}

/// `warren_surveyor_pg.planning_read_limit`: the most pages the surveyor reads while a statement is
/// planned, below the pages of the tables the statement reads (`budget`); -1 sets no limit below
/// them, and 0 reads none.
static PLANNING_READ_LIMIT: GucSetting<i32> = GucSetting::<i32>::new(-1);

/// `warren_surveyor_pg.planning_read_limit` as it stands, in pages: -1 where it sets no limit.
pub(crate) fn planning_read_limit() -> i32 {
    PLANNING_READ_LIMIT.get()
}

/// The comparisons a page of a descent is priced at.
const PAGE_CPU_MULTIPLIER: f64 = 50.0;

/// Which of the surveyor's changes a price takes. With either page rule, the index's pages are
/// counted alone, with its own share of shared buffers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rules {
    /// The share an index measures stands for the planner's estimate of it.
    pub measured: bool,
    /// A page in shared buffers now costs `PAGE_IN_MEMORY` when first fetched; without it, every
    /// page is priced as the disk.
    pub in_memory: bool,
    /// The leaves a scan walks after its descent cost `seq_page_cost` where they follow one another
    /// on disk; without it, every page from the disk costs `random_page_cost`.
    pub in_order: bool,
}

/// Every change.
pub(crate) const SURVEYED: Rules = Rules {
    measured: true,
    in_memory: true,
    in_order: true,
};

/// No change: the price the B-tree gives itself.
#[cfg_attr(not(any(test, feature = "pg_test")), allow(dead_code))]
pub(crate) const POSTGRES: Rules = Rules {
    measured: false,
    in_memory: false,
    in_order: false,
};

/// A path's price, and what it was worked out from.
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(any(test, feature = "pg_test")), allow(dead_code))]
pub(crate) struct Estimate {
    pub startup: f64,
    pub total: f64,
    pub selectivity: f64,
    pub correlation: f64,
    /// The index pages one scan touches.
    pub pages: f64,
    /// The index pages the path's scans fetch over every loop.
    pub fetched: f64,
    /// The entries one scan walks.
    pub tuples: f64,
    /// The scans one execution of the path makes: one, or one for each value of its arrays.
    pub sa_scans: f64,
    /// Whether arrays over columns no condition fixes were added to the scans.
    pub skipped: bool,
    /// The share of the table the index measured for the path's conditions, where it measured it.
    pub measured: Option<f64>,
    /// The share of the index's pages in shared buffers the price counted, under the in-memory rule.
    pub resident: Option<f64>,
    /// The share of the steps from leaf to leaf that go to the next block the price counted, under
    /// the in-order rule.
    pub order: Option<f64>,
}

type PriceFunction = unsafe extern "C-unwind" fn(
    *mut pg_sys::PlannerInfo,
    *mut pg_sys::IndexPath,
    f64,
    *mut pg_sys::Cost,
    *mut pg_sys::Cost,
    *mut pg_sys::Selectivity,
    *mut f64,
    *mut f64,
);

static mut NEXT_INFO: pg_sys::get_relation_info_hook_type = None;
static mut BTREE_PRICE: Option<PriceFunction> = None;

/// Registers the settings, and puts the hook in place, after any hook already there.
pub fn init() {
    GucRegistry::define_float_guc(
        c"warren_surveyor_pg.walked_leaf_page_cost",
        c"Sets the planner's estimate of the cost of an index leaf read from disk by a scan walking the leaves in key order.",
        c"Set, it prices every leaf such a walk reads. -1 takes the tablespace's seq_page_cost for the leaves that follow one another on disk, and random_page_cost for the rest.",
        &WALKED_LEAF_PAGE_COST,
        -1.0,
        f64::MAX,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"warren_surveyor_pg.planning_read_limit",
        c"Sets the most index pages the surveyor reads while a statement is planned.",
        c"The surveyor never reads more pages while a statement is planned than the tables the statement reads hold; this lowers that. -1 sets no lower limit, and 0 reads no page, so that every estimate is PostgreSQL's own.",
        &PLANNING_READ_LIMIT,
        -1,
        i32::MAX,
        GucContext::Suset,
        GucFlags::UNIT_BLOCKS,
    );
    unsafe {
        pg_sys::MarkGUCPrefixReserved(c"warren_surveyor_pg".as_ptr());
        NEXT_INFO = pg_sys::get_relation_info_hook;
        pg_sys::get_relation_info_hook = Some(relation_info);
    }
}

/// The B-tree's own price function.
unsafe fn btree_price() -> Option<PriceFunction> {
    let known = BTREE_PRICE;
    if known.is_some() {
        return known;
    }
    let routine = pg_sys::GetIndexAmRoutineByAmId(pg_sys::BTREE_AM_OID, false);
    let own = (*routine).amcostestimate;
    pg_sys::pfree(routine as *mut c_void);
    BTREE_PRICE = own;
    own
}

/// Gives each B-tree of a table that carries a valid surveyor the surveyor's price, where it has
/// the B-tree's own; a path through it is priced so only where the table is surveyed for the
/// question.
#[pg_guard]
unsafe extern "C-unwind" fn relation_info(
    root: *mut pg_sys::PlannerInfo,
    relid: pg_sys::Oid,
    inhparent: bool,
    rel: *mut pg_sys::RelOptInfo,
) {
    if let Some(next) = NEXT_INFO {
        next(root, relid, inhparent, rel);
    }
    if rel.is_null() {
        return;
    }
    let indexes: Vec<*mut pg_sys::IndexOptInfo> = cells((*rel).indexlist)
        .into_iter()
        .map(|i| i as *mut pg_sys::IndexOptInfo)
        .collect();
    if indexes.is_empty() {
        return;
    }
    let am = surveyor_am();
    if am == pg_sys::InvalidOid || !indexes.iter().any(|&i| (*i).relam == am) {
        return;
    }
    let Some(own) = btree_price() else {
        return;
    };
    for index in indexes {
        if (*index).relam == pg_sys::BTREE_AM_OID
            && (*index).amcostestimate.map(|f| f as usize) == Some(own as usize)
        {
            (*index).amcostestimate = Some(priced);
        }
    }
}

#[cfg(not(any(test, feature = "pg_test")))]
fn rules() -> Rules {
    SURVEYED
}

#[cfg(any(test, feature = "pg_test"))]
fn rules() -> Rules {
    tests::RULES.get()
}

/// The pages of `shared_buffers`.
#[cfg(not(any(test, feature = "pg_test")))]
unsafe fn shared_buffers() -> f64 {
    pg_sys::NBuffers as f64
}

#[cfg(any(test, feature = "pg_test"))]
unsafe fn shared_buffers() -> f64 {
    tests::SHARED_BUFFERS
        .get()
        .unwrap_or(pg_sys::NBuffers as f64)
}

/// The pages of each relation of this database held in shared buffers, by its tablespace and file.
type Resident = HashMap<(pg_sys::Oid, pg_sys::RelFileNumber), f64>;

thread_local! {
    /// The pages of each relation held in shared buffers, counted once in the round of planning.
    static RESIDENT: RefCell<Option<Resident>> = const { RefCell::new(None) };
    /// The share of each index's steps from leaf to leaf that go to the next block, read once in
    /// the round of planning.
    static ORDER: RefCell<HashMap<pg_sys::Oid, f64>> = RefCell::new(HashMap::new());
}

/// Forgets the pages counted in shared buffers in the round.
pub(crate) fn forget() {
    RESIDENT.with(|r| *r.borrow_mut() = None);
    ORDER.with(|o| o.borrow_mut().clear());
}

/// The pages of each relation of this database whose main fork's pages are held in shared buffers
/// now: one pass over the buffers' headers, each header locked while its tag is read, as
/// `pg_buffercache` reads them, and no page read.
unsafe fn count_resident() -> Resident {
    let mut counts = Resident::new();
    let valid = pg_sys::BM_VALID | pg_sys::BM_TAG_VALID;
    for at in 0..pg_sys::NBuffers.max(0) as usize {
        let desc = std::ptr::addr_of_mut!((*pg_sys::BufferDescriptors.add(at)).bufferdesc);
        let state = pg_sys::LockBufHdr(desc);
        let tag = (*desc).tag;
        pg_sys::UnlockBufHdr(desc, state);
        if state & valid == valid
            && tag.forkNum == pg_sys::ForkNumber::MAIN_FORKNUM
            && tag.dbOid == pg_sys::MyDatabaseId
        {
            *counts.entry((tag.spcOid, tag.relNumber)).or_insert(0.0) += 1.0;
        }
    }
    counts
}

/// The share of the pages of `index` held in shared buffers now, counted once in the round.
unsafe fn resident_share(index: *mut pg_sys::IndexOptInfo) -> f64 {
    // the planner already holds its lock on the index
    let rel = pg_sys::index_open((*index).indexoid, pg_sys::NoLock as pg_sys::LOCKMODE);
    let at = ((*rel).rd_locator.spcOid, (*rel).rd_locator.relNumber);
    pg_sys::index_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    let of = |counts: &Resident| counts.get(&at).copied().unwrap_or(0.0);
    let resident = if round::depth() == 0 {
        of(&count_resident())
    } else {
        RESIDENT.with(|r| of(r.borrow_mut().get_or_insert_with(|| count_resident())))
    };
    (resident / (*index).pages.max(1) as f64).clamp(0.0, 1.0)
}

/// The share of the steps from one leaf of `index` to the next in key order that go to the next
/// block, read once in the round: 1 where the index has no page above its leaves.
unsafe fn leaf_order(index: *mut pg_sys::IndexOptInfo) -> f64 {
    let oid = (*index).indexoid;
    if round::depth() > 0 {
        if let Some(kept) = ORDER.with(|o| o.borrow().get(&oid).copied()) {
            return kept;
        }
    }
    // the planner already holds its lock on the index
    let rel = pg_sys::index_open(oid, pg_sys::NoLock as pg_sys::LOCKMODE);
    let order = measure::leaf_order(rel).map_or(1.0, |(share, _)| share);
    pg_sys::index_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    if round::depth() > 0 {
        ORDER.with(|o| o.borrow_mut().insert(oid, order));
    }
    order
}

/// The surveyor's price of a path through a B-tree.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
unsafe extern "C-unwind" fn priced(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    // the planner proves a partial surveyor's predicate after the B-trees are given this price
    if !surveyed((*(*path).indexinfo).rel, true) {
        if let Some(own) = btree_price() {
            pg_sys::ffi::pg_guard_ffi_boundary(|| {
                own(
                    root,
                    path,
                    loop_count,
                    startup,
                    total,
                    selectivity,
                    correlation,
                    pages,
                )
            });
        }
        return;
    }
    let _planning = crate::budget::planning(root);
    let e = estimate(root, path, loop_count, rules());
    *startup = e.startup;
    *total = e.total;
    *selectivity = e.selectivity;
    *correlation = e.correlation;
    *pages = e.pages;
    #[cfg(any(test, feature = "pg_test"))]
    tests::record(root, path, loop_count, e);
}

/// The share of its table's rows measured for the path's conditions: only where every condition of
/// the path is one of the table's own, and the index holds exactly them. Where the round's measure
/// of the table counted exactly those conditions, its share; otherwise the index's own measure of
/// them. And the leaves the index measured them on, where it was read for them.
unsafe fn measured_share(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
) -> Option<(f64, Option<f64>)> {
    let index = (*path).indexinfo;
    let rel = (*index).rel;
    if (*rel).tuples <= 0.0 {
        return None;
    }
    let own = cells((*rel).baserestrictinfo);
    let mut on_path: Vec<*mut pg_sys::RestrictInfo> = Vec::new();
    for clause in cells((*path).indexclauses) {
        let rinfo = (*(clause as *mut pg_sys::IndexClause)).rinfo;
        if !own.contains(&(rinfo as *mut c_void)) {
            return None;
        }
        if !on_path.contains(&rinfo) {
            on_path.push(rinfo);
        }
    }
    if on_path.is_empty() {
        return None;
    }
    let holds_the_path = |clauses: &[*mut pg_sys::RestrictInfo]| {
        clauses.iter().all(|c| on_path.contains(c)) && on_path.iter().all(|c| clauses.contains(c))
    };
    let holding = conditions::holding(root, rel, index, &[])?;
    if !holds_the_path(&holding.clauses) {
        return None;
    }
    if let Some(share) = size::counted_share(root, rel, &on_path) {
        return Some((share, size::measured_leaves(rel, index, &on_path)));
    }
    let (held, leaves) = size::measure_on_leaves(root, rel, index, &[]);
    let held = held?;
    if !holds_the_path(&held.clauses) {
        return None;
    }
    Some(((held.rows / (*rel).tuples).clamp(0.0, 1.0), leaves))
}

unsafe fn tag(node: *mut pg_sys::Node) -> pg_sys::NodeTag {
    (*node).type_
}

/// The order of the table against the index's first column, from its statistics.
unsafe fn first_column_correlation(
    index: *mut pg_sys::IndexOptInfo,
    vardata: &pg_sys::VariableStatData,
) -> f64 {
    let input = *(*index).opcintype;
    let sortop = pg_sys::get_opfamily_member(
        *(*index).opfamily,
        input,
        input,
        pg_sys::BTLessStrategyNumber as i16,
    );
    let mut slot = pg_sys::AttStatsSlot::default();
    let mut correlation = 0.0;
    if sortop != pg_sys::InvalidOid
        && pg_sys::get_attstatsslot(
            &mut slot,
            vardata.statsTuple,
            pg_sys::STATISTIC_KIND_CORRELATION as i32,
            sortop,
            pg_sys::ATTSTATSSLOT_NUMBERS as i32,
        )
    {
        let mut c = *slot.numbers as f64;
        if *(*index).reverse_sort {
            c = -c;
        }
        correlation = if (*index).nkeycolumns > 1 {
            c * 0.75
        } else {
            c
        };
        pg_sys::free_attstatsslot(&mut slot);
    }
    correlation
}

/// The selectivity the planner gives `clauses` on the relation of `index`.
unsafe fn selectivity_of(
    root: *mut pg_sys::PlannerInfo,
    index: *mut pg_sys::IndexOptInfo,
    clauses: *mut pg_sys::List,
) -> f64 {
    pg_sys::clauselist_selectivity(
        root,
        clauses,
        (*(*index).rel).relid as i32,
        pg_sys::JoinType::JOIN_INNER,
        null_mut(),
    )
}

/// The pages of `index` counted as cached: all of the index's own pages, up to the whole of
/// `effective_cache_size`, whatever other tables the statement reads.
unsafe fn cache_share(index: *mut pg_sys::IndexOptInfo) -> f64 {
    let pages = (*index).pages.max(1) as f64;
    (pg_sys::effective_cache_size as f64).min(pages)
}

/// The pages of `index` counted as held in shared buffers: all of the index's own pages, up to the
/// whole of `shared_buffers`.
unsafe fn shared_share(index: *mut pg_sys::IndexOptInfo) -> f64 {
    let pages = (*index).pages.max(1) as f64;
    shared_buffers().min(pages)
}

/// The pages of `index` fetched by scans that touch `touched` pages in all, a page touched by two
/// scans counted twice. It counts them as `index_pages_fetched` does, with `b` pages of the index
/// held in place of the planner's share of the cache.
unsafe fn pages_fetched(index: *mut pg_sys::IndexOptInfo, touched: f64, b: f64) -> f64 {
    let t = (*index).pages.max(1) as f64;
    if t <= b {
        let fetched = 2.0 * t * touched / (2.0 * t + touched);
        if fetched >= t {
            t
        } else {
            fetched.ceil()
        }
    } else {
        let lim = 2.0 * t * b / (2.0 * t - b);
        let fetched = if touched <= lim {
            2.0 * t * touched / (2.0 * t + touched)
        } else {
            b + (touched - lim) * (t - b) / t
        };
        fetched.ceil()
    }
}

/// The price of the index's own pages for one execution of the path, the pages fetched over every
/// loop with the index's share of the cache, and the shares of its pages in shared buffers and of
/// its leaves in order the price counted: `pages` touched by each of `sa_scans` scans, run
/// `loop_count` times.
#[allow(clippy::too_many_arguments)]
unsafe fn page_price(
    root: *mut pg_sys::PlannerInfo,
    index: *mut pg_sys::IndexOptInfo,
    pages: f64,
    sa_scans: f64,
    loop_count: f64,
    random: f64,
    sequential: f64,
    rules: Rules,
) -> (f64, f64, Option<f64>, Option<f64>) {
    let scans = sa_scans * loop_count;
    let all = (*index).pages.max(1) as f64;
    if !(rules.in_memory || rules.in_order) {
        let fetched = if scans > 1.0 {
            pg_sys::index_pages_fetched(pages * scans, (*index).pages, (*index).pages as f64, root)
        } else {
            pages
        };
        let price = if scans > 1.0 {
            fetched * random / loop_count
        } else {
            fetched * random
        };
        return (price, fetched, None, None);
    }
    let shared = shared_share(index);
    // the pages the scans touch, the pages fetched with the share of shared buffers held, and with
    // the share of the cache held
    let (touched, fetched_shared, fetched, per) = if scans > 1.0 {
        let touched = pages * scans;
        (
            pages_fetched(index, touched, all).min(all),
            pages_fetched(index, touched, shared),
            pages_fetched(index, touched, cache_share(index)),
            loop_count,
        )
    } else {
        (pages.min(all), pages, pages, 1.0)
    };
    // each page touched is fetched first once, and the rest fetched again
    let first = touched.min(fetched_shared);
    let again = fetched_shared - first;
    // of the first fetches, the pages in shared buffers now, no more than its share of them
    let resident = rules.in_memory.then(|| resident_share(index));
    let in_buffers = (touched * resident.unwrap_or(0.0)).min(first).min(shared);
    // every other first fetch from the disk: each scan's first page by its descent, and the leaves
    // it walks after it at the price declared for a leaf walked in key order, which holds the steps
    // to a leaf that does not follow on disk; where none is declared, in order where they follow one
    // another on disk and read alone where they do not
    let descended = 1.0 / pages.max(1.0);
    let declared = WALKED_LEAF_PAGE_COST.get();
    let declared = (rules.in_order && declared >= 0.0).then_some(declared);
    let order = (rules.in_order && declared.is_none() && pages > 1.0 && first > in_buffers)
        .then(|| leaf_order(index));
    let walked = match (declared, order) {
        (Some(declared), _) => declared,
        (None, Some(order)) => order * sequential + (1.0 - order) * random,
        (None, None) => random,
    };
    let from_disk = descended * random + (1.0 - descended) * walked;
    let price = in_buffers * PAGE_IN_MEMORY + (first - in_buffers) * from_disk + again * random;
    (price / per, fetched, resident, order)
}

/// The price of `path` under `rules`.
pub(crate) unsafe fn estimate(
    root: *mut pg_sys::PlannerInfo,
    path: *mut pg_sys::IndexPath,
    loop_count: f64,
    rules: Rules,
) -> Estimate {
    let index = (*path).indexinfo;
    let rel = (*index).rel;
    let keys = (*index).nkeycolumns;
    let (measured, leaves) = match rules.measured.then(|| measured_share(root, path)).flatten() {
        Some((share, leaves)) => (Some(share), leaves),
        None => (None, None),
    };

    // the conditions that bound the scan, and the scans its arrays make
    let mut bound: *mut pg_sys::List = null_mut();
    let mut skip_quals: *mut pg_sys::List = null_mut();
    let mut column: i32 = 0;
    let mut equal_here = false;
    let mut found_row_compare = false;
    let mut found_array = false;
    let mut found_is_null = false;
    let mut have_correlation = false;
    let mut correlation = 0.0;
    let mut sa_scans = 1.0;
    let mut skipped = false;
    // whether every condition of the path bounds the scan
    let mut bounded = true;
    'clauses: for clause in cells((*path).indexclauses) {
        let iclause = clause as *mut pg_sys::IndexClause;
        let at = (*iclause).indexcol as i32;
        if column < at {
            let before = sa_scans;
            if found_row_compare {
                bounded = false;
                break;
            }
            if equal_here {
                column += 1;
                skip_quals = null_mut();
            }
            equal_here = false;
            while column < at {
                found_array = true;
                let mut vardata = pg_sys::VariableStatData::default();
                examine_indexcol(root, index, column as usize, &mut vardata);
                let mut is_default = true;
                let mut distinct = pg_sys::get_variable_numdistinct(&mut vardata, &mut is_default);
                if column == 0 {
                    if !vardata.statsTuple.is_null() {
                        correlation = first_column_correlation(index, &vardata);
                    }
                    have_correlation = true;
                }
                release(&mut vardata);
                if is_default {
                    sa_scans = before;
                    break;
                }
                if !skip_quals.is_null() {
                    let quals = pg_sys::add_predicate_to_index_quals(index, skip_quals);
                    let fraction = selectivity_of(root, index, quals);
                    if fraction < pg_sys::DEFAULT_RANGE_INEQ_SEL {
                        sa_scans = before;
                        break;
                    }
                    distinct = (distinct * fraction).round_ties_even();
                    distinct = distinct.max(1.0);
                }
                if skip_quals.is_null() {
                    distinct += 1.0;
                }
                sa_scans *= distinct;
                if ((*index).pages as f64) < sa_scans {
                    sa_scans = before;
                    break;
                }
                skipped = true;
                column += 1;
                skip_quals = null_mut();
            }
            if column != at {
                bounded = false;
                break 'clauses;
            }
        }
        for qual in cells((*iclause).indexquals) {
            let rinfo = qual as *mut pg_sys::RestrictInfo;
            let node = (*rinfo).clause as *mut pg_sys::Node;
            let mut operator = pg_sys::InvalidOid;
            match tag(node) {
                pg_sys::NodeTag::T_OpExpr => {
                    operator = (*(node as *mut pg_sys::OpExpr)).opno;
                }
                pg_sys::NodeTag::T_RowCompareExpr => {
                    let opnos = (*(node as *mut pg_sys::RowCompareExpr)).opnos;
                    operator = (*(*opnos).elements).oid_value;
                    found_row_compare = true;
                }
                pg_sys::NodeTag::T_ScalarArrayOpExpr => {
                    let saop = node as *mut pg_sys::ScalarArrayOpExpr;
                    let array = cells((*saop).args)[1] as *mut pg_sys::Node;
                    let length = pg_sys::estimate_array_length(root, array);
                    operator = (*saop).opno;
                    found_array = true;
                    if length > 1.0 {
                        sa_scans *= length;
                    }
                }
                pg_sys::NodeTag::T_NullTest => {
                    let test = node as *mut pg_sys::NullTest;
                    if (*test).nulltesttype == pg_sys::NullTestType::IS_NULL {
                        found_is_null = true;
                        equal_here = true;
                    }
                }
                other => error!("unsupported indexqual type: {:?}", other),
            }
            if operator != pg_sys::InvalidOid {
                let strategy = pg_sys::get_op_opfamily_strategy(
                    operator,
                    *(*index).opfamily.add(column as usize),
                );
                if strategy == pg_sys::BTEqualStrategyNumber as i32 {
                    equal_here = true;
                }
            }
            bound = pg_sys::lappend(bound, rinfo as *mut c_void);
            if !equal_here && !found_row_compare && column < keys - 1 {
                skip_quals = pg_sys::lappend(skip_quals, rinfo as *mut c_void);
            }
        }
    }

    // the entries one scan walks: the share measured where every condition of the path bounds the
    // scan, otherwise the planner's share of the conditions that bound it
    let mut tuples;
    if (*index).unique && column == keys - 1 && equal_here && !found_array && !found_is_null {
        tuples = 1.0;
    } else {
        let share = match measured {
            Some(share) if bounded => share,
            _ => selectivity_of(
                root,
                index,
                pg_sys::add_predicate_to_index_quals(index, bound),
            ),
        };
        tuples = share * (*rel).tuples;
        sa_scans = sa_scans.min(((*index).pages as f64 * 0.3333333).ceil());
        sa_scans = sa_scans.max(1.0);
        tuples = (tuples / sa_scans).round_ties_even();
    }

    // the rows fetched, and the index's pages and entries
    let quals = pg_sys::get_quals_from_indexclauses((*path).indexclauses);
    let order_bys = (*path).indexorderbys;
    if sa_scans < 1.0 {
        sa_scans = 1.0;
        for qual in cells(quals) {
            let node = (*(qual as *mut pg_sys::RestrictInfo)).clause as *mut pg_sys::Node;
            if tag(node) == pg_sys::NodeTag::T_ScalarArrayOpExpr {
                let saop = node as *mut pg_sys::ScalarArrayOpExpr;
                let length = pg_sys::estimate_array_length(
                    root,
                    cells((*saop).args)[1] as *mut pg_sys::Node,
                );
                if length > 1.0 {
                    sa_scans *= length;
                }
            }
        }
    }
    let selectivity = match measured {
        Some(share) => share,
        None => selectivity_of(
            root,
            index,
            pg_sys::add_predicate_to_index_quals(index, quals),
        ),
    };
    if tuples <= 0.0 {
        tuples = (selectivity * (*rel).tuples / sa_scans).round_ties_even();
    }
    if tuples > (*index).tuples {
        tuples = (*index).tuples;
    }
    if tuples < 1.0 {
        tuples = 1.0;
    }
    // the leaves one scan touches: those the index measured the path's conditions on, where every
    // condition of the path bounds the scan; otherwise the entries' share of the index's pages
    let pages = match leaves {
        Some(leaves) if bounded && leaves > 0.0 => (leaves / sa_scans).ceil(),
        _ if (*index).pages > 1 && (*index).tuples > 1.0 => {
            (tuples * (*index).pages as f64 / (*index).tuples).ceil()
        }
        _ => 1.0,
    };
    let (mut random, mut sequential) = (0.0, 0.0);
    pg_sys::get_tablespace_page_costs((*index).reltablespace, &mut random, &mut sequential);
    let (mut total, fetched, resident, order) = page_price(
        root, index, pages, sa_scans, loop_count, random, sequential, rules,
    );
    let qual_arg_cost = pg_sys::index_other_operands_eval_cost(root, quals)
        + pg_sys::index_other_operands_eval_cost(root, order_bys);
    let qual_op_cost =
        pg_sys::cpu_operator_cost * (cells(quals).len() + cells(order_bys).len()) as f64;
    let mut startup = qual_arg_cost;
    total += qual_arg_cost;
    total += tuples * sa_scans * (pg_sys::cpu_index_tuple_cost + qual_op_cost);

    // the descent's comparisons and pages, once for each scan
    if (*index).tuples > 1.0 {
        let descent = ((*index).tuples.ln() / 2.0f64.ln()).ceil() * pg_sys::cpu_operator_cost;
        startup += descent;
        total += sa_scans * descent;
    }
    let descent =
        ((*index).tree_height + 1) as f64 * PAGE_CPU_MULTIPLIER * pg_sys::cpu_operator_cost;
    startup += descent;
    total += sa_scans * descent;

    if !have_correlation {
        let mut vardata = pg_sys::VariableStatData::default();
        examine_indexcol(root, index, 0, &mut vardata);
        if !vardata.statsTuple.is_null() {
            correlation = first_column_correlation(index, &vardata);
        }
        release(&mut vardata);
    }

    Estimate {
        startup,
        total,
        selectivity,
        correlation,
        pages,
        fetched,
        tuples,
        sa_scans,
        skipped,
        measured,
        resident,
        order,
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
pub(crate) mod tests {
    use super::{estimate, Estimate, Rules, PAGE_IN_MEMORY, POSTGRES, SURVEYED};
    use crate::query::cells;
    use pgrx::pg_sys;
    use pgrx::prelude::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::CStr;

    thread_local! {
        /// The changes the surveyor's price takes.
        pub(super) static RULES: Cell<Rules> = const { Cell::new(SURVEYED) };
        /// The pages of shared buffers the surveyor's price reads, where a test gives them.
        pub(super) static SHARED_BUFFERS: Cell<Option<f64>> = const { Cell::new(None) };
        static RECORDING: Cell<bool> = const { Cell::new(false) };
        static RECORDS: RefCell<Vec<Record>> = const { RefCell::new(Vec::new()) };
    }

    /// One path priced by the surveyor: what it was asked, its price under the rules in force, its
    /// price under no change, and the B-tree's own price.
    #[derive(Clone, Debug)]
    struct Record {
        index: String,
        loop_count: f64,
        table_pages: f64,
        index_pages: f64,
        index_tuples: f64,
        quals: usize,
        ours: Estimate,
        plain: Estimate,
        theirs: [f64; 5],
    }

    /// Records a path the surveyor priced, while a test records.
    pub(super) unsafe fn record(
        root: *mut pg_sys::PlannerInfo,
        path: *mut pg_sys::IndexPath,
        loop_count: f64,
        ours: Estimate,
    ) {
        if !RECORDING.get() {
            return;
        }
        let plain = estimate(root, path, loop_count, POSTGRES);
        let own = super::btree_price().expect("the B-tree's own price");
        let mut theirs = [0.0; 5];
        let [startup, total, selectivity, correlation, pages] = &mut theirs;
        pg_sys::ffi::pg_guard_ffi_boundary(|| {
            own(
                root,
                path,
                loop_count,
                startup,
                total,
                selectivity,
                correlation,
                pages,
            )
        });
        let index = (*path).indexinfo;
        let name = CStr::from_ptr(pg_sys::get_rel_name((*index).indexoid))
            .to_string_lossy()
            .into_owned();
        let quals = cells(pg_sys::get_quals_from_indexclauses((*path).indexclauses)).len();
        RECORDS.with(|r| {
            r.borrow_mut().push(Record {
                index: name,
                loop_count,
                table_pages: (*root).total_table_pages,
                index_pages: (*index).pages as f64,
                index_tuples: (*index).tuples,
                quals,
                ours,
                plain,
                theirs,
            })
        });
    }

    const MEASURED: Rules = Rules {
        measured: true,
        in_memory: false,
        in_order: false,
    };
    const IN_MEMORY: Rules = Rules {
        measured: false,
        in_memory: true,
        in_order: false,
    };
    const PAGES: Rules = Rules {
        measured: false,
        in_memory: true,
        in_order: true,
    };

    /// Every path the surveyor priced while `queries` were planned under `rules`.
    fn recorded(rules: Rules, queries: &[&str]) -> Vec<Record> {
        RECORDS.with(|r| r.borrow_mut().clear());
        RULES.set(rules);
        RECORDING.set(true);
        let mut failed = None;
        for query in queries {
            if let Err(e) = Spi::run(&format!("EXPLAIN {query}")) {
                failed = Some(format!("{query}: {e}"));
                break;
            }
        }
        RECORDING.set(false);
        RULES.set(SURVEYED);
        if let Some(f) = failed {
            panic!("{f}");
        }
        RECORDS.with(|r| r.borrow().clone())
    }

    /// 60,000 rows on 50 shelves, 1,000 bins and 13 lots, with NULL lots, one every 17 minutes
    /// from 2020, keyed by the row, the shelf and bin, the month and instant, the lot, and the bin
    /// of one lot, with a surveyor, every page of each index read into shared buffers; and 5,000
    /// picks of rows.
    pub(crate) fn stock() {
        Spi::run(
            "CREATE TABLE stock AS \
             SELECT g AS id, g % 50 AS shelf, (g * 7) % 1000 AS bin, \
                    CASE WHEN g % 40 = 0 THEN NULL ELSE g % 13 END AS lot, \
                    timestamp '2020-01-01' + g * interval '17 minutes' AS at \
             FROM generate_series(1, 60000) g; \
             ALTER TABLE stock ADD PRIMARY KEY (id); \
             CREATE INDEX stock_shelf_bin ON stock (shelf, bin); \
             CREATE INDEX stock_month_at ON stock ((extract(month FROM at)::smallint), at); \
             CREATE INDEX stock_lot ON stock (lot); \
             CREATE INDEX stock_bin_of_lot ON stock (bin) WHERE lot = 3; \
             CREATE INDEX stock_key ON stock USING surveyor (id); \
             CREATE TABLE picks AS \
             SELECT g AS pick, 1 + (g * 7919) % 60000 AS id FROM generate_series(1, 5000) g; \
             ANALYZE stock; ANALYZE picks; \
             CREATE EXTENSION IF NOT EXISTS pg_prewarm; \
             SELECT pg_prewarm(indexrelid) FROM pg_index WHERE indrelid = 'stock'::regclass",
        )
        .expect("the stock could not be made");
    }

    /// 8,000 wide rows, about four to a page.
    fn ledger() {
        Spi::run(
            "CREATE TABLE ledger AS \
             SELECT g AS id, repeat(md5(g::text), 60) AS note FROM generate_series(1, 8000) g; \
             ANALYZE ledger",
        )
        .expect("the ledger could not be made");
    }

    /// The pages of relation `name` on disk.
    fn pages_of(name: &str) -> f64 {
        Spi::get_one::<i64>(&format!(
            "SELECT pg_relation_size('{name}') / current_setting('block_size')::int8"
        ))
        .unwrap()
        .unwrap() as f64
    }

    const DECEMBER: &str = "extract(month FROM at)::smallint = 12 \
                            AND at >= '2020-12-01' AND at < '2020-12-15'";

    /// Statements whose paths through the stock's B-trees take every way the B-tree prices.
    fn questions() -> Vec<String> {
        [
            "SELECT id FROM stock WHERE shelf = 7",
            "SELECT id FROM stock WHERE shelf = 7 AND bin < 300",
            "SELECT id FROM stock WHERE shelf IN (3, 7, 11)",
            "SELECT id FROM stock WHERE bin = 42",
            "SELECT * FROM stock WHERE id = 500",
            "SELECT id FROM stock WHERE lot IS NULL",
            "SELECT id FROM stock WHERE (shelf, bin) > (48, 900)",
            "SELECT id FROM stock WHERE lot = 3 AND bin < 100",
            "SELECT id FROM stock ORDER BY shelf, bin LIMIT 10",
            "SELECT id FROM stock WHERE shelf < 20",
            "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id",
            "SELECT count(*) FROM picks p JOIN stock s ON s.shelf = p.pick % 50 AND s.bin < 500",
        ]
        .iter()
        .map(|q| q.to_string())
        .chain([format!("SELECT id FROM stock WHERE {DECEMBER}")])
        .collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs().max(1.0)
    }

    fn setting(name: &str) -> f64 {
        Spi::get_one::<f64>(&format!("SELECT current_setting('{name}')::float8"))
            .unwrap()
            .unwrap()
    }

    /// The pages of `r`'s index its scans fetch over every loop, by the planner's own count: with
    /// the statement's tables sharing the cache, as the B-tree's own price counts them, and with the
    /// index alone in the cache, as a statement that reads no table would; and the loops they are
    /// shared among.
    fn fetched(r: &Record) -> (f64, f64, f64) {
        let scans = r.ours.sa_scans * r.loop_count;
        if scans > 1.0 {
            (
                planners_count(r, r.theirs[4] * scans, r.table_pages),
                planners_count(r, r.ours.pages * scans, 0.0),
                r.loop_count,
            )
        } else {
            (r.theirs[4], r.ours.pages, 1.0)
        }
    }

    /// The planner's own count of the pages of `r`'s index fetched by scans that touch `touched`
    /// pages in all, while the statement's tables hold `table_pages`.
    fn planners_count(r: &Record, touched: f64, table_pages: f64) -> f64 {
        let mut root: pg_sys::PlannerInfo = unsafe { std::mem::zeroed() };
        root.total_table_pages = table_pages;
        unsafe {
            pg_sys::index_pages_fetched(
                touched,
                r.index_pages as pg_sys::BlockNumber,
                r.index_pages,
                &mut root,
            )
        }
    }

    /// The pages of `r`'s index counted as cached: all of its pages, up to the whole of
    /// `effective_cache_size`.
    fn share(r: &Record) -> f64 {
        (unsafe { pg_sys::effective_cache_size } as f64).min(r.index_pages.max(1.0))
    }

    #[pg_test]
    fn with_no_change_the_surveyors_price_is_the_b_trees_own() {
        stock();
        let questions = questions();
        let queries: Vec<&str> = questions.iter().map(|q| q.as_str()).collect();
        let records = recorded(POSTGRES, &queries);
        assert!(records.len() >= 20, "{} paths priced", records.len());
        for r in &records {
            let ours = [
                r.ours.startup,
                r.ours.total,
                r.ours.selectivity,
                r.ours.correlation,
                r.ours.pages,
            ];
            for (o, t) in ours.iter().zip(r.theirs.iter()) {
                assert!(close(*o, *t), "{r:?}");
            }
        }
        // among them a skip scan, a list, repeated scans, a partial index and a unique equality
        assert!(records.iter().any(|r| r.ours.skipped), "no skip scan");
        assert!(
            records
                .iter()
                .any(|r| r.ours.sa_scans > 1.0 && !r.ours.skipped),
            "no list"
        );
        assert!(
            records.iter().any(|r| r.loop_count > 1.0),
            "no repeated scan"
        );
        assert!(records.iter().any(|r| r.index == "stock_bin_of_lot"));
        assert!(records
            .iter()
            .any(|r| r.index == "stock_pkey" && r.loop_count == 1.0 && r.ours.tuples == 1.0));
    }

    #[pg_test]
    fn only_a_table_that_carries_a_surveyor_has_its_b_trees_priced_by_it() {
        stock();
        Spi::run(
            "CREATE TABLE stock_plain AS SELECT * FROM stock; \
             CREATE INDEX stock_plain_shelf_bin ON stock_plain (shelf, bin); \
             ANALYZE stock_plain",
        )
        .unwrap();
        let records = recorded(
            SURVEYED,
            &[
                "SELECT id FROM stock_plain WHERE shelf = 7",
                "SELECT id FROM stock WHERE shelf = 7",
            ],
        );
        assert!(records.iter().all(|r| !r.index.starts_with("stock_plain")));
        assert!(records.iter().any(|r| r.index == "stock_shelf_bin"));
        Spi::run("DROP INDEX stock_key").unwrap();
        let records = recorded(SURVEYED, &["SELECT id FROM stock WHERE shelf = 7"]);
        assert!(records.is_empty(), "{records:?}");
    }

    #[pg_test]
    fn where_the_index_holds_every_condition_its_measured_share_stands_for_the_estimate() {
        stock();
        let records = recorded(
            MEASURED,
            &[
                &format!("SELECT id FROM stock WHERE {DECEMBER}"),
                "SELECT id FROM stock WHERE shelf = 7",
                "SELECT id FROM stock WHERE shelf = 7 AND bin > 100 AND bin > 200",
                "SELECT id FROM stock WHERE bin = 42",
                "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id",
                "SELECT count(*) FROM picks p JOIN stock s ON s.shelf = p.pick % 50 AND s.bin < 500",
            ],
        );
        let on_shelf = |quals: usize| {
            records
                .iter()
                .find(|r| r.index == "stock_shelf_bin" && r.loop_count == 1.0 && r.quals == quals)
                .unwrap_or_else(|| panic!("{records:?}"))
        };
        assert!(on_shelf(1).ours.measured.is_some(), "{:?}", on_shelf(1));
        let december = records
            .iter()
            .find(|r| r.index == "stock_month_at" && r.loop_count == 1.0 && r.quals == 3)
            .unwrap_or_else(|| panic!("{records:?}"));
        let share = december
            .ours
            .measured
            .expect("the month key measured nothing");
        let counted = Spi::get_one::<i64>(&format!("SELECT count(*) FROM stock WHERE {DECEMBER}"))
            .unwrap()
            .unwrap() as f64;
        let under_a_leaf = 60000.0 / (december.index_pages - 2.0);
        assert!(
            (share * 60000.0 - counted).abs() <= 2.0 * under_a_leaf,
            "{} measured, {counted} counted",
            share * 60000.0
        );
        // the planner's estimate is far from it, and the share stands for both of its own
        assert!(december.theirs[2] * 60000.0 < counted / 4.0, "{december:?}");
        assert_eq!(december.ours.selectivity, share);
        let tuples = (share * 60000.0).round_ties_even().max(1.0);
        assert_eq!(december.ours.tuples, tuples);
        // the leaves December lies on: entries of one width, so within a leaf of the entries'
        // share of the index's pages
        let average = (tuples * december.index_pages / december.index_tuples).ceil();
        assert!(
            (december.ours.pages - average).abs() <= 1.0,
            "{} pages priced, {average} by the entries' share, {december:?}",
            december.ours.pages
        );
        // what moves is the entries walked and the pages they lie on
        let plain = december.plain;
        assert_eq!(december.ours.startup, plain.startup);
        assert_eq!(december.ours.correlation, plain.correlation);
        let per_entry =
            setting("cpu_index_tuple_cost") + december.quals as f64 * setting("cpu_operator_cost");
        let moved = (december.ours.pages - plain.pages) * setting("random_page_cost")
            + (december.ours.tuples - plain.tuples) * plain.sa_scans * per_entry;
        assert!(
            close(december.ours.total, plain.total + moved),
            "{december:?}"
        );
        // a condition from another relation, or a condition the index does not hold beside those
        // it does, keeps the estimate
        let kept: Vec<&Record> = records
            .iter()
            .filter(|r| r.loop_count > 1.0)
            .chain([on_shelf(3)])
            .collect();
        assert!(kept.iter().any(|r| r.loop_count > 1.0), "{records:?}");
        for r in kept {
            assert!(r.ours.measured.is_none(), "{r:?}");
            assert_eq!(r.ours.selectivity, r.theirs[2], "{r:?}");
            assert!(close(r.ours.total, r.theirs[1]), "{r:?}");
        }
        // a condition under a leading column left open is measured through the values stepped
        let stepped = records
            .iter()
            .find(|r| r.index == "stock_shelf_bin" && r.ours.skipped && r.loop_count == 1.0)
            .unwrap_or_else(|| panic!("no skip scan: {records:?}"));
        let share = stepped
            .ours
            .measured
            .expect("the stepped key measured nothing");
        assert_eq!(stepped.ours.selectivity, share, "{stepped:?}");
        // each value stepped through lies inside one page, and takes the planner's share there
        let counted = Spi::get_one::<i64>("SELECT count(*) FROM stock WHERE bin = 42")
            .unwrap()
            .unwrap() as f64;
        assert!(
            (share * 60000.0 - counted).abs() <= 0.5 * counted,
            "{} measured, {counted} counted",
            share * 60000.0
        );
    }

    #[pg_test]
    fn where_the_index_measured_the_leaves_a_scan_walks_it_is_priced_by_those_leaves() {
        crate::conditions::tests::annotated();
        Spi::run(
            "CREATE INDEX annotated_order ON annotated USING surveyor (id); \
             CREATE EXTENSION IF NOT EXISTS pageinspect",
        )
        .unwrap();
        let per_entry = setting("cpu_index_tuple_cost") + setting("cpu_operator_cost");
        // the 7th category's leaves hold a few dozen of its parts, every other's a few hundred
        for cat in [7, 3] {
            let records = recorded(
                MEASURED,
                &[&format!("SELECT id FROM annotated WHERE cat = {cat}")],
            );
            let r = records
                .iter()
                .find(|r| r.index == "annotated_cat_id" && r.loop_count == 1.0)
                .unwrap_or_else(|| panic!("{records:?}"));
            assert!(r.ours.measured.is_some(), "{r:?}");
            let leaves = Spi::get_one::<i64>(&format!(
                "SELECT count(DISTINCT s.blkno) \
                 FROM bt_multi_page_stats('annotated_cat_id', 1, -1) s, \
                      LATERAL bt_page_items('annotated_cat_id', s.blkno::int) i \
                 WHERE s.type = 'l' AND i.data LIKE '{cat:02x} 00 00 00%' \
                   AND NOT (s.btpo_next <> 0 AND i.itemoffset = 1)"
            ))
            .unwrap()
            .unwrap() as f64;
            assert!(
                (r.ours.pages - leaves).abs() <= 1.0,
                "category {cat}: {} pages priced, {leaves} leaves, {r:?}",
                r.ours.pages
            );
            // the entries' share of the index's pages, which the leaves replace
            let average = (r.ours.tuples * r.index_pages / r.index_tuples).ceil();
            if cat == 7 {
                assert!(average < leaves / 2.0, "{average} against {leaves} leaves");
            }
            // only the pages move, at the price of a page from the disk
            let plain = r.plain;
            let moved = (r.ours.pages - plain.pages) * setting("random_page_cost")
                + (r.ours.tuples - plain.tuples) * plain.sa_scans * per_entry;
            assert!(close(r.ours.total, plain.total + moved), "{r:?}");
            assert_eq!(r.ours.startup, plain.startup, "{r:?}");
        }
    }

    #[pg_test]
    fn an_index_page_in_shared_buffers_costs_its_price_in_memory() {
        stock();
        let questions = questions();
        let queries: Vec<&str> = questions.iter().map(|q| q.as_str()).collect();
        let records = recorded(IN_MEMORY, &queries);
        let random = setting("random_page_cost");
        let mut moved = 0.0f64;
        for r in &records {
            let (theirs, ours, per) = fetched(r);
            assert_eq!(r.ours.resident, Some(1.0), "{r:?}");
            let saved = (theirs * random - ours * PAGE_IN_MEMORY) / per;
            moved = moved.max(saved);
            assert!(close(r.ours.total, r.theirs[1] - saved), "{r:?}");
            assert!(close(r.ours.startup, r.theirs[0]), "{r:?}");
            assert_eq!(r.ours.selectivity, r.theirs[2], "{r:?}");
            assert_eq!(r.ours.pages, r.theirs[4], "{r:?}");
        }
        assert!(moved > 10.0, "{moved}");
    }

    #[pg_test]
    fn an_index_has_all_its_own_pages_as_its_cache_share_whatever_the_other_tables_hold() {
        stock();
        ledger();
        let pages = pages_of("stock_pkey");
        // a cache exactly as large as the index
        Spi::run(&format!("SET LOCAL effective_cache_size = {pages}")).unwrap();
        let random = setting("random_page_cost");
        let mut walks = Vec::new();
        for query in [
            "SELECT id FROM stock WHERE id > 0",
            "SELECT s.id FROM stock s JOIN ledger l ON l.id = s.id WHERE s.id > 0",
        ] {
            let records = recorded(IN_MEMORY, &[query]);
            let walk = records
                .iter()
                .find(|r| r.index == "stock_pkey" && r.loop_count == 1.0 && r.quals == 1)
                .unwrap_or_else(|| panic!("{query}: {records:?}"))
                .clone();
            // the scan walks the whole index, and every page of it costs its price in memory
            assert_eq!(walk.index_pages, pages, "{walk:?}");
            assert_eq!(walk.theirs[4], pages, "{walk:?}");
            let saved = pages * (random - PAGE_IN_MEMORY);
            assert!(close(walk.ours.total, walk.theirs[1] - saved), "{walk:?}");
            walks.push(walk);
        }
        // beside a table many times the index's size, the walk keeps the same price
        assert!(walks[1].table_pages > 10.0 * pages, "{walks:?}");
        assert!(close(walks[1].ours.total, walks[0].ours.total), "{walks:?}");
    }

    #[pg_test]
    fn lookups_into_an_index_the_cache_holds_fetch_at_most_its_pages_whatever_else_is_read() {
        stock();
        ledger();
        let pages = pages_of("stock_pkey");
        // a cache exactly as large as the index
        Spi::run(&format!("SET LOCAL effective_cache_size = {pages}")).unwrap();
        let mut lookups = Vec::new();
        for query in [
            "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id",
            "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id JOIN ledger l ON l.id = p.pick",
        ] {
            let records = recorded(SURVEYED, &[query]);
            let lookup = records
                .iter()
                .find(|r| r.index == "stock_pkey" && r.loop_count == 5000.0)
                .unwrap_or_else(|| panic!("{query}: {records:?}"))
                .clone();
            // the lookups touch the index many times over, and fetch no more than its pages
            assert_eq!(lookup.index_pages, pages, "{lookup:?}");
            let touched = lookup.ours.pages * lookup.ours.sa_scans * lookup.loop_count;
            assert!(touched > 10.0 * pages, "{lookup:?}");
            assert!(lookup.ours.fetched <= pages, "{lookup:?}");
            lookups.push(lookup);
        }
        // beside a table many times the index's size, the lookups fetch as many pages
        assert!(lookups[1].table_pages > 10.0 * pages, "{lookups:?}");
        assert_eq!(
            lookups[1].ours.fetched, lookups[0].ours.fetched,
            "{lookups:?}"
        );
        assert!(
            close(lookups[1].ours.total, lookups[0].ours.total),
            "{lookups:?}"
        );
    }

    #[pg_test]
    fn lookups_past_shared_buffers_fetch_again_from_the_disk_and_within_them_cost_cpu_only() {
        stock();
        let random = setting("random_page_cost");
        let query = "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id";
        let lookup = |shared: Option<f64>| {
            SHARED_BUFFERS.set(shared);
            let records = recorded(IN_MEMORY, &[query]);
            SHARED_BUFFERS.set(None);
            records
                .iter()
                .find(|r| r.index == "stock_pkey" && r.loop_count == 5000.0)
                .unwrap_or_else(|| panic!("{records:?}"))
                .clone()
        };
        // the primary key within shared buffers: each of its pages costs its price in memory once,
        // however often the lookups touch it
        let inside = lookup(None);
        let (theirs, ours, per) = fetched(&inside);
        let touched = inside.ours.pages * inside.ours.sa_scans * inside.loop_count;
        assert!(touched > 10.0 * inside.index_pages, "{inside:?}");
        assert!(ours <= inside.index_pages, "{inside:?}");
        let priced = inside.theirs[1] + (ours * PAGE_IN_MEMORY - theirs * random) / per;
        assert!(close(inside.ours.total, priced), "{inside:?}");
        // shared buffers of twenty pages: past them, the lookups fetch pages first and again from
        // the disk, counted as the planner counts the pages fetched with twenty pages held
        let shared = 20.0;
        let past = lookup(Some(shared));
        let (theirs, _, per) = fetched(&past);
        let touched = past.ours.pages * past.ours.sa_scans * past.loop_count;
        Spi::run(&format!("SET LOCAL effective_cache_size = {shared}")).unwrap();
        let with_shared = planners_count(&past, touched, 0.0);
        Spi::run("RESET effective_cache_size").unwrap();
        let from_disk = with_shared - shared;
        assert!(from_disk > 10.0 * shared, "{from_disk} from the disk");
        let priced =
            past.theirs[1] + (shared * PAGE_IN_MEMORY + from_disk * random - theirs * random) / per;
        assert!(close(past.ours.total, priced), "{past:?}");
        assert!(past.ours.total > inside.ours.total, "{past:?} {inside:?}");
    }

    #[pg_test]
    fn repeated_scans_fetch_the_pages_the_planner_counts_with_the_index_alone_in_the_cache() {
        stock();
        ledger();
        let mut questions = questions();
        questions.push(
            "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id JOIN ledger l ON l.id = p.pick"
                .to_string(),
        );
        let queries: Vec<&str> = questions.iter().map(|q| q.as_str()).collect();
        let (mut fits, mut exceeds, mut differs) = (0, 0, 0);
        for cache in ["4GB", "1MB", "64kB"] {
            Spi::run(&format!("SET LOCAL effective_cache_size = '{cache}'")).unwrap();
            let records = recorded(SURVEYED, &queries);
            for r in &records {
                let (theirs, ours, _) = fetched(r);
                assert_eq!(r.ours.fetched, ours, "{cache} {r:?}");
                if r.ours.sa_scans * r.loop_count <= 1.0 {
                    continue;
                }
                if r.index_pages <= share(r) {
                    fits += 1;
                } else {
                    exceeds += 1;
                }
                if theirs != ours {
                    differs += 1;
                }
            }
        }
        // repeated scans of indexes that fit the cache and of indexes that do not, among them
        // counts the planner makes otherwise with the statement's tables sharing the cache
        assert!(
            fits > 0 && exceeds > 0 && differs > 0,
            "{fits} {exceeds} {differs}"
        );
    }

    /// The B-tree reads made while `query` was planned under every change, and the paths priced.
    fn reads_and_paths(query: &str) -> (Vec<(String, Option<u32>)>, Vec<Record>) {
        crate::conditions::tests::clear_noted();
        let records = recorded(SURVEYED, &[query]);
        (crate::conditions::tests::noted(), records)
    }

    #[pg_test]
    fn a_plan_reads_a_relations_conditions_once_whatever_the_paths_priced_and_the_next_plan_again()
    {
        stock();
        Spi::run(
            "SET LOCAL parallel_setup_cost = 0; SET LOCAL parallel_tuple_cost = 0; \
             SET LOCAL min_parallel_table_scan_size = 0; SET LOCAL min_parallel_index_scan_size = 0; \
             SET LOCAL max_parallel_workers_per_gather = 2",
        )
        .unwrap();
        let query = format!("SELECT id FROM stock WHERE {DECEMBER}");
        for plan in 0..2 {
            let (reads, records) = reads_and_paths(&query);
            let priced: Vec<&Record> = records
                .iter()
                .filter(|r| r.index == "stock_month_at" && r.quals == 3)
                .collect();
            assert!(priced.len() >= 2, "plan {plan}: {records:?}");
            assert_eq!(reads.len(), 1, "plan {plan}: {reads:?}");
            assert_eq!(reads[0].0, "stock_month_at", "plan {plan}");
            assert!(reads[0].1.is_some_and(|p| p > 0), "plan {plan}: {reads:?}");
            // every path takes the one measure, and so does the relation
            let share = priced[0]
                .ours
                .measured
                .expect("the month key measured nothing");
            assert!(
                priced.iter().all(|r| r.ours.measured == Some(share)),
                "plan {plan}: {priced:?}"
            );
            let line = Spi::get_one::<String>(&format!("EXPLAIN {query}"))
                .unwrap()
                .unwrap();
            let rows: f64 = line
                .split(" rows=")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|r| r.parse().ok())
                .unwrap_or_else(|| panic!("no estimate in {line}"));
            assert!(
                (rows - share * 60000.0).abs() <= 1.0,
                "plan {plan}: {line} against {share}"
            );
        }
    }

    #[pg_test]
    fn a_key_holding_only_conditions_another_index_counted_is_never_read_and_takes_their_share() {
        stock();
        Spi::run(
            "CREATE INDEX stock_shelf_month_at ON stock (shelf, (extract(month FROM at)::smallint), at); \
             ANALYZE stock",
        )
        .unwrap();
        let query = format!("SELECT id FROM stock WHERE {DECEMBER}");
        let (reads, records) = reads_and_paths(&query);
        let names: Vec<&str> = reads.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["stock_month_at"], "{reads:?}");
        let share = records
            .iter()
            .find(|r| r.index == "stock_month_at" && r.quals == 3)
            .and_then(|r| r.ours.measured)
            .unwrap_or_else(|| panic!("{records:?}"));
        let stepped: Vec<&Record> = records
            .iter()
            .filter(|r| r.index == "stock_shelf_month_at" && r.quals == 3)
            .collect();
        assert!(!stepped.is_empty(), "{records:?}");
        for r in stepped {
            assert_eq!(r.ours.measured, Some(share), "{r:?}");
        }
        // the key stepping through the shelves is read where no other index holds the conditions;
        // predicting that read asks for the shelves' statistics, whose list of fifty values would
        // spend the statistics' half of the statement's pages and leave the read too few, so they
        // list none here
        Spi::run("DROP INDEX stock_month_at; SET LOCAL default_statistics_target = 1").unwrap();
        let (reads, _) = reads_and_paths(&query);
        assert_eq!(reads.len(), 1, "{reads:?}");
        assert_eq!(reads[0].0, "stock_shelf_month_at");
        assert!(reads[0].1.is_some_and(|p| p > 0), "{reads:?}");
    }

    /// The pages of relation `name` held in shared buffers, by `pg_buffercache`.
    fn in_buffers(name: &str) -> f64 {
        Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM pg_buffercache \
             WHERE relfilenode = pg_relation_filenode('{name}') AND relforknumber = 0 \
               AND reldatabase = (SELECT oid FROM pg_database WHERE datname = current_database())"
        ))
        .unwrap()
        .unwrap() as f64
    }

    /// The price `r` counted for a first fetch from the disk at `random` a page read alone: the
    /// scan's first page by its descent, and the leaves it walks after it at `declared` where a price
    /// of a leaf walked in key order is declared, otherwise at `sequential` for the share of them
    /// that follow one another on disk and at `random` for the rest.
    fn from_disk(r: &Record, random: f64, sequential: f64, declared: Option<f64>) -> f64 {
        let walked = match declared {
            Some(declared) => declared,
            None => r
                .ours
                .order
                .map_or(random, |o| o * sequential + (1.0 - o) * random),
        };
        let descended = 1.0 / r.ours.pages.max(1.0);
        descended * random + (1.0 - descended) * walked
    }

    /// The price of `r`'s own pages where the index's share of shared buffers holds every page its
    /// scans touch: each page fetched first once, in memory for the share of them in shared buffers,
    /// otherwise from the disk; per loop.
    fn own_pages(r: &Record, random: f64, sequential: f64, declared: Option<f64>) -> f64 {
        let (_, touched, per) = fetched(r);
        let in_memory = (touched * r.ours.resident.unwrap_or(0.0)).min(touched);
        let from_disk = from_disk(r, random, sequential, declared);
        (in_memory * PAGE_IN_MEMORY + (touched - in_memory) * from_disk) / per
    }

    #[pg_test]
    fn an_index_in_shared_buffers_is_priced_in_memory_and_pushed_out_from_the_disk() {
        stock();
        Spi::run("CREATE EXTENSION IF NOT EXISTS pg_buffercache").unwrap();
        let (random, sequential) = (setting("random_page_cost"), setting("seq_page_cost"));
        // lookups into the primary key, and a walk of it whole
        let ways = [
            (
                "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id",
                5000.0,
            ),
            ("SELECT id FROM stock WHERE id > 0", 1.0),
        ];
        let priced = |query: &str, loops: f64| {
            let records = recorded(PAGES, &[query]);
            records
                .into_iter()
                .find(|r| r.index == "stock_pkey" && r.loop_count == loops && r.quals == 1)
                .unwrap_or_else(|| panic!("{query}"))
        };
        let pages = pages_of("stock_pkey");
        let mut resident = Vec::new();
        assert_eq!(in_buffers("stock_pkey"), pages);
        for (query, loops) in ways {
            let r = priced(query, loops);
            // every page in shared buffers: each first fetch at its price in memory
            assert_eq!(r.ours.resident, Some(1.0), "{r:?}");
            let (theirs, ours, per) = fetched(&r);
            let at = r.theirs[1] + (ours * PAGE_IN_MEMORY - theirs * random) / per;
            assert!(close(r.ours.total, at), "{r:?}");
            resident.push(r);
        }
        Spi::run("SELECT pg_buffercache_evict_relation('stock_pkey')").unwrap();
        assert_eq!(in_buffers("stock_pkey"), 0.0);
        for ((query, loops), inside) in ways.into_iter().zip(resident) {
            let r = priced(query, loops);
            // the few pages the plan's own reads brought in before the price; every other first
            // fetch from the disk, the scan's first page by its descent, the rest walked in order
            let share = r.ours.resident.expect("no share counted");
            assert!(share < 0.1, "{r:?}");
            let (theirs, _, per) = fetched(&r);
            let at = r.theirs[1] + own_pages(&r, random, sequential, None) - theirs * random / per;
            assert!(close(r.ours.total, at), "{r:?}");
            assert!(r.ours.total > inside.ours.total, "{r:?} {inside:?}");
            if r.ours.pages > 1.0 {
                // the key was built whole: its leaves follow one another on disk
                assert!(r.ours.order.is_some_and(|o| o > 0.9), "{r:?}");
            }
        }
        // a walk past a share of shared buffers of twenty pages: every page it touches is fetched
        // first, from the disk but for the few in shared buffers
        SHARED_BUFFERS.set(Some(20.0));
        let r = priced(ways[1].0, ways[1].1);
        SHARED_BUFFERS.set(None);
        assert!(r.ours.pages > 100.0, "{r:?}");
        let share = r.ours.resident.expect("no share counted");
        let in_memory = (r.ours.pages * share).min(20.0);
        let at = r.theirs[1]
            + in_memory * PAGE_IN_MEMORY
            + (r.ours.pages - in_memory) * from_disk(&r, random, sequential, None)
            - r.theirs[4] * random;
        assert!(close(r.ours.total, at), "{r:?}");
    }

    #[pg_test]
    fn a_page_in_shared_buffers_costs_the_same_whatever_the_tablespaces_page_costs() {
        stock();
        let query = "SELECT id FROM stock WHERE id > 0";
        let walk = || {
            recorded(PAGES, &[query])
                .into_iter()
                .find(|r| r.index == "stock_pkey" && r.loop_count == 1.0 && r.quals == 1)
                .unwrap_or_else(|| panic!("no walk of the primary key"))
        };
        let before = walk();
        Spi::run("ALTER TABLESPACE pg_default SET (seq_page_cost = 3, random_page_cost = 9)")
            .unwrap();
        let after = walk();
        // every page in shared buffers, each at its price in memory both times
        for (r, random) in [(&before, 4.0), (&after, 9.0)] {
            assert_eq!(r.ours.resident, Some(1.0), "{r:?}");
            let at = r.theirs[1] + r.ours.pages * PAGE_IN_MEMORY - r.theirs[4] * random;
            assert!(close(r.ours.total, at), "{r:?}");
        }
    }

    #[pg_test]
    fn every_index_page_from_outside_shared_buffers_is_priced_at_its_tablespaces_page_costs() {
        stock();
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             SELECT pg_buffercache_evict_relation(indexrelid) FROM pg_index \
             WHERE indrelid = 'stock'::regclass",
        )
        .unwrap();
        let questions = questions();
        let queries: Vec<&str> = questions.iter().map(|q| q.as_str()).collect();
        let defaults = (setting("random_page_cost"), setting("seq_page_cost"));
        let mut totals = Vec::new();
        for (random, sequential) in [defaults, (2.5, 0.5)] {
            if (random, sequential) != defaults {
                Spi::run(&format!(
                    "ALTER TABLESPACE pg_default \
                     SET (random_page_cost = {random}, seq_page_cost = {sequential})"
                ))
                .unwrap();
            }
            let records = recorded(PAGES, &queries);
            let (mut from_outside, mut walked) = (0, 0);
            for r in &records {
                let (theirs, ours, per) = fetched(r);
                if r.ours.resident.is_some_and(|s| ours * s < ours) {
                    from_outside += 1;
                }
                if r.ours.order.is_some() {
                    walked += 1;
                }
                let at =
                    r.theirs[1] + own_pages(r, random, sequential, None) - theirs * random / per;
                assert!(close(r.ours.total, at), "{random} {sequential} {r:?}");
                assert!(close(r.ours.startup, r.theirs[0]), "{r:?}");
            }
            assert!(from_outside > 5 && walked > 0, "{from_outside} {walked}");
            totals.push(records.iter().map(|r| r.ours.total).sum::<f64>());
        }
        assert!(totals[1] < totals[0], "{totals:?}");
    }

    #[pg_test]
    fn a_declared_price_of_a_leaf_walked_in_key_order_prices_the_leaves_walked_in_order() {
        stock();
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             SELECT pg_buffercache_evict_relation(indexrelid) FROM pg_index \
             WHERE indrelid = 'stock'::regclass",
        )
        .unwrap();
        let questions = questions();
        let mut questions = questions;
        questions.push("SELECT id FROM stock WHERE id > 0".to_string());
        let queries: Vec<&str> = questions.iter().map(|q| q.as_str()).collect();
        let (random, sequential) = (setting("random_page_cost"), setting("seq_page_cost"));
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT current_setting('warren_surveyor_pg.walked_leaf_page_cost')"
            ),
            Ok(Some("-1".to_string()))
        );
        // unset, a leaf walked in key order takes the tablespace's seq_page_cost where it follows on
        // disk; declared, every leaf walked takes the declared price, and no order is read
        let mut walks = Vec::new();
        for declared in [None, Some(0.3), Some(3.0)] {
            if let Some(d) = declared {
                Spi::run(&format!(
                    "SET LOCAL warren_surveyor_pg.walked_leaf_page_cost = {d}"
                ))
                .unwrap();
            }
            let records = recorded(PAGES, &queries);
            for r in &records {
                let (theirs, _, per) = fetched(r);
                let at = r.theirs[1] + own_pages(r, random, sequential, declared)
                    - theirs * random / per;
                assert!(close(r.ours.total, at), "{declared:?} {r:?}");
            }
            let walk = records
                .iter()
                .find(|r| r.index == "stock_pkey" && r.loop_count == 1.0 && r.ours.pages > 100.0)
                .unwrap_or_else(|| panic!("no walk of the primary key: {records:?}"))
                .clone();
            match declared {
                None => assert!(walk.ours.order.is_some_and(|o| o > 0.9), "{walk:?}"),
                Some(_) => assert_eq!(walk.ours.order, None, "{walk:?}"),
            }
            walks.push(walk.ours.total);
        }
        assert!(walks[1] < walks[0] && walks[0] < walks[2], "{walks:?}");
    }

    #[pg_test]
    fn leaves_that_do_not_follow_one_another_on_disk_are_walked_as_pages_read_alone() {
        // a key in place while its rows are written in scattered order, then the same key rebuilt
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             CREATE TABLE scattered (k int, v int); \
             CREATE INDEX scattered_k ON scattered (k); \
             INSERT INTO scattered SELECT (g * 7919) % 100000, g FROM generate_series(1, 100000) g; \
             CREATE INDEX scattered_order ON scattered USING surveyor (v); \
             ANALYZE scattered",
        )
        .unwrap();
        let (random, sequential) = (setting("random_page_cost"), setting("seq_page_cost"));
        let walk = || {
            Spi::run("SELECT pg_buffercache_evict_relation('scattered_k')").unwrap();
            let records = recorded(PAGES, &["SELECT k FROM scattered WHERE k >= 0"]);
            records
                .into_iter()
                .find(|r| r.index == "scattered_k" && r.loop_count == 1.0)
                .unwrap_or_else(|| panic!("no walk of scattered_k"))
        };
        let before = walk();
        let order = before.ours.order.expect("no order read");
        assert!(order < 0.2, "{before:?}");
        let (theirs, _, per) = fetched(&before);
        let at =
            before.theirs[1] + own_pages(&before, random, sequential, None) - theirs * random / per;
        assert!(close(before.ours.total, at), "{before:?}");
        Spi::run("REINDEX INDEX scattered_k; ANALYZE scattered").unwrap();
        let after = walk();
        assert!(after.ours.order.is_some_and(|o| o > 0.9), "{after:?}");
        assert!(after.ours.total < before.ours.total, "{after:?} {before:?}");
    }

    #[pg_test]
    fn a_walk_under_a_declared_price_takes_it_for_every_leaf_and_none_as_a_page_read_alone() {
        // a key in place while its rows are written in scattered order, so that few of its leaves
        // follow one another on disk
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             CREATE TABLE strewn (k int, v int); \
             CREATE INDEX strewn_k ON strewn (k); \
             INSERT INTO strewn SELECT (g * 7919) % 100000, g FROM generate_series(1, 100000) g; \
             CREATE INDEX strewn_order ON strewn USING surveyor (v); \
             ANALYZE strewn",
        )
        .unwrap();
        let random = setting("random_page_cost");
        let walk = || {
            Spi::run("SELECT pg_buffercache_evict_relation('strewn_k')").unwrap();
            let records = recorded(PAGES, &["SELECT k FROM strewn WHERE k >= 0"]);
            records
                .into_iter()
                .find(|r| r.index == "strewn_k" && r.loop_count == 1.0)
                .unwrap_or_else(|| panic!("no walk of strewn_k"))
        };
        assert!(walk().ours.order.is_some_and(|o| o < 0.2));
        let declared = 0.5;
        Spi::run(&format!(
            "SET LOCAL warren_surveyor_pg.walked_leaf_page_cost = {declared}"
        ))
        .unwrap();
        let r = walk();
        // the pages in shared buffers at the price in memory; of the rest, the descent's page read
        // alone and every leaf walked after it at the declared price
        let in_memory = r.ours.pages * r.ours.resident.unwrap_or(0.0);
        let descended = 1.0 / r.ours.pages;
        let own = in_memory * PAGE_IN_MEMORY
            + (r.ours.pages - in_memory) * (descended * random + (1.0 - descended) * declared);
        let at = r.theirs[1] + own - r.theirs[4] * random;
        assert!(
            close(r.ours.total, at),
            "{} priced, {at} declared: {r:?}",
            r.ours.total
        );
    }

    #[pg_test]
    fn a_path_whose_conditions_do_not_bound_its_scan_walks_every_entry_and_returns_the_measured_rows(
    ) {
        stock();
        // a key led by the row, too many values to step through, then the month and the instant
        Spi::run(
            "CREATE INDEX stock_id_month_at ON stock (id, (extract(month FROM at)::smallint), at); \
             ANALYZE stock",
        )
        .unwrap();
        let query = format!("SELECT id FROM stock WHERE {DECEMBER}");
        let records = recorded(SURVEYED, &[&query]);
        let month = records
            .iter()
            .find(|r| r.index == "stock_month_at" && r.quals == 3)
            .unwrap_or_else(|| panic!("{records:?}"));
        let share = month.ours.measured.expect("the month key measured nothing");
        // where the conditions bound the scan, it walks the entries measured
        assert_eq!(
            month.ours.tuples,
            (share * 60000.0).round_ties_even().max(1.0)
        );
        let walked: Vec<&Record> = records
            .iter()
            .filter(|r| r.index == "stock_id_month_at" && r.quals == 3)
            .collect();
        assert!(!walked.is_empty(), "{records:?}");
        for r in walked {
            assert!(!r.ours.skipped, "{r:?}");
            assert_eq!(r.ours.measured, Some(share), "{r:?}");
            assert_eq!(r.ours.selectivity, share, "{r:?}");
            assert_eq!(r.ours.tuples, 60000.0, "{r:?}");
            assert_eq!(r.ours.pages, r.index_pages, "{r:?}");
        }
    }

    /// Runs `f` with every drive taken to be busy for `share` of the time.
    fn busy<T>(share: f64, f: impl FnOnce() -> T) -> T {
        crate::drive::tests::BUSY_SHARE.set(Some(share));
        let out = f();
        crate::drive::tests::BUSY_SHARE.set(Some(0.0));
        out
    }

    #[pg_test]
    fn a_busy_drive_weighs_every_index_page_read_from_it() {
        stock();
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             SELECT pg_buffercache_evict_relation(indexrelid) FROM pg_index \
             WHERE indrelid = 'stock'::regclass",
        )
        .unwrap();
        let questions = questions();
        let queries: Vec<&str> = questions.iter().map(|q| q.as_str()).collect();
        let (random, sequential) = (setting("random_page_cost"), setting("seq_page_cost"));
        let mut totals = Vec::new();
        // planned with the page costs multiplied by the weight, a page in shared buffers at its price
        for (share, heat) in [(0.0, 1.0), (0.5, 2.0), (0.75, 4.0)] {
            let records = busy(share, || recorded(PAGES, &queries));
            let (random, sequential) = (random * heat, sequential * heat);
            let mut from_disk = 0;
            for r in &records {
                let (theirs, ours, per) = fetched(r);
                if r.ours.resident.is_some_and(|s| ours * s < ours) {
                    from_disk += 1;
                }
                let at =
                    r.theirs[1] + own_pages(r, random, sequential, None) - theirs * random / per;
                assert!(close(r.ours.total, at), "{share} {r:?}");
            }
            assert!(from_disk > 5, "{from_disk}");
            totals.push(records.iter().map(|r| r.ours.total).sum::<f64>());
        }
        assert!(totals[0] < totals[1] && totals[1] < totals[2], "{totals:?}");
    }

    thread_local! {
        static PATHS: RefCell<Vec<(String, f64)>> = const { RefCell::new(Vec::new()) };
    }
    static mut NEXT_PATHLIST: pg_sys::set_rel_pathlist_hook_type = None;

    /// Records each path of the stock's relation once the hooks before it have run: what it is, and
    /// its total price.
    #[pg_guard]
    unsafe extern "C-unwind" fn record_paths(
        root: *mut pg_sys::PlannerInfo,
        rel: *mut pg_sys::RelOptInfo,
        rti: pg_sys::Index,
        rte: *mut pg_sys::RangeTblEntry,
    ) {
        if let Some(next) = NEXT_PATHLIST {
            next(root, rel, rti, rte);
        }
        if (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
            || (*rel).reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
            || CStr::from_ptr(pg_sys::get_rel_name((*rte).relid)).to_bytes() != b"stock"
        {
            return;
        }
        for list in [(*rel).pathlist, (*rel).partial_pathlist] {
            for p in cells(list) {
                let path = p as *mut pg_sys::Path;
                let index = match (*path).pathtype {
                    pg_sys::NodeTag::T_IndexScan | pg_sys::NodeTag::T_IndexOnlyScan => {
                        let i = (*(path as *mut pg_sys::IndexPath)).indexinfo;
                        CStr::from_ptr(pg_sys::get_rel_name((*i).indexoid))
                            .to_string_lossy()
                            .into_owned()
                    }
                    _ => String::new(),
                };
                let kind = format!(
                    "{:?} {index} parameterized={} workers={} keys={}",
                    (*path).pathtype,
                    !(*path).param_info.is_null(),
                    (*path).parallel_workers,
                    cells((*path).pathkeys).len()
                );
                PATHS.with(|s| s.borrow_mut().push((kind, (*path).total_cost)));
            }
        }
    }

    /// Each path of the stock's relation while `query` was planned after `settings`: what it is,
    /// numbered among its kind, and its total price.
    fn paths_of(settings: &str, query: &str) -> Vec<(String, f64)> {
        PATHS.with(|s| s.borrow_mut().clear());
        unsafe {
            NEXT_PATHLIST = pg_sys::set_rel_pathlist_hook;
            pg_sys::set_rel_pathlist_hook = Some(record_paths);
        }
        let planned = Spi::run(&format!("{settings}; EXPLAIN {query}"));
        unsafe { pg_sys::set_rel_pathlist_hook = NEXT_PATHLIST };
        Spi::run(
            "RESET enable_seqscan; RESET enable_indexscan; RESET enable_bitmapscan; \
             RESET enable_hashjoin; RESET enable_mergejoin; RESET parallel_setup_cost; \
             RESET parallel_tuple_cost; RESET min_parallel_table_scan_size; \
             RESET min_parallel_index_scan_size; RESET max_parallel_workers_per_gather",
        )
        .unwrap();
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        let mut seen: Vec<(String, f64)> = Vec::new();
        for (kind, total) in PATHS.with(|s| s.borrow().clone()) {
            let n = seen.iter().filter(|(k, _)| k.starts_with(&kind)).count();
            seen.push((format!("{kind} #{n} in {query}"), total));
        }
        seen
    }

    #[pg_test]
    fn a_page_in_shared_buffers_costs_the_same_however_busy_the_drive() {
        stock();
        let random = setting("random_page_cost");
        let walk = |share: f64| {
            busy(share, || {
                recorded(PAGES, &["SELECT id FROM stock WHERE id > 0"])
            })
            .into_iter()
            .find(|r| r.index == "stock_pkey" && r.loop_count == 1.0 && r.quals == 1)
            .unwrap_or_else(|| panic!("no walk of the primary key"))
        };
        for (share, heat) in [(0.0, 1.0), (0.5, 2.0), (0.75, 4.0)] {
            let r = walk(share);
            // every page in shared buffers, each at its price in memory, where the B-tree's own
            // price takes the weighed page cost
            assert_eq!(r.ours.resident, Some(1.0), "{r:?}");
            let at = r.theirs[1] + r.ours.pages * PAGE_IN_MEMORY - r.theirs[4] * random * heat;
            assert!(close(r.ours.total, at), "{share} {r:?}");
        }
    }

    #[pg_test]
    fn a_busy_drive_weighs_exactly_the_page_part_of_every_path_through_the_table() {
        stock();
        let parallel = "SET LOCAL parallel_setup_cost = 0; SET LOCAL parallel_tuple_cost = 0; \
                        SET LOCAL min_parallel_table_scan_size = 0; \
                        SET LOCAL min_parallel_index_scan_size = 0; \
                        SET LOCAL max_parallel_workers_per_gather = 2";
        let parallel_table = format!(
            "{parallel}; SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off"
        );
        let parallel_bitmap =
            format!("{parallel}; SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off");
        let ways = [
            (
                "SET LOCAL enable_indexscan = off; SET LOCAL enable_bitmapscan = off",
                "SELECT * FROM stock WHERE lot = 5",
            ),
            (parallel_table.as_str(), "SELECT * FROM stock WHERE lot = 5"),
            (
                parallel_bitmap.as_str(),
                "SELECT * FROM stock WHERE lot = 5",
            ),
            (
                "SET LOCAL enable_seqscan = off; SET LOCAL enable_indexscan = off",
                "SELECT * FROM stock WHERE lot = 5",
            ),
            (
                "SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off",
                "SELECT * FROM stock WHERE shelf = 7",
            ),
            (
                "SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off",
                "SELECT shelf, bin FROM stock WHERE shelf = 7",
            ),
            (
                "SET LOCAL enable_hashjoin = off; SET LOCAL enable_mergejoin = off",
                "SELECT s.bin FROM picks p JOIN stock s ON s.id = p.id",
            ),
        ];
        let all = |share: f64, free: bool| {
            Spi::run(if free {
                "ALTER TABLESPACE pg_default SET (random_page_cost = 0, seq_page_cost = 0)"
            } else {
                "ALTER TABLESPACE pg_default RESET (random_page_cost, seq_page_cost)"
            })
            .unwrap();
            busy(share, || {
                ways.iter()
                    .enumerate()
                    .flat_map(|(w, (settings, query))| {
                        paths_of(settings, query)
                            .into_iter()
                            .map(move |(k, t)| (format!("way {w}: {k}"), t))
                    })
                    .collect::<Vec<_>>()
            })
        };
        // each path at the page costs and with the pages free, the drive idle and half busy
        let (priced, free) = (all(0.0, false), all(0.0, true));
        let (priced_busy, free_busy) = (all(0.5, false), all(0.5, true));
        let at = |paths: &[(String, f64)], kind: &str| {
            paths.iter().find(|(k, _)| k == kind).map(|(_, t)| *t)
        };
        let mut weighed = Vec::new();
        for (kind, total) in &priced {
            let (Some(f), Some(pb), Some(fb)) = (
                at(&free, kind),
                at(&priced_busy, kind),
                at(&free_busy, kind),
            ) else {
                continue;
            };
            let pages = total - f;
            // the rest of the price stays, and the drive's part doubles
            assert!((fb - f).abs() <= 1e-9 * f.max(1.0), "{kind}: {f} {fb}");
            assert!(
                (pb - fb - 2.0 * pages).abs() <= 1e-9 * pb.max(1.0),
                "{kind}: {total} {f} {pb} {fb}"
            );
            if pages > 0.0 {
                weighed.push(kind.clone());
            }
        }
        for wanted in [
            "T_SeqScan  parameterized=false workers=0",
            "T_BitmapHeapScan  parameterized=false workers=0",
            "T_IndexScan stock_shelf_bin parameterized=false",
            "T_IndexOnlyScan stock_shelf_bin parameterized=false",
            "T_IndexScan stock_pkey parameterized=true",
        ] {
            assert!(
                weighed.iter().any(|k| k.contains(wanted)),
                "{wanted}: {weighed:?}"
            );
        }
        // parallel scans of the table
        for wanted in ["T_SeqScan", "T_BitmapHeapScan"] {
            assert!(
                weighed
                    .iter()
                    .any(|k| k.contains(wanted) && !k.contains("workers=0")),
                "{wanted}: {weighed:?}"
            );
        }
    }

    #[pg_test]
    fn a_busy_drive_moves_the_choice_from_the_faster_plan_to_the_one_that_reads_less_from_it() {
        // 60,000 rows written all-visible, its primary key out of shared buffers, a surveyor, and
        // 3,000 picks of the first 300 rows
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             CREATE TABLE lean (id int, pad int DEFAULT 0); \
             COPY lean (id) FROM PROGRAM 'seq 1 60000' WITH (FREEZE); \
             ALTER TABLE lean ADD PRIMARY KEY (id); \
             CREATE INDEX lean_order ON lean USING surveyor (id); \
             CREATE TABLE few AS SELECT 1 + g % 300 AS id FROM generate_series(1, 3000) g; \
             ANALYZE lean; ANALYZE few; \
             SELECT pg_buffercache_evict_relation('lean_pkey'); \
             SET LOCAL max_parallel_workers_per_gather = 0; \
             SET LOCAL enable_mergejoin = off; SET LOCAL enable_memoize = off",
        )
        .unwrap();
        let query = "SELECT count(*) FROM few p JOIN lean l ON l.id = p.id";
        let price = |plan: &str| -> f64 {
            plan.lines()
                .next()
                .and_then(|l| l.split("..").nth(1))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|r| r.parse().ok())
                .unwrap_or_else(|| panic!("{plan}"))
        };
        let way = |share: f64, settings: &str| {
            busy(share, || {
                Spi::run(settings).unwrap();
                let plan = texts(&format!("EXPLAIN {query}")).join("\n");
                Spi::run("RESET enable_hashjoin; RESET enable_nestloop").unwrap();
                plan
            })
        };
        let chosen = |share: f64| {
            let plan = way(share, "SELECT 1");
            if plan.contains("Nested Loop") {
                "lookups"
            } else if plan.contains("Hash Join") {
                "hash"
            } else {
                panic!("{plan}")
            }
        };
        // idle, the lookups into the key, priced lower; busy, the hash join, which reads less from
        // the drive
        let (lookups, table) = (
            way(0.0, "SET LOCAL enable_hashjoin = off"),
            way(0.0, "SET LOCAL enable_nestloop = off"),
        );
        assert!(price(&lookups) < price(&table), "{lookups}\n{table}");
        assert_eq!(chosen(0.0), "lookups");
        assert_eq!(chosen(0.75), "hash");
        assert_eq!(chosen(0.9), "hash");
    }

    #[pg_test]
    fn a_busy_drive_chooses_the_scan_that_reads_fewer_pages_from_it() {
        // 60,000 rows written all-visible, a primary key of fewer pages than the table out of shared
        // buffers, and a surveyor
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_buffercache; \
             CREATE TABLE lean (id int, pad int DEFAULT 0); \
             COPY lean (id) FROM PROGRAM 'seq 1 60000' WITH (FREEZE); \
             ALTER TABLE lean ADD PRIMARY KEY (id); \
             CREATE INDEX lean_order ON lean USING surveyor (id); \
             ANALYZE lean; \
             SELECT pg_buffercache_evict_relation('lean_pkey'); \
             SET LOCAL max_parallel_workers_per_gather = 0",
        )
        .unwrap();
        let query = "SELECT count(*) FROM lean WHERE id > 0";
        let plan = |share: f64, settings: &str| {
            busy(share, || {
                Spi::run(settings).unwrap();
                let plan = texts(&format!("EXPLAIN {query}")).join("\n");
                Spi::run(
                    "RESET enable_seqscan; RESET enable_indexscan; RESET enable_indexonlyscan",
                )
                .unwrap();
                plan
            })
        };
        let price = |plan: &str| -> f64 {
            plan.lines()
                .next()
                .and_then(|l| l.split("..").nth(1))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|r| r.parse().ok())
                .unwrap_or_else(|| panic!("{plan}"))
        };
        // idle, the scan of the table, priced lower
        assert!(plan(0.0, "SELECT 1").contains("Seq Scan on lean"));
        // busy, the scan of the key, which reads fewer pages from the drive, priced lower and chosen
        let (table, key) = (
            plan(
                0.75,
                "SET LOCAL enable_indexscan = off; SET LOCAL enable_indexonlyscan = off",
            ),
            plan(0.75, "SET LOCAL enable_seqscan = off"),
        );
        assert!(price(&key) < price(&table), "{key}\n{table}");
        let chosen = plan(0.75, "SELECT 1");
        assert!(
            chosen.contains("Index Only Scan using lean_pkey"),
            "{chosen}\n{key}\n{table}"
        );
        assert_eq!(price(&chosen), price(&key), "{chosen}\n{key}");
    }

    fn texts(sql: &str) -> Vec<String> {
        crate::tests::texts(sql)
    }
}
