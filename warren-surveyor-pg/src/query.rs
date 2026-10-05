// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the surveyor's planning reads of a statement and of the catalog: whether a table is
//! surveyed for the question being planned, an expression beneath its relabelling, the leading key
//! column of each of a table's B-trees, the statistics of an index's key column, and whether
//! ANALYZE read every row of a table.

use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::c_void;

/// The rows ANALYZE samples for each unit of the largest statistics target it gathers
/// (`analyze.c`, `std_typanalyze`).
const ROWS_PER_TARGET: f64 = 300.0;

/// Whether the last ANALYZE of the table `relid` read every row of it, and the table has not grown
/// past what it read: the table's pages and rows, as that ANALYZE left them in `pg_class`, and the
/// pages the table has now with the rows they hold at the rows a page ANALYZE recorded, are no more
/// than the rows it samples. It samples 300 rows for each unit of the largest statistics target
/// among the table's columns, its indexes' expressions and its extended statistics, a target not
/// set read as `default_statistics_target`. The targets read are the ones set now: the catalog
/// keeps no record of the sample an ANALYZE took, so a table analyzed under another target is read
/// as though analyzed under the current one. False where `pg_class` holds no count of the table's
/// rows.
pub(crate) unsafe fn every_row_analyzed(relid: pg_sys::Oid) -> bool {
    // the statement already holds its lock on every table it reads
    let rel = pg_sys::relation_open(relid, pg_sys::NoLock as pg_sys::LOCKMODE);
    let (pages, rows) = ((*(*rel).rd_rel).relpages, (*(*rel).rd_rel).reltuples);
    // the pages the table has now, and the rows they hold at the rows a page ANALYZE recorded
    let now = pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM) as f64;
    let rows_now = if pages > 0 {
        now * rows as f64 / pages as f64
    } else if now > 0.0 {
        f64::INFINITY
    } else {
        0.0
    };
    let mut target = 0;
    let columns = (*(*rel).rd_att).natts;
    for attnum in 1..=columns {
        let attribute = pg_sys::TupleDescAttr((*rel).rd_att, attnum - 1);
        if !(*attribute).attisdropped {
            target = target.max(column_target(relid, attnum as i16));
        }
    }
    for index in oids(pg_sys::RelationGetIndexList(rel)) {
        let idx = pg_sys::index_open(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        if !pg_sys::RelationGetIndexExpressions(idx).is_null() {
            let form = (*idx).rd_index;
            for i in 0..(*form).indnkeyatts as usize {
                if *(*form).indkey.values.as_ptr().add(i) == 0 {
                    target = target.max(column_target(index, i as i16 + 1));
                }
            }
        }
        pg_sys::index_close(idx, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    }
    for statistics in oids(pg_sys::RelationGetStatExtList(rel)) {
        let tuple = pg_sys::SearchSysCache1(
            pg_sys::SysCacheIdentifier::STATEXTOID as i32,
            pg_sys::Datum::from(statistics),
        );
        if !tuple.is_null() {
            let mut null = false;
            let set = pg_sys::SysCacheGetAttr(
                pg_sys::SysCacheIdentifier::STATEXTOID as i32,
                tuple,
                pg_sys::Anum_pg_statistic_ext_stxstattarget as pg_sys::AttrNumber,
                &mut null,
            );
            if !null {
                target = target.max(set.value() as i16 as i32);
            }
            pg_sys::ReleaseSysCache(tuple);
        }
    }
    pg_sys::relation_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    let sampled = ROWS_PER_TARGET * target as f64;
    rows >= 0.0
        && target > 0
        && pages as f64 <= sampled
        && rows as f64 <= sampled
        && now <= sampled
        && rows_now <= sampled
}

/// The statistics target ANALYZE gathers column `attnum` of relation `relid` at: its own where set,
/// else `default_statistics_target`; 0 where it gathers none.
unsafe fn column_target(relid: pg_sys::Oid, attnum: i16) -> i32 {
    let tuple = pg_sys::SearchSysCache2(
        pg_sys::SysCacheIdentifier::ATTNUM as i32,
        pg_sys::Datum::from(relid),
        pg_sys::Datum::from(attnum as i32),
    );
    if tuple.is_null() {
        return 0;
    }
    let mut null = false;
    let set = pg_sys::SysCacheGetAttr(
        pg_sys::SysCacheIdentifier::ATTNUM as i32,
        tuple,
        pg_sys::Anum_pg_attribute_attstattarget as pg_sys::AttrNumber,
        &mut null,
    );
    pg_sys::ReleaseSysCache(tuple);
    let set = if null { -1 } else { set.value() as i16 as i32 };
    if set < 0 {
        pg_sys::default_statistics_target
    } else {
        set
    }
}

/// The surveyor's access method, or none where the extension is not in this database.
pub(crate) unsafe fn surveyor_am() -> pg_sys::Oid {
    pg_sys::get_index_am_oid(c"surveyor".as_ptr(), true)
}

/// Whether the planner's relation `rel` is surveyed: among its valid indexes, a surveyor over the
/// whole table, or where `proved`, a partial surveyor whose predicate the planner proved from the
/// question's own conditions. Several surveyors on one table count as one.
pub(crate) unsafe fn surveyed(rel: *mut pg_sys::RelOptInfo, proved: bool) -> bool {
    let am = surveyor_am();
    if rel.is_null() || am == pg_sys::InvalidOid {
        return false;
    }
    cells((*rel).indexlist).into_iter().any(|i| {
        let index = i as *mut pg_sys::IndexOptInfo;
        (*index).relam == am && ((*index).indpred.is_null() || (proved && (*index).predOK))
    })
}

/// The expression beneath any relabelling.
pub(crate) unsafe fn bare(mut node: *mut pg_sys::Node) -> *mut pg_sys::Node {
    while !node.is_null() && (*node).type_ == pg_sys::NodeTag::T_RelabelType {
        node = (*(node as *mut pg_sys::RelabelType)).arg as *mut pg_sys::Node;
    }
    node
}

pub(crate) unsafe fn oids(list: *mut pg_sys::List) -> Vec<pg_sys::Oid> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).oid_value)
        .collect()
}

