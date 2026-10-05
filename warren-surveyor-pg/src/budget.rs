// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The pages the surveyor reads while a statement is planned, held to one budget for the statement.
//!
//! Every read the surveyor makes while a statement is planned draws on one budget: the pages of the
//! tables the statement reads, each counted once, the tables of its subqueries, its sublinks and its
//! WITH queries among them, and every member of an inheritance parent or a partitioned table read
//! with its members, never a table an INSERT only writes. Those pages are the scan the reads are
//! there to cancel. A table this session holds a lock on is counted as it stands, any other as
//! VACUUM or ANALYZE last counted it, so that counting waits for no lock and locks no partition the
//! planner prunes. The tables are those the statement names when its planning starts; they are
//! counted when the first read asks what is left. `warren_surveyor_pg.planning_read_limit` lowers
//! the budget, and at 0 no index page is read while planning.
//!
//! Each index page read draws one page, found in shared buffers or read in. A B-tree's own scan of
//! the rows' addresses matched across indexes, and the run of a recursive query, draw the pages
//! PostgreSQL counts for them, and stop once those pass what is left. A read whose size is known
//! before it starts does not start where it would pass what is left; any other read stops at the
//! page that would pass it and gives up its measure, so that PostgreSQL's own estimate stands for
//! it. A read the budget stops or keeps from starting is noted at DEBUG1. The pass over shared
//! buffers' headers that prices a B-tree reads no page and draws none.
//!
//! A subtransaction rolled back while a statement is planned, a respelling's among them, gives back
//! what was drawn inside it (`round`), so the planning after it has what it would have had; the
//! subtransaction of one of the surveyor's own reads keeps what it drew.
//!
//! A planning that does not enter through the planner's hook, such as a direct call of
//! `standard_planner`, keeps a budget of its own the same way: counted from the tables its levels
//! name the first time one of the surveyor's hooks runs for it, shared by any planning made inside
//! it, and forgotten once its last level is planned. It keeps what the surveyor measures as a round
//! does, so that it reads what a round would. Outside any planning no budget is kept: a read is
//! held to its own table's pages alone, and a setting of 0 still reads no page.

use crate::query::{cells, oids, surveyor_am};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::ptr::null_mut;

extern "C-unwind" {
    fn find_all_inheritors(
        parent: pg_sys::Oid,
        lockmode: pg_sys::LOCKMODE,
        numparents: *mut *mut pg_sys::List,
    ) -> *mut pg_sys::List;
}

thread_local! {
    /// The budget of the statement being planned; none outside its planning.
    static BUDGET: RefCell<Option<Budget>> = const { RefCell::new(None) };
    /// How many readings of the planner's column statistics are under way.
    static STATISTICS: Cell<u32> = const { Cell::new(0) };
    /// Outside a round, the planning one of the surveyor's hooks runs for: its planner's global
    /// state, while the hook runs; 0 otherwise.
    static PLANNING: Cell<usize> = const { Cell::new(0) };
}

/// Whether the planner's column statistics are being read.
fn reading_statistics() -> bool {
    STATISTICS.get() > 0
}

/// One statement's budget.
#[derive(Clone)]
struct Budget {
    /// The planning it belongs to outside a round, by its planner's global state; 0 for a round's.
    planning: usize,
    /// Each table the statement reads, and whether it is read with its members.
    tables: Vec<(pg_sys::Oid, bool)>,
    /// The rows of the statement's tables and the pages the surveyor may read, once counted.
    counted: Option<Counted>,
    /// The pages drawn so far.
    drawn: u64,
    /// The pages the reads of column statistics have drawn so far.
    statistics: u64,
    /// The pages drawn from each index.
    by_index: HashMap<pg_sys::Oid, u64>,
    /// Each read whose stop (true) or skip (false) has been noted.
    noted: Vec<(String, bool)>,
}

/// The rows the planner takes the statement's tables to hold, the pages the surveyor may read while
/// the statement is planned, and the half of them the reads of column statistics may read.
#[derive(Clone, Copy)]
struct Counted {
    limit: u64,
    half: u64,
}

