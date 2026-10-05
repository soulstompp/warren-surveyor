// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a B-tree measures of a stretch of its key: the rows between two ends, read from a few of
//! its pages above the leaves, and from five of the stretch's own leaves where it spans whole
//! ones.
//!
//! Each downlink of a page above the leaves stands for one page of the level below and everything
//! under it. The read descends from the root, following the downlinks whose pages can hold an
//! entry between the ends, and stops at the coarsest level where at least ten downlinks lie
//! between them, or at the level just above the leaves. Each page is read as the B-tree reads it:
//! locked to share, copied, and let go, and draws on the statement's planning-read budget
//! (`budget`): with no page left, the read stops as it stops at its own most. A page VACUUM deletes
//! between the read of its parent and the read of it is passed over, as the B-tree's own scans pass
//! over it, or the read is given up and the planner keeps its own estimate; no item of a page is
//! read unless it lies whole on it.
//!
//! Leaves fill by bytes, so the rows a leaf holds differ from leaf to leaf with the widths of its
//! entries. Where at least two downlinks lie between the ends, the stretch's own leaves are read:
//! the first, which holds the lower end, the last, which holds the upper end, and those at the
//! quarter, the middle and the three-quarter of the stretch. The rows are the stretch's entries on
//! the first and on the last, and the rows the three between them hold on average for each leaf
//! between the ends: as many leaves as the downlinks stand for. Where reading them would pass the
//! pages the read may read, the rows are those downlinks times the rows under one page of the
//! level below: the table's rows over that level's pages. The leaves the stretch lies on are
//! given with its rows, for the price of walking them.
//!
//! The leaves read stand for the stretch only where every column the index stores, in its key and
//! its INCLUDE, is of one width, and the three read between the ends hold the same rows: the
//! leaves of a block built in one pass then hold the same rows. Where a column is of more than
//! one width, each leaf's rows follow its entries' lengths, which run in stretches along the key;
//! and where the three hold different rows, the index has split since it was built. Either way
//! the read says so, and the stretch can be read whole: from the leaf holding its lower end along
//! the leaves to the one holding its upper end, every entry between them counted.
//!
//! Where the ends lie inside one leaf, or in two side by side, those one or two leaves are read and
//! the entries between the ends counted: the rows are read from the pages themselves. Where reading
//! them would pass the pages the read may read, the index only brackets the stretch: it holds from
//! none to those pages' rows.
//!
//! Each entry between the ends on a leaf the read reads can be handed on, with where its leaf lies
//! in the stretch, so that the conditions another index holds can be tested on the entries
//! (`carried`).
//!
//! Three more reads go the same way down: the places of a column inside a block of the key, each
//! named where a page of the leaves begins, the first and last values of the key's leading column,
//! and the share of the steps from one leaf to the next in key order that go to the next block.

use pgrx::pg_sys;
use std::cmp::Ordering;

const BTREE_METAPAGE: pg_sys::BlockNumber = 0;
const BTREE_MAGIC: u32 = 0x053162;
const BTREE_NOVAC_VERSION: u32 = 3;
const BTREE_VERSION: u32 = 4;
const P_NONE: pg_sys::BlockNumber = 0;
const P_HIKEY: pg_sys::OffsetNumber = 1;
const BTP_LEAF: u16 = 1 << 0;
const BTP_DELETED: u16 = 1 << 2;
const BTP_HALF_DEAD: u16 = 1 << 4;
const INDEX_ALT_TID_MASK: u16 = 0x2000;
const INDEX_NULL_MASK: u16 = 0x8000;
const INDEX_SIZE_MASK: u16 = 0x1FFF;
const BT_OFFSET_MASK: u16 = 0x0FFF;
const BT_PIVOT_HEAP_TID_ATTR: u16 = 0x1000;
const BT_IS_POSTING: u16 = 0x2000;
const BTORDER_PROC: u16 = 1;

/// The downlinks between the ends at which the read stops above the level just over the leaves.
pub(crate) const DOWNLINKS: u64 = 10;

/// The leaves a read of the key's first or last value passes over when they hold no entry.
const EMPTY_LEAVES: u32 = 8;

/// The special space of a B-tree page.
#[repr(C)]
struct Opaque {
    prev: pg_sys::BlockNumber,
    next: pg_sys::BlockNumber,
    level: u32,
    flags: u16,
    cycle: u16,
}

/// A B-tree's metapage.
#[repr(C)]
struct Meta {
    magic: u32,
    version: u32,
    root: pg_sys::BlockNumber,
    level: u32,
    fastroot: pg_sys::BlockNumber,
    fastlevel: u32,
    deleted_pages: u32,
    cleanup_heap_tuples: f64,
    all_equal_image: bool,
}

/// One part of an end: a value with its type, or NULL.
pub(crate) type Part = Option<(pg_sys::Datum, pg_sys::Oid)>;

/// One end of a stretch of the key: a part for each of its leading columns, and whether the end
/// lies after the entries equal to them or before them.
#[derive(Clone)]
pub(crate) struct End {
    pub parts: Vec<Part>,
    pub after: bool,
}

/// What a read measured: the rows between the ends, the level it stopped at, and the pages it
/// read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measured {
    pub rows: f64,
    pub level: u32,
    pub pages: u32,
    /// The pages of the level below the one stopped at, which the rows stand on.
    pub below: f64,
    /// The downlinks between the ends at the level stopped at, or the entries counted where the
    /// root is a leaf.
    pub between: u64,
    /// Whether the rows are the entries counted on every leaf the stretch lies on.
    pub counted: bool,
    /// The leaves the stretch lies on: those the downlinks between its ends stand for, and the
    /// one holding its lower end; the root where it is a leaf; none in an empty index.
    pub leaves: f64,
    /// Whether the stretch spans whole leaves whose rows the leaves read cannot stand for: the
    /// index stores a column of more than one width, or the leaves read between the ends hold
    /// different rows. Its leaves are then counted by reading them all (`every_leaf`).
    pub uneven: bool,
}

impl Measured {
    /// Whether the index only brackets the stretch: at most one downlink lies between its ends, so
    /// it holds from none to the rows of the one or two pages of the level below that hold them.
    pub(crate) fn bracketed(&self) -> bool {
        self.level >= 1 && self.between <= 1
    }

    /// The pages a read of every leaf of the stretch is predicted to read: the pages this read
    /// read, and the leaves.
    pub(crate) fn whole_pages(&self) -> f64 {
        self.pages as f64 + self.leaves
    }

    /// Whether the index only brackets the stretch, its one or two leaves not read: it holds from
    /// none to the rows of the pages of the level below that hold its ends.
    pub(crate) fn only_bracketed(&self) -> bool {
        self.bracketed() && !self.counted
    }

    /// The rows of the stretch, for a table of `tuples` rows: the rows the read measures; and
    /// where the index only brackets it, the planner's own share of the table, from `planner`,
    /// held between none and those pages' rows. None where the planner gives no share.
    pub(crate) fn rows_within(
        &self,
        tuples: f64,
        planner: impl FnOnce() -> Option<f64>,
    ) -> Option<f64> {
        if !self.only_bracketed() {
            return Some(self.rows);
        }
        let pages = if self.below > 0.0 {
            ((self.between + 1) as f64 * tuples / self.below).min(tuples)
        } else {
            tuples
        };
        Some((planner()? * tuples).clamp(0.0, pages))
    }
}

/// Where a leaf the read reads lies in its stretch: at an end of a stretch read at a few of its
/// leaves, holding the lower end or the upper; between the ends of such a stretch, among the leaves
/// that stand for the rest; or anywhere in a stretch whose every leaf is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leaf {
    End,
    Between,
    Every,
}

/// What takes each entry between the ends on a leaf the read reads: where the leaf lies, the
/// entry, and the rows it stands for.
pub(crate) type Each<'a> = dyn FnMut(Leaf, pg_sys::IndexTuple, u64) + 'a;

/// Whether a type's values lie one apart, so that the values between two of them are counted:
/// the integer types and the date.
pub(crate) fn countable(kind: pg_sys::Oid) -> bool {
    kind == pg_sys::INT2OID
        || kind == pg_sys::INT4OID
        || kind == pg_sys::INT8OID
        || kind == pg_sys::DATEOID
}

/// A whole number as a value of the countable type `kind`.
pub(crate) fn datum_of(n: i64, kind: pg_sys::Oid) -> pg_sys::Datum {
    if kind == pg_sys::INT2OID {
        pg_sys::Datum::from(n as i16)
    } else if kind == pg_sys::INT4OID || kind == pg_sys::DATEOID {
        pg_sys::Datum::from(n as i32)
    } else {
        pg_sys::Datum::from(n)
    }
}

/// Whether a type is stored as one count of microseconds: the timestamp, with or without a time
/// zone.
pub(crate) fn instant(kind: pg_sys::Oid) -> bool {
    kind == pg_sys::TIMESTAMPOID || kind == pg_sys::TIMESTAMPTZOID
}

/// A value of a countable type, or of a timestamp, as a whole number.
pub(crate) fn whole(datum: pg_sys::Datum, kind: pg_sys::Oid) -> Option<i64> {
    let raw = datum.value();
    if kind == pg_sys::INT2OID {
        Some(raw as i16 as i64)
    } else if kind == pg_sys::INT4OID || kind == pg_sys::DATEOID {
        Some(raw as i32 as i64)
    } else if kind == pg_sys::INT8OID || instant(kind) {
        Some(raw as i64)
    } else {
        None
    }
}

/// A copy of one page, whose header holds together as a B-tree page's: its item pointers, its
/// items and its special space lie inside it, in that order.
struct Page(Box<[u64]>);

/// Where a page's item pointers begin.
const HEADER: usize = std::mem::offset_of!(pg_sys::PageHeaderData, pd_linp);

impl Page {
    fn ptr(&self) -> pg_sys::Page {
        self.0.as_ptr() as pg_sys::Page
    }

    fn header(&self) -> &pg_sys::PageHeaderData {
        unsafe { &*(self.ptr() as *const pg_sys::PageHeaderData) }
    }

    /// Whether the header holds together: item pointers from its end to `pd_lower`, items from
    /// `pd_upper`, and a B-tree's special space from `pd_special` to the page's end. A page never
    /// written, or one of another kind, does not.
    fn holds_together(&self) -> bool {
        let h = self.header();
        let (lower, upper, special) = (
            h.pd_lower as usize,
            h.pd_upper as usize,
            h.pd_special as usize,
        );
        let opaque =
            std::mem::size_of::<Opaque>().next_multiple_of(pg_sys::MAXIMUM_ALIGNOF as usize);
        HEADER <= lower
            && lower <= upper
            && upper <= special
            && special == pg_sys::BLCKSZ as usize - opaque
    }

    unsafe fn opaque(&self) -> &Opaque {
        &*(self.ptr() as *const u8)
            .add(self.header().pd_special as usize)
            .cast::<Opaque>()
    }

    /// Whether VACUUM has deleted the page, or is deleting it. It holds no entry, and a deleted
    /// page holds the transaction that deleted it where its item pointers began, so its items are
    /// never read: a read that reaches it moves right past it, as the B-tree's own scans do, or
    /// gives up.
    unsafe fn ignored(&self) -> bool {
        self.opaque().flags & (BTP_DELETED | BTP_HALF_DEAD) != 0
    }

