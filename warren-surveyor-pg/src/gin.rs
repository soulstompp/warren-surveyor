// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a GIN measures of a relation's conditions, read from the index's own pages while planning.
//!
//! Each key a condition asks for is found by its entry. A key whose rows are listed beside its
//! entry is counted by the list's length. A key whose rows are kept in a tree of their own is
//! counted from the tree's root, its first leaf, one leaf in its middle and its last leaf: the
//! first, the middle once for each leaf between the first and the last, and the last. A tree of one
//! or two leaves is counted whole.
//!
//! The rows waiting in the index's pending list are never read. A key's rows are taken as a share
//! of the rows the index has placed, the relation's rows less the pending rows the metapage
//! counts, and the measure is that share of the relation's rows.
//!
//! A text search (`@@`) is measured word by word, and the words' shares among the rows that are
//! not NULL are combined as PostgreSQL combines them for `@@`. An `ILIKE` a trigram index holds,
//! and a `LIKE` whose pattern holds no letter that changes with case, is measured by the fewest
//! rows any trigram it requires holds; any other `LIKE` is left to the planner.
//!
//! The read stops once its pages would pass the pages of the table, or what is left of the
//! statement's planning-read budget (`budget`), and its conditions are left to the planner.

use crate::conditions::{compares_as, Held};
use crate::reading::{address, block_of, on_first_column, read, support_name, PageCopy};
use pgrx::pg_sys;
use std::cmp::Ordering;

const METAPAGE: pg_sys::BlockNumber = 0;
const ENTRY_ROOT: pg_sys::BlockNumber = 1;
const GIN_LEAF: u16 = 1 << 1;
const GIN_DELETED: u16 = 1 << 2;
const GIN_META: u16 = 1 << 3;
const GIN_COMPRESSED: u16 = 1 << 7;
/// The count of listed rows an entry carries when its rows are in a tree.
const IN_A_TREE: u16 = 0xffff;
const NORMAL_KEY: i8 = 0;
const NULL_ITEM: i8 = 3;
const COMPARE_PROC: u16 = 1;
const EXTRACT_QUERY_PROC: i16 = 3;
const INDEX_NULL_MASK: u16 = 0x8000;
/// `tsvector_ops`'s `@@` and `@@@`.
const TEXT_MATCH: [i32; 2] = [1, 2];
/// `gin_trgm_ops`'s `LIKE` and `ILIKE`.
const TRIGRAM_PATTERN: [i32; 2] = [3, 4];
/// `gin_trgm_ops`'s `LIKE`.
const TRIGRAM_LIKE: i32 = 3;
const TRIGRAM_EXTRACT: &str = "gin_extract_query_trgm";
/// The bytes of a text search query before its first item: its length and its item count.
const QUERY_HEADER: usize = 8;
/// The bytes of one downlink of a posting tree's internal page: a block, then a heap address.
const POSTING_ITEM: usize = 10;
/// The bytes of a compressed posting list's header: its first heap address and its length.
const SEGMENT_HEADER: usize = 8;
/// The bytes a posting tree page keeps after its header for its right bound.
const RIGHT_BOUND: usize = 8;

/// A key to find among a GIN's entries: a value of the index's storage type, or a category of
/// NULL.
#[derive(Clone, Copy)]
pub(crate) struct Key {
    category: i8,
    value: pg_sys::Datum,
}

impl Key {
    pub(crate) fn value(value: pg_sys::Datum) -> Key {
        Key {
            category: NORMAL_KEY,
            value,
        }
    }

    /// The key under which a GIN lists the rows whose indexed value is NULL.
    pub(crate) fn null_rows() -> Key {
        Key {
            category: NULL_ITEM,
            value: pg_sys::Datum::from(0usize),
        }
    }
}

/// Where an entry keeps its rows.
pub(crate) enum Posting {
    Listed(u64),
    Tree(pg_sys::BlockNumber),
    Absent,
}

/// What a posting tree measures: its rows, its leaves, and the pages read for it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TreeCount {
    pub rows: f64,
    pub leaves: f64,
    pub pages: u32,
}

/// A GIN opened for reading: how it compares its keys, and the pages read so far.
pub(crate) struct Gin {
    index: pg_sys::Relation,
    compare: *mut pg_sys::FmgrInfo,
    collation: pg_sys::Oid,
    pub pages: u32,
    /// The most pages the read may read.
    most: u32,
}

impl Gin {
    /// A GIN of one key column, read for at most `most` pages. None for any other index.
    pub(crate) unsafe fn open(index: pg_sys::Relation, most: u32) -> Option<Gin> {
        if (*(*index).rd_rel).relam != pg_sys::GIN_AM_OID || (*(*index).rd_index).indnkeyatts != 1 {
            return None;
        }
        let collation = match *(*index).rd_indcollation {
            pg_sys::InvalidOid => pg_sys::DEFAULT_COLLATION_OID,
            c => c,
        };
        Some(Gin {
            index,
            compare: pg_sys::index_getprocinfo(index, 1, COMPARE_PROC),
            collation,
            pages: 0,
            most,
        })
    }

    /// Page `block`, counted; none where it would pass the most the read may read, or what is left
    /// of the statement's planning-read budget.
    unsafe fn page(&mut self, block: pg_sys::BlockNumber) -> Option<PageCopy> {
        if self.pages >= self.most {
            return None;
        }
        let page = read(self.index, block)?;
        self.pages += 1;
        Some(page)
    }

