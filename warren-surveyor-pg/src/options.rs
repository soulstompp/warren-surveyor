// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! A surveyor's storage parameters, set with `CREATE INDEX … WITH (…)` or `ALTER INDEX … SET (…)`:
//! it takes none, and refuses any it is given.

use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::c_int;

static mut KIND: pg_sys::relopt_kind::Type = 0;

/// The parsed parameters, as Postgres stores them on the index: none.
#[repr(C)]
pub struct Options {
    vl_len_: i32,
}

/// Registers the kind of the parameters, once, when the library loads.
pub fn init() {
    unsafe {
        KIND = pg_sys::add_reloption_kind();
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn parse(
    reloptions: pg_sys::Datum,
    validate: bool,
) -> *mut pg_sys::bytea {
    pg_sys::build_reloptions(
        reloptions,
        validate,
        KIND,
        std::mem::size_of::<Options>(),
        std::ptr::null(),
        0 as c_int,
    ) as *mut pg_sys::bytea
}