    unsafe fn rightmost(&self) -> bool {
        self.opaque().next == P_NONE
    }

    /// The offset of the first item past the high key.
    unsafe fn first_data(&self) -> pg_sys::OffsetNumber {
        if self.rightmost() {
            P_HIKEY
        } else {
            P_HIKEY + 1
        }
    }

    unsafe fn last(&self) -> pg_sys::OffsetNumber {
        ((self.header().pd_lower as usize).saturating_sub(HEADER)
            / std::mem::size_of::<pg_sys::ItemIdData>()) as pg_sys::OffsetNumber
    }

    /// The item at `offset`: None on a page VACUUM has deleted, past the page's last item, where
    /// a scan has marked the entry dead (it found every row the entry names gone, and VACUUM has
    /// yet to remove it: it is no row), or where the item does not lie whole among the page's
    /// items.
    unsafe fn item(&self, offset: pg_sys::OffsetNumber) -> Option<pg_sys::IndexTuple> {
        if self.ignored() || offset < P_HIKEY || offset > self.last() {
            return None;
        }
        let id = &*(self.ptr() as *const u8)
            .add(HEADER + (offset as usize - 1) * std::mem::size_of::<pg_sys::ItemIdData>())
            .cast::<pg_sys::ItemIdData>();
        let (at, len) = (id.lp_off() as usize, id.lp_len() as usize);
        let h = self.header();
        if id.lp_flags() != pg_sys::LP_NORMAL
            || at < h.pd_upper as usize
            || at + len > h.pd_special as usize
            || at % pg_sys::MAXIMUM_ALIGNOF as usize != 0
            || len < std::mem::size_of::<pg_sys::IndexTupleData>()
        {
            return None;
        }
        let tuple = (self.ptr() as *const u8).add(at) as pg_sys::IndexTuple;
        (((*tuple).t_info & INDEX_SIZE_MASK) as usize <= len).then_some(tuple)
    }

    /// The downlinks this page holds, where it lies above the leaves.
    unsafe fn downlinks(&self) -> u64 {
        (self.last() as u64 + 1).saturating_sub(self.first_data() as u64)
    }

    /// The block the downlink at `offset` points down to, where it is one of the page's downlinks.
    unsafe fn child(&self, offset: pg_sys::OffsetNumber) -> Option<pg_sys::BlockNumber> {
        if offset < self.first_data() {
            return None;
        }
        self.item(offset).map(|t| downlink(t))
    }

    /// The item after `offset` in key order: the next item, the high key past the last one, or
    /// none on the rightmost page. None where that item cannot be read.
    unsafe fn next_of(&self, offset: pg_sys::OffsetNumber) -> Option<Option<pg_sys::IndexTuple>> {
        if offset < self.last() {
            self.item(offset + 1).map(Some)
        } else if self.rightmost() {
            Some(None)
        } else {
            self.item(P_HIKEY).map(Some)
        }
    }

    /// Whether the position `x` lies past this page: at or after its high key. False on the
    /// rightmost page. None where the high key cannot be read.
    unsafe fn past(&self, order: &Order, x: &End) -> Option<bool> {
        if self.rightmost() {
            return Some(false);
        }
        Some(order.place(self.item(P_HIKEY)?, x) != Ordering::Greater)
    }
}

/// A copy of one index tuple: a bound of a page.
#[derive(Clone)]
struct Bound(Box<[u64]>);

impl Bound {
    unsafe fn of(tuple: pg_sys::IndexTuple) -> Bound {
        let size = ((*tuple).t_info & INDEX_SIZE_MASK) as usize;
        let mut copy = vec![0u64; size.div_ceil(8)].into_boxed_slice();
        std::ptr::copy_nonoverlapping(tuple as *const u8, copy.as_mut_ptr() as *mut u8, size);
        Bound(copy)
    }

    fn tuple(&self) -> pg_sys::IndexTuple {
        self.0.as_ptr() as pg_sys::IndexTuple
    }
}

/// A page read on the way down, with the bound at or above which its entries lie: none on the
/// leftmost page of its level.
struct Read {
    page: Page,
    lower: Option<Bound>,
}

/// Whether block `block` lies inside `index`: below the blocks this session last saw it hold, for
/// a B-tree never shrinks while it is open, or below those it holds now.
unsafe fn inside(index: pg_sys::Relation, block: pg_sys::BlockNumber) -> bool {
    let fork = pg_sys::ForkNumber::MAIN_FORKNUM;
    let seen = (*pg_sys::RelationGetSmgr(index)).smgr_cached_nblocks[fork as usize];
    (seen != pg_sys::InvalidBlockNumber && block < seen)
        || block < pg_sys::RelationGetNumberOfBlocksInFork(index, fork)
}