    /// The rows waiting in the pending list, from the metapage. None where block 0 is not a GIN
    /// metapage, or the read has read its most.
    pub(crate) unsafe fn pending(&mut self) -> Option<f64> {
        let page = self.page(METAPAGE)?;
        if page.special::<pg_sys::GinPageOpaqueData>().flags & GIN_META == 0 {
            return None;
        }
        let meta = std::ptr::read_unaligned(page.contents() as *const pg_sys::GinMetaPageData);
        Some(meta.nPendingHeapTuples as f64)
    }

    /// The key an entry tuple holds.
    unsafe fn key_of(&self, tuple: pg_sys::IndexTuple) -> Key {
        if (*tuple).t_info & INDEX_NULL_MASK != 0 {
            // the category follows the tuple's header and its bitmap of NULLs
            let at = (std::mem::size_of::<pg_sys::IndexTupleData>()
                + std::mem::size_of::<pg_sys::IndexAttributeBitMapData>()
                + 7)
                & !7;
            Key {
                category: *((tuple as *const u8).add(at) as *const i8),
                value: pg_sys::Datum::from(0usize),
            }
        } else {
            Key::value(pg_sys::nocache_index_getattr(
                tuple,
                1,
                (*self.index).rd_att,
            ))
        }
    }

    /// Where `key` lies against the key of an entry tuple.
    unsafe fn order(&self, key: &Key, tuple: pg_sys::IndexTuple) -> Ordering {
        let held = self.key_of(tuple);
        if key.category != held.category {
            return key.category.cmp(&held.category);
        }
        if key.category != NORMAL_KEY {
            return Ordering::Equal;
        }
        let r = pg_sys::FunctionCall2Coll(self.compare, self.collation, key.value, held.value)
            .value() as i32;
        r.cmp(&0)
    }

    /// Where the entry for `key` keeps its rows; none where the read has read its most.
    pub(crate) unsafe fn posting(&mut self, key: &Key) -> Option<Posting> {
        Some(match self.entry(key)? {
            None => Posting::Absent,
            Some((page, at)) => {
                let tuple = page.item(at);
                let listed = (*tuple).t_tid.ip_posid;
                if listed == IN_A_TREE {
                    Posting::Tree(block_of(tuple))
                } else {
                    Posting::Listed(listed as u64)
                }
            }
        })
    }

    /// The entry for `key`, found by descending the entry tree from its root as GIN's own search
    /// does: the leaf page holding it and its offset there, or none where no entry holds the key.
    /// None where the read has read its most.
    unsafe fn entry(&mut self, key: &Key) -> Option<Option<(PageCopy, pg_sys::OffsetNumber)>> {
        let mut block = ENTRY_ROOT;
        loop {
            let mut page = self.page(block)?;
            // a page split after its parent was read holds the keys past its last on its right
            while block != ENTRY_ROOT {
                let right = page.special::<pg_sys::GinPageOpaqueData>().rightlink;
                let last = page.last();
                if right == pg_sys::InvalidBlockNumber
                    || last < 1
                    || self.order(key, page.item(last)) != Ordering::Greater
                {
                    break;
                }
                block = right;
                page = self.page(block)?;
            }
            let opaque = page.special::<pg_sys::GinPageOpaqueData>();
            let (flags, rightmost) = (opaque.flags, opaque.rightlink == pg_sys::InvalidBlockNumber);
            let last = page.last();
            if last < 1 {
                return Some(None);
            }
            if flags & GIN_LEAF != 0 {
                let (mut low, mut high) = (1, last + 1);
                while high > low {
                    let mid = low + (high - low) / 2;
                    match self.order(key, page.item(mid)) {
                        Ordering::Equal => return Some(Some((page, mid))),
                        Ordering::Greater => low = mid + 1,
                        Ordering::Less => high = mid,
                    }
                }
                return Some(None);
            }
            // the first downlink whose key is at or after the key; the last of the rightmost page
            // stands for every key after the others
            let (mut low, mut high) = (1, last + 1);
            let mut at = None;
            while high > low {
                let mid = low + (high - low) / 2;
                let o = if mid == last && rightmost {
                    Ordering::Less
                } else {
                    self.order(key, page.item(mid))
                };
                match o {
                    Ordering::Equal => {
                        at = Some(mid);
                        break;
                    }
                    Ordering::Greater => low = mid + 1,
                    Ordering::Less => high = mid,
                }
            }
            block = block_of(page.item(at.unwrap_or(high.min(last))));
        }
    }

    /// The rows the index lists for `key`; none where the read has read its most.
    pub(crate) unsafe fn rows(&mut self, key: &Key) -> Option<f64> {
        Some(match self.posting(key)? {
            Posting::Listed(n) => n as f64,
            Posting::Tree(root) => self.tree(root)?.rows,
            Posting::Absent => 0.0,
        })
    }

