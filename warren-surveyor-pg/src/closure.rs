// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The size of a WITH RECURSIVE query, read while planning. A recursive query that reads a table
//! surveyed for it (a surveyor over the whole table, or a partial one whose predicate the planner
//! proved from the recursive query's own conditions) is run once while the statement is planned,
//! and the number of rows it returned is the size the statement is planned with. Its rows are still
//! read when the statement runs, so a plan kept and run again returns what the tables hold then.
//!
//! The query is read only where it takes nothing from the statement around it: no parameter, no
//! column of an outer query, no other WITH query, and no volatile function, and only while a
//! statement is planned, whose budget counts its tables (`budget`). It is read inside a
//! subtransaction, never in parallel mode, a row at a time, and stops once the pages it has read
//! pass what is left of the statement's planning-read budget, or the rows it has returned pass the
//! rows the planner takes the tables the whole statement reads to hold, each table counted once as
//! the budget counts its pages. The size read decides how the statement reads those tables, so a
//! read is worth at most a scan of the tables it can keep the plan from scanning: their pages and
//! their rows. A recursive query whose steps read no page, such as one that only a LIMIT ends,
//! stops at those rows. The pages it has read are every page in shared or local buffers, hit or
//! read in, and every temporary page read back, as PostgreSQL counts them for EXPLAIN's buffers,
//! and they are drawn on the budget once it stops, an error included; with no page left, the read
//! never starts. Past either, or where the read raises an error, the planner's own estimate stands.
//! A cancel is raised again.
//!
//! Where the statement equates a column of the query with another relation's column, the read also
//! keeps, for that column, each value it returned with the rows holding it, and the rows holding
//! NULL: the column's statistics are those values, and the other relation's column is measured at
//! them (`leaves`), so that the join is sized by the rows each value meets rather than by one
//! average. Only an integer or a date column is kept, and only while its values are no more than a
//! statistics list holds. The values are never written into the plan, and are forgotten when the
//! statement's planning ends.

use crate::budget;
use crate::measure;
use crate::query::{bare, cells, surveyed, surveyor_am};
use crate::round;
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::{PgSqlErrorCode, PgTryBuilder};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::ptr::null_mut;

static mut NEXT_PATHLIST: pg_sys::set_rel_pathlist_hook_type = None;

thread_local! {
    /// Set while a WITH query is read, so that nothing planned for the read is read too.
    static READING: Cell<bool> = const { Cell::new(false) };
    /// While a read runs: the pages read before it started, and the pages left of the statement's
    /// budget then.
    static RUN: Cell<Option<(i64, u32)>> = const { Cell::new(None) };
    /// What each read kept in this round.
    static KEPT: RefCell<Vec<Kept>> = const { RefCell::new(Vec::new()) };
}

/// What a read kept: the depth of the planning, the planner's state and the place in its range
/// table of the WITH query read, and each column kept.
#[derive(Clone)]
struct Kept {
    depth: u32,
    root: usize,
    rti: pg_sys::Index,
    columns: Vec<Walked>,
}

/// What a read kept of one column of a WITH query: the rows the query returned, those holding NULL
/// in the column, and each value with the rows holding it, the most rows first.
#[derive(Clone, Debug)]
pub(crate) struct Walked {
    pub column: pg_sys::AttrNumber,
    pub kind: pg_sys::Oid,
    pub collation: pg_sys::Oid,
    pub rows: u64,
    pub nulls: u64,
    pub values: Vec<(i64, u64)>,
}

/// Forgets what the reads of the plannings at `depth` and deeper kept.
pub(crate) fn forget_from(depth: u32) {
    KEPT.with(|k| k.borrow_mut().retain(|e| e.depth < depth));
}

/// What the reads kept in the round so far, to be put back by `put_back`.
pub(crate) struct Saved(Vec<Kept>);

/// What the reads kept in the round so far.
pub(crate) fn saved() -> Saved {
    Saved(KEPT.with(|k| k.borrow().clone()))
}

/// Puts back what `saved` holds as what the reads kept in the round.
pub(crate) fn put_back(saved: Saved) {
    KEPT.with(|k| *k.borrow_mut() = saved.0);
}