/// Reads block `block` of `index`, locked to share, into a copy. None where the block lies past
/// the index's end, the statement's planning-read budget has no page left, or the page's header
/// does not hold together.
unsafe fn read(index: pg_sys::Relation, block: pg_sys::BlockNumber) -> Option<Page> {
    if !inside(index, block) || !crate::budget::draw((*index).rd_id) {
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
    let page = Page(copy);
    page.holds_together().then_some(page)
}

/// Reads block `block` of `index`, a page of level `level` that VACUUM has not deleted. None where
/// it lies past the index's end, is not such a page, or does not hold together.
unsafe fn read_at(index: pg_sys::Relation, block: pg_sys::BlockNumber, level: u32) -> Option<Page> {
    let page = read(index, block)?;
    (!page.ignored() && page.opaque().level == level).then_some(page)
}

/// The metapage of `index`, where it is a B-tree of version 4 or later.
unsafe fn metapage(index: pg_sys::Relation) -> Option<(Page, Meta)> {
    if (*(*index).rd_rel).relam != pg_sys::BTREE_AM_OID {
        return None;
    }
    let page = read(index, BTREE_METAPAGE)?;
    let meta = std::ptr::read(pg_sys::PageGetContents(page.ptr()) as *const Meta);
    (meta.magic == BTREE_MAGIC && meta.version >= BTREE_VERSION).then_some((page, meta))
}

fn offset_bits(tuple: &pg_sys::IndexTupleData) -> u16 {
    tuple.t_tid.ip_posid
}

/// The page a pivot points down to.
unsafe fn downlink(tuple: pg_sys::IndexTuple) -> pg_sys::BlockNumber {
    let block = (*tuple).t_tid.ip_blkid;
    ((block.bi_hi as u32) << 16) | block.bi_lo as u32
}

/// Whether an index tuple is a pivot: a high key or a downlink.
unsafe fn is_pivot(tuple: pg_sys::IndexTuple) -> bool {
    (*tuple).t_info & INDEX_ALT_TID_MASK != 0 && offset_bits(&*tuple) & BT_IS_POSTING == 0
}

/// The key columns a tuple holds: fewer than the key's on a pivot whose last columns were cut.
unsafe fn columns_held(tuple: pg_sys::IndexTuple, keys: usize) -> usize {
    if is_pivot(tuple) {
        (offset_bits(&*tuple) & BT_OFFSET_MASK) as usize
    } else {
        keys
    }
}

/// Whether a tuple carries a heap address after its key: every entry, and a pivot whose address
/// was not cut.
unsafe fn holds_heap_address(tuple: pg_sys::IndexTuple) -> bool {
    !is_pivot(tuple) || offset_bits(&*tuple) & BT_PIVOT_HEAP_TID_ATTR != 0
}

/// The rows an entry stands for: one, or each address of a posting list.
unsafe fn entries(tuple: pg_sys::IndexTuple) -> u64 {
    if (*tuple).t_info & INDEX_ALT_TID_MASK != 0 && offset_bits(&*tuple) & BT_IS_POSTING != 0 {
        (offset_bits(&*tuple) & BT_OFFSET_MASK) as u64
    } else {
        1
    }
}

/// Column `column` (from 1) of a tuple, or None where it is NULL.
pub(crate) unsafe fn value(
    tuple: pg_sys::IndexTuple,
    column: usize,
    desc: pg_sys::TupleDesc,
) -> Option<pg_sys::Datum> {
    if (*tuple).t_info & INDEX_NULL_MASK != 0 {
        let bits = (tuple as *const u8).add(std::mem::size_of::<pg_sys::IndexTupleData>());
        let at = column - 1;
        if *bits.add(at >> 3) & (1 << (at & 7)) == 0 {
            return None;
        }
    }
    Some(pg_sys::nocache_index_getattr(tuple, column as i32, desc))
}

/// How a B-tree compares one of its key columns: its operator family's comparison against each
/// type an end gives it, under its collation, in its direction, with its NULLs at its end.
struct Column {
    family: pg_sys::Oid,
    input: pg_sys::Oid,
    compares: Vec<(pg_sys::Oid, *mut pg_sys::FmgrInfo)>,
    collation: pg_sys::Oid,
    descending: bool,
    nulls_first: bool,
}

/// The order of the B-tree `index`: its key columns' comparisons, and its tuples' shape.
pub(crate) struct Order {
    columns: Vec<Column>,
    desc: pg_sys::TupleDesc,
}

impl Order {
    pub(crate) unsafe fn of(index: pg_sys::Relation) -> Order {
        let keys = (*(*index).rd_index).indnkeyatts as usize;
        let columns = (0..keys)
            .map(|i| {
                let option = *(*index).rd_indoption.add(i) as u32;
                let input = *(*index).rd_opcintype.add(i);
                Column {
                    family: *(*index).rd_opfamily.add(i),
                    input,
                    compares: vec![(
                        input,
                        pg_sys::index_getprocinfo(index, (i + 1) as i16, BTORDER_PROC),
                    )],
                    collation: *(*index).rd_indcollation.add(i),
                    descending: option & pg_sys::INDOPTION_DESC != 0,
                    nulls_first: option & pg_sys::INDOPTION_NULLS_FIRST != 0,
                }
            })
            .collect();
        Order {
            columns,
            desc: (*index).rd_att,
        }
    }

    /// Whether every column the index stores, in its key and its INCLUDE, is of one width.
    unsafe fn fixed_width(&self) -> bool {
        (0..(*self.desc).natts).all(|i| (*pg_sys::TupleDescAttr(self.desc, i)).attlen > 0)
    }

    /// Finds each column's comparison against the types `end` gives it: false where the operator
    /// family has none.
    pub(crate) unsafe fn prepare(&mut self, end: &End) -> bool {
        for (i, part) in end.parts.iter().enumerate().take(self.columns.len()) {
            let Some((_, kind)) = *part else { continue };
            let c = &mut self.columns[i];
            if c.compares.iter().any(|(k, _)| *k == kind) {
                continue;
            }
            let proc_ = pg_sys::get_opfamily_proc(c.family, c.input, kind, BTORDER_PROC as i16);
            if proc_ == pg_sys::InvalidOid {
                return false;
            }
            let info =
                pg_sys::palloc0(std::mem::size_of::<pg_sys::FmgrInfo>()) as *mut pg_sys::FmgrInfo;
            pg_sys::fmgr_info(proc_, info);
            c.compares.push((kind, info));
        }
        true
    }

    /// One column of a tuple against one part of an end.
    unsafe fn column(&self, i: usize, held: Option<pg_sys::Datum>, part: Part) -> Ordering {
        let c = &self.columns[i];
        match (held, part) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => {
                if c.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (Some(_), None) => {
                if c.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Some(a), Some((b, kind))) => {
                let compare = c
                    .compares
                    .iter()
                    .find(|(k, _)| *k == kind)
                    .map(|(_, f)| *f)
                    .expect("each end is prepared before it is compared");
                let r = pg_sys::FunctionCall2Coll(compare, c.collation, a, b).value() as i32;
                let r = if c.descending {
                    -(r.signum())
                } else {
                    r.signum()
                };
                r.cmp(&0)
            }
        }
    }

    /// Where a tuple lies against an end. A column cut from a pivot reads as below every value, and
    /// a tuple holding a heap address lies inside the entries equal to its key.
    unsafe fn place(&self, tuple: pg_sys::IndexTuple, end: &End) -> Ordering {
        let keys = self.columns.len();
        let held = columns_held(tuple, keys);
        let compared = end.parts.len().min(keys);
        for i in 0..compared {
            if i >= held {
                return Ordering::Less;
            }
            let o = self.column(i, value(tuple, i + 1, self.desc), end.parts[i]);
            if o != Ordering::Equal {
                return o;
            }
        }
        // the tuple lies among the entries the end's parts name, or at their start
        let inside = held > compared || (held == keys && holds_heap_address(tuple));
        match (inside, end.after) {
            (_, true) => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, false) => Ordering::Equal,
        }
    }

    unsafe fn between(&self, tuple: pg_sys::IndexTuple, lower: &End, upper: &End) -> bool {
        self.place(tuple, lower) == Ordering::Greater
            && self.place(tuple, upper) != Ordering::Greater
    }

    /// The rows a leaf holds, and those of them between `lower` and `upper`, each entry between them
    /// handed to `each` with where the leaf lies.
    unsafe fn rows_on(
        &self,
        leaf: &Page,
        lower: &End,
        upper: &End,
        lies: Leaf,
        each: &mut Each<'_>,
    ) -> (u64, u64) {
        let (mut all, mut inside) = (0u64, 0u64);
        for offset in leaf.first_data()..=leaf.last() {
            // an entry marked dead is no row
            let Some(tuple) = leaf.item(offset) else {
                continue;
            };
            let n = entries(tuple);
            all += n;
            if self.between(tuple, lower, upper) {
                inside += n;
                each(lies, tuple, n);
            }
        }
        (all, inside)
    }

    /// The pages of the level below `pages` that can hold an entry between `lower` and `upper`, in
    /// the key's order, each with the bound at or above which its entries lie. None where a page's
    /// downlinks cannot be read.
    unsafe fn children(
        &self,
        pages: &[Read],
        lower: &End,
        upper: &End,
    ) -> Option<Vec<(pg_sys::BlockNumber, Option<Bound>)>> {
        let mut children = Vec::new();
        for read in pages {
            let page = &read.page;
            let (first, last) = (page.first_data(), page.last());
            // a child holds the entries from its downlink's key to the next key: the children
            // whose next key lies past the lower end, and whose own key lies before the upper
            let past_lower = self.first_after(page, first + 1, last, lower)?;
            let start = if past_lower <= last {
                past_lower - 1
            } else if !page.past(self, lower)? {
                last
            } else {
                continue;
            };
            let end = self.first_where(page, first + 1, last, |o| o != Ordering::Less, upper)? - 1;
            for offset in start..=end {
                let child = page.child(offset)?;
                let bound = if offset == first {
                    read.lower.clone()
                } else {
                    Some(Bound::of(page.item(offset)?))
                };
                children.push((child, bound));
            }
        }
        Some(children)
    }

    /// The leaf reached from page `block` of level `level`: toward the position `x`, moving right
    /// past a page whose high key lies at or before it, or, with no position, by the middle
    /// downlink of each page. A page VACUUM has deleted since its parent was read is passed over
    /// toward a position, since the page right of it now holds its part of the key, and gives the
    /// read up with none. The downlinks and the pages read above the leaves, other than a level's
    /// rightmost, are added to `above`. None where the read, which has read `read_so_far` pages,
    /// would read more than `most`, or a page is not of the level its parent names.
    #[allow(clippy::too_many_arguments)]
    unsafe fn leaf_from(
        &self,
        index: pg_sys::Relation,
        mut block: pg_sys::BlockNumber,
        mut level: u32,
        x: Option<&End>,
        most: u32,
        read_so_far: &mut u32,
        above: &mut (u64, u64),
    ) -> Option<Page> {
        loop {
            room(*read_so_far, most)?;
            let mut page = read(index, block)?;
            *read_so_far += 1;
            while page.ignored() || x.map_or(Some(false), |x| page.past(self, x))? {
                if x.is_none() || page.rightmost() {
                    return None;
                }
                room(*read_so_far, most)?;
                page = read(index, page.opaque().next)?;
                *read_so_far += 1;
            }
            if page.opaque().level != level {
                return None;
            }
            if level == 0 {
                return Some(page);
            }
            if !page.rightmost() {
                above.0 += page.downlinks();
                above.1 += 1;
            }
            let (first, last) = (page.first_data(), page.last());
            let at = match x {
                Some(x) => self.first_after(&page, first + 1, last, x)? - 1,
                None => first + last.saturating_sub(first) / 2,
            };
            block = page.child(at)?;
            level -= 1;
        }
    }

    /// The entries between `lower` and `upper` on every leaf the stretch lies on, and those leaves:
    /// from page `block` of level `level` down to the leaf holding the lower end, then along the
    /// leaves to the one holding the upper end, passing over any VACUUM has deleted, each entry
    /// handed to `each`. None where the read, which has read `read_so_far` pages, would read more
    /// than `most`, or a page along them is not a leaf.
    #[allow(clippy::too_many_arguments)]
    unsafe fn count_from(
        &self,
        index: pg_sys::Relation,
        block: pg_sys::BlockNumber,
        level: u32,
        lower: &End,
        upper: &End,
        most: u32,
        read_so_far: &mut u32,
        each: &mut Each<'_>,
    ) -> Option<(u64, u64)> {
        let mut above = (0u64, 0u64);
        let mut leaf = self.leaf_from(
            index,
            block,
            level,
            Some(lower),
            most,
            read_so_far,
            &mut above,
        )?;
        let (mut rows, mut leaves) = (0u64, 0u64);
        loop {
            if !leaf.ignored() {
                rows += self.rows_on(&leaf, lower, upper, Leaf::Every, each).1;
                leaves += 1;
                if leaf.rightmost() || self.place(leaf.item(P_HIKEY)?, upper) != Ordering::Less {
                    return Some((rows, leaves));
                }
            } else if leaf.rightmost() {
                return Some((rows, leaves));
            }
            room(*read_so_far, most)?;
            leaf = read(index, leaf.opaque().next)?;
            *read_so_far += 1;
            if leaf.opaque().level != 0 {
                return None;
            }
        }
    }

    /// The stretch's own leaves under the pages `pages` of level `level`: the stretch's rows on the
    /// first leaf, which holds `lower`; the rows each of the leaves at the quarter, the middle and
    /// the three-quarter of the stretch holds; and the stretch's rows on the last leaf, which holds
    /// `upper`. The leaves between the ends are reached through the children wholly inside the
    /// stretch. Once all five are read, their entries between the ends are handed to `each`. None
    /// where the pages hold fewer than three children for it, or the read, which has read
    /// `read_so_far` pages, would read more than `most` or than the statement's planning-read
    /// budget has left. The downlinks and the pages read above the leaves on the way down, other
    /// than a level's rightmost, are added to `above`.
    #[allow(clippy::too_many_arguments)]
    unsafe fn sample(
        &self,
        index: pg_sys::Relation,
        pages: &[Read],
        level: u32,
        lower: &End,
        upper: &End,
        most: u32,
        read_so_far: &mut u32,
        above: &mut (u64, u64),
        each: &mut Each<'_>,
    ) -> Option<(u64, Vec<u64>, u64)> {
        let children = self.children(pages, lower, upper)?;
        let n = children.len();
        if n < 3 || level == 0 {
            return None;
        }
        let mut between: Vec<usize> = [n / 4, n / 2, 3 * n / 4]
            .iter()
            .map(|&at| at.clamp(1, n - 2))
            .collect();
        between.dedup();
        let descents = (2 + between.len() as u32) * level;
        if read_so_far.saturating_add(descents) > most
            || !crate::budget::fits((*index).rd_id, descents as f64)
        {
            return None;
        }
        let below = level - 1;
        let (first, last) = (children[0].0, children[n - 1].0);
        let first = self.leaf_from(index, first, below, Some(lower), most, read_so_far, above)?;
        let mut leaves = Vec::with_capacity(between.len());
        for &at in &between {
            leaves.push(self.leaf_from(
                index,
                children[at].0,
                below,
                None,
                most,
                read_so_far,
                above,
            )?);
        }
        let last = self.leaf_from(index, last, below, Some(upper), most, read_so_far, above)?;
        let first = self.rows_on(&first, lower, upper, Leaf::End, each).1;
        let held = leaves
            .iter()
            .map(|leaf| self.rows_on(leaf, lower, upper, Leaf::Between, each).0)
            .collect();
        let last = self.rows_on(&last, lower, upper, Leaf::End, each).1;
        Some((first, held, last))
    }

    /// The first offset from `lo` to `hi` of `page`, whose items lie in the key's order, at which
    /// `is_past` holds of the item, or `hi + 1` where it holds of none: a binary search, as the
    /// B-tree's own search reads a page. None where an item it reads cannot be read.
    unsafe fn first_where(
        &self,
        page: &Page,
        lo: pg_sys::OffsetNumber,
        hi: pg_sys::OffsetNumber,
        is_past: impl Fn(Ordering) -> bool,
        x: &End,
    ) -> Option<pg_sys::OffsetNumber> {
        let (mut a, mut b) = (lo, hi + 1);
        while a < b {
            let mid = a + (b - a) / 2;
            if is_past(self.place(page.item(mid)?, x)) {
                b = mid;
            } else {
                a = mid + 1;
            }
        }
        Some(a)
    }

    /// The first offset from `lo` to `hi` whose item lies after `x`, or `hi + 1`.
    unsafe fn first_after(
        &self,
        page: &Page,
        lo: pg_sys::OffsetNumber,
        hi: pg_sys::OffsetNumber,
        x: &End,
    ) -> Option<pg_sys::OffsetNumber> {
        self.first_where(page, lo, hi, |o| o == Ordering::Greater, x)
    }

    /// Whether a tuple holds column `column`, with the values `end` gives every column before it.
    unsafe fn same_block(&self, tuple: pg_sys::IndexTuple, end: &End, column: usize) -> bool {
        columns_held(tuple, self.columns.len()) > column
            && (0..column).all(|i| {
                self.column(i, value(tuple, i + 1, self.desc), end.parts[i]) == Ordering::Equal
            })
    }

    /// The page of the level below `pages` that holds the position `x`: its lower bound, none on
    /// the leftmost page, its upper bound, none on the rightmost, and its block. None where no page
    /// holds it, or a page's downlinks cannot be read.
    unsafe fn child_holding(
        &self,
        pages: &[Read],
        x: &End,
    ) -> Option<(Option<Bound>, Option<Bound>, pg_sys::BlockNumber)> {
        for read in pages {
            let page = &read.page;
            let (first, last) = (page.first_data(), page.last());
            // the last downlink at or before the position
            let offset = self.first_after(page, first + 1, last, x)? - 1;
            if offset == first
                && read
                    .lower
                    .as_ref()
                    .is_some_and(|b| self.place(b.tuple(), x) == Ordering::Greater)
            {
                return None;
            }
            let next = page.next_of(offset)?;
            if next.is_some_and(|n| self.place(n, x) != Ordering::Greater) {
                // the position lies on a page further right
                continue;
            }
            let child = page.child(offset)?;
            let lower = if offset == first {
                read.lower.clone()
            } else {
                Some(Bound::of(page.item(offset)?))
            };
            return Some((lower, next.map(|n| Bound::of(n)), child));
        }
        None
    }
}