    /// The rows of the posting tree whose root is `root`, from its root, its first leaf, a leaf in
    /// its middle and its last leaf. A level under a level of more than two pages is counted from
    /// the first, the middle and the last page of the level above it in the same way. None where the
    /// read has read its most.
    pub(crate) unsafe fn tree(&mut self, root: pg_sys::BlockNumber) -> Option<TreeCount> {
        let before = self.pages;
        let page = self.page(root)?;
        if is_leaf(&page) {
            return Some(TreeCount {
                rows: leaf_rows(&page) as f64,
                leaves: 1.0,
                pages: self.pages - before,
            });
        }
        // the level's pages, and its first, middle and last pages read
        let mut count = 1.0;
        let mut first = page;
        let mut middle: Option<PageCopy> = None;
        let mut last: Option<PageCopy> = None;
        loop {
            let (next, first_block, middle_block, last_block) = if count <= 2.0 {
                // every page of the level is read: its children are the next level whole
                let mut all = children(&first);
                if let Some(l) = &last {
                    all.extend(children(l));
                }
                let n = all.len();
                (
                    n as f64,
                    all.first().copied(),
                    (n >= 3).then(|| all[n / 2]),
                    (n >= 2).then(|| all[n - 1]),
                )
            } else {
                let (f, m, l) = (
                    children(&first),
                    children(middle.as_ref().unwrap_or(&first)),
                    children(last.as_ref().unwrap_or(&first)),
                );
                (
                    f.len() as f64 + m.len() as f64 * (count - 2.0) + l.len() as f64,
                    f.first().copied(),
                    m.get(m.len() / 2).copied(),
                    l.last().copied(),
                )
            };
            let Some(first_block) = first_block else {
                return Some(TreeCount {
                    rows: 0.0,
                    leaves: 0.0,
                    pages: self.pages - before,
                });
            };
            first = self.page(first_block)?;
            middle = match middle_block {
                Some(b) => Some(self.page(b)?),
                None => None,
            };
            last = match last_block {
                Some(b) => Some(self.page(b)?),
                None => None,
            };
            count = next;
            if is_leaf(&first) {
                let rows = match (&middle, &last) {
                    (Some(m), Some(l)) => {
                        leaf_rows(&first) as f64
                            + leaf_rows(m) as f64 * (count - 2.0)
                            + leaf_rows(l) as f64
                    }
                    (None, Some(l)) => leaf_rows(&first) as f64 + leaf_rows(l) as f64,
                    _ => leaf_rows(&first) as f64,
                };
                return Some(TreeCount {
                    rows,
                    leaves: count,
                    pages: self.pages - before,
                });
            }
        }
    }
}

unsafe fn is_leaf(page: &PageCopy) -> bool {
    page.special::<pg_sys::GinPageOpaqueData>().flags & GIN_LEAF != 0
}

/// The pages an internal page of a posting tree points down to, in order.
unsafe fn children(page: &PageCopy) -> Vec<pg_sys::BlockNumber> {
    let n = page.special::<pg_sys::GinPageOpaqueData>().maxoff as usize;
    let items = page.contents().add(RIGHT_BOUND);
    (0..n)
        .map(|i| {
            let at = items.add(i * POSTING_ITEM);
            let hi = std::ptr::read_unaligned(at as *const u16) as u32;
            let lo = std::ptr::read_unaligned(at.add(2) as *const u16) as u32;
            (hi << 16) | lo
        })
        .collect()
}

/// The heap addresses a leaf of a posting tree holds.
unsafe fn leaf_rows(page: &PageCopy) -> u64 {
    let opaque = page.special::<pg_sys::GinPageOpaqueData>();
    if opaque.flags & GIN_COMPRESSED == 0 {
        return opaque.maxoff as u64;
    }
    let start = page.contents().add(RIGHT_BOUND);
    let end = (page.ptr() as *const u8).add(page.lower());
    let mut at = start;
    let mut rows = 0;
    while at.add(SEGMENT_HEADER) <= end {
        let bytes = std::ptr::read_unaligned(at.add(6) as *const u16) as usize;
        rows += 1 + encoded(at.add(SEGMENT_HEADER), bytes);
        at = at.add(SEGMENT_HEADER + ((bytes + 1) & !1));
    }
    rows
}

/// The numbers in `len` bytes of GIN's variable-length encoding: each takes bytes until one has
/// its high bit clear, or six.
unsafe fn encoded(bytes: *const u8, len: usize) -> u64 {
    let (mut at, mut n) = (0, 0);
    while at < len {
        let mut taken = 0;
        loop {
            let c = *bytes.add(at);
            at += 1;
            taken += 1;
            if c & 0x80 == 0 || taken == 6 {
                break;
            }
        }
        n += 1;
    }
    n
}

/// The address stored at `at`: its block's two halves, then its place on the block.
unsafe fn address_at(at: *const u8) -> u64 {
    let hi = std::ptr::read_unaligned(at as *const u16) as u32;
    let lo = std::ptr::read_unaligned(at.add(2) as *const u16) as u32;
    let place = std::ptr::read_unaligned(at.add(4) as *const u16);
    address((hi << 16) | lo, place)
}

/// The keys the operator class extracts from `query` for `strategy`, as a scan of the index would
/// search them. None where the class asks for a partial match or a NULL key, or extracts none, or
/// asks for a scan of the whole index where `whole` is false.
unsafe fn extract(
    index: *mut pg_sys::IndexOptInfo,
    strategy: i32,
    query: pg_sys::Datum,
    whole: bool,
) -> Option<Vec<pg_sys::Datum>> {
    let input = *(*index).opcintype;
    let proc_ = pg_sys::get_opfamily_proc(*(*index).opfamily, input, input, EXTRACT_QUERY_PROC);
    if proc_ == pg_sys::InvalidOid {
        return None;
    }
    let flinfo = pg_sys::palloc0(std::mem::size_of::<pg_sys::FmgrInfo>()) as *mut pg_sys::FmgrInfo;
    pg_sys::fmgr_info(proc_, flinfo);
    if !(*index).opclassoptions.is_null() {
        pg_sys::set_fn_opclass_options(flinfo, *(*index).opclassoptions);
    }
    let collation = match *(*index).indexcollations {
        pg_sys::InvalidOid => pg_sys::DEFAULT_COLLATION_OID,
        c => c,
    };
    let asked = extraction(flinfo, collation, strategy, query)?;
    let scans_all = asked.mode == pg_sys::GIN_SEARCH_MODE_ALL as i32;
    if (asked.mode != pg_sys::GIN_SEARCH_MODE_DEFAULT as i32 && !(whole && scans_all))
        || asked.partial
        || asked.null
    {
        return None;
    }
    Some(asked.keys)
}