/// What the read of the WITH query at `rti` of `root`'s range table kept of its column `column`.
pub(crate) fn walked(
    root: *mut pg_sys::PlannerInfo,
    rti: pg_sys::Index,
    column: pg_sys::AttrNumber,
) -> Option<Walked> {
    KEPT.with(|k| {
        k.borrow()
            .iter()
            .filter(|e| e.root == root as usize && e.rti == rti)
            .flat_map(|e| e.columns.iter())
            .find(|w| w.column == column)
            .cloned()
    })
}

/// The values the reads of WITH queries kept of the columns the planner equates with column
/// `attnum` of the relation at `varno` of `root`'s range table, each once.
pub(crate) unsafe fn equated_values(
    root: *mut pg_sys::PlannerInfo,
    varno: pg_sys::Index,
    attnum: pg_sys::AttrNumber,
) -> Vec<i64> {
    let mut values = Vec::new();
    if KEPT.with(|k| k.borrow().is_empty()) {
        return values;
    }
    for ec in cells((*root).eq_classes) {
        let ec = ec as *mut pg_sys::EquivalenceClass;
        if (*ec).ec_has_volatile || (*ec).ec_broken {
            continue;
        }
        let columns = class_columns(ec);
        if !columns.contains(&(varno, attnum)) {
            continue;
        }
        for &(at, column) in &columns {
            if at == varno {
                continue;
            }
            if let Some(walked) = walked(root, at, column) {
                values.extend(walked.values.iter().map(|&(v, _)| v));
            }
        }
    }
    values.sort_unstable();
    values.dedup();
    values
}

/// The columns of its own query level an equivalence class equates, as (range table index,
/// column).
unsafe fn class_columns(
    ec: *mut pg_sys::EquivalenceClass,
) -> Vec<(pg_sys::Index, pg_sys::AttrNumber)> {
    let mut columns = Vec::new();
    for m in cells((*ec).ec_members) {
        let m = m as *mut pg_sys::EquivalenceMember;
        if (*m).em_is_const || (*m).em_is_child {
            continue;
        }
        let e = bare((*m).em_expr as *mut pg_sys::Node);
        if e.is_null() || (*e).type_ != pg_sys::NodeTag::T_Var {
            continue;
        }
        let var = e as *mut pg_sys::Var;
        if (*var).varlevelsup == 0 && (*var).varattno > 0 {
            columns.push(((*var).varno as pg_sys::Index, (*var).varattno));
        }
    }
    columns
}

/// The columns of the WITH query `cte`, read at `rti` of `root`'s range table, that the planner
/// equates with another relation's column, where they are integers or dates: each column, its
/// type and its collation.
unsafe fn joined_columns(
    root: *mut pg_sys::PlannerInfo,
    rti: pg_sys::Index,
    cte: *mut pg_sys::CommonTableExpr,
) -> Vec<(pg_sys::AttrNumber, pg_sys::Oid, pg_sys::Oid)> {
    let mut joined = Vec::new();
    for ec in cells((*root).eq_classes) {
        let ec = ec as *mut pg_sys::EquivalenceClass;
        if (*ec).ec_has_volatile || (*ec).ec_broken {
            continue;
        }
        let columns = class_columns(ec);
        if !columns.iter().any(|&(at, _)| at != rti) {
            continue;
        }
        for &(at, column) in &columns {
            if at == rti && !joined.iter().any(|&(c, _, _)| c == column) {
                let types = (*cte).ctecoltypes;
                let collations = (*cte).ctecolcollations;
                let i = column as usize - 1;
                if types.is_null() || i >= (*types).length as usize {
                    continue;
                }
                let kind = (*(*types).elements.add(i)).oid_value;
                let collation = if collations.is_null() || i >= (*collations).length as usize {
                    pg_sys::InvalidOid
                } else {
                    (*(*collations).elements.add(i)).oid_value
                };
                if measure::countable(kind) {
                    joined.push((column, kind, collation));
                }
            }
        }
    }
    joined
}

/// Clears `READING` when a read ends, or fails.
struct Reading;

impl Drop for Reading {
    fn drop(&mut self) {
        READING.set(false);
    }
}

