// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! One round of planning: a statement, from the planner's entry to its return. What the surveyor
//! measures while a statement is planned is kept for that round and forgotten when it ends: a
//! relation's measure and the values a WITH query's read kept when the planning that made them
//! returns, everything else when the outermost one does. Outside the planner nothing is kept. The outermost statement is planned with
//! its pages from the disk weighed by how busy their drives are (`drive`), and every page the
//! surveyor reads while it is planned, statements planned inside it among them, draws on its one
//! budget (`budget`).
//!
//! A subtransaction rolled back while a statement is planned, such as a respelling that raised an
//! error, gives back what was measured and drawn inside it: the B-trees' leaves, the WITH queries'
//! reads, the relations' conditions and the budget are put back as they stood when it began, so the
//! planning after it is the one it would have been. The subtransaction of one of the surveyor's own
//! reads keeps them, as the read counts the pages it read.

use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_void};

static mut NEXT_PLANNER: pg_sys::planner_hook_type = None;

thread_local! {
    /// How deep the statements being planned are nested.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    /// For each subtransaction begun while a statement is planned and not yet ended, innermost
    /// last: its id, and what the surveyor kept and drew when it began.
    static BEGUN: RefCell<Vec<(pg_sys::SubTransactionId, Kept)>> = const { RefCell::new(Vec::new()) };
    /// Set while one of the surveyor's own reads begins its subtransaction.
    static OWN_READ: Cell<bool> = const { Cell::new(false) };
}

/// Puts the hook in place, after any hook already there, and follows the subtransactions begun
/// while a statement is planned.
pub fn init() {
    unsafe {
        NEXT_PLANNER = pg_sys::planner_hook;
        pg_sys::planner_hook = Some(plan);
        pg_sys::RegisterSubXactCallback(Some(subtransaction), std::ptr::null_mut());
    }
}

/// Begins the subtransaction one of the surveyor's own reads runs in. Rolled back, it keeps what
/// the surveyor measured and drew inside it: the read counts the pages it read.
pub(crate) unsafe fn begin_own_read() {
    OWN_READ.set(true);
    pg_sys::BeginInternalSubTransaction(std::ptr::null());
    OWN_READ.set(false);
}

/// How deep the statements being planned are nested: 0 outside the planner, where nothing is kept.
/// A planning that did not enter through the planner's hook counts as one while the surveyor's
/// hooks run for it (`budget`), and keeps what they measure until its top level is planned.
pub(crate) fn depth() -> u32 {
    DEPTH.get() + crate::budget::planning_outside_round() as u32
}

/// Whether a statement is being planned through the planner's hook.
pub(crate) fn in_round() -> bool {
    DEPTH.get() > 0
}

/// Whether a statement entering the planner's hook now is the outermost being planned: none is in
/// a round, and none is planned outside one while the surveyor's hooks run for it.
fn outermost() -> bool {
    depth() == 0
}

/// Forgets everything kept for the round, or for a planning outside one.
pub(crate) fn forget_all() {
    crate::leaves::forget();
    crate::closure::forget_from(0);
    crate::size::forget_from(0);
    crate::price::forget();
    crate::drive::forget();
    crate::budget::forget();
}

/// A statement's planning: entered at the planner's entry, left at its return.
struct Round;

impl Round {
    fn enter() -> Round {
        if outermost() {
            forget_all();
        }
        DEPTH.set(DEPTH.get() + 1);
        Round
    }
}

impl Drop for Round {
    fn drop(&mut self) {
        crate::size::forget_from(depth());
        crate::closure::forget_from(depth());
        DEPTH.set(DEPTH.get().saturating_sub(1));
        if outermost() {
            forget_all();
        }
    }
}

/// Plans the statement `parse` through `plan`, inside a round. The outermost statement starts the
/// budget its reads draw on (`budget`). The outermost statement that reads a table carrying a
/// surveyor is planned with the cache counted as shared buffers, and, where its drives are busy,
/// with its pages from the disk weighed by them (`drive`); outside a parallel operation, where
/// settings cannot be made.
unsafe fn planned(
    parse: *mut pg_sys::Query,
    plan: impl FnOnce() -> *mut pg_sys::PlannedStmt,
) -> *mut pg_sys::PlannedStmt {
    let outermost = outermost();
    let _round = Round::enter();
    if outermost {
        crate::budget::begin(parse);
    }
    let heat = if outermost && !pg_sys::IsInParallelMode() {
        crate::drive::statement_weight(parse)
    } else {
        None
    };
    match heat {
        Some(heat) => crate::drive::planned_held(heat, plan),
        None => plan(),
    }
}

