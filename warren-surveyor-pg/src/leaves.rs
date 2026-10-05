// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

use pgrx::pg_sys;

/// The share of the rows the statistics `gathered` give `value`, where their most common values
/// name it, compared by the operator `equality` under `collation` as the planner compares a
/// constant with them, and read only where the planner would read them: where every row of the
/// column may be read (`readable`), or the operator is leakproof. None where they name no such
/// value.
pub(crate) unsafe fn listed_share(
    gathered: pg_sys::HeapTuple,
    equality: pg_sys::Oid,
    collation: pg_sys::Oid,
    value: pg_sys::Datum,
    readable: bool,
) -> Option<f64> {
    if gathered.is_null() {
        return None;
    }
    let function = pg_sys::get_opcode(equality);
    let mut vardata = pg_sys::VariableStatData {
        statsTuple: gathered,
        acl_ok: readable,
        ..Default::default()
    };
    if !pg_sys::statistic_proc_security_check(&mut vardata, function) {
        return None;
    }
    let mut slot = pg_sys::AttStatsSlot::default();
    if !pg_sys::get_attstatsslot(
        &mut slot,
        gathered,
        pg_sys::STATISTIC_KIND_MCV as i32,
        pg_sys::InvalidOid,
        (pg_sys::ATTSTATSSLOT_VALUES | pg_sys::ATTSTATSSLOT_NUMBERS) as i32,
    ) {
        return None;
    }
    let mut compare: pg_sys::FmgrInfo = std::mem::zeroed();
    pg_sys::fmgr_info(function, &mut compare);
    let mut share = None;
    for i in 0..(slot.nvalues.min(slot.nnumbers)) as usize {
        let equal = pg_sys::FunctionCall2Coll(&mut compare, collation, *slot.values.add(i), value);
        if equal.value() != 0 {
            share = Some(*slot.numbers.add(i) as f64);
            break;
        }
    }
    pg_sys::free_attstatsslot(&mut slot);
    share
}

/// What ANALYZE gathered for column `attnum` of relation `relid`, alone and not with inheritance
/// children, from the catalog's cache; null when it gathered nothing.
pub(crate) unsafe fn gathered_statistics(
    relid: pg_sys::Oid,
    attnum: pg_sys::AttrNumber,
) -> pg_sys::HeapTuple {
    pg_sys::SearchSysCache3(
        pg_sys::SysCacheIdentifier::STATRELATTINH as i32,
        pg_sys::Datum::from(relid),
        pg_sys::Datum::from(attnum as i32),
        pg_sys::Datum::from(false),
    )
}