impl Budget {
    fn new(tables: Vec<(pg_sys::Oid, bool)>, planning: usize) -> Budget {
        Budget {
            planning,
            tables,
            counted: None,
            drawn: 0,
            statistics: 0,
            by_index: HashMap::new(),
            noted: Vec::new(),
        }
    }

    /// The rows of the statement's tables and the pages the surveyor may read, counted the first
    /// time they are asked for.
    unsafe fn counted(&mut self) -> Counted {
        if let Some(c) = self.counted {
            return c;
        }
        let (pages, _) = count(&self.tables);
        let pages = pages.max(0.0) as u64;
        let limit = match crate::price::planning_read_limit() {
            0 => 0,
            setting if setting > 0 => pages.min(setting as u64),
            _ => pages,
        };
        let c = Counted {
            limit,
            half: limit / 2,
        };
        self.counted = Some(c);
        c
    }

    /// The pages the surveyor may still read, of the whole.
    unsafe fn whole_left(&mut self) -> u64 {
        self.counted().limit.saturating_sub(self.drawn)
    }

    /// The pages the reads of column statistics may still read, of their half.
    unsafe fn statistics_left(&mut self) -> u64 {
        self.counted().half.saturating_sub(self.statistics)
    }

    /// The pages the read under way may still read: of the whole, and, while column statistics are
    /// read, of their half.
    unsafe fn left(&mut self) -> u64 {
        let whole = self.whole_left();
        if reading_statistics() {
            whole.min(self.statistics_left())
        } else {
            whole
        }
    }

    /// Whether the statistics' half, not the whole, is what holds the read under way.
    unsafe fn held_to_half(&mut self) -> bool {
        reading_statistics() && self.statistics_left() < self.whole_left()
    }

    /// Draws `pages` read.
    fn take(&mut self, pages: u64) {
        self.drawn += pages;
        if reading_statistics() {
            self.statistics += pages;
        }
    }

    /// Whether the read `what` has not been noted for `stopped` yet in this round; notes it.
    fn first(&mut self, what: &str, stopped: bool) -> bool {
        if self.noted.iter().any(|(w, s)| w == what && *s == stopped) {
            return false;
        }
        self.noted.push((what.to_string(), stopped));
        true
    }
}

/// Starts the budget of the statement `parse`, whose planning starts now: none where this database
/// has no surveyor.
pub(crate) unsafe fn begin(parse: *mut pg_sys::Query) {
    let tables = if parse.is_null() || surveyor_am() == pg_sys::InvalidOid {
        None
    } else {
        Some(tables_of(&[parse]))
    };
    BUDGET.with(|b| *b.borrow_mut() = tables.map(|tables| Budget::new(tables, 0)));
}

/// The tables the queries `queries` read, never a table an INSERT only writes.
unsafe fn tables_of(queries: &[*mut pg_sys::Query]) -> Vec<(pg_sys::Oid, bool)> {
    let mut tables = Tables {
        read: Vec::new(),
        written: Vec::new(),
    };
    for &q in queries {
        tables_walker(
            q as *mut pg_sys::Node,
            &mut tables as *mut Tables as *mut c_void,
        );
    }
    tables.read
}

/// One of the surveyor's hooks running for a planning; restores what it replaced when it ends.
pub(crate) struct Planning(Option<usize>);

impl Drop for Planning {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            PLANNING.set(previous);
        }
    }
}