/// What an operator class's query extraction gives for one query: the keys a scan searches, as a
/// list and as the array the class made, whether any asks for a partial match or is NULL, the
/// data the class keeps for its consistent function, and how the scan searches the index.
struct Extraction {
    keys: Vec<pg_sys::Datum>,
    values: *mut pg_sys::Datum,
    partial: bool,
    null: bool,
    extra: *mut pg_sys::Pointer,
    mode: i32,
}

/// What the extraction function `flinfo` gives for `query` under `strategy` and `collation`. None
/// where it gives no key.
unsafe fn extraction(
    flinfo: *mut pg_sys::FmgrInfo,
    collation: pg_sys::Oid,
    strategy: i32,
    query: pg_sys::Datum,
) -> Option<Extraction> {
    let mut n: i32 = 0;
    let mut partial: *mut bool = std::ptr::null_mut();
    let mut extra: *mut pg_sys::Pointer = std::ptr::null_mut();
    let mut nulls: *mut bool = std::ptr::null_mut();
    let mut mode: i32 = pg_sys::GIN_SEARCH_MODE_DEFAULT as i32;
    let keys = pg_sys::FunctionCall7Coll(
        flinfo,
        collation,
        query,
        pg_sys::Datum::from(&mut n as *mut i32),
        pg_sys::Datum::from(strategy as u16 as usize),
        pg_sys::Datum::from(&mut partial as *mut *mut bool),
        pg_sys::Datum::from(&mut extra as *mut *mut pg_sys::Pointer),
        pg_sys::Datum::from(&mut nulls as *mut *mut bool),
        pg_sys::Datum::from(&mut mode as *mut i32),
    )
    .cast_mut_ptr::<pg_sys::Datum>();
    if n <= 0 || keys.is_null() {
        return None;
    }
    let n = n as usize;
    Some(Extraction {
        keys: (0..n).map(|j| *keys.add(j)).collect(),
        values: keys,
        partial: !partial.is_null() && (0..n).any(|j| *partial.add(j)),
        null: !nulls.is_null() && (0..n).any(|j| *nulls.add(j)),
        extra,
        mode,
    })
}

/// What a text search query is made of: for each item, the word it is, where it is one, and
/// whether any item joins others.
struct Words {
    items: *const pg_sys::QueryItem,
    size: usize,
    word: Vec<Option<usize>>,
    words: usize,
    joined: bool,
}

/// Reads a text search query. None where it is empty, or a word in it carries a weight or asks for
/// a prefix.
unsafe fn words(query: pg_sys::Datum) -> Option<Words> {
    let q = pg_sys::pg_detoast_datum(query.cast_mut_ptr()) as *const u8;
    let size = std::ptr::read_unaligned(q.add(4) as *const i32);
    if size <= 0 {
        return None;
    }
    let size = size as usize;
    let items = q.add(QUERY_HEADER) as *const pg_sys::QueryItem;
    let mut word = vec![None; size];
    let (mut words, mut joined) = (0, false);
    for (i, w) in word.iter_mut().enumerate() {
        let item = &*items.add(i);
        match item.type_ as u32 {
            pg_sys::QI_VAL => {
                let operand = item.qoperand;
                if operand.prefix || operand.weight != 0 {
                    return None;
                }
                *w = Some(words);
                words += 1;
            }
            pg_sys::QI_OPR => joined = true,
            _ => return None,
        }
    }
    Some(Words {
        items,
        size,
        word,
        words,
        joined,
    })
}

