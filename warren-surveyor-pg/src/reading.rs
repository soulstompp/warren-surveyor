// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the GIN and GiST reads share: a copy of an index page, read as the index reads it and drawn
//! on the statement's planning-read budget (`budget`), a row's address as one number, and a
//! relation's condition on an index's first column against a constant.

use crate::conditions::{column_of, constant, strategy};
use crate::query::cells;
use pgrx::pg_sys;

/// A copy of one page of an index.
#[derive(Clone)]
pub(crate) struct PageCopy(Box<[u64]>);

impl PageCopy {
    pub(crate) fn ptr(&self) -> pg_sys::Page {
        self.0.as_ptr() as pg_sys::Page
    }

    /// The page's special space, read as `T`.
    pub(crate) unsafe fn special<T>(&self) -> &T {
        &*(pg_sys::PageGetSpecialPointer(self.ptr()) as *const T)
    }

    /// The page's contents after its header.
    pub(crate) unsafe fn contents(&self) -> *const u8 {
        pg_sys::PageGetContents(self.ptr()) as *const u8
    }

    /// The end of the space the page's header says is in use from its start.
    pub(crate) unsafe fn lower(&self) -> usize {
        (*(self.ptr() as *const pg_sys::PageHeaderData)).pd_lower as usize
    }

    /// The page's WAL position.
    pub(crate) unsafe fn lsn(&self) -> u64 {
        pg_sys::PageGetLSN(self.ptr())
    }

    /// The offset of the page's last item.
    pub(crate) unsafe fn last(&self) -> pg_sys::OffsetNumber {
        pg_sys::PageGetMaxOffsetNumber(self.ptr())
    }

    pub(crate) unsafe fn item(&self, offset: pg_sys::OffsetNumber) -> pg_sys::IndexTuple {
        pg_sys::PageGetItem(self.ptr(), pg_sys::PageGetItemId(self.ptr(), offset))
            as pg_sys::IndexTuple
    }

    /// Whether the item at `offset` is marked dead.
    pub(crate) unsafe fn dead(&self, offset: pg_sys::OffsetNumber) -> bool {
        (*pg_sys::PageGetItemId(self.ptr(), offset)).lp_flags() == pg_sys::LP_DEAD
    }
}

/// Reads block `block` of `index`, locked to share, into a copy. None where the statement's
/// planning-read budget has no page left.
pub(crate) unsafe fn read(index: pg_sys::Relation, block: pg_sys::BlockNumber) -> Option<PageCopy> {
    if !crate::budget::draw((*index).rd_id) {
        return None;
    }
    let buffer = pg_sys::ReadBuffer(index, block);
    pg_sys::LockBuffer(buffer, pg_sys::BUFFER_LOCK_SHARE as i32);
    let words = pg_sys::BLCKSZ as usize / std::mem::size_of::<u64>();
    let mut copy = vec![0u64; words].into_boxed_slice();
    std::ptr::copy_nonoverlapping(
        pg_sys::BufferGetPage(buffer) as *const u8,
        copy.as_mut_ptr() as *mut u8,
        pg_sys::BLCKSZ as usize,
    );
    pg_sys::UnlockReleaseBuffer(buffer);
    Some(PageCopy(copy))
}

/// A row's address as one number: its block, then its place on the block.
pub(crate) fn address(block: pg_sys::BlockNumber, place: pg_sys::OffsetNumber) -> u64 {
    ((block as u64) << 16) | place as u64
}

/// The block a tuple's address names.
pub(crate) unsafe fn block_of(tuple: pg_sys::IndexTuple) -> pg_sys::BlockNumber {
    let block = (*tuple).t_tid.ip_blkid;
    ((block.bi_hi as u32) << 16) | block.bi_lo as u32
}

/// A condition of a relation on an index's first column against a constant: the operator's
/// strategy in the column's operator family, the constant, and the condition.
pub(crate) struct OnFirstColumn {
    pub strategy: i32,
    pub value: *mut pg_sys::Const,
    pub clause: *mut pg_sys::RestrictInfo,
}

/// Each condition of the base relation `rel` that compares the first column of `index` with a
/// constant by an operator of the column's operator family, written either way round, other than
/// those in `counted`.
pub(crate) unsafe fn on_first_column(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Vec<OnFirstColumn> {
    let mut found = Vec::new();
    for ri in cells((*rel).baserestrictinfo) {
        let ri = ri as *mut pg_sys::RestrictInfo;
        if counted.contains(&ri) {
            continue;
        }
        let clause = (*ri).clause as *mut pg_sys::Node;
        if (*ri).pseudoconstant || clause.is_null() || (*clause).type_ != pg_sys::NodeTag::T_OpExpr
        {
            continue;
        }
        let op = clause as *mut pg_sys::OpExpr;
        let args = cells((*op).args);
        if args.len() != 2 {
            continue;
        }
        let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
        let (other, opno) = if column_of(rel, index, l) == Some(0) {
            (r, (*op).opno)
        } else if column_of(rel, index, r) == Some(0) {
            (l, pg_sys::get_commutator((*op).opno))
        } else {
            continue;
        };
        if opno == pg_sys::InvalidOid {
            continue;
        }
        let Some((strategy, _)) = strategy(opno, *(*index).opfamily) else {
            continue;
        };
        let Some(value) = constant(root, other) else {
            continue;
        };
        if (*value).constisnull {
            continue;
        }
        found.push(OnFirstColumn {
            strategy,
            value,
            clause: ri,
        });
    }
    found
}

/// The name of support function `number` of the first column of `index`, where it has one.
pub(crate) unsafe fn support_name(index: *mut pg_sys::IndexOptInfo, number: i16) -> Option<String> {
    let input = *(*index).opcintype;
    let proc_ = pg_sys::get_opfamily_proc(*(*index).opfamily, input, input, number);
    if proc_ == pg_sys::InvalidOid {
        return None;
    }
    let name = pg_sys::get_func_name(proc_);
    (!name.is_null()).then(|| {
        std::ffi::CStr::from_ptr(name)
            .to_string_lossy()
            .into_owned()
    })
}