/// Marks one of the surveyor's hooks running for the planning `root` is a level of. Inside a round
/// the round's budget holds; outside one, the planning's reads draw on a budget of its own, counted
/// the first time a hook runs for it from the tables its levels name, its sublinks planned so far
/// among them, and shared by a planning made while the hook runs.
pub(crate) unsafe fn planning(root: *mut pg_sys::PlannerInfo) -> Planning {
    if root.is_null() || (*root).glob.is_null() || crate::round::in_round() || PLANNING.get() != 0 {
        return Planning(None);
    }
    let glob = (*root).glob as usize;
    let kept = BUDGET.with(|b| b.borrow().as_ref().is_some_and(|b| b.planning == glob));
    if !kept {
        // what an earlier planning kept is not this one's
        crate::round::forget_all();
        let tables = (surveyor_am() != pg_sys::InvalidOid).then(|| {
            let mut top = root;
            while !(*top).parent_root.is_null() {
                top = (*top).parent_root;
            }
            let mut queries = vec![(*top).parse];
            // a sublink planned already keeps its query only in its own planner state
            for sub in cells((*(*root).glob).subroots) {
                let sub = sub as *mut pg_sys::PlannerInfo;
                if !sub.is_null() {
                    queries.push((*sub).parse);
                }
            }
            tables_of(&queries)
        });
        BUDGET.with(|b| *b.borrow_mut() = tables.map(|t| Budget::new(t, glob)));
    }
    Planning(Some(PLANNING.replace(glob)))
}

/// Whether one of the surveyor's hooks runs for a planning outside a round.
pub(crate) fn planning_outside_round() -> bool {
    PLANNING.get() != 0
}

/// The statement's budget as it stands now, to be put back by `put_back`.
pub(crate) struct Saved(Option<Budget>);

/// The statement's budget as it stands now.
pub(crate) fn saved() -> Saved {
    Saved(BUDGET.with(|b| b.borrow().clone()))
}

/// Puts back the budget `saved` holds.
pub(crate) fn put_back(saved: Saved) {
    BUDGET.with(|b| *b.borrow_mut() = saved.0);
}

/// Forgets the budget once the statement's planning ends.
pub(crate) fn forget() {
    let budget = BUDGET.with(|b| b.borrow_mut().take());
    #[cfg(any(test, feature = "pg_test"))]
    if let Some(b) = &budget {
        tests::LAST.set(Some((b.drawn, b.counted.map(|c| c.limit))));
    }
    drop(budget);
}

/// `f` on the statement's budget; none outside a statement's planning.
fn with<R>(f: impl FnOnce(&mut Budget) -> R) -> Option<R> {
    BUDGET.with(|b| {
        let mut b = b.borrow_mut();
        let budget = b.as_mut()?;
        (budget.planning == 0 || budget.planning == PLANNING.get()).then(|| f(budget))
    })
}

/// The pages the surveyor may still read while the statement is planned; outside a statement's
/// planning, none where the setting is 0 and no limit otherwise.
pub(crate) fn left() -> u32 {
    match with(|b| unsafe { b.left() }) {
        Some(left) => left.min(u32::MAX as u64) as u32,
        None if crate::price::planning_read_limit() == 0 => 0,
        None => u32::MAX,
    }
}

/// Draws one page of the index `index` for a read about to read it. False, and the read stops,
/// where no page is left, or, while column statistics are read, none of their half; noted at DEBUG1
/// once for the index in the round.
pub(crate) unsafe fn draw(index: pg_sys::Oid) -> bool {
    let drawn = with(|b| {
        if b.left() == 0 {
            let what = index_name(index);
            let limit = b.counted().limit;
            if b.held_to_half() {
                let first = b.first(&format!("{what} statistics"), true);
                return Err(first.then(|| Stop::Half {
                    what,
                    read: b.statistics,
                    left: b.whole_left(),
                    limit,
                }));
            }
            let read = b.by_index.get(&index).copied().unwrap_or(0);
            let first = b.first(&what, true);
            return Err(first.then_some(Stop::Whole { what, read, limit }));
        }
        b.take(1);
        *b.by_index.entry(index).or_insert(0) += 1;
        Ok(())
    });
    match drawn {
        Some(Ok(())) => true,
        Some(Err(stop)) => {
            match stop {
                Some(Stop::Whole { what, read, limit }) => note_stop(&what, read, 0, limit),
                Some(Stop::Half {
                    what,
                    read,
                    left,
                    limit,
                }) => note_half(&what, read, left, limit),
                None => {}
            }
            false
        }
        None => crate::price::planning_read_limit() != 0,
    }
}