/// Puts the hook in place, after any hook already there.
pub fn init() {
    unsafe {
        NEXT_PATHLIST = pg_sys::set_rel_pathlist_hook;
        pg_sys::set_rel_pathlist_hook = Some(pathlist);
    }
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
    if rte.is_null()
        || (*rte).rtekind != pg_sys::RTEKind::RTE_CTE
        || (*rte).self_reference
        || READING.get()
    {
        return;
    }
    let Some(cte) = readable_query(root, rte) else {
        return;
    };
    let _planning = budget::planning(root);
    let joined = if round::depth() > 0 {
        joined_columns(root, rti, cte)
    } else {
        Vec::new()
    };
    if let Some((rows, kept)) = rows_of(cte, &joined) {
        if !kept.is_empty() {
            KEPT.with(|k| {
                k.borrow_mut().push(Kept {
                    depth: round::depth(),
                    root: root as usize,
                    rti,
                    columns: kept,
                })
            });
        }
        pg_sys::set_cte_size_estimates(root, rel, rows as f64);
        for path in cells((*rel).pathlist) {
            let path = path as *mut pg_sys::Path;
            if (*path).pathtype == pg_sys::NodeTag::T_CteScan {
                pg_sys::cost_ctescan(path, root, rel, (*path).param_info);
            }
        }
    }
}

/// The WITH RECURSIVE query `rte` reads, where it may be read while planning.
unsafe fn readable_query(
    root: *mut pg_sys::PlannerInfo,
    rte: *mut pg_sys::RangeTblEntry,
) -> Option<*mut pg_sys::CommonTableExpr> {
    let mut owner = root;
    for _ in 0..(*rte).ctelevelsup {
        owner = (*owner).parent_root;
        if owner.is_null() {
            return None;
        }
    }
    let name = CStr::from_ptr((*rte).ctename);
    let (at, cte) = cells((*(*owner).parse).cteList)
        .into_iter()
        .map(|c| c as *mut pg_sys::CommonTableExpr)
        .enumerate()
        .find(|&(_, c)| CStr::from_ptr((*c).ctename) == name)?;
    let q = (*cte).ctequery as *mut pg_sys::Query;
    if !(*cte).cterecursive
        || q.is_null()
        || (*(q as *mut pg_sys::Node)).type_ != pg_sys::NodeTag::T_Query
        || (*q).commandType != pg_sys::CmdType::CMD_SELECT
        || !self_contained(q, name)
        || pg_sys::contain_volatile_functions(q as *mut pg_sys::Node)
    {
        return None;
    }
    if surveyor_am() == pg_sys::InvalidOid {
        return None;
    }
    let planned = planned_query(owner, at)?;
    reads_surveyed(planned, (*owner).glob).then_some(cte)
}

/// The planner's state for the WITH query at `at` of `owner`'s WITH list, once it is planned.
unsafe fn planned_query(
    owner: *mut pg_sys::PlannerInfo,
    at: usize,
) -> Option<*mut pg_sys::PlannerInfo> {
    let ids = (*owner).cte_plan_ids;
    if ids.is_null() || at >= (*ids).length as usize {
        return None;
    }
    let plan_id = (*(*ids).elements.add(at)).int_value;
    if plan_id < 1 {
        return None;
    }
    let planned = *cells((*(*owner).glob).subroots).get(plan_id as usize - 1)?;
    (!planned.is_null()).then_some(planned as *mut pg_sys::PlannerInfo)
}

/// Whether a table the planned query `top` reads is surveyed for it: at its own level, in the
/// subqueries and set operations under it, and in the subqueries of its conditions, which `glob`
/// keeps.
unsafe fn reads_surveyed(top: *mut pg_sys::PlannerInfo, glob: *mut pg_sys::PlannerGlobal) -> bool {
    let under = |mut r: *mut pg_sys::PlannerInfo| {
        while !r.is_null() {
            if r == top {
                return true;
            }
            r = (*r).parent_root;
        }
        false
    };
    level_surveyed(top)
        || cells((*glob).subroots)
            .into_iter()
            .map(|r| r as *mut pg_sys::PlannerInfo)
            .any(|r| r != top && under(r) && level_surveyed(r))
}

/// Whether a table the planned query level `root` reads, or a subquery or set operation under it
/// reads, is surveyed for the question.
unsafe fn level_surveyed(root: *mut pg_sys::PlannerInfo) -> bool {
    pg_sys::check_stack_depth();
    if root.is_null() || (*root).simple_rel_array.is_null() {
        return false;
    }
    (1..(*root).simple_rel_array_size as usize).any(|at| {
        let rel = *(*root).simple_rel_array.add(at);
        !rel.is_null()
            && (level_surveyed((*rel).subroot)
                || ((*rel).rtekind == pg_sys::RTEKind::RTE_RELATION && surveyed(rel, true)))
    })
}

