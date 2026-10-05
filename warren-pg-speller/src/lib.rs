// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! warren-pg-speller: a PostgreSQL planner hook that plans a SELECT through another spelling of it
//! that returns the same rows.
//!
//! A statement is respelled only where the statement and the catalog show that one of three
//! identities holds, and nowhere else:
//!
//! - `distribute`: a relation joined alike in every arm of a UNION ALL is joined once, to the union
//!   of the rest of each arm.
//! - `fold`: a grouping over a join is taken on each side of the join first, and the grouped sides
//!   are joined at the key they meet in.
//! - `keep`: the side the counted table is not on is grouped only over the keys the counted side's
//!   grouping holds.
//!
//! Only inner joins are read. A query that reads a column of a query around it, reads a relation
//! beside it laterally, or calls a volatile function is left as written, and so is every statement
//! where a condition fails. A spelling is built from the statement as it was parsed, never from its
//! text. It reads no table the statement does not read, and reads each as the statement does: as the
//! owner of the view it lies in or as the user running it, under the same row security. The indexes
//! a table carries play no part. A plan made from a spelling is made again when an index on a table
//! it reads changes.
//!
//! A spelling is built and planned inside an internal subtransaction, begun only once one is being
//! built; where either raises an ERROR other than a cancel, the statement is planned as written.
//! Nothing is respelled inside a parallel operation.
//!
//! An extension installs the hook by calling [`init`] from its `_PG_init`. A statement is respelled
//! only when that extension's library is loaded before the statement is planned: through
//! `shared_preload_libraries`, `session_preload_libraries`, or `LOAD`.

mod catalog;
mod distribute;
mod fold;
mod keep;
mod levels;
mod nodes;
mod reading;