#[cfg(not(feature = "pg19"))]
#[pg_guard]
unsafe extern "C-unwind" fn plan(
    parse: *mut pg_sys::Query,
    query_string: *const c_char,
    cursor_options: c_int,
    bound_params: pg_sys::ParamListInfo,
) -> *mut pg_sys::PlannedStmt {
    planned(parse, || match NEXT_PLANNER {
        Some(next) => pg_sys::ffi::pg_guard_ffi_boundary(|| {
            next(parse, query_string, cursor_options, bound_params)
        }),
        None => pg_sys::standard_planner(parse, query_string, cursor_options, bound_params),
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
    planned(parse, || match NEXT_PLANNER {
        Some(next) => pg_sys::ffi::pg_guard_ffi_boundary(|| {
            next(parse, query_string, cursor_options, bound_params, es)
        }),
        None => pg_sys::standard_planner(parse, query_string, cursor_options, bound_params, es),
    })
}

/// What the surveyor has measured and drawn for the statement being planned: what its B-trees'
/// leaves, its WITH queries' reads and its relations' conditions measured, and its budget.
struct Kept {
    leaves: crate::leaves::Saved,
    closure: crate::closure::Saved,
    size: crate::size::Saved,
    budget: crate::budget::Saved,
}

impl Kept {
    fn now() -> Kept {
        Kept {
            leaves: crate::leaves::saved(),
            closure: crate::closure::saved(),
            size: crate::size::saved(),
            budget: crate::budget::saved(),
        }
    }

    fn restore(self) {
        crate::leaves::put_back(self.leaves);
        crate::closure::put_back(self.closure);
        crate::size::put_back(self.size);
        crate::budget::put_back(self.budget);
    }
}

/// A subtransaction rolled back while a statement is planned gives back what the surveyor kept and
/// drew inside it, so that the planning after it reads as one that never tried it.
#[pg_guard]
unsafe extern "C-unwind" fn subtransaction(
    event: pg_sys::SubXactEvent::Type,
    id: pg_sys::SubTransactionId,
    _parent: pg_sys::SubTransactionId,
    _arg: *mut c_void,
) {
    match event {
        pg_sys::SubXactEvent::SUBXACT_EVENT_START_SUB if depth() > 0 && !OWN_READ.get() => {
            BEGUN.with(|b| b.borrow_mut().push((id, Kept::now())));
        }
        pg_sys::SubXactEvent::SUBXACT_EVENT_COMMIT_SUB => {
            BEGUN.with(|b| b.borrow_mut().retain(|(begun, _)| *begun != id));
        }
        pg_sys::SubXactEvent::SUBXACT_EVENT_ABORT_SUB => {
            let kept = BEGUN.with(|b| {
                let mut b = b.borrow_mut();
                let at = b.iter().position(|(begun, _)| *begun == id)?;
                // those begun inside it ended with it
                Some(b.split_off(at).swap_remove(0).1)
            });
            // once the statement's planning has ended there is nothing to put back
            if let Some(kept) = kept.filter(|_| depth() > 0) {
                kept.restore();
            }
        }
        _ => {}
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use pgrx::prelude::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::CStr;
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

    static mut NEXT_PATHLIST: pg_sys::set_rel_pathlist_hook_type = None;
    static mut NEXT_EMIT: pg_sys::emit_log_hook_type = None;

    /// The speller's note of the respelling the test makes fail.
    const FAILED: &str =
        "warren-speller-pg: planned as written, the respelling failed: a seeded failure";

    thread_local! {
        /// The pages of the budget left when a scan of a WITH query named `side_a` raised its
        /// seeded error, once one has.
        static LEFT_AT_FAILURE: Cell<Option<u32>> = const { Cell::new(None) };
        /// The surveyor's and the speller's notes, while a test keeps them.
        static NOTES: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    #[pg_guard]
    unsafe extern "C-unwind" fn failing_at_side_a(
        root: *mut pg_sys::PlannerInfo,
        rel: *mut pg_sys::RelOptInfo,
        rti: pg_sys::Index,
        rte: *mut pg_sys::RangeTblEntry,
    ) {
        if let Some(next) = NEXT_PATHLIST {
            next(root, rel, rti, rte);
        }
        if (*rte).rtekind == pg_sys::RTEKind::RTE_CTE
            && CStr::from_ptr((*rte).ctename).to_bytes() == b"side_a"
        {
            LEFT_AT_FAILURE.set(Some(crate::budget::left()));
            ereport!(
                PgLogLevel::ERROR,
                PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                "a seeded failure"
            );
        }
    }

    /// Keeps each of the surveyor's and the speller's notes as it goes to the server's log.
    #[pg_guard]
    unsafe extern "C-unwind" fn keep_note(edata: *mut pg_sys::ErrorData) {
        if let Some(next) = NEXT_EMIT {
            next(edata);
        }
        if edata.is_null() || (*edata).message.is_null() {
            return;
        }
        let text = CStr::from_ptr((*edata).message).to_string_lossy();
        if text.starts_with("surveyor:") || text.starts_with("warren-speller-pg:") {
            NOTES.with(|n| {
                if let Some(n) = n.borrow_mut().as_mut() {
                    n.push(text.into_owned());
                }
            });
        }
    }

    /// Runs `f` while a scan of a WITH query named `side_a` raises an ERROR as it is planned.
    fn failing_at_side_a_while<T>(f: impl FnOnce() -> T) -> T {
        LEFT_AT_FAILURE.set(None);
        unsafe {
            NEXT_PATHLIST = pg_sys::set_rel_pathlist_hook;
            pg_sys::set_rel_pathlist_hook = Some(failing_at_side_a);
        }
        let result = catch_unwind(AssertUnwindSafe(f));
        unsafe { pg_sys::set_rel_pathlist_hook = NEXT_PATHLIST };
        result.unwrap_or_else(|e| resume_unwind(e))
    }

    /// Runs `f` with every statement planned through the surveyor's round alone, no respelling
    /// tried.
    fn never_respelled<T>(f: impl FnOnce() -> T) -> T {
        let speller = unsafe { super::NEXT_PLANNER };
        unsafe { super::NEXT_PLANNER = None };
        let result = catch_unwind(AssertUnwindSafe(f));
        unsafe { super::NEXT_PLANNER = speller };
        result.unwrap_or_else(|e| resume_unwind(e))
    }

    /// What planning `sql` gives: its plan, each line with the rows and width it is estimated at
    /// and without its costs; the surveyor's and the speller's notes at DEBUG1; and the pages the
    /// surveyor drew, with its budget.
    fn planned(sql: &str) -> (Vec<String>, Vec<String>, (u64, Option<u64>)) {
        crate::budget::tests::LAST.set(None);
        NOTES.with(|n| *n.borrow_mut() = Some(Vec::new()));
        unsafe {
            NEXT_EMIT = pg_sys::emit_log_hook;
            pg_sys::emit_log_hook = Some(keep_note);
        }
        let lines = catch_unwind(|| {
            Spi::run("SET LOCAL log_min_messages = debug1").unwrap();
            crate::tests::texts(&format!("EXPLAIN {sql}"))
        });
        unsafe { pg_sys::emit_log_hook = NEXT_EMIT };
        Spi::run("RESET log_min_messages").unwrap();
        let notes = NOTES.with(|n| n.borrow_mut().take().unwrap_or_default());
        let plan = lines
            .unwrap_or_else(|e| resume_unwind(e))
            .into_iter()
            .map(|line| match (line.find("(cost="), line.find(" rows=")) {
                (Some(from), Some(to)) => format!("{}({}", &line[..from], &line[to + 1..]),
                _ => line,
            })
            .collect();
        let drawn = crate::budget::tests::LAST
            .get()
            .expect("a statement planned");
        (plan, notes, drawn)
    }

    /// A respelling that measures and then fails leaves the statement planned as written as a
    /// planning that never tried it plans it: the same sizes, measured by the same reads, and the
    /// same pages drawn of the same budget.
    #[pg_test]
    fn a_statement_whose_respelling_fails_is_planned_with_the_sizes_and_budget_of_one_never_respelled(
    ) {
        lego();
        // the purchases of each December, which the surveyor on the purchases measures
        let query = BY_ZONE_AND_CATEGORY.replace(
            "GROUP BY 1, 2",
            "WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12 GROUP BY 1, 2",
        );
        let (never_plan, never_notes, never_drawn) = never_respelled(|| planned(&query));
        let (plan, notes, drawn) = failing_at_side_a_while(|| planned(&query));
        let left = LEFT_AT_FAILURE.get().expect("the respelling did not fail");
        let limit = never_drawn.1.expect("no budget counted");
        // the surveyor measured inside the respelling, and drew on the budget, before it failed
        assert!(
            u64::from(left) < limit,
            "{left} pages left of {limit} when the respelling failed"
        );
        let failed = notes.iter().position(|n| n == FAILED);
        let failed = failed.unwrap_or_else(|| panic!("{notes:#?}"));
        let measured = |n: &[String]| {
            n.iter()
                .filter(|n| n.starts_with("surveyor: lego_purchases measured at "))
                .count()
        };
        assert_eq!(measured(&notes[..failed]), 1, "{notes:#?}");
        assert_eq!(measured(&never_notes), 1, "{never_notes:#?}");
        assert!(!plan.join("\n").contains("side_a"), "{plan:#?}");
        assert_eq!(plan, never_plan);
        assert_eq!(notes[failed + 1..], never_notes[..]);
        assert_eq!(drawn, never_drawn);
    }
}