/// A read the budget stops, to be noted: at the whole, with the pages of the index read in the
/// round; or at the statistics' half, with the pages the reads of statistics read and those left
/// of the whole.
enum Stop {
    Whole {
        what: String,
        read: u64,
        limit: u64,
    },
    Half {
        what: String,
        read: u64,
        left: u64,
        limit: u64,
    },
}

/// Whether a read of `needed` pages of the index `index`, known before it starts, fits what is
/// left. Where it does not, it does not start, noted at DEBUG1 once for the index in the round.
pub(crate) unsafe fn fits(index: pg_sys::Oid, needed: f64) -> bool {
    fits_read(needed, || index_name(index))
}

/// As `fits`, for the read `what` names.
pub(crate) unsafe fn fits_read(needed: f64, what: impl FnOnce() -> String) -> bool {
    let left = left();
    if needed <= left as f64 {
        return true;
    }
    let what = what();
    let noted = with(|b| {
        let half = b.held_to_half();
        let key = if half {
            format!("{what} statistics")
        } else {
            what.clone()
        };
        (b.first(&key, false), half, b.counted())
    });
    match noted {
        Some((true, false, c)) => note_skip(&what, needed, left as u64, c.limit),
        Some((true, true, c)) => note_half_skip(&what, needed, left as u64, c.half),
        _ => {}
    }
    false
}

/// The name of the index `index` and of the table it is on.
pub(crate) unsafe fn index_name(index: pg_sys::Oid) -> String {
    let table = pg_sys::IndexGetRelation(index, true);
    format!("{} on {}", rel_name(index), rel_name(table))
}

/// The name of the relation `relid`, or its OID where it has none.
unsafe fn rel_name(relid: pg_sys::Oid) -> String {
    let n = pg_sys::get_rel_name(relid);
    if n.is_null() {
        relid.to_u32().to_string()
    } else {
        CStr::from_ptr(n).to_string_lossy().into_owned()
    }
}

/// `n` pages, in words.
fn pages(n: f64) -> String {
    if n == 1.0 {
        "1 page".to_string()
    } else {
        format!("{n:.0} pages")
    }
}

/// Notes at DEBUG1 a read of `what` the budget stopped after it read `read` pages, with `left` of
/// the statement's `limit` pages left when it started.
unsafe fn note_stop(what: &str, read: u64, left: u64, limit: u64) {
    if pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
        debug1!(
            "surveyor: {what}: read stopped at the planning-read limit after {}, {left} of the \
             statement's {} left",
            pages(read as f64),
            pages(limit as f64)
        );
    }
}

/// Notes at DEBUG1 a read of `what`, of `needed` pages, that the budget kept from starting with
/// `left` of the statement's `limit` pages left.
unsafe fn note_skip(what: &str, needed: f64, left: u64, limit: u64) {
    if pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
        debug1!(
            "surveyor: {what}: read of {} not started at the planning-read limit, {left} of the \
             statement's {} left",
            pages(needed.ceil()),
            pages(limit as f64)
        );
    }
}

/// Notes at DEBUG1 a read of `what`, made for column statistics, stopped once the reads of
/// statistics had read `read` pages, their half of the statement's `limit`, with `left` of the
/// whole left for the statement's other reads.
unsafe fn note_half(what: &str, read: u64, left: u64, limit: u64) {
    if pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
        debug1!(
            "surveyor: {what}: statistics read stopped at half the planning-read limit after {}, \
             {left} of the statement's {} left",
            pages(read as f64),
            pages(limit as f64)
        );
    }
}

/// Notes at DEBUG1 a read of `what`, made for column statistics, of `needed` pages, that their half
/// of the budget, `half`, kept from starting with `left` of it left.
unsafe fn note_half_skip(what: &str, needed: f64, left: u64, half: u64) {
    if pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
        debug1!(
            "surveyor: {what}: statistics read of {} not started at half the planning-read limit, \
             {left} of the statistics' {} left",
            pages(needed.ceil()),
            pages(half as f64)
        );
    }
}

