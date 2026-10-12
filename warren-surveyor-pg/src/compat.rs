// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What differs between the PostgreSQL versions the surveyor builds for, each behind one name.

use crate::price::PriceFunction;
use pgrx::pg_sys;
use std::ffi::{c_char, c_int};

/// The largest statistics target a column takes.
#[cfg(any(feature = "pg17", feature = "pg18", feature = "pg19"))]
pub(crate) const MAX_STATISTICS_TARGET: usize = pg_sys::MAX_STATISTICS_TARGET as usize;
#[cfg(any(feature = "pg14", feature = "pg15", feature = "pg16"))]
pub(crate) const MAX_STATISTICS_TARGET: usize = 10000;

/// The number a relation's file is known by in its tablespace.
#[cfg(any(feature = "pg14", feature = "pg15"))]
pub(crate) type RelFileNumber = pg_sys::Oid;
#[cfg(not(any(feature = "pg14", feature = "pg15")))]
pub(crate) type RelFileNumber = pg_sys::RelFileNumber;

#[cfg(any(feature = "pg18", feature = "pg19"))]
pub(crate) use pg_sys::{GinMetaPageData, GinPageOpaqueData};

/// The special space of a GIN page.
#[cfg(not(any(feature = "pg18", feature = "pg19")))]
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct GinPageOpaqueData {
    pub(crate) rightlink: pg_sys::BlockNumber,
    pub(crate) maxoff: pg_sys::OffsetNumber,
    pub(crate) flags: u16,
}

/// The contents of a GIN's metapage.
#[cfg(not(any(feature = "pg18", feature = "pg19")))]
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
pub(crate) struct GinMetaPageData {
    pub(crate) head: pg_sys::BlockNumber,
    pub(crate) tail: pg_sys::BlockNumber,
    pub(crate) tailFreeSize: u32,
    pub(crate) nPendingPages: pg_sys::BlockNumber,
    pub(crate) nPendingHeapTuples: i64,
    pub(crate) nTotalPages: pg_sys::BlockNumber,
    pub(crate) nEntryPages: pg_sys::BlockNumber,
    pub(crate) nDataPages: pg_sys::BlockNumber,
    pub(crate) nEntries: i64,
    pub(crate) ginVersion: i32,
}

/// Column `i` of `desc`, counted from 0.
pub(crate) unsafe fn attribute(
    desc: pg_sys::TupleDesc,
    i: c_int,
) -> *mut pg_sys::FormData_pg_attribute {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::TupleDescAttr(desc, i)
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        (*desc).attrs.as_mut_ptr().add(i as usize)
    }
}

/// The range table entry a column reads.
pub(crate) unsafe fn varno(var: *mut pg_sys::Var) -> c_int {
    #[cfg(feature = "pg14")]
    {
        (*var).varno as c_int
    }
    #[cfg(not(feature = "pg14"))]
    {
        (*var).varno
    }
}

/// A column `attno` of range table entry `varno`, of the type, type modifier and collation given,
/// `levelsup` query levels out.
pub(crate) unsafe fn make_var(
    varno: c_int,
    attno: i16,
    type_: pg_sys::Oid,
    typmod: i32,
    collation: pg_sys::Oid,
    levelsup: pg_sys::Index,
) -> *mut pg_sys::Var {
    #[cfg(feature = "pg14")]
    let varno = varno as pg_sys::Index;
    pg_sys::makeVar(varno, attno, type_, typmod, collation, levelsup)
}

/// Runs `desc` forward until it has returned `count` more rows, or all of them.
pub(crate) unsafe fn executor_run(desc: *mut pg_sys::QueryDesc, count: u64) {
    let forward = pg_sys::ScanDirection::ForwardScanDirection;
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::ExecutorRun(desc, forward, count);
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        // it is run again for the rows that follow
        pg_sys::ExecutorRun(desc, forward, count, false);
    }
}

/// Begins a scan of `index` over `heap` under `snapshot`, with `nkeys` keys and `norderbys`
/// orderings.
pub(crate) unsafe fn index_beginscan(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    snapshot: pg_sys::Snapshot,
    nkeys: c_int,
    norderbys: c_int,
) -> pg_sys::IndexScanDesc {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::index_beginscan(
            heap,
            index,
            snapshot,
            std::ptr::null_mut(),
            nkeys,
            norderbys,
        )
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        pg_sys::index_beginscan(heap, index, snapshot, nkeys, norderbys)
    }
}

/// The planner's estimate of the elements of the array `array`.
pub(crate) unsafe fn estimate_array_length(
    root: *mut pg_sys::PlannerInfo,
    array: *mut pg_sys::Node,
) -> f64 {
    #[cfg(any(feature = "pg14", feature = "pg15", feature = "pg16"))]
    {
        let _ = root;
        pg_sys::estimate_array_length(array) as f64
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15", feature = "pg16")))]
    {
        pg_sys::estimate_array_length(root, array)
    }
}

/// The GROUP BY the planner groups by.
pub(crate) unsafe fn group_clause(root: *mut pg_sys::PlannerInfo) -> *mut pg_sys::List {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        (*(*root).parse).groupClause
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        (*root).processed_groupClause
    }
}

