// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::pg_sys;

/// `warren_surveyor_pg.planning_read_limit`: the most pages the surveyor reads while a statement is
/// planned, below the pages of the tables the statement reads (`budget`); -1 sets no limit below
/// them, and 0 reads none.
static PLANNING_READ_LIMIT: GucSetting<i32> = GucSetting::<i32>::new(-1);

/// `warren_surveyor_pg.planning_read_limit` as it stands, in pages: -1 where it sets no limit.
pub(crate) fn planning_read_limit() -> i32 {
    PLANNING_READ_LIMIT.get()
}

pub fn init() {
    GucRegistry::define_int_guc(
        c"warren_surveyor_pg.planning_read_limit",
        c"Sets the most index pages the surveyor reads while a statement is planned.",
        c"The surveyor never reads more pages while a statement is planned than the tables the statement reads hold; this lowers that. -1 sets no lower limit, and 0 reads no page, so that every estimate is PostgreSQL's own.",
        &PLANNING_READ_LIMIT,
        -1,
        i32::MAX,
        GucContext::Suset,
        GucFlags::UNIT_BLOCKS,
    );
    unsafe {
        pg_sys::MarkGUCPrefixReserved(c"warren_surveyor_pg".as_ptr());
    }
}