/// The pages of the tables `tables` hold, and the rows the planner takes them to hold, each table
/// counted once, with every member of a table read with its members. A table this session holds a
/// lock on is counted as it stands, and any other as VACUUM or ANALYZE last counted it.
unsafe fn count(tables: &[(pg_sys::Oid, bool)]) -> (f64, f64) {
    let (mut pages, mut rows) = (0.0, 0.0);
    let mut seen = Vec::new();
    let mut add = |relid: pg_sys::Oid| {
        if seen.contains(&relid) {
            return;
        }
        seen.push(relid);
        let Some(class) = class_of(relid) else {
            return;
        };
        if !stored(class.kind) {
            return;
        }
        if pg_sys::CheckRelationOidLockedByMe(
            relid,
            pg_sys::AccessShareLock as pg_sys::LOCKMODE,
            true,
        ) {
            let rel = pg_sys::relation_open(relid, pg_sys::NoLock as pg_sys::LOCKMODE);
            pages += pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
                as f64;
            let (mut p, mut r, mut visible) = (0, 0.0, 0.0);
            pg_sys::estimate_rel_size(rel, null_mut(), &mut p, &mut r, &mut visible);
            rows += r;
            pg_sys::relation_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        } else {
            pages += class.pages.max(0) as f64;
            rows += class.rows.max(0.0) as f64;
        }
    };
    for &(relid, _) in tables {
        add(relid);
    }
    for &(relid, members) in tables {
        if !members {
            continue;
        }
        let list = pg_sys::ffi::pg_guard_ffi_boundary(|| {
            find_all_inheritors(relid, pg_sys::NoLock as pg_sys::LOCKMODE, null_mut())
        });
        for member in oids(list) {
            add(member);
        }
    }
    (pages, rows)
}

/// Whether a relation of kind `kind` keeps rows in pages of its own.
fn stored(kind: u8) -> bool {
    kind == pg_sys::RELKIND_RELATION
        || kind == pg_sys::RELKIND_MATVIEW
        || kind == pg_sys::RELKIND_TOASTVALUE
}

/// What `pg_class` says of a relation: its kind, and its pages and rows as VACUUM or ANALYZE last
/// counted them.
struct Class {
    kind: u8,
    pages: i32,
    rows: f32,
}

/// What `pg_class` says of the relation `relid`; none where it is not there.
unsafe fn class_of(relid: pg_sys::Oid) -> Option<Class> {
    let tuple = pg_sys::SearchSysCache1(
        pg_sys::SysCacheIdentifier::RELOID as i32,
        pg_sys::Datum::from(relid),
    );
    if tuple.is_null() {
        return None;
    }
    let form = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_class>(tuple);
    let class = Class {
        kind: (*form).relkind as u8,
        pages: (*form).relpages,
        rows: (*form).reltuples,
    };
    pg_sys::ReleaseSysCache(tuple);
    Some(class)
}

/// The tables a statement names.
struct Tables {
    /// Each table read, and whether it is read with its members.
    read: Vec<(pg_sys::Oid, bool)>,
    /// The entries naming a table an INSERT writes, and its new rows under ON CONFLICT.
    written: Vec<*mut pg_sys::RangeTblEntry>,
}