use catalog::Facts;
use distribute::distribute;
use fold::fold;
use nodes::{cells, copy, tag};
use pgrx::pg_sys;
use pgrx::pg_sys::panic::CaughtError;
use pgrx::prelude::*;
use std::cell::Cell;
use std::ffi::{c_char, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::ptr::{null, null_mut};

static mut NEXT_PLANNER: pg_sys::planner_hook_type = None;

thread_local! {
    /// Set while a statement is being respelled, so that nothing planned meanwhile is respelled too.
    static RESPELLING: Cell<bool> = const { Cell::new(false) };
}

/// Puts the planner hook in place, after any hook already there.
pub fn init() {
    unsafe {
        NEXT_PLANNER = pg_sys::planner_hook;
        pg_sys::planner_hook = Some(plan);
    }
}

/// Clears `RESPELLING` when a respelling ends, or fails.
pub(crate) struct Respelling;

impl Drop for Respelling {
    fn drop(&mut self) {
        RESPELLING.set(false);
    }
}

/// What a plan made from a statement rests on besides the tables it reads.
#[derive(Default)]
pub(crate) struct Depends {
    /// The tables whose indexes and columns were read from the catalog.
    relations: Vec<pg_sys::Oid>,
}

impl Depends {
    unsafe fn mark(&self, stmt: *mut pg_sys::PlannedStmt) {
        if stmt.is_null() {
            return;
        }
        for &relid in &self.relations {
            if !pg_sys::list_member_oid((*stmt).relationOids, relid) {
                (*stmt).relationOids = pg_sys::lappend_oid((*stmt).relationOids, relid);
            }
        }
    }
}

/// The internal subtransaction a statement's spelling is built and planned in. It begins when the
/// respelling starts to build a spelling, before it changes anything in the statement, and keeps
/// a copy of the statement as it was parsed; a statement left as written takes neither.
pub(crate) struct Net {
    statement: *mut pg_sys::Query,
    /// The memory context and the resource owner the statement is planned in.
    context: pg_sys::MemoryContext,
    owner: pg_sys::ResourceOwner,
    written: Cell<*mut pg_sys::Query>,
    begun: Cell<bool>,
}

impl Net {
    unsafe fn new(statement: *mut pg_sys::Query) -> Self {
        Net {
            statement,
            context: pg_sys::CurrentMemoryContext,
            owner: pg_sys::CurrentResourceOwner,
            written: Cell::new(null_mut()),
            begun: Cell::new(false),
        }
    }

    /// Copies the statement and begins the subtransaction, unless it has begun, and goes on in the
    /// statement's memory context.
    pub(crate) unsafe fn begin(&self) {
        if !self.begun.get() {
            self.written.set(copy(self.statement));
            pg_sys::BeginInternalSubTransaction(null());
            pg_sys::CurrentMemoryContext = self.context;
            self.begun.set(true);
        }
    }

    /// Keeps what was done inside the subtransaction, where it has begun.
    unsafe fn release(&self) {
        if self.begun.replace(false) {
            pg_sys::ReleaseCurrentSubTransaction();
            self.restore();
        }
    }

    /// Undoes what was done inside the subtransaction, after an ERROR there, and gives the copy of
    /// the statement as it was parsed.
    unsafe fn roll_back(&self) -> *mut pg_sys::Query {
        pg_sys::CurrentMemoryContext = self.context;
        pg_sys::RollbackAndReleaseCurrentSubTransaction();
        self.begun.set(false);
        self.restore();
        self.written.get()
    }

    unsafe fn restore(&self) {
        pg_sys::CurrentMemoryContext = self.context;
        pg_sys::CurrentResourceOwner = self.owner;
    }
}

/// Plans the statement `parse` through `next`, the planner after this hook. A SELECT the
/// respelling changes is planned respelled; from the moment the respelling starts to build a
/// spelling, it and the planning of the respelled statement run inside an internal subtransaction.
/// Where that raises an ERROR other than a cancel, the subtransaction is rolled back and the
/// statement is planned as written, from the copy taken as it began. Nothing is respelled in a
/// parallel operation.
unsafe fn planned(
    parse: *mut pg_sys::Query,
    next: impl Fn(*mut pg_sys::Query) -> *mut pg_sys::PlannedStmt,
) -> *mut pg_sys::PlannedStmt {
    if RESPELLING.get()
        || parse.is_null()
        || (*parse).commandType != pg_sys::CmdType::CMD_SELECT
        || pg_sys::IsInParallelMode()
    {
        return next(parse);
    }
    let net = Net::new(parse);
    let (net, plan) = (AssertUnwindSafe(&net), AssertUnwindSafe(&next));
    let planned = PgTryBuilder::new(AssertUnwindSafe(|| {
        let (respelled, depends) = {
            RESPELLING.set(true);
            let _respelling = Respelling;
            respell(parse, net.0)
        };
        let stmt = respelled.map(|q| plan.0(q));
        net.0.release();
        match stmt {
            Some(stmt) => {
                depends.mark(stmt);
                Ok(stmt)
            }
            None => Err(parse),
        }
    }))
    .catch_others(|e| {
        let net = &net;
        if !net.0.begun.get() {
            e.rethrow()
        }
        let report = match &e {
            CaughtError::PostgresError(r) | CaughtError::ErrorReport(r) => r,
            CaughtError::RustPanic { ereport, .. } => ereport,
        };
        if report.sql_error_code() == PgSqlErrorCode::ERRCODE_QUERY_CANCELED {
            net.0.roll_back();
            e.rethrow()
        }
        let message = report.message().to_string();
        pg_sys::FlushErrorState();
        let written = net.0.roll_back();
        debug1!("warren-pg-speller: planned as written, the respelling failed: {message}");
        Err(written)
    })
    .execute();
    planned.unwrap_or_else(next)
}

#[cfg(not(feature = "pg19"))]
#[pg_guard]
unsafe extern "C-unwind" fn plan(
    parse: *mut pg_sys::Query,
    query_string: *const c_char,
    cursor_options: c_int,
    bound_params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    planned(parse, |q| match NEXT_PLANNER {
        Some(next) => pg_sys::ffi::pg_guard_ffi_boundary(|| {
            next(q, query_string, cursor_options, bound_params)
        }),
        None => pg_sys::standard_planner(q, query_string, cursor_options, bound_params),
    })
}

#[cfg(feature = "pg19")]
#[pg_guard]
unsafe extern "C-unwind" fn plan(
    parse: *mut pg_sys::Query,
    query_string: *const c_char,
    cursor_options: c_int,
    bound_params: pg_sys::ParamListInfo,
    es: *mut pg_sys::ExplainState,
) -> *mut pg_sys::PlannedStmt {
    planned(parse, |q| match NEXT_PLANNER {
        Some(next) => pg_sys::ffi::pg_guard_ffi_boundary(|| {
            next(q, query_string, cursor_options, bound_params, es)
        }),
        None => pg_sys::standard_planner(q, query_string, cursor_options, bound_params, es),
    })
}

/// A statement being read, query by query.
pub(crate) struct Walk<'a> {
    facts: &'a mut Facts,
    /// The queries around the one being read, the outermost first.
    levels: Vec<*mut pg_sys::Query>,
    net: &'a Net,
}

