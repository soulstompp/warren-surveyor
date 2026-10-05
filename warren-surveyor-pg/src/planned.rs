// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! For the tests: what the indexes measure of each base relation's conditions while a statement is
//! planned, read through the planner's own path hook.

use crate::query::cells;
use pgrx::prelude::*;
use std::cell::RefCell;
use std::ffi::CStr;

type Visit =
    Box<dyn FnMut(*mut pg_sys::PlannerInfo, *mut pg_sys::RelOptInfo, *mut pg_sys::IndexOptInfo)>;

thread_local! {
    static VISIT: RefCell<Option<Visit>> = const { RefCell::new(None) };
}
static mut NEXT: pg_sys::set_rel_pathlist_hook_type = None;

#[pg_guard]
unsafe extern "C-unwind" fn visit(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rti: pg_sys::Index,
    rte: *mut pg_sys::RangeTblEntry,
) {
    if let Some(next) = NEXT {
        next(root, rel, rti, rte);
    }
    if (*rel).reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
        || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
    {
        return;
    }
    for index in cells((*rel).indexlist) {
        VISIT.with(|v| {
            if let Some(f) = v.borrow_mut().as_mut() {
                crate::budget::tests::outside(|| f(root, rel, index as *mut pg_sys::IndexOptInfo));
            }
        });
    }
}

/// Plans `query` under EXPLAIN, calling `f` for each index of each base relation as the
/// relation's paths are made, outside the statement's planning-read budget.
pub(crate) fn planning(
    query: &str,
    f: impl FnMut(*mut pg_sys::PlannerInfo, *mut pg_sys::RelOptInfo, *mut pg_sys::IndexOptInfo)
        + 'static,
) {
    VISIT.with(|v| *v.borrow_mut() = Some(Box::new(f)));
    unsafe {
        NEXT = pg_sys::set_rel_pathlist_hook;
        pg_sys::set_rel_pathlist_hook = Some(visit);
    }
    let planned = Spi::run(&format!("EXPLAIN {query}"));
    unsafe { pg_sys::set_rel_pathlist_hook = NEXT };
    VISIT.with(|v| *v.borrow_mut() = None);
    planned.unwrap_or_else(|e| panic!("{query}: {e}"));
}

/// The name of the index an `IndexOptInfo` describes.
pub(crate) unsafe fn name(index: *mut pg_sys::IndexOptInfo) -> String {
    CStr::from_ptr(pg_sys::get_rel_name((*index).indexoid))
        .to_string_lossy()
        .into_owned()
}

#[pgrx::pg_schema]
mod tests {
    use super::{name, planning};
    use pgrx::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// What each GIN and GiST measured of `query`'s conditions while it was planned: the index,
    /// the rows, the pages read and the conditions held; for a GiST also the depth it stopped at,
    /// the pages it took for the level below, the children it named and the leaves it read.
    #[pg_extern]
    #[allow(clippy::type_complexity)]
    fn surveyed(
        query: &str,
    ) -> TableIterator<
        'static,
        (
            name!(index, String),
            name!(rows, f64),
            name!(pages, i32),
            name!(clauses, i32),
            name!(depth, Option<i32>),
            name!(below, Option<f64>),
            name!(named, Option<i32>),
            name!(leaves_read, Option<i32>),
        ),
    > {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let s = seen.clone();
        planning(query, move |root, rel, index| unsafe {
            if let Some(h) = crate::gin::held(root, rel, index) {
                s.borrow_mut().push((
                    name(index),
                    h.rows,
                    h.pages as i32,
                    h.clauses.len() as i32,
                    None,
                    None,
                    None,
                    None,
                ));
            }
            if let Some((m, clauses)) = crate::gist::survey(root, rel, index) {
                s.borrow_mut().push((
                    name(index),
                    m.rows,
                    m.pages as i32,
                    clauses.len() as i32,
                    Some(m.depth as i32),
                    Some(m.below),
                    Some(m.named as i32),
                    Some(m.leaves_read as i32),
                ));
            }
        });
        let out = seen.borrow().clone();
        TableIterator::new(out)
    }
}