/// Gathers into the `Tables` at `context` every table the queries under `node` read.
#[pg_guard]
unsafe extern "C-unwind" fn tables_walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    match (*node).type_ {
        pg_sys::NodeTag::T_Query => {
            let q = node as *mut pg_sys::Query;
            if (*q).commandType == pg_sys::CmdType::CMD_INSERT {
                let rtable = cells((*q).rtable);
                let tables = &mut *(context as *mut Tables);
                let mut at = vec![(*q).resultRelation];
                if !(*q).onConflict.is_null() {
                    at.push((*(*q).onConflict).exclRelIndex);
                }
                for at in at {
                    if let Some(&e) = rtable.get((at as usize).wrapping_sub(1)) {
                        tables.written.push(e as *mut pg_sys::RangeTblEntry);
                    }
                }
            }
            pg_sys::query_tree_walker(
                q,
                Some(tables_walker),
                context,
                pg_sys::QTW_EXAMINE_RTES_BEFORE as i32,
            )
        }
        pg_sys::NodeTag::T_RangeTblEntry => {
            let e = node as *mut pg_sys::RangeTblEntry;
            let tables = &mut *(context as *mut Tables);
            if (*e).rtekind == pg_sys::RTEKind::RTE_RELATION && !tables.written.contains(&e) {
                tables.read.push(((*e).relid, (*e).inh));
            }
            false
        }
        _ => pg_sys::expression_tree_walker(node, Some(tables_walker), context),
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
pub(crate) mod tests {
    use pgrx::prelude::*;
    use std::cell::Cell;

    thread_local! {
        /// The pages drawn while the last statement was planned, and its budget where it was
        /// counted.
        pub(crate) static LAST: Cell<Option<(u64, Option<u64>)>> = const { Cell::new(None) };
    }

    /// `f`, a test's own look at what a read measures while a statement is planned, run outside the
    /// statement's budget: it draws no page from it and is held to nothing but its own most, so
    /// that it neither takes pages from the reads it looks at nor is kept short by them.
    pub(crate) fn outside<R>(f: impl FnOnce() -> R) -> R {
        let kept = super::BUDGET.with(|b| b.borrow_mut().take());
        let out = f();
        super::BUDGET.with(|b| *b.borrow_mut() = kept);
        out
    }

    /// The pages drawn while `query` was last planned, and its budget.
    fn drawn(query: &str) -> (u64, Option<u64>) {
        Spi::run(&format!("EXPLAIN {query}")).unwrap();
        LAST.get().expect("a statement planned")
    }

    /// Two tables of 100,000 narrow rows, each with a B-tree on (a, b) filled to a tenth of each
    /// leaf and with no entries merged, so that it stands several times its table, and a surveyor.
    fn ledgers() {
        Spi::run(
            "CREATE TABLE ledger_one AS SELECT g AS id, g % 90 AS a, g % 7 AS b \
             FROM generate_series(1, 100000) g; \
             CREATE TABLE ledger_two AS SELECT g AS id, g % 90 AS a, g % 7 AS b \
             FROM generate_series(1, 100000) g; \
             CREATE INDEX ledger_one_ab ON ledger_one (a, b) \
                 WITH (fillfactor = 10, deduplicate_items = off); \
             CREATE INDEX ledger_two_ab ON ledger_two (a, b) \
                 WITH (fillfactor = 10, deduplicate_items = off); \
             CREATE INDEX ledger_one_order ON ledger_one USING surveyor (id); \
             CREATE INDEX ledger_two_order ON ledger_two USING surveyor (id); \
             ANALYZE ledger_one; ANALYZE ledger_two",
        )
        .unwrap();
    }

    /// 60,000 rows in a lane (5) and a slot (10) whose lane it decides, with a B-tree on both and a
    /// surveyor: the planner multiplies the two conditions' shares, a fifth of the rows they select.
    fn lanes() {
        Spi::run(
            "CREATE TABLE lanes AS SELECT g AS id, (g % 5)::smallint AS lane, \
                    (g % 10)::smallint AS slot \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX lanes_lane_slot ON lanes (lane, slot); \
             ANALYZE lanes",
        )
        .unwrap();
    }

    /// The pages of shared and local buffers this session has asked for, found or read in, and the
    /// temporary pages it has read back.
    fn pages() -> i64 {
        let used = unsafe { &*std::ptr::addr_of!(pg_sys::pgBufferUsage) };
        used.shared_blks_hit
            + used.shared_blks_read
            + used.local_blks_hit
            + used.local_blks_read
            + used.temp_blks_read
    }

    /// The pages asked for while `query` is planned, once a planning before it has filled the
    /// caches.
    fn planning_pages(query: &str) -> i64 {
        let explain = format!("EXPLAIN {query}");
        Spi::run(&explain).unwrap();
        let before = pages();
        Spi::run(&explain).unwrap();
        pages() - before
    }

    /// The pages of `index` asked for while `query` is planned, found or read in.
    fn index_pages(query: &str, index: &str) -> i64 {
        let fetched = || {
            Spi::get_one::<i64>(&format!(
                "SELECT pg_stat_get_xact_blocks_fetched('{index}'::regclass)"
            ))
            .unwrap()
            .unwrap()
        };
        let before = fetched();
        Spi::run(&format!("EXPLAIN {query}")).unwrap();
        fetched() - before
    }

    /// The rows the first line of `query`'s plan gives.
    fn planned_rows(query: &str) -> f64 {
        crate::tests::texts(&format!("EXPLAIN {query}"))
            .first()
            .and_then(|l| l.split(" rows=").nth(1))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("{query}: no estimate"))
    }

    #[pg_test]
    fn the_reads_while_a_statement_is_planned_stay_within_the_pages_of_its_tables() {
        ledgers();
        // each table's own reads, the statistics of its join column and its conditions stepped
        // through that column, each held to the table's pages alone, would pass both tables' pages
        let query = "SELECT count(*) FROM ledger_one o JOIN ledger_two t ON o.a = t.a \
                     WHERE o.b = 5 AND t.b = 3";
        let budget = Spi::get_one::<i64>(
            "SELECT (pg_relation_size('ledger_one') + pg_relation_size('ledger_two')) \
                    / current_setting('block_size')::int",
        )
        .unwrap()
        .unwrap();
        let surveyed = planning_pages(query);
        let (drawn, limit) = drawn(query);
        assert_eq!(limit, Some(budget as u64));
        assert!(drawn <= budget as u64, "{drawn} pages drawn of {budget}");
        Spi::run("DROP INDEX ledger_one_order, ledger_two_order").unwrap();
        let own = planning_pages(query);
        let read = surveyed - own;
        assert!(
            read <= budget,
            "{read} pages read while planning, the tables hold {budget}"
        );
        assert!(
            2 * read > budget,
            "{read} pages read while planning, the tables hold {budget}"
        );
    }

    #[pg_test]
    fn at_a_planning_read_limit_of_0_no_index_page_is_read_and_the_estimates_are_postgresqls() {
        lanes();
        let queries = [
            "SELECT id FROM lanes WHERE lane = 1 AND slot = 6",
            "SELECT id FROM lanes WHERE lane = 1 AND slot IN (1, 6)",
            "SELECT count(*) FROM lanes a JOIN lanes b ON a.slot = b.slot WHERE a.lane = 2",
        ];
        let own: Vec<f64> = queries.iter().map(|q| planned_rows(q)).collect();
        Spi::run("CREATE INDEX lanes_order ON lanes USING surveyor (id)").unwrap();
        let measured: Vec<f64> = queries.iter().map(|q| planned_rows(q)).collect();
        assert_ne!(measured, own);
        assert!(index_pages(queries[0], "lanes_lane_slot") > 0);
        Spi::run("SET LOCAL warren_surveyor_pg.planning_read_limit = 0").unwrap();
        let limited: Vec<f64> = queries.iter().map(|q| planned_rows(q)).collect();
        assert_eq!(limited, own, "measured without the limit: {measured:?}");
        for q in queries {
            assert_eq!(index_pages(q, "lanes_lane_slot"), 0, "{q}");
            assert_eq!(drawn(q).0, 0, "{q}");
        }
    }

    #[pg_test(
        error = "permission denied to set parameter \"warren_surveyor_pg.planning_read_limit\""
    )]
    fn only_a_superuser_sets_the_planning_read_limit() {
        Spi::run(
            "CREATE ROLE limit_reader; SET LOCAL ROLE limit_reader; \
             SET warren_surveyor_pg.planning_read_limit = 0",
        )
        .unwrap();
    }
}