/// `q` respelled, each query inside it first; none where nothing in it changed.
pub(crate) unsafe fn visit(
    walk: &mut Walk,
    q: *mut pg_sys::Query,
    respell: bool,
) -> Option<*mut pg_sys::Query> {
    walk.levels.push(q);
    let mut changed = false;
    for e in cells((*q).rtable) {
        let e = e as *mut pg_sys::RangeTblEntry;
        if (*e).rtekind == pg_sys::RTEKind::RTE_SUBQUERY && !(*e).subquery.is_null() {
            if let Some(r) = visit(walk, (*e).subquery, true) {
                (*e).subquery = r;
                changed = true;
            }
        }
    }
    for c in cells((*q).cteList) {
        let c = c as *mut pg_sys::CommonTableExpr;
        let sub = (*c).ctequery as *mut pg_sys::Query;
        if !sub.is_null()
            && tag(sub as *mut c_void) == pg_sys::NodeTag::T_Query
            && (*sub).commandType == pg_sys::CmdType::CMD_SELECT
        {
            if let Some(r) = visit(walk, sub, !(*c).cterecursive) {
                (*c).ctequery = r as *mut pg_sys::Node;
                changed = true;
            }
        }
    }
    struct Links<'w, 'a> {
        walk: &'w mut Walk<'a>,
        changed: bool,
    }
    #[pg_guard]
    unsafe extern "C-unwind" fn links(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        if (*node).type_ == pg_sys::NodeTag::T_SubLink {
            let l = &mut *(context as *mut Links);
            let link = node as *mut pg_sys::SubLink;
            let sub = (*link).subselect as *mut pg_sys::Query;
            if !sub.is_null() && tag(sub as *mut c_void) == pg_sys::NodeTag::T_Query {
                if let Some(r) = visit(l.walk, sub, true) {
                    (*link).subselect = r as *mut pg_sys::Node;
                    l.changed = true;
                }
            }
            return links((*link).testexpr, context);
        }
        if (*node).type_ == pg_sys::NodeTag::T_Query {
            return false;
        }
        pg_sys::expression_tree_walker(node, Some(links), context)
    }
    let mut l = Links {
        walk,
        changed: false,
    };
    pg_sys::query_tree_walker(
        q,
        Some(links),
        &mut l as *mut Links as *mut c_void,
        (pg_sys::QTW_IGNORE_RT_SUBQUERIES | pg_sys::QTW_IGNORE_CTE_SUBQUERIES) as c_int,
    );
    changed |= l.changed;
    walk.levels.pop();
    let mut current = q;
    if respell {
        if let Some(j) = distribute(current, &walk.levels, walk.net) {
            current = j;
            changed = true;
        }
        if let Some(o) = fold(current, walk.facts, walk.net) {
            current = o;
            changed = true;
        }
    }
    changed.then_some(current)
}

/// `parse` respelled, in place; none where nothing in it changed. `net` begins before a spelling
/// is built.
pub(crate) unsafe fn respell(
    parse: *mut pg_sys::Query,
    net: &Net,
) -> (Option<*mut pg_sys::Query>, Depends) {
    let mut facts = Facts::default();
    let respelled = {
        let mut walk = Walk {
            facts: &mut facts,
            levels: Vec::new(),
            net,
        };
        visit(&mut walk, parse, true)
    };
    let relations = facts.tables.keys().copied().collect();
    (respelled, Depends { relations })
}