/// Whether the WITH query `q`, named `name`, reads nothing from outside itself but itself: no
/// parameter, no column of a query around it, and no other WITH query of one.
unsafe fn self_contained(q: *mut pg_sys::Query, name: &CStr) -> bool {
    struct Scope<'a> {
        /// The queries entered from the WITH query's owner: 1 inside the WITH query itself.
        depth: u32,
        name: &'a CStr,
        outside: bool,
    }

    #[pg_guard]
    unsafe extern "C-unwind" fn walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        let scope = &mut *(context as *mut Scope);
        match (*node).type_ {
            pg_sys::NodeTag::T_Query => {
                scope.depth += 1;
                let stop = pg_sys::query_tree_walker(
                    node as *mut pg_sys::Query,
                    Some(walker),
                    context,
                    pg_sys::QTW_EXAMINE_RTES_BEFORE as i32,
                );
                scope.depth -= 1;
                stop
            }
            pg_sys::NodeTag::T_RangeTblEntry => {
                let e = node as *mut pg_sys::RangeTblEntry;
                if (*e).rtekind == pg_sys::RTEKind::RTE_CTE && (*e).ctelevelsup >= scope.depth {
                    let itself = (*e).self_reference
                        && (*e).ctelevelsup == scope.depth
                        && CStr::from_ptr((*e).ctename) == scope.name;
                    scope.outside |= !itself;
                }
                scope.outside
            }
            pg_sys::NodeTag::T_Var => {
                scope.outside |= (*(node as *mut pg_sys::Var)).varlevelsup >= scope.depth;
                scope.outside
            }
            pg_sys::NodeTag::T_Param => {
                scope.outside = true;
                true
            }
            _ => pg_sys::expression_tree_walker(node, Some(walker), context),
        }
    }

    let mut scope = Scope {
        depth: 0,
        name,
        outside: false,
    };
    walker(
        q as *mut pg_sys::Node,
        &mut scope as *mut Scope as *mut c_void,
    );
    !scope.outside
}

/// What the DEBUG1 notes of a read call the WITH query `cte`.
unsafe fn read_name(cte: *mut pg_sys::CommonTableExpr) -> String {
    format!(
        "the WITH query {}",
        CStr::from_ptr((*cte).ctename).to_string_lossy()
    )
}

/// The rows `cte` returns, read now while the statement is planned, and what it kept of the columns
/// `joined` (each column, its type and its collation); none where it is not read.
unsafe fn rows_of(
    cte: *mut pg_sys::CommonTableExpr,
    joined: &[(pg_sys::AttrNumber, pg_sys::Oid, pg_sys::Oid)],
) -> Option<(u64, Vec<Walked>)> {
    if pg_sys::IsInParallelMode() || !pg_sys::ActiveSnapshotSet() {
        return None;
    }
    // the rows of the tables the statement reads, none outside a statement's planning; and a run
    // reads one page at least
    let most = budget::statement_rows()?;
    if !budget::fits_read(1.0, || read_name(cte)) {
        return None;
    }
    READING.set(true);
    let _reading = Reading;
    let context = pg_sys::CurrentMemoryContext;
    let owner = pg_sys::CurrentResourceOwner;
    round::begin_own_read();
    pg_sys::CurrentMemoryContext = context;
    let rows = PgTryBuilder::new(|| {
        let rows = count(cte, most, joined);
        pg_sys::ReleaseCurrentSubTransaction();
        rows
    })
    .catch_others(|e| {
        pg_sys::CurrentMemoryContext = context;
        pg_sys::FlushErrorState();
        pg_sys::RollbackAndReleaseCurrentSubTransaction();
        pg_sys::CurrentMemoryContext = context;
        pg_sys::CurrentResourceOwner = owner;
        match e {
            pg_sys::panic::CaughtError::PostgresError(ref report)
            | pg_sys::panic::CaughtError::ErrorReport(ref report)
                if report.sql_error_code() != PgSqlErrorCode::ERRCODE_QUERY_CANCELED =>
            {
                None
            }
            // the error was flushed for the rollback, so a cancel is raised again as a new error
            // that carries its code and message, never by throwing the error that was flushed
            pg_sys::panic::CaughtError::PostgresError(report) => {
                pg_sys::panic::CaughtError::ErrorReport(report).rethrow()
            }
            _ => e.rethrow(),
        }
    })
    .execute();
    pg_sys::CurrentMemoryContext = context;
    pg_sys::CurrentResourceOwner = owner;
    // a run that raised an error draws the pages it read
    if let Some((start, left)) = RUN.take() {
        let read = (pages_read() - start).max(0) as u64;
        budget::spend(read, false, left, || read_name(cte));
    }
    rows
}