/// The share of the rows matching the query from item `at` on, from each word's share, combined
/// as PostgreSQL combines them for `@@`.
unsafe fn combine(q: &Words, at: usize, shares: &[f64]) -> Option<f64> {
    pg_sys::check_stack_depth();
    if at >= q.size {
        return None;
    }
    let item = &*q.items.add(at);
    let share = match item.type_ as u32 {
        pg_sys::QI_VAL => *shares.get(q.word[at]?)?,
        pg_sys::QI_OPR => {
            let op = item.qoperator;
            let right = combine(q, at + 1, shares)?;
            match op.oper as u32 {
                pg_sys::OP_NOT => 1.0 - right,
                pg_sys::OP_AND | pg_sys::OP_PHRASE => {
                    right * combine(q, at + op.left as usize, shares)?
                }
                pg_sys::OP_OR => {
                    let left = combine(q, at + op.left as usize, shares)?;
                    right + left - right * left
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(share.clamp(0.0, 1.0))
}

/// A condition the index holds, and the keys it asks for.
enum Asked {
    Words(pg_sys::Datum, Vec<pg_sys::Datum>),
    Pattern(Vec<pg_sys::Datum>),
}

/// Whether the text `pattern` holds no letter that changes with case: the default collation's
/// lower and upper case each leave it as it is, the collation a trigram index folds case by.
unsafe fn caseless(pattern: pg_sys::Datum) -> bool {
    let text = |d: pg_sys::Datum| {
        let s = pg_sys::text_to_cstring(d.cast_mut_ptr::<pg_sys::text>());
        let owned = std::ffi::CStr::from_ptr(s).to_bytes().to_vec();
        pg_sys::pfree(s as *mut std::ffi::c_void);
        owned
    };
    let collation = pg_sys::DEFAULT_COLLATION_OID;
    let lowered = pg_sys::OidFunctionCall1Coll(pg_sys::F_LOWER_TEXT.into(), collation, pattern);
    let raised = pg_sys::OidFunctionCall1Coll(pg_sys::F_UPPER_TEXT.into(), collation, pattern);
    let own = text(pattern);
    text(lowered) == own && text(raised) == own
}

/// What the GIN `index` of the base relation `rel` holds of `rel`'s constant conditions: a text
/// search on a `tsvector` key, and on a trigram key an `ILIKE`, or a `LIKE` whose pattern holds no
/// letter that changes with case. None where it holds none, where it is partial or of more than
/// one column, or where its metapage cannot be read.
pub(crate) unsafe fn held(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
) -> Option<Held> {
    held_except(root, rel, index, &[])
}

/// The conditions the GIN `index` of the base relation `rel` would hold, other than those in
/// `counted`, read from the conditions alone, before any page of the index is read.
pub(crate) unsafe fn holds(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Vec<*mut pg_sys::RestrictInfo> {
    asked(root, rel, index, counted)
        .into_iter()
        .map(|(_, clause)| clause)
        .collect()
}

/// Each condition of the base relation `rel`, other than those in `counted`, that the GIN `index`
/// is asked under the collation its key keeps, with the keys it asks for.
unsafe fn asked(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Vec<(Asked, *mut pg_sys::RestrictInfo)> {
    let mut asked = Vec::new();
    if (*index).relam != pg_sys::GIN_AM_OID
        || !(*index).indpred.is_null()
        || (*index).nkeycolumns != 1
    {
        return asked;
    }
    let input = *(*index).opcintype;
    let trigrams = support_name(index, EXTRACT_QUERY_PROC).as_deref() == Some(TRIGRAM_EXTRACT);
    for c in on_first_column(root, rel, index, counted) {
        let op = (*c.clause).clause as *mut pg_sys::OpExpr;
        if !compares_as(index, 0, (*op).inputcollid) {
            continue;
        }
        let value = (*c.value).constvalue;
        if input == pg_sys::TSVECTOROID
            && (*c.value).consttype == pg_sys::TSQUERYOID
            && TEXT_MATCH.contains(&c.strategy)
        {
            if let Some(keys) = extract(index, c.strategy, value, true) {
                asked.push((Asked::Words(value, keys), c.clause));
            }
        } else if trigrams
            && TRIGRAM_PATTERN.contains(&c.strategy)
            && (c.strategy != TRIGRAM_LIKE || caseless(value))
        {
            if let Some(keys) = extract(index, c.strategy, value, false) {
                asked.push((Asked::Pattern(keys), c.clause));
            }
        }
    }
    asked
}

/// As `held`, leaving out the conditions in `counted`.
pub(crate) unsafe fn held_except(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<Held> {
    let asked = asked(root, rel, index, counted);
    if asked.is_empty() {
        return None;
    }
    let tuples = (*rel).tuples;
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let held = (|| {
        let mut gin = Gin::open(rel_index, (*rel).pages)?;
        let placed = tuples - gin.pending()?;
        if placed < 1.0 {
            return None;
        }
        let mut nulls = None;
        let mut share = 1.0;
        let mut clauses = Vec::new();
        for (a, clause) in asked {
            let measured = match a {
                Asked::Words(query, keys) => {
                    let Some(q) = words(query) else { continue };
                    if q.words != keys.len() {
                        continue;
                    }
                    let mut rows = Vec::with_capacity(keys.len());
                    for k in &keys {
                        rows.push(gin.rows(&Key::value(*k))?);
                    }
                    if !q.joined {
                        Some(rows[0] / placed)
                    } else {
                        let null = match nulls {
                            Some(n) => n,
                            None => *nulls.insert(gin.rows(&Key::null_rows())?),
                        };
                        let present = placed - null;
                        if present <= 0.0 {
                            Some(0.0)
                        } else {
                            let shares: Vec<f64> =
                                rows.iter().map(|r| (r / present).clamp(0.0, 1.0)).collect();
                            combine(&q, 0, &shares).map(|s| s * present / placed)
                        }
                    }
                }
                Asked::Pattern(keys) => {
                    let mut fewest: Option<f64> = None;
                    for k in &keys {
                        let rows = gin.rows(&Key::value(*k))?;
                        fewest = Some(fewest.map_or(rows, |f| f.min(rows)));
                    }
                    fewest.map(|f| f / placed)
                }
            };
            if let Some(s) = measured {
                share *= s.clamp(0.0, 1.0);
                clauses.push(clause);
            }
        }
        (!clauses.is_empty()).then_some(Held {
            rows: share * tuples,
            pages: gin.pages,
            clauses,
        })
    })();
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    held
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::{Gin, Key, Posting};
    use crate::planned::planning;
    use pgrx::prelude::*;
    use pgrx::IntoDatum;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The names of rows `from` to `to`: NULL for every 50th, otherwise "brick" in two of three,
    /// "plate" in one of five, "tile" in one of seven, "bright" in one of eleven, "quick" in one of
    /// thirteen, "arch" in one of a thousand, and one of 997 words "w<n>" in each.
    fn names(from: u32, to: u32) -> String {
        format!(
            "SELECT g AS id, CASE WHEN g % 50 = 1 THEN NULL ELSE concat_ws(' ', \
                 CASE WHEN g % 3 <> 0 THEN 'brick' END, CASE WHEN g % 5 = 0 THEN 'plate' END, \
                 CASE WHEN g % 7 = 0 THEN 'tile' END, CASE WHEN g % 11 = 0 THEN 'bright' END, \
                 CASE WHEN g % 13 = 0 THEN 'quick' END, CASE WHEN g % 1000 = 0 THEN 'arch' END, \
                 'w' || (g % 997)) END AS name \
             FROM generate_series({from}, {to}) g"
        )
    }

    /// A table of `rows` names with a word GIN, built with `with`, and analyzed.
    fn named(table: &str, rows: u32, with: &str) {
        Spi::run(&format!(
            "CREATE TABLE {table} AS {}; \
             CREATE INDEX {table}_words ON {table} USING gin (to_tsvector('simple', name)) {with}; \
             ANALYZE {table}",
            names(1, rows)
        ))
        .unwrap();
    }

    fn count(sql: &str) -> f64 {
        Spi::get_one::<i64>(sql).unwrap().unwrap() as f64
    }

    fn open(index: &str) -> pg_sys::Relation {
        let oid = Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{index}'::regclass::oid"))
            .unwrap()
            .unwrap();
        unsafe { pg_sys::index_open(oid, pg_sys::AccessShareLock as pg_sys::LOCKMODE) }
    }

    fn close(rel: pg_sys::Relation) {
        unsafe { pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE) };
    }

    /// What a GIN held of `query`'s conditions while it was planned: the rows, the pages read and
    /// the conditions held.
    fn held(query: &str) -> Option<(f64, u32, usize)> {
        let seen = Rc::new(RefCell::new(None));
        let s = seen.clone();
        planning(query, move |root, rel, index| unsafe {
            if let Some(h) = super::held(root, rel, index) {
                *s.borrow_mut() = Some((h.rows, h.pages, h.clauses.len()));
            }
        });
        let out = *seen.borrow();
        out
    }

    /// The leaves of the posting tree under `root`, walked along its leaf level by pageinspect:
    /// how many, their heap addresses, and the most one holds.
    fn leaves_of(index: &str, root: pg_sys::BlockNumber) -> (f64, f64, f64) {
        Spi::run("CREATE EXTENSION IF NOT EXISTS pageinspect").unwrap();
        let sql = format!(
            "WITH RECURSIVE r AS (SELECT get_raw_page('{index}', {root}) AS p), \
             first AS (SELECT CASE WHEN 'leaf' = ANY ((gin_page_opaque_info(p)).flags) THEN {root}::bigint \
                 ELSE (((get_byte(p, 32) | (get_byte(p, 33) << 8))::bigint << 16) \
                       | (get_byte(p, 34) | (get_byte(p, 35) << 8))) END AS b FROM r), \
             chain(b) AS (SELECT b FROM first UNION ALL \
                 SELECT (gin_page_opaque_info(get_raw_page('{index}', c.b::int))).rightlink::bigint \
                 FROM chain c \
                 WHERE (gin_page_opaque_info(get_raw_page('{index}', c.b::int))).rightlink <> 4294967295) \
             SELECT count(*)::float8, sum(n)::float8, max(n)::float8 FROM ( \
                 SELECT (SELECT sum(cardinality(i.tids)) FROM gin_leafpage_items(get_raw_page('{index}', c.b::int)) i) AS n \
                 FROM chain c) x"
        );
        Spi::get_three::<f64, f64, f64>(&sql)
            .map(|(a, b, c)| (a.unwrap(), b.unwrap(), c.unwrap()))
            .unwrap()
    }

    const WORDS: &str = "to_tsvector('simple', name)";

    #[pg_test]
    fn a_word_listed_beside_its_entry_is_counted_exactly() {
        named("named", 60000, "");
        for word in ["arch", "w5", "w996", "nothing"] {
            let cond = format!("{WORDS} @@ '{word}'::tsquery");
            let (rows, pages, held) = held(&format!("SELECT id FROM named WHERE {cond}"))
                .unwrap_or_else(|| panic!("{word}: nothing held"));
            let counted = count(&format!("SELECT count(*) FROM named WHERE {cond}"));
            assert!(
                (rows - counted).abs() <= 1e-9 * counted.max(1.0),
                "{word}: {rows} measured, {counted} counted"
            );
            assert_eq!(held, 1, "{word}");
            assert!(pages <= 4, "{word}: {pages} pages");
        }
        let rel = open("named_words");
        let mut gin = unsafe { Gin::open(rel, u32::MAX) }.unwrap();
        let nulls = unsafe { gin.rows(&Key::null_rows()) }.unwrap();
        close(rel);
        assert_eq!(
            nulls,
            count("SELECT count(*) FROM named WHERE name IS NULL")
        );
    }

    #[pg_test]
    fn a_word_in_a_tree_of_its_own_is_counted_within_one_leaf_from_four_of_its_pages() {
        named("named", 60000, "");
        let rel = open("named_words");
        let mut gin = unsafe { Gin::open(rel, u32::MAX) }.unwrap();
        let mut most = 0.0f64;
        for word in ["brick", "plate", "tile"] {
            let key = Key::value(word.into_datum().unwrap());
            let Some(Posting::Tree(root)) = (unsafe { gin.posting(&key) }) else {
                panic!("{word} is not in a tree of its own");
            };
            let tree = unsafe { gin.tree(root) }.unwrap();
            let (leaves, exact, fullest) = leaves_of("named_words", root);
            let counted = count(&format!(
                "SELECT count(*) FROM named WHERE {WORDS} @@ '{word}'::tsquery"
            ));
            assert_eq!(exact, counted, "{word}: the leaves hold every row");
            assert_eq!(tree.leaves, leaves, "{word}: {tree:?}");
            assert!(tree.pages <= 4, "{word}: {tree:?}");
            if leaves <= 2.0 {
                assert_eq!(tree.rows, counted, "{word}: {tree:?}");
            }
            assert!(
                (tree.rows - counted).abs() <= fullest,
                "{word}: {} rows measured, {counted} counted, {fullest} in the fullest leaf",
                tree.rows
            );
            most = most.max(leaves);
        }
        close(rel);
        assert!(most >= 3.0, "no tree of three leaves or more");
        let (rows, _, _) = held(&format!(
            "SELECT id FROM named WHERE {WORDS} @@ 'brick'::tsquery"
        ))
        .expect("brick is held");
        let counted = count(&format!(
            "SELECT count(*) FROM named WHERE {WORDS} @@ 'brick'::tsquery"
        ));
        assert!(
            (rows - counted).abs() <= 0.02 * counted,
            "{rows} measured, {counted} counted"
        );
    }

    #[pg_test]
    fn words_in_a_combination_are_each_measured_and_combined_as_postgres_combines_them() {
        named("named", 60000, "");
        let tuples = count("SELECT reltuples::bigint FROM pg_class WHERE relname = 'named'");
        let one = |word: &str| {
            held(&format!(
                "SELECT id FROM named WHERE {WORDS} @@ '{word}'::tsquery"
            ))
            .unwrap()
            .0
        };
        let (brick, plate, tile) = (one("brick"), one("plate"), one("tile"));
        let present = tuples - count("SELECT count(*) FROM named WHERE name IS NULL");
        let (b, p, t) = (brick / present, plate / present, tile / present);
        for (query, share) in [
            ("brick & plate", b * p),
            ("brick | plate", b + p - b * p),
            ("!brick", 1.0 - b),
            ("brick & !tile", b * (1.0 - t)),
            ("brick <-> plate", b * p),
            ("(brick | tile) & plate", (b + t - b * t) * p),
        ] {
            let (rows, _, held) = held(&format!(
                "SELECT id FROM named WHERE {WORDS} @@ '{query}'::tsquery"
            ))
            .unwrap_or_else(|| panic!("{query}: nothing held"));
            let expected = share * present;
            assert!(
                (rows - expected).abs() <= 1e-6 * expected,
                "{query}: {rows} measured, {expected} from each word's rows"
            );
            assert_eq!(held, 1, "{query}");
        }
    }

    #[pg_test]
    fn rows_pending_carry_the_share_the_placed_rows_carry() {
        named(
            "waiting",
            50000,
            "WITH (fastupdate = on, gin_pending_list_limit = 65536)",
        );
        Spi::run(&format!("INSERT INTO waiting {}", names(50001, 60000))).unwrap();
        let rel = open("waiting_words");
        let pending = unsafe { Gin::open(rel, u32::MAX).unwrap().pending() }.unwrap();
        close(rel);
        assert_eq!(
            pending, 10000.0,
            "the rows added after the build wait in the list"
        );
        for word in ["brick", "plate", "tile", "w5"] {
            let cond = format!("{WORDS} @@ '{word}'::tsquery");
            let (rows, _, _) = held(&format!("SELECT id FROM waiting WHERE {cond}"))
                .unwrap_or_else(|| panic!("{word}: nothing held"));
            let counted = count(&format!("SELECT count(*) FROM waiting WHERE {cond}"));
            assert!(
                (rows - counted).abs() <= 0.03 * counted + 2.0,
                "{word}: {rows} measured, {counted} counted"
            );
        }
    }

    /// 60,000 names with a trigram GIN, analyzed.
    fn patterned() {
        Spi::run(&format!(
            "CREATE EXTENSION IF NOT EXISTS pg_trgm; \
             CREATE TABLE patterned AS {}; \
             CREATE INDEX patterned_trgm ON patterned USING gin (name gin_trgm_ops); \
             ANALYZE patterned",
            names(1, 60000)
        ))
        .unwrap();
    }

    #[pg_test]
    fn an_ilike_and_a_like_with_no_letter_that_changes_with_case_are_measured_by_their_fewest_trigram(
    ) {
        patterned();
        for (op, word) in [
            ("ILIKE", "brick"),
            ("ILIKE", "Plate"),
            ("ILIKE", "tile"),
            ("ILIKE", "arch"),
            ("LIKE", "996"),
        ] {
            let cond = format!("name {op} '%{word}%'");
            let (rows, _, held) = held(&format!("SELECT id FROM patterned WHERE {cond}"))
                .unwrap_or_else(|| panic!("{cond}: nothing held"));
            assert_eq!(held, 1, "{cond}");
            let fewest = count(&format!(
                "SELECT min(n) FROM ( \
                     SELECT (SELECT count(*) FROM patterned p WHERE t = ANY (show_trgm(p.name))) AS n \
                     FROM (SELECT substr(lower('{word}'), i, 3) AS t \
                           FROM generate_series(1, length('{word}') - 2) i) t) x"
            ));
            let counted = count(&format!("SELECT count(*) FROM patterned WHERE {cond}"));
            assert!(
                (rows - fewest).abs() <= 0.02 * fewest + 1.0,
                "{cond}: {rows} measured, {fewest} in its fewest trigram"
            );
            assert!(
                rows >= 0.98 * counted,
                "{cond}: {rows} measured, {counted} counted"
            );
        }
    }

    #[pg_test]
    fn a_like_whose_pattern_holds_a_letter_that_changes_with_case_is_left_to_the_planner() {
        patterned();
        for word in ["plate", "Plate", "w99"] {
            let query = format!("SELECT id FROM patterned WHERE name LIKE '%{word}%'");
            assert!(held(&query).is_none(), "{query}");
        }
    }

    #[pg_test]
    fn a_pattern_compared_under_another_collation_than_its_trigram_index_is_left_to_the_planner() {
        patterned();
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        for (collation, scanned) in [("", true), (" COLLATE \"C\"", false)] {
            let query = format!("SELECT id FROM patterned WHERE name ILIKE '%brick%'{collation}");
            // PostgreSQL scans the index only for a pattern under the collation the index keeps
            let plan = crate::tests::texts(&format!("EXPLAIN {query}")).join("\n");
            assert_eq!(plan.contains("patterned_trgm"), scanned, "{plan}");
            assert_eq!(held(&query).is_some(), scanned, "{query}");
        }
    }

    #[pg_test]
    fn a_gin_read_stops_once_its_pages_would_pass_the_pages_of_its_table() {
        // 3,000 names on a few pages, and the same names each beside a note that fills a page
        // with a few of them
        Spi::run(&format!(
            "CREATE EXTENSION IF NOT EXISTS pg_trgm; \
             CREATE TABLE few AS {}; \
             CREATE TABLE padded (id int, name text, note text); \
             ALTER TABLE padded ALTER note SET STORAGE PLAIN; \
             INSERT INTO padded SELECT id, name, repeat('x', 2000) FROM few; \
             CREATE INDEX few_trgm ON few USING gin (name gin_trgm_ops); \
             CREATE INDEX padded_trgm ON padded USING gin (name gin_trgm_ops); \
             ANALYZE few; ANALYZE padded",
            names(1, 3000)
        ))
        .unwrap();
        let pages = |table: &str| {
            Spi::get_one::<i32>(&format!(
                "SELECT relpages FROM pg_class WHERE relname = '{table}'"
            ))
            .unwrap()
            .unwrap() as u32
        };
        // a long pattern, each of its trigrams found by its own entry
        let long = "name ILIKE '%brick plate tile bright quick arch%'";
        let (_, read, _) = held(&format!("SELECT id FROM padded WHERE {long}"))
            .unwrap_or_else(|| panic!("{long}: nothing held"));
        assert!(read > pages("few"), "{read} pages read");
        assert!(held(&format!("SELECT id FROM few WHERE {long}")).is_none());
        // a short one reads fewer pages than the table holds
        let short = "name ILIKE '%arch%'";
        let (_, read, _) = held(&format!("SELECT id FROM few WHERE {short}"))
            .unwrap_or_else(|| panic!("{short}: nothing held"));
        assert!(read <= pages("few"), "{read} pages read");
    }

    /// Each key of `index` whose rows are in a tree of their own: the key, the tree's root, its
    /// rows counted from four of its pages, its leaves, and the pages read for it.
    #[pg_extern]
    fn gin_trees(
        index: pg_sys::Oid,
    ) -> TableIterator<
        'static,
        (
            name!(key, String),
            name!(root, i64),
            name!(rows, f64),
            name!(leaves, f64),
            name!(pages, i32),
        ),
    > {
        let mut out = Vec::new();
        unsafe {
            let rel = pg_sys::index_open(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
            let mut gin = Gin::open(rel, u32::MAX).expect("a GIN of one column");
            let (mut output, mut varlena) = (pg_sys::InvalidOid, false);
            let storage = (*pg_sys::TupleDescAttr((*rel).rd_att, 0)).atttypid;
            pg_sys::getTypeOutputInfo(storage, &mut output, &mut varlena);
            // down the leftmost downlinks to the first entry leaf, then along the leaves
            let mut block = super::ENTRY_ROOT;
            loop {
                let page = super::read(rel, block).expect("a page");
                if super::is_leaf(&page) {
                    break;
                }
                block = super::block_of(page.item(1));
            }
            while block != pg_sys::InvalidBlockNumber {
                let page = super::read(rel, block).expect("a page");
                for offset in 1..=page.last() {
                    let tuple = page.item(offset);
                    if (*tuple).t_tid.ip_posid != super::IN_A_TREE {
                        continue;
                    }
                    let key = gin.key_of(tuple);
                    let text = if key.category == super::NORMAL_KEY {
                        std::ffi::CStr::from_ptr(pg_sys::OidOutputFunctionCall(output, key.value))
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        format!("<category {}>", key.category)
                    };
                    let root = super::block_of(tuple);
                    let tree = gin.tree(root).expect("a tree is counted");
                    out.push((text, root as i64, tree.rows, tree.leaves, tree.pages as i32));
                }
                block = page.special::<pg_sys::GinPageOpaqueData>().rightlink;
            }
            pg_sys::index_close(rel, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        }
        TableIterator::new(out)
    }
}