/// The rows of the B-tree `index` between `lower` and `upper`, for a table of `rows` rows. None
/// where the index is not one this read knows: an index of another kind, or a B-tree of a version
/// before 4.
pub(crate) unsafe fn rows(
    index: pg_sys::Relation,
    lower: &End,
    upper: &End,
    rows: f64,
) -> Option<Measured> {
    rows_at(index, lower, upper, rows, true, u32::MAX)
}

/// Whether a read that has read `read` pages may read one more, reading at most `most`.
fn room(read: u32, most: u32) -> Option<()> {
    (read < most).then_some(())
}

/// As `rows`, and where the ends lie inside one leaf or two and `count` asks for it, those leaves
/// read and the entries between the ends counted; none where the read would read more than `most`
/// pages, every page it reads counted, before it has a measure.
pub(crate) unsafe fn rows_at(
    index: pg_sys::Relation,
    lower: &End,
    upper: &End,
    rows: f64,
    count: bool,
    most: u32,
) -> Option<Measured> {
    on_leaves(index, lower, upper, rows, count, most, &mut |_, _, _| {})
}

/// As `rows_at`, each entry between the ends on every leaf the read reads handed to `each`: the
/// leaves it counts, and the first, the last and the three between them that it reads of a stretch
/// spanning whole leaves.
pub(crate) unsafe fn on_leaves(
    index: pg_sys::Relation,
    lower: &End,
    upper: &End,
    rows: f64,
    count: bool,
    most: u32,
    each: &mut Each<'_>,
) -> Option<Measured> {
    let mut order = Order::of(index);
    if !order.prepare(lower) || !order.prepare(upper) {
        return None;
    }
    room(0, most)?;
    let (_metapage, meta) = metapage(index)?;
    let mut pages_read = 1;
    if meta.fastroot == P_NONE {
        return Some(Measured {
            rows: 0.0,
            level: 0,
            pages: pages_read,
            below: 0.0,
            between: 0,
            counted: true,
            leaves: 0.0,
            uneven: false,
        });
    }
    let top = meta.fastlevel;
    room(pages_read, most)?;
    let root = read_at(index, meta.fastroot, top)?;
    pages_read += 1;
    let root_downlinks = root.downlinks();
    let mut level_pages = vec![Read {
        page: root,
        lower: None,
    }];
    let mut level = top;
    // the downlinks and pages read below the root, for the pages of each level
    let (mut below_root_downlinks, mut below_root_pages) = (0u64, 0u64);
    loop {
        if level == 0 {
            // the root is a leaf: its entries are counted
            let mut counted = 0u64;
            for read in &level_pages {
                let page = &read.page;
                for offset in page.first_data()..=page.last() {
                    // an entry marked dead is no row
                    let Some(tuple) = page.item(offset) else {
                        continue;
                    };
                    if order.between(tuple, lower, upper) {
                        counted += entries(tuple);
                        each(Leaf::Every, tuple, entries(tuple));
                    }
                }
            }
            return Some(Measured {
                rows: counted as f64,
                level: 0,
                pages: pages_read,
                below: 0.0,
                between: counted,
                counted: true,
                leaves: 1.0,
                uneven: false,
            });
        }
        let mut between = 0u64;
        for read in &level_pages {
            let page = &read.page;
            if !page.rightmost() && order.between(page.item(P_HIKEY)?, lower, upper) {
                between += 1;
            }
            // the downlinks after the first, past the lower end and not past the upper
            let (lo, hi) = (page.first_data() + 1, page.last());
            let past_lower = order.first_after(page, lo, hi, lower)?;
            let past_upper = order.first_after(page, lo, hi, upper)?;
            between += past_upper.saturating_sub(past_lower) as u64;
        }
        if between >= DOWNLINKS || level == 1 {
            let below = pages_below(
                index,
                &meta,
                top,
                level,
                root_downlinks,
                below_root_downlinks,
                below_root_pages,
            );
            // a stretch inside one leaf or two: those leaves read, its entries counted
            let mut counted = None;
            if count && between <= 1 && level == 1 {
                if let Some((_, _, block)) = order.child_holding(&level_pages, lower) {
                    counted = order.count_from(
                        index,
                        block,
                        0,
                        lower,
                        upper,
                        most,
                        &mut pages_read,
                        each,
                    );
                }
            }
            if let Some((entries, on)) = counted {
                return Some(Measured {
                    rows: entries as f64,
                    level,
                    pages: pages_read,
                    below,
                    between,
                    counted: true,
                    leaves: on as f64,
                    uneven: false,
                });
            }
            let average = if below > 0.0 {
                between as f64 * rows / below
            } else {
                0.0
            };
            // the stretch's own leaves, where it spans whole leaves and the read has room for a
            // descent to each of them
            let mut read_below = (0u64, 0u64);
            let sampled = if between >= 2 && below > 0.0 {
                order.sample(
                    index,
                    &level_pages,
                    level,
                    lower,
                    upper,
                    most,
                    &mut pages_read,
                    &mut read_below,
                    each,
                )
            } else {
                None
            };
            // the leaves the downlinks between the ends stand for
            let spanned = if level == 1 || below <= 0.0 {
                between as f64
            } else {
                between as f64
                    * pages_below(
                        index,
                        &meta,
                        top,
                        1,
                        root_downlinks,
                        below_root_downlinks + read_below.0,
                        below_root_pages + read_below.1,
                    )
                    / below
            };
            let spanned = if spanned.is_finite() && spanned >= 0.0 {
                spanned
            } else {
                between as f64
            };
            // the leaves read stand for the stretch only where every column is of one width and
            // they hold the same rows
            let fixed = order.fixed_width();
            let uneven = match &sampled {
                Some((_, held, _)) => !fixed || held.windows(2).any(|w| w[0] != w[1]),
                None => between >= 2 && !fixed,
            };
            let rows = match sampled {
                Some((first, held, last)) if spanned > 0.0 => {
                    let mean = held.iter().sum::<u64>() as f64 / held.len().max(1) as f64;
                    first as f64 + last as f64 + mean * (spanned - 1.0).max(0.0)
                }
                _ => average,
            };
            return Some(Measured {
                rows,
                level,
                pages: pages_read,
                below,
                between,
                counted: false,
                leaves: spanned + 1.0,
                uneven,
            });
        }
        // a child VACUUM has deleted since its parent was read gives the read up
        let children = order.children(&level_pages, lower, upper)?;
        let mut next_level = Vec::with_capacity(children.len());
        for (block, bound) in children {
            room(pages_read, most)?;
            let page = read_at(index, block, level - 1)?;
            pages_read += 1;
            if !page.rightmost() {
                below_root_downlinks += page.downlinks();
                below_root_pages += 1;
            }
            next_level.push(Read { page, lower: bound });
        }
        if below_root_pages == 0 {
            // every page read is its level's rightmost, which holds what was left over: the
            // fill is read from the page to its left, where there is one
            if let Some(first) = next_level.first() {
                let left = first.page.opaque().prev;
                if left != P_NONE {
                    room(pages_read, most)?;
                    let page = read_at(index, left, level - 1)?;
                    pages_read += 1;
                    below_root_downlinks += page.downlinks();
                    below_root_pages += 1;
                } else {
                    for read in &next_level {
                        below_root_downlinks += read.page.downlinks();
                        below_root_pages += 1;
                    }
                }
            }
        }
        level_pages = next_level;
        level -= 1;
    }
}

/// The rows of the B-tree `index` between `lower` and `upper` counted on every leaf the stretch
/// lies on, from the leaf holding the lower end along the leaves to the one holding the upper end,
/// each entry between them handed to `each`, and those leaves; and the pages read. None where the
/// read would read more than `most` pages.
pub(crate) unsafe fn every_leaf(
    index: pg_sys::Relation,
    lower: &End,
    upper: &End,
    most: u32,
    each: &mut Each<'_>,
) -> (Option<(f64, f64)>, u32) {
    let mut order = Order::of(index);
    if !order.prepare(lower) || !order.prepare(upper) || room(0, most).is_none() {
        return (None, 0);
    }
    let Some((_metapage, meta)) = metapage(index) else {
        return (None, 0);
    };
    let mut pages = 1;
    if meta.fastroot == P_NONE {
        return (Some((0.0, 0.0)), pages);
    }
    let counted = order.count_from(
        index,
        meta.fastroot,
        meta.fastlevel,
        lower,
        upper,
        most,
        &mut pages,
        each,
    );
    (
        counted.map(|(rows, leaves)| (rows as f64, leaves as f64)),
        pages,
    )
}

/// The pages of the level below `level`, which no page stores. The root's downlinks give the level
/// under it exactly. The levels further down share out the index's other pages, each holding as
/// many times the pages of the level above as the pages read below the root hold downlinks on
/// average.
unsafe fn pages_below(
    index: pg_sys::Relation,
    meta: &Meta,
    top: u32,
    level: u32,
    root_downlinks: u64,
    downlinks_read: u64,
    pages_read: u64,
) -> f64 {
    if level == top {
        return root_downlinks as f64;
    }
    let fanout = downlinks_read as f64 / pages_read.max(1) as f64;
    let deleted = if meta.version >= BTREE_NOVAC_VERSION {
        meta.deleted_pages as f64
    } else {
        0.0
    };
    let all =
        pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM) as f64;
    // the metapage, the pages above the fast root, the root and the level under it
    let rest =
        all - 1.0 - (meta.level - meta.fastlevel) as f64 - 1.0 - root_downlinks as f64 - deleted;
    // levels 0 to top - 2, each fanout times the pages of the level above
    let depth = top as i32 - 2;
    let shares: f64 = (0..=depth).map(|j| fanout.powi(j)).sum();
    let under_root = rest / shares;
    under_root * fanout.powi(depth - (level as i32 - 1))
}