/// The pointers a list holds.
pub(crate) unsafe fn cells(list: *mut pg_sys::List) -> Vec<*mut c_void> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).ptr_value)
        .collect()
}

/// Releases what `examine_indexcol` found.
pub(crate) unsafe fn release(vardata: &mut pg_sys::VariableStatData) {
    if !vardata.statsTuple.is_null() {
        if let Some(free) = vardata.freefunc {
            free(vardata.statsTuple);
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn release_from_cache(tuple: pg_sys::HeapTuple) {
    pg_sys::ReleaseSysCache(tuple);
}

/// The statistics of key column `column` of `index`: the table's for a column, the index's own for
/// an expression, through the statistics hooks where they answer.
pub(crate) unsafe fn examine_indexcol(
    root: *mut pg_sys::PlannerInfo,
    index: *mut pg_sys::IndexOptInfo,
    column: usize,
    vardata: &mut pg_sys::VariableStatData,
) {
    let key = *(*index).indexkeys.add(column);
    if key != 0 {
        let rte = *(*root).simple_rte_array.add((*(*index).rel).relid as usize);
        let attnum = key as pg_sys::AttrNumber;
        vardata.rel = (*index).rel;
        let answered = match pg_sys::get_relation_stats_hook {
            Some(hook) => hook(root, rte, attnum, vardata),
            None => false,
        };
        if answered {
            if !vardata.statsTuple.is_null() && vardata.freefunc.is_none() {
                error!("no function provided to release variable stats with");
            }
        } else {
            vardata.statsTuple = pg_sys::SearchSysCache3(
                pg_sys::SysCacheIdentifier::STATRELATTINH as i32,
                pg_sys::Datum::from((*rte).relid),
                pg_sys::Datum::from(attnum as i32),
                pg_sys::Datum::from((*rte).inh),
            );
            vardata.freefunc = Some(release_from_cache);
        }
    } else {
        let relid = (*index).indexoid;
        let attnum = (column + 1) as pg_sys::AttrNumber;
        let answered = match pg_sys::get_index_stats_hook {
            Some(hook) => hook(root, relid, attnum, vardata),
            None => false,
        };
        if answered {
            if !vardata.statsTuple.is_null() && vardata.freefunc.is_none() {
                error!("no function provided to release variable stats with");
            }
        } else {
            vardata.statsTuple = pg_sys::SearchSysCache3(
                pg_sys::SysCacheIdentifier::STATRELATTINH as i32,
                pg_sys::Datum::from(relid),
                pg_sys::Datum::from(attnum as i32),
                pg_sys::Datum::from(false),
            );
            vardata.freefunc = Some(release_from_cache);
        }
    }
}

/// The distinct values of key column `column` of `index`, from the planner's statistics: the
/// statistics hooks' where they answer, else ANALYZE's, else the planner's default.
pub(crate) unsafe fn distinct_of(
    root: *mut pg_sys::PlannerInfo,
    index: *mut pg_sys::IndexOptInfo,
    column: usize,
) -> f64 {
    let mut vardata = pg_sys::VariableStatData::default();
    examine_indexcol(root, index, column, &mut vardata);
    vardata.rel = (*index).rel;
    let mut is_default = false;
    let distinct = pg_sys::get_variable_numdistinct(&mut vardata, &mut is_default);
    release(&mut vardata);
    distinct
}
