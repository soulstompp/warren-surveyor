// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a surveyor writes: nothing. It holds no entries, so a build, a new row and VACUUM leave its
//! file empty, and a scan of it is refused.

use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::{c_int, c_void};
use std::ptr::null_mut;

/// Builds nothing, and reports no count of rows, so the table's own count is left as it was.
#[pg_guard]
pub unsafe extern "C-unwind" fn build(
    _heap: pg_sys::Relation,
    _index: pg_sys::Relation,
    _info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let result = pg_sys::palloc0(std::mem::size_of::<pg_sys::IndexBuildResult>())
        as *mut pg_sys::IndexBuildResult;
    (*result).heap_tuples = -1.0;
    (*result).index_tuples = -1.0;
    result
}

/// An unlogged table's surveyor starts as empty as any other.
#[pg_guard]
pub unsafe extern "C-unwind" fn build_empty(_index: pg_sys::Relation) {}

/// A new row is nothing to a surveyor.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C-unwind" fn insert(
    _index: pg_sys::Relation,
    _values: *mut pg_sys::Datum,
    _isnull: *mut bool,
    _tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _unique: pg_sys::IndexUniqueCheck::Type,
    _unchanged: bool,
    _info: *mut pg_sys::IndexInfo,
) -> bool {
    false
}

/// VACUUM finds no entry to remove.
#[pg_guard]
pub unsafe extern "C-unwind" fn bulk_delete(
    _info: *mut pg_sys::IndexVacuumInfo,
    _stats: *mut pg_sys::IndexBulkDeleteResult,
    _callback: pg_sys::IndexBulkDeleteCallback,
    _callback_state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    null_mut()
}

/// VACUUM has nothing to count.
#[pg_guard]
pub unsafe extern "C-unwind" fn cleanup(
    _info: *mut pg_sys::IndexVacuumInfo,
    _stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    null_mut()
}

#[pg_guard]
pub unsafe extern "C-unwind" fn begin_scan(
    _index: pg_sys::Relation,
    _nkeys: c_int,
    _norderbys: c_int,
) -> pg_sys::IndexScanDesc {
    error!("a surveyor holds no entries and is never scanned")
}

#[pg_guard]
pub unsafe extern "C-unwind" fn rescan(
    _scan: pg_sys::IndexScanDesc,
    _keys: pg_sys::ScanKey,
    _nkeys: c_int,
    _orderbys: pg_sys::ScanKey,
    _norderbys: c_int,
) {
    error!("a surveyor holds no entries and is never scanned")
}

#[pg_guard]
pub unsafe extern "C-unwind" fn end_scan(_scan: pg_sys::IndexScanDesc) {
    error!("a surveyor holds no entries and is never scanned")
}