/// Descends `index` from its root to the page of level `stop` that holds the position `x`, moving
/// right past a page whose high key lies at or before it, and past one VACUUM has deleted since its
/// parent was read, whose part of the key the page right of it now holds; reading at most `most`
/// pages. The page, and the pages read. None where a page is not of the level its parent names.
unsafe fn descend(
    index: pg_sys::Relation,
    meta: &Meta,
    order: &Order,
    x: &End,
    stop: u32,
    most: u32,
) -> Option<(Read, u32)> {
    if meta.fastroot == P_NONE || meta.fastlevel < stop {
        return None;
    }
    room(0, most)?;
    let mut pages = 1;
    let mut level = meta.fastlevel;
    let mut current = Read {
        page: read(index, meta.fastroot)?,
        lower: None,
    };
    loop {
        while current.page.ignored() || current.page.past(order, x)? {
            if current.page.rightmost() {
                return None;
            }
            room(pages, most)?;
            // the page right of a deleted one now begins where the deleted one began
            let high = if current.page.ignored() {
                current.lower.clone()
            } else {
                Some(Bound::of(current.page.item(P_HIKEY)?))
            };
            current = Read {
                page: read(index, current.page.opaque().next)?,
                lower: high,
            };
            pages += 1;
        }
        if current.page.opaque().level != level {
            return None;
        }
        if level == stop {
            return Some((current, pages));
        }
        let page = &current.page;
        let first = page.first_data();
        // the last downlink at or before the position
        let chosen = order.first_after(page, first + 1, page.last(), x)? - 1;
        let child = page.child(chosen)?;
        let lower = if chosen == first {
            current.lower.clone()
        } else {
            Some(Bound::of(page.item(chosen)?))
        };
        room(pages, most)?;
        current = Read {
            page: read(index, child)?,
            lower,
        };
        pages += 1;
        level -= 1;
    }
}

/// The places of column `column` of the B-tree `index` inside the block `prefix` names on the
/// columns before it, each a value of the column at which a page of the leaves begins inside the
/// block, in the key's order, NULL among them: none past `most` places. The next place is looked
/// for on the page above the leaves already read while that page holds the position, and by a
/// descent from the root once it does not. None where the read does not know the index, or would
/// read more than `most_pages` pages. The places, and the pages read.
pub(crate) unsafe fn steps(
    index: pg_sys::Relation,
    prefix: &[Part],
    column: usize,
    most: usize,
    most_pages: u32,
) -> Option<(Option<Vec<Part>>, u32)> {
    let mut order = Order::of(index);
    let start = End {
        parts: prefix.to_vec(),
        after: false,
    };
    if column >= order.columns.len() || !order.prepare(&start) {
        return None;
    }
    let kind = order.columns[column].input;
    let attr = pg_sys::TupleDescAttr(order.desc, column as i32);
    let (byval, len) = ((*attr).attbyval, (*attr).attlen as i32);
    let keep =
        |v: Option<pg_sys::Datum>| -> Part { v.map(|d| (pg_sys::datumCopy(d, byval, len), kind)) };
    room(0, most_pages)?;
    let (_metapage, meta) = metapage(index)?;
    let mut pages = 1;
    let mut found: Vec<Part> = Vec::new();
    if meta.fastroot == P_NONE {
        return Some((Some(found), pages));
    }
    if meta.fastlevel == 0 {
        // the root is a leaf: its entries name every place
        room(pages, most_pages)?;
        let root = read_at(index, meta.fastroot, 0)?;
        pages += 1;
        for offset in root.first_data()..=root.last() {
            // an entry marked dead is no row
            let Some(t) = root.item(offset) else {
                continue;
            };
            if !order.same_block(t, &start, column) {
                continue;
            }
            let v = value(t, column + 1, order.desc);
            let seen = found
                .last()
                .is_some_and(|last| order.column(column, v, *last) == Ordering::Equal);
            if !seen {
                found.push(keep(v));
                if found.len() > most {
                    return Some((None, pages));
                }
            }
        }
        return Some((Some(found), pages));
    }
    let mut from = start.clone();
    let mut at: Option<Read> = None;
    loop {
        let holds = match &at {
            Some(r) => !r.page.past(&order, &from)?,
            None => false,
        };
        if !holds {
            let (page, read_now) = descend(
                index,
                &meta,
                &order,
                &from,
                1,
                most_pages.saturating_sub(pages),
            )?;
            pages += read_now;
            at = Some(page);
        }
        let page = &at.as_ref().expect("a page holds the position").page;
        let mut next = None;
        let after = order.first_after(page, page.first_data() + 1, page.last(), &from)?;
        if after <= page.last() {
            next = Some(page.item(after)?);
        } else if !page.rightmost() {
            let high = page.item(P_HIKEY)?;
            if order.place(high, &from) == Ordering::Greater {
                next = Some(high);
            }
        }
        let Some(q) = next else { break };
        if !order.same_block(q, &start, column) {
            break;
        }
        let v = keep(value(q, column + 1, order.desc));
        found.push(v);
        if found.len() > most {
            return Some((None, pages));
        }
        let mut parts = prefix.to_vec();
        parts.push(v);
        from = End { parts, after: true };
        if !order.prepare(&from) {
            return None;
        }
    }
    Some((Some(found), pages))
}

/// The share of the steps from one leaf of the B-tree `index` to the next in key order that go to
/// the next block, read from the downlinks of one page just above the leaves, reached from the root
/// by the middle downlink of each page, and the pages read. None where the index is not one this
/// read knows, its root is a leaf, or a page on the way down is not of the level its parent names
/// or VACUUM has deleted it since.
pub(crate) unsafe fn leaf_order(index: pg_sys::Relation) -> Option<(f64, u32)> {
    let (_metapage, meta) = metapage(index)?;
    if meta.fastroot == P_NONE || meta.fastlevel == 0 {
        return None;
    }
    let mut page = read_at(index, meta.fastroot, meta.fastlevel)?;
    let mut pages = 2;
    for level in (1..meta.fastlevel).rev() {
        let (first, last) = (page.first_data(), page.last());
        let middle = first + (last.saturating_sub(first)) / 2;
        page = read_at(index, page.child(middle)?, level)?;
        pages += 1;
    }
    let leaves = (page.first_data()..=page.last())
        .map(|offset| page.child(offset))
        .collect::<Option<Vec<pg_sys::BlockNumber>>>()?;
    let steps = leaves.len().saturating_sub(1);
    if steps == 0 {
        return Some((1.0, pages));
    }
    let next = leaves.windows(2).filter(|w| w[1] == w[0] + 1).count();
    Some((next as f64 / steps as f64, pages))
}

/// The first and last values other than NULL of the leading column of the B-tree `index`, where
/// its type counts its values, read from the entries at the two ends of the key: the smaller and
/// the larger, and the pages read. None where the read would read more than `most` pages.
pub(crate) unsafe fn ends(index: pg_sys::Relation, most: u32) -> Option<(i64, i64, u32)> {
    let order = Order::of(index);
    let c = order.columns.first()?;
    if !countable(c.input) {
        return None;
    }
    room(0, most)?;
    let (_metapage, meta) = metapage(index)?;
    // the NULL block lies at one end of the key, and the values between the two positions
    let (start, stop) = if c.nulls_first {
        (
            End {
                parts: vec![None],
                after: true,
            },
            End {
                parts: Vec::new(),
                after: true,
            },
        )
    } else {
        (
            End {
                parts: Vec::new(),
                after: false,
            },
            End {
                parts: vec![None],
                after: false,
            },
        )
    };
    let (a, p1) = entry_next_to(index, &meta, &order, &start, true, most - 1)?;
    let (b, p2) = entry_next_to(index, &meta, &order, &stop, false, most - 1 - p1)?;
    Some((a.min(b), a.max(b), 1 + p1 + p2))
}