/// `SELECT` the columns `joined` (each column, its type and its collation) `FROM cte`, planned and
/// run until it ends, or until the pages it has read pass what is left of the statement's
/// planning-read budget or the rows it has returned pass `most`: the rows it returned and what was
/// kept of each column, or none past them. The pages the run read are drawn on the budget.
unsafe fn count(
    cte: *mut pg_sys::CommonTableExpr,
    most: f64,
    joined: &[(pg_sys::AttrNumber, pg_sys::Oid, pg_sys::Oid)],
) -> Option<(u64, Vec<Walked>)> {
    let copy = pg_sys::copyObjectImpl(cte as *const c_void) as *mut pg_sys::CommonTableExpr;
    (*copy).cterefcount = 1;
    let rte =
        pg_sys::palloc0(std::mem::size_of::<pg_sys::RangeTblEntry>()) as *mut pg_sys::RangeTblEntry;
    (*rte).type_ = pg_sys::NodeTag::T_RangeTblEntry;
    (*rte).rtekind = pg_sys::RTEKind::RTE_CTE;
    (*rte).ctename = (*copy).ctename;
    (*rte).coltypes = pg_sys::list_copy((*copy).ctecoltypes);
    (*rte).coltypmods = pg_sys::list_copy((*copy).ctecoltypmods);
    (*rte).colcollations = pg_sys::list_copy((*copy).ctecolcollations);
    (*rte).eref = pg_sys::makeAlias(
        (*copy).ctename,
        pg_sys::copyObjectImpl((*copy).ctecolnames as *const c_void) as *mut pg_sys::List,
    );
    (*rte).inFromCl = true;
    let reference =
        pg_sys::palloc0(std::mem::size_of::<pg_sys::RangeTblRef>()) as *mut pg_sys::RangeTblRef;
    (*reference).type_ = pg_sys::NodeTag::T_RangeTblRef;
    (*reference).rtindex = 1;
    let q = pg_sys::palloc0(std::mem::size_of::<pg_sys::Query>()) as *mut pg_sys::Query;
    (*q).type_ = pg_sys::NodeTag::T_Query;
    (*q).commandType = pg_sys::CmdType::CMD_SELECT;
    (*q).querySource = pg_sys::QuerySource::QSRC_ORIGINAL;
    (*q).canSetTag = true;
    (*q).hasRecursive = true;
    (*q).cteList = pg_sys::lappend(null_mut(), copy as *mut c_void);
    (*q).rtable = pg_sys::lappend(null_mut(), rte as *mut c_void);
    (*q).jointree = pg_sys::makeFromExpr(
        pg_sys::lappend(null_mut(), reference as *mut c_void),
        null_mut(),
    );
    let mut targets = null_mut();
    for (at, &(column, kind, collation)) in joined.iter().enumerate() {
        let var = pg_sys::makeVar(1, column, kind, -1, collation, 0);
        let target = pg_sys::makeTargetEntry(
            var as *mut pg_sys::Expr,
            (at + 1) as pg_sys::AttrNumber,
            null_mut(),
            false,
        );
        targets = pg_sys::lappend(targets, target as *mut c_void);
    }
    (*q).targetList = targets;
    let mut tally = Box::new(Tally {
        receiver: pg_sys::DestReceiver {
            receiveSlot: Some(receive),
            rStartup: Some(startup),
            rShutdown: Some(shutdown),
            rDestroy: Some(shutdown),
            mydest: pg_sys::CommandDest::DestNone,
        },
        columns: joined
            .iter()
            .map(|&(column, kind, collation)| Column {
                column,
                kind,
                collation,
                nulls: 0,
                counts: Some(HashMap::new()),
            })
            .collect(),
    });
    let dest = if joined.is_empty() {
        pg_sys::None_Receiver
    } else {
        &mut tally.receiver as *mut pg_sys::DestReceiver
    };
    let stmt = planned(q);
    let desc = pg_sys::CreateQueryDesc(
        stmt,
        c"".as_ptr(),
        pg_sys::GetActiveSnapshot(),
        null_mut(),
        dest,
        null_mut(),
        null_mut(),
        0,
    );
    let left = budget::left();
    let start = pages_read();
    RUN.set(Some((start, left)));
    pg_sys::ExecutorStart(desc, 0);
    let mut rows = 0;
    let (within, over) = loop {
        pg_sys::ExecutorRun(desc, pg_sys::ScanDirection::ForwardScanDirection, 1);
        let got = (*(*desc).estate).es_processed;
        rows += got;
        let over = pages_read() - start > left as i64;
        if over || rows as f64 > most {
            break (false, over);
        }
        if got == 0 {
            break (true, false);
        }
    };
    pg_sys::ExecutorFinish(desc);
    pg_sys::ExecutorEnd(desc);
    pg_sys::FreeQueryDesc(desc);
    RUN.set(None);
    budget::spend((pages_read() - start) as u64, over, left, || read_name(cte));
    if !within {
        return None;
    }
    let kept = tally
        .columns
        .drain(..)
        .filter_map(|c| {
            let mut values: Vec<(i64, u64)> = c.counts?.into_iter().collect();
            values.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            Some(Walked {
                column: c.column,
                kind: c.kind,
                collation: c.collation,
                rows,
                nulls: c.nulls,
                values,
            })
        })
        .collect();
    Some((rows, kept))
}

