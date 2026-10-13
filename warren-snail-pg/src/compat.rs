// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What differs between the PostgreSQL versions the wrapper builds for, each behind one name.

use pgrx::pg_sys;
use std::ptr::null_mut;

/// A path that scans the foreign table alone, in no order, with no parameter, at the rows and costs
/// given.
pub(crate) unsafe fn foreign_scan_path(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rows: f64,
    startup: f64,
    total: f64,
) -> *mut pg_sys::ForeignPath {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::create_foreignscan_path(
            root,
            rel,
            null_mut(),
            rows,
            0,
            startup,
            total,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    }
    #[cfg(feature = "pg17")]
    {
        pg_sys::create_foreignscan_path(
            root,
            rel,
            null_mut(),
            rows,
            startup,
            total,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    }
    #[cfg(any(feature = "pg14", feature = "pg15", feature = "pg16"))]
    {
        pg_sys::create_foreignscan_path(
            root,
            rel,
            null_mut(),
            rows,
            startup,
            total,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    }
}