/// The leading value, as a whole number, of the first entry after the position `x` (`forward`), or
/// of the last entry before it, passing over a few leaves that hold none. None where it is NULL or
/// not found, or where the read would read more than `most` pages.
unsafe fn entry_next_to(
    index: pg_sys::Relation,
    meta: &Meta,
    order: &Order,
    x: &End,
    forward: bool,
    most: u32,
) -> Option<(i64, u32)> {
    let (at, mut pages) = descend(index, meta, order, x, 0, most)?;
    let mut page = at.page;
    for _ in 0..EMPTY_LEAVES {
        if page.opaque().flags & BTP_LEAF == 0 {
            return None;
        }
        let (first, last) = (page.first_data(), page.last());
        let mut offsets: Vec<pg_sys::OffsetNumber> = (first..=last).collect();
        if !forward {
            offsets.reverse();
        }
        let wanted = if forward {
            Ordering::Greater
        } else {
            Ordering::Less
        };
        // a leaf VACUUM has deleted holds none, and an entry marked dead is no row
        for offset in offsets {
            let Some(t) = page.item(offset) else {
                continue;
            };
            if order.place(t, x) == wanted {
                let kind = order.columns[0].input;
                return value(t, 1, order.desc)
                    .and_then(|v| whole(v, kind))
                    .map(|v| (v, pages));
            }
        }
        let sibling = if forward {
            page.opaque().next
        } else {
            page.opaque().prev
        };
        if sibling == P_NONE {
            return None;
        }
        room(pages, most)?;
        page = read(index, sibling)?;
        pages += 1;
    }
    None
}
#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::{rows, End, Meta, Order, Page, Read, BTP_DELETED, BTP_HALF_DEAD, P_NONE};
    use pgrx::prelude::*;
    use pgrx::IntoDatum;
    use std::collections::HashSet;
    use std::ffi::c_void;

    /// Block `block` of `index`, a page that holds together.
    unsafe fn read(index: pg_sys::Relation, block: pg_sys::BlockNumber) -> Page {
        super::read(index, block).expect("a B-tree page")
    }

    /// `rows` keys of 384 bytes of text that does not compress, under the C collation, with
    /// `columns` before the key: a handful to a page above the leaves, so the B-tree stands
    /// several levels high.
    fn wide(table: &str, columns: &str, rows: u32) {
        Spi::run(&format!(
            "CREATE TABLE {table} AS SELECT {columns} \
                 (SELECT string_agg(md5(g::text || '.' || i), '') FROM generate_series(1, 12) i) \
                 COLLATE \"C\" AS k, g AS v \
             FROM generate_series(1, {rows}) g; \
             ANALYZE {table}"
        ))
        .unwrap();
    }

    fn open(index: &str) -> pg_sys::Relation {
        let oid = Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{index}'::regclass::oid"))
            .unwrap()
            .unwrap();
        unsafe { pg_sys::index_open(oid, pg_sys::AccessShareLock as pg_sys::LOCKMODE) }
    }

    fn reltuples(table: &str) -> f64 {
        Spi::get_one::<f64>(&format!(
            "SELECT reltuples::float8 FROM pg_class WHERE oid = '{table}'::regclass"
        ))
        .unwrap()
        .unwrap()
    }

    fn count(sql: &str) -> f64 {
        Spi::get_one::<i64>(sql).unwrap().unwrap() as f64
    }

    /// The pages of each level of `index`, counted along each level from its leftmost page.
    unsafe fn census(index: pg_sys::Relation) -> Vec<f64> {
        let metapage = read(index, 0);
        let meta = &*(pg_sys::PageGetContents(metapage.ptr()) as *const Meta);
        let mut counts = vec![0.0; meta.fastlevel as usize + 1];
        let mut leftmost = meta.fastroot;
        for level in (0..=meta.fastlevel as usize).rev() {
            let mut block = leftmost;
            let mut below = None;
            while block != P_NONE {
                let page = read(index, block);
                counts[level] += 1.0;
                if below.is_none() && level > 0 {
                    below = page.child(page.first_data());
                }
                block = page.opaque().next;
            }
            if let Some(b) = below {
                leftmost = b;
            }
        }
        counts
    }

    fn text(s: &str) -> super::Part {
        s.to_string().into_datum().map(|d| (d, pg_sys::TEXTOID))
    }

    fn int(n: i32) -> super::Part {
        n.into_datum().map(|d| (d, pg_sys::INT4OID))
    }

    /// Each stretch measured on `index` against its count: within one page of the level below
    /// the one stopped at, at each end, and the share of the count its fourth field allows for
    /// entries wider or narrower than the table's average; that level's pages within 3% of the
    /// census; a few pages read, the descent and five of the stretch's leaves. The levels
    /// stopped at.
    fn holds(index: &str, table: &str, stretches: Vec<(End, End, String, f64)>) -> Vec<u32> {
        let rel = open(index);
        let all = reltuples(table);
        let levels = unsafe { census(rel) };
        let mut stopped = Vec::new();
        for (lower, upper, truth, width) in stretches {
            let m = unsafe { rows(rel, &lower, &upper, all) }.expect("a B-tree is measured");
            let counted = count(&truth);
            assert!(m.level >= 1, "{truth}: {m:?}");
            let census_below = levels[m.level as usize - 1];
            let under = all / census_below;
            assert!(
                (m.rows - counted).abs() <= 2.0 * under + width * counted,
                "{truth}: {} rows measured, {counted} counted, {under} under one downlink, {m:?}",
                m.rows
            );
            assert!(
                (m.below - census_below).abs() <= 0.03 * census_below,
                "{truth}: {} pages below estimated, {census_below} counted, {levels:?}",
                m.below
            );
            if m.level >= 2 {
                // at least ten downlinks between the ends, each end within one of the truth
                let share = 2.0 / (super::DOWNLINKS - 1) as f64 + width;
                assert!(
                    (m.rows - counted).abs() <= share * counted,
                    "{truth}: {} rows measured, {counted} counted, {m:?}",
                    m.rows
                );
            }
            // the descent, then one to each of five leaves
            assert!(m.pages <= 12 + 5 * m.level, "{truth}: {m:?}");
            stopped.push(m.level);
        }
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
        stopped
    }

    fn end(parts: Vec<super::Part>, after: bool) -> End {
        End { parts, after }
    }

    #[pg_test]
    fn a_stretch_of_one_column_is_measured_within_a_page_of_the_level_it_stops_at() {
        wide("wide", "", 30000);
        Spi::run("CREATE INDEX wide_k ON wide (k)").unwrap();
        let mut stretches = Vec::new();
        for (lo, hi) in [
            ("0", "g"),
            ("0", "8"),
            ("3", "4"),
            ("3a", "3b"),
            ("3a5", "3a6"),
        ] {
            stretches.push((
                end(vec![text(lo)], false),
                end(vec![text(hi)], false),
                format!("SELECT count(*) FROM wide WHERE k >= '{lo}' AND k < '{hi}'"),
                0.0,
            ));
        }
        stretches.push((
            end(vec![text("7")], false),
            end(vec![], true),
            "SELECT count(*) FROM wide WHERE k >= '7'".to_string(),
            0.0,
        ));
        let levels = holds("wide_k", "wide", stretches);
        assert!(levels.iter().any(|&l| l >= 2), "{levels:?}");
        assert!(levels.contains(&1), "{levels:?}");
    }

    #[pg_test]
    fn a_stretch_under_a_fixed_prefix_and_the_null_block_are_measured_alike() {
        wide(
            "stepped",
            "CASE WHEN g % 50 = 0 THEN NULL ELSE g % 4 + 1 END AS a,",
            30000,
        );
        Spi::run("CREATE INDEX stepped_ak ON stepped (a, k)").unwrap();
        let stretches = vec![
            (
                end(vec![int(2)], false),
                end(vec![int(2)], true),
                "SELECT count(*) FROM stepped WHERE a = 2".to_string(),
                0.0,
            ),
            (
                end(vec![int(2), text("3")], false),
                end(vec![int(2), text("9")], false),
                "SELECT count(*) FROM stepped WHERE a = 2 AND k >= '3' AND k < '9'".to_string(),
                0.0,
            ),
            (
                end(vec![int(3), text("c")], false),
                end(vec![int(3)], true),
                "SELECT count(*) FROM stepped WHERE a = 3 AND k >= 'c'".to_string(),
                0.0,
            ),
            (
                end(vec![int(4), text("3a")], false),
                end(vec![int(4), text("3b")], false),
                "SELECT count(*) FROM stepped WHERE a = 4 AND k >= '3a' AND k < '3b'".to_string(),
                0.0,
            ),
            (
                end(vec![None], false),
                end(vec![None], true),
                "SELECT count(*) FROM stepped WHERE a IS NULL".to_string(),
                // a NULL entry carries a bitmap of its NULLs, 8 bytes more here, so fewer fit a page
                0.10,
            ),
            (
                // the last rows of the index: every page read is its level's rightmost
                end(vec![None, text("f")], false),
                end(vec![None], true),
                "SELECT count(*) FROM stepped WHERE a IS NULL AND k >= 'f'".to_string(),
                0.10,
            ),
        ];
        let levels = holds("stepped_ak", "stepped", stretches);
        assert!(levels.iter().any(|&l| l >= 2), "{levels:?}");
    }

    #[pg_test]
    fn a_stretch_whose_entries_are_wider_than_the_rest_is_counted_from_its_own_leaves() {
        // four blocks of 6,000 rows on keys of 384 bytes, the second's entries carrying a note of
        // 640 bytes more, so that its leaves hold fewer than half the entries of any other's
        Spi::run(
            "CREATE TABLE layered AS SELECT g % 4 + 1 AS a, \
                 (SELECT string_agg(md5(g::text || '.' || i), '') FROM generate_series(1, 12) i) \
                 COLLATE \"C\" AS k, \
                 CASE WHEN g % 4 = 1 \
                      THEN (SELECT string_agg(md5(g::text || ':' || i), '') FROM generate_series(1, 20) i) \
                      ELSE '' END AS note \
             FROM generate_series(1, 24000) g; \
             CREATE INDEX layered_ak ON layered (a, k) INCLUDE (note); \
             ANALYZE layered",
        )
        .unwrap();
        let rel = open("layered_ak");
        let all = reltuples("layered");
        let mut levels = Vec::new();
        for (a, lo, hi) in [
            (2, None, None),
            (1, None, None),
            (4, None, None),
            (2, Some("3"), Some("c")),
            (3, Some("5"), Some("6")),
        ] {
            let lower = end(std::iter::once(int(a)).chain(lo.map(text)).collect(), false);
            let upper = match hi {
                Some(h) => end(vec![int(a), text(h)], false),
                None => end(vec![int(a)], true),
            };
            let truth = format!(
                "SELECT count(*) FROM layered WHERE a = {a}{}{}",
                lo.map_or(String::new(), |l| format!(" AND k >= '{l}'")),
                hi.map_or(String::new(), |h| format!(" AND k < '{h}'"))
            );
            let m = unsafe { rows(rel, &lower, &upper, all) }.expect("a B-tree is measured");
            let counted = count(&truth);
            assert!(m.between >= 2, "{truth}: {m:?}");
            assert!(
                (m.rows - counted).abs() <= 0.05 * counted,
                "{truth}: {} rows measured, {counted} counted, {m:?}",
                m.rows
            );
            // the descent, then one to each of five leaves
            assert!(m.pages <= 12 + 5 * m.level, "{truth}: {m:?}");
            levels.push(m.level);
        }
        assert!(levels.iter().any(|&l| l >= 2), "{levels:?}");
        assert!(levels.contains(&1), "{levels:?}");
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    #[pg_test]
    fn a_stretch_whose_leaves_hold_unequal_rows_along_it_is_counted_at_their_mean_between_its_ends()
    {
        // three blocks of 12,000 rows keyed by the block and a number, each carrying a note: on the
        // second block's first and last quarters of rows a note twice as wide as on the rest, so
        // that a third of its leaves at each end hold half the rows of the third in the middle
        Spi::run(
            "CREATE TABLE striped AS SELECT a, n, \
                 (SELECT string_agg(md5(a || '.' || n || '.' || i), '') \
                  FROM generate_series(1, CASE WHEN a = 2 AND (n < 3000 OR n >= 9000) \
                                               THEN 12 ELSE 6 END) i) AS note \
             FROM generate_series(1, 3) a, generate_series(0, 11999) n; \
             CREATE INDEX striped_an ON striped (a, n) INCLUDE (note); \
             ANALYZE striped; \
             CREATE EXTENSION IF NOT EXISTS pageinspect",
        )
        .unwrap();
        let rel = open("striped_an");
        let all = reltuples("striped");
        let m = unsafe {
            rows(
                rel,
                &end(vec![int(2)], false),
                &end(vec![int(2)], true),
                all,
            )
        }
        .expect("a B-tree is measured");
        assert_eq!(m.level, 1, "{m:?}");
        assert!(
            (m.rows - 12000.0).abs() <= 0.03 * 12000.0,
            "{} rows measured, 12000 counted, {m:?}",
            m.rows
        );
        // the leaves the block lies on
        let leaves = count(
            "SELECT count(DISTINCT s.blkno) \
             FROM bt_multi_page_stats('striped_an', 1, -1) s, \
                  LATERAL bt_page_items('striped_an', s.blkno::int) i \
             WHERE s.type = 'l' AND i.data LIKE '02 00 00 00%' \
               AND NOT (s.btpo_next <> 0 AND i.itemoffset = 1)",
        );
        assert!(
            (m.leaves - leaves).abs() <= 1.0,
            "{} leaves measured, {leaves} counted, {m:?}",
            m.leaves
        );
        // the index stores text, so the leaves read do not stand for the block, and every leaf of
        // it counts its rows exactly
        assert!(m.uneven, "{m:?}");
        let (counted, pages) = unsafe {
            super::every_leaf(
                rel,
                &end(vec![int(2)], false),
                &end(vec![int(2)], true),
                u32::MAX,
                &mut |_, _, _| {},
            )
        };
        assert_eq!(counted, Some((12000.0, leaves)), "{pages} pages");
        // and no more pages than the leaves and the descent
        let (none, read) = unsafe {
            super::every_leaf(
                rel,
                &end(vec![int(2)], false),
                &end(vec![int(2)], true),
                leaves as u32,
                &mut |_, _, _| {},
            )
        };
        assert!(none.is_none() && read <= leaves as u32, "{read} pages read");
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    #[pg_test]
    fn a_b_tree_whose_root_is_a_leaf_is_counted_exactly() {
        Spi::run(
            "CREATE TABLE tiny AS SELECT g AS a FROM generate_series(1, 100) g; \
             CREATE INDEX tiny_a ON tiny (a); ANALYZE tiny",
        )
        .unwrap();
        let rel = open("tiny_a");
        let m = unsafe {
            rows(
                rel,
                &end(vec![int(10)], false),
                &end(vec![int(20)], false),
                100.0,
            )
        }
        .unwrap();
        assert_eq!((m.rows, m.level), (10.0, 0), "{m:?}");
        let m = unsafe {
            rows(
                rel,
                &end(vec![int(10)], false),
                &end(vec![int(20)], true),
                100.0,
            )
        }
        .unwrap();
        assert_eq!(m.rows, 11.0, "{m:?}");
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    fn small(n: i16) -> super::Part {
        n.into_datum().map(|d| (d, pg_sys::INT2OID))
    }

    /// The places `steps` names as whole numbers, NULL as None.
    fn named(places: &[super::Part]) -> Vec<Option<i64>> {
        places
            .iter()
            .map(|p| p.and_then(|(d, k)| super::whole(d, k)))
            .collect()
    }

    #[pg_test]
    fn the_places_of_a_column_are_stepped_from_the_pages_above_the_leaves() {
        wide(
            "laned",
            "CASE WHEN g % 50 = 0 THEN NULL ELSE (g % 5)::smallint END AS lane, (g % 4)::smallint AS b,",
            30000,
        );
        Spi::run("CREATE INDEX laned_lane_k ON laned (lane, k); CREATE INDEX laned_lane_b_k ON laned (lane, b, k)")
            .unwrap();
        let rel = open("laned_lane_k");
        let (places, pages) =
            unsafe { super::steps(rel, &[], 0, 200, u32::MAX) }.expect("a B-tree is stepped");
        let places = places.expect("no more places than asked");
        assert_eq!(
            named(&places),
            vec![Some(0), Some(1), Some(2), Some(3), Some(4), None],
            "{pages} pages"
        );
        // a descent a place, and one past the last
        let height = unsafe {
            let metapage = read(rel, 0);
            (*(pg_sys::PageGetContents(metapage.ptr()) as *const Meta)).fastlevel as usize
        };
        assert!(height >= 2, "{height}");
        assert!(
            pages as usize <= 1 + (places.len() + 1) * height,
            "{pages} pages"
        );
        assert!(unsafe { super::steps(rel, &[], 0, 3, u32::MAX) }
            .unwrap()
            .0
            .is_none());
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
        let rel = open("laned_lane_b_k");
        let places = unsafe { super::steps(rel, &[small(1)], 1, 200, u32::MAX) }
            .unwrap()
            .0
            .unwrap();
        assert_eq!(named(&places), vec![Some(0), Some(1), Some(2), Some(3)]);
        // the NULL block's rows are every 50th, whose b is 0 or 2
        let places = unsafe { super::steps(rel, &[None], 1, 200, u32::MAX) }
            .unwrap()
            .0
            .unwrap();
        assert_eq!(named(&places), vec![Some(0), Some(2)]);
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    #[pg_test]
    fn values_near_a_page_in_size_are_counted_on_their_one_or_two_leaves() {
        // 150 rows a day, each its own entry, so a leaf holds two or three days, some in part
        Spi::run(
            "CREATE TABLE dense AS SELECT date '2000-01-01' + g / 150 AS d, g AS v \
             FROM generate_series(0, 89999) g; \
             CREATE INDEX dense_d ON dense (d) WITH (deduplicate_items = off); \
             ANALYZE dense",
        )
        .unwrap();
        let rel = open("dense_d");
        let all = reltuples("dense");
        let mut measured = 0.0;
        for offset in (1..60).map(|k| k * 9 + 2) {
            let d =
                Spi::get_one::<pgrx::datum::Date>(&format!("SELECT date '2000-01-01' + {offset}"))
                    .unwrap()
                    .unwrap()
                    .into_datum()
                    .map(|d| (d, pg_sys::DATEOID));
            let m = unsafe {
                super::rows_at(
                    rel,
                    &end(vec![d], false),
                    &end(vec![d], true),
                    all,
                    true,
                    u32::MAX,
                )
            }
            .unwrap();
            assert!(m.bracketed() && m.counted, "{m:?}");
            measured += m.rows_within(all, || None).expect("the leaves are counted");
        }
        let counted = 59.0 * 150.0;
        assert!(
            measured == counted,
            "{measured} measured, {counted} counted"
        );
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    #[pg_test]
    fn a_column_with_more_places_than_asked_is_left_within_a_few_pages() {
        // 33,000 values of six rows each, a page of the leaves beginning inside each of hundreds
        Spi::run(
            "CREATE TABLE crowded AS SELECT g / 6 AS k, g AS v FROM generate_series(1, 200000) g; \
             CREATE INDEX crowded_kv ON crowded (k, v); ANALYZE crowded",
        )
        .unwrap();
        let rel = open("crowded_kv");
        let (places, pages) = unsafe { super::steps(rel, &[], 0, 200, u32::MAX) }.unwrap();
        assert!(places.is_none());
        assert!(pages <= 8, "{pages} pages");
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    #[pg_test]
    fn the_first_and_last_values_of_a_countable_leading_column_are_read_at_the_two_ends() {
        Spi::run(
            "CREATE TABLE ended AS SELECT CASE WHEN g % 7 = 0 THEN NULL ELSE 16 + g END AS a, g AS v \
             FROM generate_series(1, 40000) g; \
             CREATE INDEX ended_a ON ended (a); \
             CREATE INDEX ended_a_first ON ended (a NULLS FIRST); \
             CREATE INDEX ended_a_desc ON ended (a DESC); \
             CREATE INDEX ended_a_desc_last ON ended (a DESC NULLS LAST); \
             ANALYZE ended",
        )
        .unwrap();
        let truth = Spi::get_two::<i32, i32>("SELECT min(a), max(a) FROM ended").unwrap();
        let truth = (truth.0.unwrap() as i64, truth.1.unwrap() as i64);
        for index in [
            "ended_a",
            "ended_a_first",
            "ended_a_desc",
            "ended_a_desc_last",
        ] {
            let rel = open(index);
            let (lo, hi, pages) =
                unsafe { super::ends(rel, u32::MAX) }.expect("a countable column's ends");
            assert_eq!((lo, hi), truth, "{index}");
            assert!(pages <= 12, "{index}: {pages} pages");
            unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
        }
    }

    #[pg_test]
    fn a_value_inside_one_leaf_or_two_is_counted_on_them_and_past_the_limit_only_bracketed() {
        // three rows a day, each its own entry
        Spi::run(
            "CREATE TABLE daily AS SELECT date '2000-01-01' + g / 3 AS d, g AS v \
             FROM generate_series(0, 89999) g; \
             CREATE INDEX daily_d ON daily (d) WITH (deduplicate_items = off); \
             ANALYZE daily",
        )
        .unwrap();
        let rel = open("daily_d");
        let all = reltuples("daily");
        let day = |offset: i32| {
            Spi::get_one::<pgrx::datum::Date>(&format!("SELECT date '2000-01-01' + {offset}"))
                .unwrap()
                .unwrap()
                .into_datum()
                .map(|d| (d, pg_sys::DATEOID))
        };
        // days through the key, the first page's first day and the last page's last among them
        let mut inside = 0;
        for offset in [0, 29999].into_iter().chain((1..20).map(|k| k * 1499 + 7)) {
            let d = day(offset);
            let m = unsafe {
                super::rows_at(
                    rel,
                    &end(vec![d], false),
                    &end(vec![d], true),
                    all,
                    true,
                    u32::MAX,
                )
            }
            .unwrap();
            assert!(m.bracketed() && m.counted, "{offset}: {m:?}");
            assert_eq!(m.rows_within(all, || None), Some(3.0), "{offset}: {m:?}");
            if m.leaves == 1.0 {
                inside += 1;
            }
        }
        assert!(inside >= 15, "{inside} days inside one leaf");
        // past the pages the read may read, the leaves are not reached: the day is only bracketed,
        // and takes the planner's share held to the pages that hold it
        let d = day(7502);
        let m =
            unsafe { super::rows_at(rel, &end(vec![d], false), &end(vec![d], true), all, true, 2) }
                .unwrap();
        assert!(m.only_bracketed(), "{m:?}");
        assert_eq!(m.rows_within(all, || Some(3.0 / all)), Some(3.0), "{m:?}");
        let pages = (m.between + 1) as f64 * all / m.below;
        assert_eq!(m.rows_within(all, || Some(0.5)), Some(pages), "{m:?}");
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    #[pg_test]
    fn a_stretch_its_leaves_counted_takes_their_count_and_one_only_bracketed_the_planners_share_held_to_them(
    ) {
        let near = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() <= 1e-9 * b);
        let m = |between: u64, rows: f64, counted: bool| super::Measured {
            rows,
            level: 1,
            pages: 3,
            below: 100.0,
            between,
            counted,
            leaves: between as f64 + 1.0,
            uneven: false,
        };
        let all = 10_000.0;
        // downlinks between the ends: their rows
        assert!(near(m(5, 500.0, false).rows_within(all, || None), 500.0));
        assert!(near(
            m(2, 200.0, false).rows_within(all, || Some(0.05)),
            200.0
        ));
        // inside one page or across two, counted on them: the count, whatever the planner's share
        assert!(near(m(0, 3.0, true).rows_within(all, || Some(0.05)), 3.0));
        assert!(near(m(1, 7.0, true).rows_within(all, || None), 7.0));
        // inside one page whose leaf was not read: the planner's share, held to the page
        assert!(near(
            m(0, 0.0, false).rows_within(all, || Some(0.003)),
            30.0
        ));
        assert!(near(
            m(0, 0.0, false).rows_within(all, || Some(0.05)),
            100.0
        ));
        assert_eq!(m(0, 0.0, false).rows_within(all, || None), None);
        // across two pages whose leaves were not read: the planner's share, held to the two pages
        assert!(near(
            m(1, 0.0, false).rows_within(all, || Some(0.003)),
            30.0
        ));
        assert!(near(
            m(1, 0.0, false).rows_within(all, || Some(0.05)),
            200.0
        ));
        // a root that is a leaf: its entries
        let leaf = super::Measured {
            rows: 7.0,
            level: 0,
            pages: 2,
            below: 0.0,
            between: 7,
            counted: true,
            leaves: 1.0,
            uneven: false,
        };
        assert!(near(leaf.rows_within(all, || Some(0.5)), 7.0));
    }

    #[pg_test]
    fn the_leaves_order_on_disk_is_read_from_one_page_above_them() {
        // the same keys built whole, and written row by row in scattered order with the key in place
        wide("built", "", 30000);
        Spi::run(
            "CREATE INDEX built_k ON built (k); \
             CREATE TABLE written (k text COLLATE \"C\", v int); \
             CREATE INDEX written_k ON written (k); \
             INSERT INTO written SELECT k, v FROM built ORDER BY md5(v::text)",
        )
        .unwrap();
        for (index, ordered) in [("built_k", true), ("written_k", false)] {
            let rel = open(index);
            let height = unsafe {
                let metapage = read(rel, 0);
                (*(pg_sys::PageGetContents(metapage.ptr()) as *const Meta)).fastlevel
            };
            assert!(height >= 2, "{index}: {height}");
            let (share, pages) = unsafe { super::leaf_order(rel) }.expect("a B-tree's order");
            // the metapage, and a page of each level down to the one above the leaves
            assert_eq!(pages, height + 1, "{index}");
            if ordered {
                assert!(share > 0.9, "{index}: {share}");
            } else {
                assert!(share < 0.2, "{index}: {share}");
            }
            unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
        }
    }

    /// The heap address of each row `sql` names, as one number: its block, then its place on the
    /// block.
    fn addresses(sql: &str) -> HashSet<u64> {
        Spi::connect(|client| {
            let mut out = HashSet::new();
            for row in client.select(sql, None, &[])? {
                out.insert(row.get::<i64>(1)?.expect("an address") as u64);
            }
            Ok::<_, pgrx::spi::SpiError>(out)
        })
        .unwrap()
    }

    /// Whether the row at `tid` is among the addresses `state` holds: what VACUUM asks of each
    /// entry it reads.
    unsafe extern "C-unwind" fn among(tid: pg_sys::ItemPointer, state: *mut c_void) -> bool {
        let gone = &*(state as *const HashSet<u64>);
        let block = ((*tid).ip_blkid.bi_hi as u64) << 16 | (*tid).ip_blkid.bi_lo as u64;
        gone.contains(&(block << 16 | (*tid).ip_posid as u64))
    }

    /// VACUUM's own pass over the B-tree `index` of `table`, removing the entries of the rows at
    /// the addresses `gone` holds: each leaf it empties is deleted, as a VACUUM running beside a
    /// read deletes it.
    fn vacuum_entries(table: &str, index: &str, gone: &HashSet<u64>) {
        let oid = |name: &str| {
            Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{name}'::regclass::oid"))
                .unwrap()
                .unwrap()
        };
        let tuples = reltuples(table);
        unsafe {
            let heap_lock = pg_sys::ShareUpdateExclusiveLock as pg_sys::LOCKMODE;
            let index_lock = pg_sys::RowExclusiveLock as pg_sys::LOCKMODE;
            let heap = pg_sys::table_open(oid(table), heap_lock);
            let rel = pg_sys::index_open(oid(index), index_lock);
            let mut info = pg_sys::IndexVacuumInfo {
                index: rel,
                heaprel: heap,
                analyze_only: false,
                report_progress: false,
                estimated_count: true,
                message_level: pg_sys::DEBUG2 as i32,
                num_heap_tuples: tuples,
                strategy: std::ptr::null_mut(),
            };
            pg_sys::index_bulk_delete(
                &mut info,
                std::ptr::null_mut(),
                Some(among),
                gone as *const HashSet<u64> as *mut c_void,
            );
            pg_sys::index_close(rel, index_lock);
            pg_sys::table_close(heap, heap_lock);
        }
    }

    #[pg_test]
    fn a_page_vacuum_deletes_after_a_read_named_it_is_passed_over_or_the_read_given_up() {
        Spi::run(
            "CREATE TABLE emptied AS SELECT g AS k, g AS v FROM generate_series(1, 100000) g; \
             CREATE INDEX emptied_k ON emptied (k) WITH (deduplicate_items = off); \
             ANALYZE emptied",
        )
        .unwrap();
        let rel = open("emptied_k");
        let blocks = unsafe {
            pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
        };
        // the page above the leaves, as a read that began before VACUUM holds it
        let above = unsafe {
            let metapage = read(rel, 0);
            let meta = &*(pg_sys::PageGetContents(metapage.ptr()) as *const Meta);
            read(rel, meta.fastroot)
        };
        assert_eq!(unsafe { above.opaque().level }, 1);
        // the rows from 20,001 to 60,000 deleted, and VACUUM's pass over the index
        let gone = addresses(
            "SELECT (((ctid::text::point)[0])::bigint << 16) | ((ctid::text::point)[1])::bigint \
             FROM emptied WHERE k BETWEEN 20001 AND 60000",
        );
        Spi::run("DELETE FROM emptied WHERE k BETWEEN 20001 AND 60000").unwrap();
        vacuum_entries("emptied", "emptied_k", &gone);
        let deleted: Vec<pg_sys::BlockNumber> = (1..blocks)
            .filter(|&b| unsafe { read(rel, b).opaque().flags } & BTP_DELETED != 0)
            .collect();
        assert!(deleted.len() >= 50, "{} pages deleted", deleted.len());
        // where a page's item pointers began, VACUUM wrote the transaction that deleted it
        for &b in &deleted {
            let lower =
                unsafe { (*(read(rel, b).ptr() as *const pg_sys::PageHeaderData)).pd_lower };
            assert_eq!(lower, 32, "block {b}");
        }
        let at = |k: i32, after: bool| end(vec![int(k)], after);
        let (lower, upper) = (at(30000, false), at(70000, true));
        let mut order = unsafe { Order::of(rel) };
        assert!(unsafe { order.prepare(&lower) && order.prepare(&upper) });
        let live = |p: &Page| unsafe { p.opaque().flags } & (BTP_DELETED | BTP_HALF_DEAD) == 0;
        for &b in &deleted {
            let (mut pages, mut above_leaves) = (0, (0, 0));
            // reached with no position, as a leaf between a stretch's ends is: given up
            let leaf = unsafe {
                order.leaf_from(rel, b, 0, None, u32::MAX, &mut pages, &mut above_leaves)
            };
            assert!(leaf.as_ref().is_none_or(live), "block {b}");
            // reached toward a position, as the leaf holding an end is: the live leaf right of it,
            // and the entries from there to the upper end those left
            let leaf = unsafe {
                order.leaf_from(
                    rel,
                    b,
                    0,
                    Some(&lower),
                    u32::MAX,
                    &mut pages,
                    &mut above_leaves,
                )
            };
            assert!(leaf.as_ref().is_some_and(live), "block {b}");
            let counted = unsafe {
                order.count_from(
                    rel,
                    b,
                    0,
                    &lower,
                    &upper,
                    u32::MAX,
                    &mut pages,
                    &mut |_, _, _| {},
                )
            };
            assert_eq!(counted.map(|(rows, _)| rows), Some(10000), "block {b}");
        }
        // the leaves of a stretch reached through the page read before VACUUM: given up, or only
        // live entries read
        let first = at(10000, false);
        assert!(unsafe { order.prepare(&first) });
        let mut handed = Vec::new();
        let sampled = unsafe {
            order.sample(
                rel,
                &[Read {
                    page: above,
                    lower: None,
                }],
                1,
                &first,
                &upper,
                u32::MAX,
                &mut 0,
                &mut (0, 0),
                &mut |_, tuple, _| {
                    handed.push(super::value(tuple, 1, (*rel).rd_att).map(|d| d.value() as i32))
                },
            )
        };
        assert!(
            sampled.is_none()
                || handed.iter().all(|k| k.is_some_and(|k| {
                    (10000..=70000).contains(&k) && !(20001..=60000).contains(&k)
                })),
            "{sampled:?}"
        );
        // a block past the index's end, as an item read from no page could name: not read
        let mut pages = 0;
        let past = unsafe {
            order.leaf_from(
                rel,
                blocks + 100,
                0,
                None,
                u32::MAX,
                &mut pages,
                &mut (0, 0),
            )
        };
        assert!(past.is_none());
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }
}

/// Tests that need a session of their own, outside a test's transaction.
#[cfg(test)]
mod sessions {
    use std::time::{Duration, Instant};

    /// A session on `name`, a database of its own on the test server, made afresh with the
    /// extension and pageinspect installed and the library loaded, and a session on the test's
    /// database beside it: what the first commits touches no test's transaction, and no test's
    /// snapshot keeps the rows it deletes in sight.
    fn own_database(name: &str) -> (postgres::Client, postgres::Client) {
        pgrx_tests::run_test(
            "a_b_tree_whose_root_is_a_leaf_is_counted_exactly",
            None,
            crate::pg_test::postgresql_conf_options(),
        )
        .expect("the test server did not start");
        let mut beside = pgrx_tests::client()
            .expect("no session on the test server")
            .0;
        let server = beside
            .query_one(
                "SELECT coalesce(host(inet_server_addr()), \
                                 current_setting('unix_socket_directories')), \
                        current_setting('port')::int, current_user::text",
                &[],
            )
            .unwrap();
        beside
            .batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .unwrap();
        beside
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .unwrap();
        let mut db = postgres::Config::new()
            .host(&server.get::<_, String>(0))
            .port(server.get::<_, i32>(1) as u16)
            .user(&server.get::<_, String>(2))
            .dbname(name)
            .connect(postgres::NoTls)
            .expect("no session on the database of its own");
        db.batch_execute(
            "CREATE EXTENSION warren_surveyor_pg CASCADE; CREATE EXTENSION pageinspect; \
             LOAD 'warren_surveyor_pg'",
        )
        .unwrap();
        (db, beside)
    }

    /// The rows EXPLAIN gives the top node of `query`'s plan.
    fn planned(db: &mut postgres::Client, query: &str) -> f64 {
        let line: String = db
            .query(&format!("EXPLAIN {query}"), &[])
            .unwrap()
            .first()
            .expect("a plan")
            .get(0);
        line.split(" rows=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("no estimate in {line}"))
    }

    #[test]
    fn entries_a_scan_found_dead_are_not_counted() {
        let (mut db, mut beside) = own_database("drained_entries");
        db.batch_execute(
            "CREATE TABLE drained (id int, k int) WITH (autovacuum_enabled = off); \
             INSERT INTO drained SELECT g, g FROM generate_series(1, 100000) g; \
             CREATE INDEX drained_k ON drained (k) WITH (deduplicate_items = off); \
             CREATE INDEX drained_order ON drained USING surveyor (id); \
             ANALYZE drained",
        )
        .unwrap();
        db.batch_execute("DELETE FROM drained WHERE k BETWEEN 20001 AND 60000")
            .unwrap();
        // a scan through the index after the delete marks the entries of the rows it finds gone
        // dead, once no session's snapshot can still see those rows
        let gone = "SELECT id FROM drained WHERE k BETWEEN 20001 AND 60000";
        let started = Instant::now();
        let dead = loop {
            db.batch_execute(&format!(
                "SET enable_seqscan = off; SET enable_bitmapscan = off; \
                 SET enable_indexonlyscan = off; SELECT count(*) FROM ({gone}) s; \
                 RESET enable_seqscan; RESET enable_bitmapscan; RESET enable_indexonlyscan"
            ))
            .unwrap();
            let dead: i64 = db
                .query_one(
                    "SELECT count(*) FROM bt_multi_page_stats('drained_k', 1, -1) s, \
                         LATERAL bt_page_items('drained_k', s.blkno::int) i \
                     WHERE s.type = 'l' AND i.dead",
                    &[],
                )
                .unwrap()
                .get(0);
            if dead >= 40000 || started.elapsed() > Duration::from_secs(120) {
                break dead;
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        assert!(dead >= 40000, "{dead} entries marked dead");
        // the rows gone are counted as none, and those beside them as they are
        let rows = planned(&mut db, gone);
        assert!(rows <= 10.0, "{rows} planned");
        let kept = planned(
            &mut db,
            "SELECT id FROM drained WHERE k BETWEEN 60001 AND 80000",
        );
        assert!((kept - 20000.0).abs() <= 0.05 * 20000.0, "{kept} planned");
        drop(db);
        beside
            .batch_execute("DROP DATABASE drained_entries WITH (FORCE)")
            .unwrap();
    }
}