/// Receives the rows a read returns and keeps each column's values with the rows holding each.
#[repr(C)]
struct Tally {
    receiver: pg_sys::DestReceiver,
    columns: Vec<Column>,
}

/// One column a read keeps: its rows holding NULL, and each value with the rows holding it, while
/// its values are no more than a statistics list holds.
struct Column {
    column: pg_sys::AttrNumber,
    kind: pg_sys::Oid,
    collation: pg_sys::Oid,
    nulls: u64,
    counts: Option<HashMap<i64, u64>>,
}

#[pg_guard]
unsafe extern "C-unwind" fn receive(
    slot: *mut pg_sys::TupleTableSlot,
    receiver: *mut pg_sys::DestReceiver,
) -> bool {
    let tally = &mut *(receiver as *mut Tally);
    for (at, column) in tally.columns.iter_mut().enumerate() {
        let mut null = false;
        let datum = pg_sys::slot_getattr(slot, at as i32 + 1, &mut null);
        if null {
            column.nulls += 1;
            continue;
        }
        let value = measure::whole(datum, column.kind);
        if let Some(counts) = column.counts.as_mut() {
            match value {
                Some(v) => {
                    *counts.entry(v).or_insert(0) += 1;
                    if counts.len() > pg_sys::MAX_STATISTICS_TARGET as usize {
                        column.counts = None;
                    }
                }
                None => column.counts = None,
            }
        }
    }
    true
}

#[pg_guard]
unsafe extern "C-unwind" fn startup(
    _receiver: *mut pg_sys::DestReceiver,
    _operation: std::ffi::c_int,
    _typeinfo: pg_sys::TupleDesc,
) {
}

#[pg_guard]
unsafe extern "C-unwind" fn shutdown(_receiver: *mut pg_sys::DestReceiver) {}

/// The pages this session has read in shared and local buffers, hit or read in, and the temporary
/// pages it has read back, as PostgreSQL counts them for EXPLAIN's buffers.
unsafe fn pages_read() -> i64 {
    let used = pg_sys::pgBufferUsage;
    used.shared_blks_hit
        + used.shared_blks_read
        + used.local_blks_hit
        + used.local_blks_read
        + used.temp_blks_read
}

#[cfg(not(feature = "pg19"))]
unsafe fn planned(q: *mut pg_sys::Query) -> *mut pg_sys::PlannedStmt {
    pg_sys::standard_planner(q, c"".as_ptr(), 0, null_mut())
}

#[cfg(feature = "pg19")]
unsafe fn planned(q: *mut pg_sys::Query) -> *mut pg_sys::PlannedStmt {
    pg_sys::standard_planner(q, c"".as_ptr(), 0, null_mut(), null_mut())
}