/// The access method of the relation `relid`.
pub(crate) unsafe fn get_rel_relam(relid: pg_sys::Oid) -> pg_sys::Oid {
    #[cfg(any(feature = "pg14", feature = "pg15", feature = "pg16"))]
    {
        let tuple = pg_sys::SearchSysCache1(
            pg_sys::SysCacheIdentifier::RELOID as i32,
            pg_sys::Datum::from(relid),
        );
        if tuple.is_null() {
            pgrx::error!("cache lookup failed for relation {relid}");
        }
        let form = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_class>(tuple);
        let relam = (*form).relam;
        pg_sys::ReleaseSysCache(tuple);
        relam
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15", feature = "pg16")))]
    {
        pg_sys::get_rel_relam(relid)
    }
}

/// The database, the tablespace and the file of the page a buffer's tag names.
pub(crate) fn tagged(tag: &pg_sys::BufferTag) -> (pg_sys::Oid, pg_sys::Oid, RelFileNumber) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        (tag.rnode.dbNode, tag.rnode.spcNode, tag.rnode.relNode)
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        (tag.dbOid, tag.spcOid, tag.relNumber)
    }
}

/// The tablespace and the file of the relation `rel`.
pub(crate) unsafe fn relation_file(rel: pg_sys::Relation) -> (pg_sys::Oid, RelFileNumber) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        ((*rel).rd_node.spcNode, (*rel).rd_node.relNode)
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        ((*rel).rd_locator.spcOid, (*rel).rd_locator.relNumber)
    }
}

/// Unlocks the header of the buffer `desc`, which `LockBufHdr` locked and returned `state` from.
pub(crate) unsafe fn unlock_buffer_header(desc: *mut pg_sys::BufferDesc, state: u32) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        let word =
            std::sync::atomic::AtomicU32::from_ptr(std::ptr::addr_of_mut!((*desc).state.value));
        word.store(
            state & !pg_sys::BM_LOCKED,
            std::sync::atomic::Ordering::Release,
        );
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        pg_sys::UnlockBufHdr(desc, state);
    }
}

/// The WAL position the page `page` was last written at.
pub(crate) unsafe fn page_lsn(page: pg_sys::Page) -> u64 {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        let lsn = (*(page as *const pg_sys::PageHeaderData)).pd_lsn;
        ((lsn.xlogid as u64) << 32) | lsn.xrecoff as u64
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        pg_sys::PageGetLSN(page)
    }
}

/// Reserves the settings that begin with `prefix`, so that one of them the surveyor does not
/// define is refused.
pub(crate) unsafe fn reserve_prefix(prefix: *const c_char) {
    #[cfg(feature = "pg14")]
    {
        pg_sys::EmitWarningsOnPlaceholders(prefix);
    }
    #[cfg(not(feature = "pg14"))]
    {
        pg_sys::MarkGUCPrefixReserved(prefix);
    }
}

/// Makes `price` the function the planner prices paths through `index` with.
pub(crate) unsafe fn set_index_price(index: *mut pg_sys::IndexOptInfo, price: PriceFunction) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        // the field's type names no arguments on these versions
        let price = std::mem::transmute::<PriceFunction, unsafe extern "C-unwind" fn()>(price);
        (*index).amcostestimate = Some(price);
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        (*index).amcostestimate = Some(price);
    }
}

/// The statement `stmt`, whose text is `text`, analyzed and rewritten, with no parameters.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) unsafe fn analyze_and_rewrite(
    stmt: *mut pg_sys::RawStmt,
    text: *const c_char,
) -> *mut pg_sys::List {
    #[cfg(feature = "pg14")]
    {
        pg_sys::pg_analyze_and_rewrite(stmt, text, std::ptr::null_mut(), 0, std::ptr::null_mut())
    }
    #[cfg(not(feature = "pg14"))]
    {
        pg_sys::pg_analyze_and_rewrite_fixedparams(
            stmt,
            text,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
        )
    }
}

/// Seeds the session's random state that a GiST build draws from.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) unsafe fn seed_random(seed: u32) {
    #[cfg(feature = "pg14")]
    {
        extern "C" {
            fn srandom(seed: std::ffi::c_uint);
        }
        srandom(seed);
    }
    #[cfg(not(feature = "pg14"))]
    {
        pg_sys::pg_prng_seed(
            std::ptr::addr_of_mut!(pg_sys::pg_global_prng_state),
            seed as u64,
        );
    }
}

/// The pages of the B-tree `index` after its metapage, each with the columns `bt_page_stats` gives
/// it, as an item of a FROM list.
#[cfg(any(test, feature = "pg_test"))]
pub(crate) fn page_stats(index: &str) -> String {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        format!(
            "(SELECT p.* FROM generate_series(1::int8, \
             pg_relation_size('{index}') / current_setting('block_size')::int8 - 1) b, \
             LATERAL bt_page_stats('{index}', b) p)"
        )
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        format!("bt_multi_page_stats('{index}', 1, -1)")
    }
}
