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
/// surveyor, whose drives are busy, is planned with its pages from the disk weighed by them
/// (`drive`); outside a parallel operation, where settings cannot be made.
unsafe fn planned(
    parse: *mut pg_sys::Query,
    plan: impl FnOnce() -> *mut pg_sys::PlannedStmt,
) -> *mut pg_sys::PlannedStmt {
    let outermost = outermost();
    let _round = Round::enter();
    if outermost {
        crate::budget::begin(parse);
    }
    plan()
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
