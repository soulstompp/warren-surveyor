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

/// Whether `role` may use the foreign server `server`.
pub(crate) unsafe fn server_usage(
    server: pg_sys::Oid,
    role: pg_sys::Oid,
) -> pg_sys::AclResult::Type {
    let usage = pg_sys::ACL_USAGE as pg_sys::AclMode;
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        pg_sys::pg_foreign_server_aclcheck(server, role, usage)
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        pg_sys::object_aclcheck(pg_sys::ForeignServerRelationId, server, role, usage)
    }
}
