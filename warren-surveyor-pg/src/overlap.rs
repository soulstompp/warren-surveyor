// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The rows of a relation in every one of the constant conditions that two or more of its B-trees
//! and GIN indexes hold, read from the indexes while planning.
//!
//! Each index measures the rows of its own conditions. Where the relation's measure counted
//! conditions with more than one B-tree or GIN, the rows in all of them are counted together, in
//! place of the indexes' shares taken one by one, the way the DBA's indexes lay the conditions out:
//!
//! - Where a B-tree's entries carry every column another index's conditions read, in its key or its
//!   INCLUDE, those conditions are tested on the entries of the B-tree's own block (`carried`). The
//!   B-tree holding the fewest rows that carries another's columns is read first.
//! - The conditions left, where two or more indexes hold them, each in its own index with neither
//!   carrying the other's columns, are matched by the addresses of their rows. That they are read
//!   together, by which indexes, how many pages the match read and whether it stopped, is noted at
//!   DEBUG1: two columns asked for together with no index relating them.
//!
//! A B-tree's addresses are those of the entries of its conditions' blocks, posting lists
//! included, read by the B-tree's own scan, which reads no table page. A GIN's are those its
//! operator class's consistent function keeps of the addresses listed beside each key a condition
//! asks for, or in the key's own tree; a trigram index's are the rows holding every trigram of the
//! pattern, which its recheck against the table may drop. A GIN's pending list is never read: the
//! rows are a share of the rows every GIN read has placed.
//!
//! The index whose conditions hold the fewest rows is read first, and each index read after it
//! keeps only the addresses already found, a GIN as it reads each key's list. The match stops once
//! its pages would pass the pages of the table, or what is left of the statement's planning-read
//! budget (`budget`): no index is read from the first whose share of its pages, with those before
//! it, would pass them, and every read stops at the page that passes them.
//! It also stops once the addresses it holds at once would pass what `work_mem` holds, eight bytes
//! to an address: it never starts where the first index's rows would pass them, and every read
//! stops at the address that passes them. The indexes read before it stops count their rows
//! together where there are two or more of them; the others are counted one by one.

use crate::carried;
use crate::conditions::{column_of, compares_as, constant, strategy};
use crate::gin::Gin;
use crate::query::cells;
use crate::reading::{address, on_first_column};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ptr::null_mut;

/// One index's part of a relation's measure: the index, the conditions it counted, and its share of
/// the table's rows.
pub(crate) type Part<'a> = (
    *mut pg_sys::IndexOptInfo,
    &'a [*mut pg_sys::RestrictInfo],
    f64,
);

/// The rows in all of several indexes' conditions: the parts of the relation's measure read
/// together, their share of the table's rows, the pages read, whether they were tested on one
/// index's own entries or matched by their rows' addresses, and whether every leaf of that
/// index's block was read.
#[derive(Clone, Debug)]
pub(crate) struct Together {
    pub parts: Vec<usize>,
    pub share: f64,
    pub pages: u32,
    pub on_entries: bool,
    pub every_leaf: bool,
}

/// The addresses one index holds for its conditions, of those already found, the rows it holds
/// pending, and the pages read.
struct Listed {
    addresses: Vec<u64>,
    pending: f64,
    pages: u32,
}

/// The rows' addresses a match may hold at once: what `work_mem` holds, eight bytes to an address,
/// and whether a read stopped at them. Addresses are gathered outside PostgreSQL's memory
/// contexts: a planning that ran out of memory there would abort its backend, and the server would
/// end every session to recover.
pub(crate) struct Room {
    most: usize,
    passed: bool,
}

impl Room {
    fn of_work_mem() -> Room {
        let bytes = unsafe { pg_sys::work_mem }.max(64) as usize * 1024;
        Room {
            most: bytes / std::mem::size_of::<u64>(),
            passed: false,
        }
    }

    /// Whether `held` addresses fit; where they do not, the match stops at them.
    pub(crate) fn fits(&mut self, held: f64) -> bool {
        let fits = held <= self.most as f64;
        self.passed |= !fits;
        fits
    }
}

/// The rows of the base relation `rel` in all the conditions of its measure's `parts` that B-trees
/// and GIN indexes counted, where two or more did: first each B-tree's block with the conditions
/// whose columns its entries carry, then the rest by their rows' addresses. Each group read
/// together, none where none is.
pub(crate) unsafe fn together(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    parts: &[Part],
) -> Vec<Together> {
    let mut left: Vec<usize> = (0..parts.len())
        .filter(|&at| {
            matches!(
                (*parts[at].0).relam,
                pg_sys::BTREE_AM_OID | pg_sys::GIN_AM_OID
            )
        })
        .collect();
    let mut groups = Vec::new();
    if left.len() < 2 || (*rel).tuples <= 0.0 {
        return groups;
    }
    left.sort_by(|&a, &b| parts[a].2.total_cmp(&parts[b].2));
    let rte = *(*root).simple_rte_array.add((*rel).relid as usize);
    while let Some(g) = on_entries(root, rel, parts, &left) {
        left.retain(|at| !g.parts.contains(at));
        groups.push(g);
    }
    if left.len() >= 2 {
        groups.extend(matched(root, rel, (*rte).relid, parts, &left));
    }
    groups
}

/// The first B-tree of `order`, of the parts of `rel`'s measure, that stores the values the
/// conditions of one or more of the other parts compare, with those conditions tested on the
/// block's own entries.
unsafe fn on_entries(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    parts: &[Part],
    order: &[usize],
) -> Option<Together> {
    let tuples = (*rel).tuples;
    for &at in order {
        let (index, own, _) = parts[at];
        let others: Vec<usize> = order
            .iter()
            .copied()
            .filter(|&o| o != at && carried::testable(root, rel, index, parts[o].1))
            .collect();
        if others.is_empty() {
            continue;
        }
        let tested: Vec<*mut pg_sys::RestrictInfo> = others
            .iter()
            .flat_map(|&o| parts[o].1.iter().copied())
            .collect();
        let Some((rows, pages, every_leaf)) =
            carried::rows(root, rel, index, own, &tested, (*rel).pages)
        else {
            continue;
        };
        let mut read = vec![at];
        read.extend(others);
        #[cfg(any(test, feature = "pg_test"))]
        tests::note(
            true,
            read.iter().map(|&p| (*parts[p].0).indexoid).collect(),
            Some(pages),
        );
        return Some(Together {
            parts: read,
            share: (rows / tuples).clamp(0.0, 1.0),
            pages,
            on_entries: true,
            every_leaf,
        });
    }
    None
}

/// The conditions `clauses` of `rel`, as written.
unsafe fn spelled(
    rel: *mut pg_sys::RelOptInfo,
    relid: pg_sys::Oid,
    clauses: &[*mut pg_sys::RestrictInfo],
) -> String {
    let context = pg_sys::deparse_context_for(pg_sys::get_rel_name(relid), relid);
    clauses
        .iter()
        .map(|&ri| {
            let clause = pg_sys::copyObjectImpl((*ri).clause as *const std::ffi::c_void)
                as *mut pg_sys::Node;
            pg_sys::ChangeVarNodes(clause, (*rel).relid as i32, 1, 0);
            std::ffi::CStr::from_ptr(pg_sys::deparse_expression(clause, context, false, false))
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Notes at DEBUG1 a match of the rows' addresses of `rel`'s parts `order`, each in its own index
/// with no index carrying another's columns: the indexes and their conditions, the pages each
/// index's list read, where it was read whole, the pages in all, and whether it stopped at the
/// table's pages, `most`, at the statement's planning-read limit (`limited`), or at the addresses
/// `work_mem` holds.
#[allow(clippy::too_many_arguments)]
unsafe fn noted(
    rel: *mut pg_sys::RelOptInfo,
    relid: pg_sys::Oid,
    parts: &[Part],
    order: &[usize],
    lists: &[(usize, u32)],
    most: u32,
    stopped: bool,
    limited: bool,
    room: &Room,
) {
    if !pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
        return;
    }
    let each: Vec<String> = order
        .iter()
        .map(|&at| {
            let read = lists
                .iter()
                .find(|l| l.0 == at)
                .map_or("not counted".to_string(), |l| format!("{} pages", l.1));
            format!(
                "{} on {} ({})",
                spelled(rel, relid, parts[at].1),
                name((*parts[at].0).indexoid),
                read
            )
        })
        .collect();
    let pages: u32 = lists.iter().map(|l| l.1).sum();
    let end = if room.passed {
        format!(
            "stopped at the {} rows' addresses work_mem holds",
            room.most
        )
    } else if stopped && limited {
        "stopped at the planning-read limit".to_string()
    } else if stopped {
        format!("stopped at the table's {most} pages")
    } else {
        format!("within the table's {most} pages")
    };
    debug1!(
        "surveyor: {} read by its rows' addresses, no index relating its conditions: {}; {} pages, {}",
        name(relid),
        each.join("; "),
        pages,
        end
    );
}

/// The rows of `rel` in all the conditions of the parts `order` of its measure, matched by their
/// rows' addresses. None where fewer than two of them are read before the match would pass the
/// pages of the table, what is left of the statement's planning-read budget, or the addresses
/// `work_mem` holds.
unsafe fn matched(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    relid: pg_sys::Oid,
    parts: &[Part],
    order: &[usize],
) -> Option<Together> {
    let most = (*rel).pages;
    let mut room = Room::of_work_mem();
    // no index is read from the first whose share of its pages, with those before it, would pass
    // the pages of the table or what is left of the budget; none at all where the rows of the
    // first, the fewest, would pass the addresses the match may hold
    let mut predicted = 0.0;
    let mut limited = false;
    let reach = order
        .iter()
        .position(|&at| {
            predicted += parts[at].2 * (*parts[at].0).pages as f64;
            if predicted > most as f64 {
                return true;
            }
            limited = !crate::budget::fits_read(predicted, || {
                format!("the rows' addresses of {}", name(relid))
            });
            limited
        })
        .unwrap_or(order.len());
    let reach = match order.first() {
        Some(&first) if !room.fits(parts[first].2 * (*rel).tuples) => 0,
        _ => reach,
    };
    let mut found: Option<Vec<u64>> = None;
    let (mut pages, mut pending) = (0u32, 0.0f64);
    let mut read = Vec::new();
    let mut lists = Vec::new();
    let mut stopped = reach < order.len();
    for &at in &order[..if reach < 2 { 0 } else { reach }] {
        let (index, clauses, share) = parts[at];
        if found.as_ref().is_some_and(|f| f.is_empty()) {
            // no row is in the conditions read so far, so none is in these as well
            read.push(at);
            continue;
        }
        let left = most.saturating_sub(pages);
        let listed = match (*index).relam {
            pg_sys::BTREE_AM_OID => btree(
                root,
                rel,
                relid,
                index,
                clauses,
                share,
                found.as_deref(),
                left,
                &mut room,
            ),
            _ => gin(root, rel, index, clauses, found.as_deref(), left, &mut room),
        };
        let Some(listed) = listed else {
            stopped = true;
            limited |= crate::budget::left() < left;
            break;
        };
        pages += listed.pages;
        lists.push((at, listed.pages));
        pending = pending.max(listed.pending);
        found = Some(listed.addresses);
        read.push(at);
    }
    noted(
        rel, relid, parts, order, &lists, most, stopped, limited, &room,
    );
    #[cfg(any(test, feature = "pg_test"))]
    tests::note(
        false,
        read.iter().map(|&at| (*parts[at].0).indexoid).collect(),
        (!stopped).then_some(pages),
    );
    let placed = (*rel).tuples - pending;
    if read.len() < 2 || placed < 1.0 {
        return None;
    }
    let rows = found.map_or(0, |f| f.len()) as f64;
    Some(Together {
        parts: read,
        share: (rows / placed).clamp(0.0, 1.0),
        pages,
        on_entries: false,
        every_leaf: false,
    })
}

unsafe fn name(relid: pg_sys::Oid) -> String {
    let n = pg_sys::get_rel_name(relid);
    if n.is_null() {
        relid.to_u32().to_string()
    } else {
        std::ffi::CStr::from_ptr(n).to_string_lossy().into_owned()
    }
}

/// The pages of the backend's buffers pinned so far, shared and local, found in them or read in.
fn pinned() -> i64 {
    let usage = unsafe { &*std::ptr::addr_of!(pg_sys::pgBufferUsage) };
    usage.shared_blks_hit + usage.shared_blks_read + usage.local_blks_hit + usage.local_blks_read
}

/// The key the B-tree `index` of `rel` scans for one condition it holds against a constant: an
/// equality or a bound, a list of values, or a test for NULL. None for any other condition.
unsafe fn scan_key(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    ri: *mut pg_sys::RestrictInfo,
) -> Option<pg_sys::ScanKeyData> {
    let clause = (*ri).clause as *mut pg_sys::Node;
    if (*ri).pseudoconstant || clause.is_null() {
        return None;
    }
    let mut key = pg_sys::ScanKeyData::default();
    match (*clause).type_ {
        pg_sys::NodeTag::T_OpExpr => {
            let op = clause as *mut pg_sys::OpExpr;
            let args = cells((*op).args);
            if args.len() != 2 {
                return None;
            }
            let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
            let (at, other, opno) = if let Some(at) = column_of(rel, index, l) {
                (at, r, (*op).opno)
            } else {
                let at = column_of(rel, index, r)?;
                (at, l, pg_sys::get_commutator((*op).opno))
            };
            if opno == pg_sys::InvalidOid || !compares_as(index, at, (*op).inputcollid) {
                return None;
            }
            let (number, right) = strategy(opno, *(*index).opfamily.add(at))?;
            let value = constant(root, other)?;
            if (*value).constisnull {
                return None;
            }
            pg_sys::ScanKeyEntryInitialize(
                &mut key,
                0,
                (at + 1) as pg_sys::AttrNumber,
                number as pg_sys::StrategyNumber,
                right,
                (*op).inputcollid,
                pg_sys::get_opcode(opno),
                (*value).constvalue,
            );
        }
        pg_sys::NodeTag::T_ScalarArrayOpExpr => {
            let op = clause as *mut pg_sys::ScalarArrayOpExpr;
            let args = cells((*op).args);
            if !(*op).useOr || args.len() != 2 {
                return None;
            }
            let at = column_of(rel, index, args[0] as *mut pg_sys::Node)?;
            if !compares_as(index, at, (*op).inputcollid) {
                return None;
            }
            let (number, right) = strategy((*op).opno, *(*index).opfamily.add(at))?;
            if number != pg_sys::BTEqualStrategyNumber as i32 {
                return None;
            }
            let value = constant(root, args[1] as *mut pg_sys::Node)?;
            if (*value).constisnull {
                return None;
            }
            pg_sys::ScanKeyEntryInitialize(
                &mut key,
                pg_sys::SK_SEARCHARRAY as i32,
                (at + 1) as pg_sys::AttrNumber,
                number as pg_sys::StrategyNumber,
                right,
                (*op).inputcollid,
                pg_sys::get_opcode((*op).opno),
                (*value).constvalue,
            );
        }
        pg_sys::NodeTag::T_NullTest => {
            let test = clause as *mut pg_sys::NullTest;
            if (*test).nulltesttype != pg_sys::NullTestType::IS_NULL || (*test).argisrow {
                return None;
            }
            let at = column_of(rel, index, (*test).arg as *mut pg_sys::Node)?;
            pg_sys::ScanKeyEntryInitialize(
                &mut key,
                (pg_sys::SK_ISNULL | pg_sys::SK_SEARCHNULL) as i32,
                (at + 1) as pg_sys::AttrNumber,
                0,
                pg_sys::InvalidOid,
                pg_sys::InvalidOid,
                pg_sys::InvalidOid,
                pg_sys::Datum::from(0usize),
            );
        }
        _ => return None,
    }
    Some(key)
}

/// The addresses of the rows the B-tree `index` of `rel`, a table of oid `relid`, holds for
/// `clauses`, of those in `among` where they are given, read by the B-tree's own scan: its
/// descents and the leaves of its blocks, and no table page. None where a condition is not one the
/// scan takes, where `share` of the index's pages would pass `most`, or it or a descent would pass
/// what is left of the statement's planning-read budget, or where the scan's pages pass either;
/// none as well where the addresses held, `among` with this index's, pass `room`. The pages the
/// scan read are drawn on the budget once it stops.
#[allow(clippy::too_many_arguments)]
unsafe fn btree(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    relid: pg_sys::Oid,
    index: *mut pg_sys::IndexOptInfo,
    clauses: &[*mut pg_sys::RestrictInfo],
    share: f64,
    among: Option<&[u64]>,
    most: u32,
    room: &mut Room,
) -> Option<Listed> {
    let share_of_pages = share * (*index).pages as f64;
    let descent = (*index).tree_height.max(0) as f64 + 1.0;
    if share_of_pages > most as f64
        || !crate::budget::fits((*index).indexoid, share_of_pages.max(descent))
    {
        return None;
    }
    let found = among.map_or(0, <[u64]>::len) as f64;
    let mut keys = Vec::with_capacity(clauses.len());
    for &ri in clauses {
        keys.push(scan_key(root, rel, index, ri)?);
    }
    if keys.is_empty() {
        return None;
    }
    keys.sort_by_key(|k| k.sk_attno);
    let heap = pg_sys::relation_open(relid, pg_sys::NoLock as pg_sys::LOCKMODE);
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let scan = pg_sys::index_beginscan(
        heap,
        rel_index,
        std::ptr::addr_of_mut!(pg_sys::SnapshotAnyData),
        null_mut(),
        keys.len() as i32,
        0,
    );
    pg_sys::index_rescan(scan, keys.as_mut_ptr(), keys.len() as i32, null_mut(), 0);
    let left = crate::budget::left();
    let start = pinned();
    let mut addresses = Vec::new();
    let (mut passed, mut over) = (false, false);
    loop {
        let tid = pg_sys::index_getnext_tid(scan, pg_sys::ScanDirection::ForwardScanDirection);
        if pinned() - start > most.min(left) as i64 {
            (passed, over) = (true, true);
            break;
        }
        if tid.is_null() {
            break;
        }
        let block = (*tid).ip_blkid;
        let a = address(
            ((block.bi_hi as u32) << 16) | block.bi_lo as u32,
            (*tid).ip_posid,
        );
        if among.is_none_or(|among| among.binary_search(&a).is_ok()) {
            if !room.fits(found + addresses.len() as f64 + 1.0) {
                passed = true;
                break;
            }
            addresses.push(a);
        }
    }
    let pages = (pinned() - start) as u32;
    crate::budget::spend(pages as u64, over && left < most, left, || {
        crate::budget::index_name((*index).indexoid)
    });
    pg_sys::index_endscan(scan);
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    pg_sys::relation_close(heap, pg_sys::NoLock as pg_sys::LOCKMODE);
    if passed {
        return None;
    }
    addresses.sort_unstable();
    addresses.dedup();
    Some(Listed {
        addresses,
        pending: 0.0,
        pages,
    })
}

/// The addresses of the rows the GIN `index` of `rel` holds for `clauses`, of those in `among`
/// where they are given: for each condition, those its consistent function keeps. None where a
/// condition is not one the GIN is asked against a constant on its column, where its function
/// would need every row, where the read would read more than `most` pages, or where the addresses
/// held, `among` with this index's, would pass `room`.
unsafe fn gin(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    clauses: &[*mut pg_sys::RestrictInfo],
    among: Option<&[u64]>,
    most: u32,
    room: &mut Room,
) -> Option<Listed> {
    let asked = on_first_column(root, rel, index, &[]);
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let listed = (|| {
        let mut gin = Gin::open(rel_index, most)?;
        let pending = gin.pending()?;
        let mut found: Option<Vec<u64>> = None;
        for &clause in clauses {
            let c = asked.iter().find(|c| c.clause == clause)?;
            let held = among.map_or(0, <[u64]>::len) + found.as_ref().map_or(0, Vec::len);
            let within = found.as_deref().or(among);
            let kept = gin.kept(c.strategy, (*c.value).constvalue, within, held, room)?;
            found = Some(kept);
        }
        Some(Listed {
            addresses: found?,
            pending,
            pages: gin.pages,
        })
    })();
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    listed
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
pub(crate) mod tests {
    use pgrx::prelude::*;
    use std::cell::RefCell;
    use std::ffi::CStr;

    /// A read of the rows in all of a relation's conditions: whether they were tested on one
    /// index's own entries, the indexes read, and the pages read; none where it stopped before
    /// every index was read.
    type Noted = (bool, Vec<pg_sys::Oid>, Option<u32>);

    thread_local! {
        static NOTED: RefCell<Vec<Noted>> = const { RefCell::new(Vec::new()) };
    }

    /// Notes a read of the rows in all of a relation's conditions.
    pub(crate) fn note(on_entries: bool, indexes: Vec<pg_sys::Oid>, pages: Option<u32>) {
        NOTED.with(|n| n.borrow_mut().push((on_entries, indexes, pages)));
    }

    /// Each read made while `query` was planned: whether on one index's own entries, the indexes
    /// read, by name, and the pages read; none where it stopped before every index was read.
    fn reads(query: &str) -> Vec<(bool, Vec<String>, Option<u32>)> {
        NOTED.with(|n| n.borrow_mut().clear());
        Spi::run(&format!("EXPLAIN {query}")).unwrap_or_else(|e| panic!("{query}: {e}"));
        NOTED.with(|n| {
            n.borrow()
                .iter()
                .map(|(on_entries, indexes, pages)| {
                    let names = indexes
                        .iter()
                        .map(|&oid| unsafe {
                            CStr::from_ptr(pg_sys::get_rel_name(oid))
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect();
                    (*on_entries, names, *pages)
                })
                .collect()
        })
    }

    fn count(sql: &str) -> f64 {
        Spi::get_one::<i64>(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .unwrap() as f64
    }

    fn relpages(table: &str) -> u32 {
        Spi::get_one::<i32>(&format!(
            "SELECT relpages FROM pg_class WHERE relname = '{table}'"
        ))
        .unwrap()
        .unwrap() as u32
    }

    /// The rows the planner gives the scan of `table` in `query`'s plan.
    fn planned(query: &str, table: &str) -> f64 {
        let node = format!("on {table}");
        let lines = crate::tests::texts(&format!("EXPLAIN {query}"));
        let line = lines
            .iter()
            .find(|l| l.contains(&node))
            .unwrap_or_else(|| panic!("no scan of {table}: {lines:?}"));
        line.split(" rows=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("no estimate in {line}"))
    }

    /// 30,000 parts in 60 categories, about one in twenty named as a helmet. Three helmets in four
    /// are in category 27, with about one in two hundred of the other parts; the others lie in turn
    /// over the other 59. The word "bar" is in one part in seven, whatever its category. A B-tree on
    /// the category and the part, and a GIN on the name's words.
    fn parts() {
        Spi::run(
            "SELECT setseed(0.41); \
             CREATE TABLE parts AS \
             SELECT g AS part_num, \
                    CASE WHEN h AND random() < 0.75 THEN 27 \
                         WHEN random() < 0.005 THEN 27 \
                         ELSE g % 59 + (CASE WHEN g % 59 >= 27 THEN 1 ELSE 0 END) \
                    END AS part_cat_id, \
                    concat_ws(' ', CASE WHEN h THEN 'helmet' END, \
                                   CASE WHEN random() < 1.0 / 7 THEN 'bar' END, \
                                   'w' || (g % 991)) AS name \
             FROM (SELECT g, random() < 0.05 AS h FROM generate_series(1, 30000) g) s; \
             CREATE INDEX parts_cat ON parts (part_cat_id, part_num); \
             CREATE INDEX parts_words ON parts USING gin (to_tsvector('simple', name)); \
             ANALYZE parts",
        )
        .unwrap();
    }

    const WORDS: &str = "to_tsvector('simple', name)";

    #[pg_test]
    fn two_conditions_one_mostly_inside_the_other_are_counted_by_the_rows_in_both() {
        parts();
        let cond = format!("part_cat_id = 27 AND {WORDS} @@ 'helmet'::tsquery");
        let query = format!("SELECT part_num FROM parts WHERE {cond}");
        let alone = planned(&query, "parts");
        Spi::run("CREATE INDEX parts_order ON parts USING surveyor (part_num)").unwrap();
        let counted = count(&format!("SELECT count(*) FROM parts WHERE {cond}"));
        let helmets = count(&format!(
            "SELECT count(*) FROM parts WHERE {WORDS} @@ 'helmet'::tsquery"
        ));
        // most helmets are in the category, where the planner takes them to be spread
        assert!(counted > 0.6 * helmets, "{counted} of {helmets}");
        assert!(
            alone < counted / 10.0,
            "{alone} planned alone, {counted} counted"
        );
        assert_eq!(planned(&query, "parts"), counted, "planned alone {alone}");
        let reads = reads(&query);
        assert_eq!(reads.len(), 1, "{reads:?}");
        let (on_entries, indexes, pages) = &reads[0];
        assert!(!on_entries, "{reads:?}");
        assert_eq!(indexes.len(), 2, "{reads:?}");
        assert!(
            pages.is_some_and(|p| p <= relpages("parts")),
            "{reads:?}, {} pages",
            relpages("parts")
        );
    }

    #[pg_test]
    fn two_conditions_spread_over_each_other_are_counted_by_the_rows_in_both() {
        parts();
        Spi::run("CREATE INDEX parts_order ON parts USING surveyor (part_num)").unwrap();
        // on the category and the part, then on the category alone, its parts in posting lists
        for index in ["parts_cat", "parts_cat_only"] {
            if index == "parts_cat_only" {
                Spi::run(
                    "DROP INDEX parts_cat; CREATE INDEX parts_cat_only ON parts (part_cat_id)",
                )
                .unwrap();
            }
            for (cat, word) in [(27, "bar"), (12, "bar"), (40, "helmet")] {
                let cond = format!("part_cat_id = {cat} AND {WORDS} @@ '{word}'::tsquery");
                let query = format!("SELECT part_num FROM parts WHERE {cond}");
                let counted = count(&format!("SELECT count(*) FROM parts WHERE {cond}"));
                assert!(counted > 0.0, "{cond}");
                assert_eq!(planned(&query, "parts"), counted, "{index}: {cond}");
                let reads = reads(&query);
                assert!(
                    reads.len() == 1 && !reads[0].0 && reads[0].1.contains(&index.to_string()),
                    "{index}: {reads:?}"
                );
            }
        }
    }

    #[pg_test]
    fn a_trigram_pattern_counts_the_rows_holding_every_trigram_it_asks_for() {
        // 30,000 names in 10 lanes: in lane 3, one in seven holds "brick" and one in seven holds
        // "bric" and "ick" apart; elsewhere one in 53 holds "brick"
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_trgm; \
             CREATE TABLE patterned AS \
             SELECT g AS id, g % 10 AS lane, \
                    CASE WHEN g % 10 = 3 AND g % 7 = 0 THEN 'red brick' \
                         WHEN g % 10 = 3 AND g % 7 = 1 THEN 'bric ick' \
                         WHEN g % 10 <> 3 AND g % 53 = 0 THEN 'brick ' || g % 89 \
                         ELSE 'plate ' || g % 97 END AS name \
             FROM generate_series(1, 30000) g; \
             CREATE INDEX patterned_lane ON patterned (lane, id); \
             CREATE INDEX patterned_trgm ON patterned USING gin (name gin_trgm_ops); \
             CREATE INDEX patterned_order ON patterned USING surveyor (id); \
             ANALYZE patterned",
        )
        .unwrap();
        let cond = "lane = 3 AND name ILIKE '%brick%'";
        let held = count(
            "SELECT count(*) FROM patterned WHERE lane = 3 AND show_trgm(name) @> \
                 ARRAY(SELECT substr('brick', i, 3) FROM generate_series(1, 3) i)",
        );
        let counted = count(&format!("SELECT count(*) FROM patterned WHERE {cond}"));
        // the rows holding every trigram of the pattern, more than hold the pattern
        assert!(
            held > 1.5 * counted,
            "{held} holding every trigram, {counted} counted"
        );
        let query = format!("SELECT id FROM patterned WHERE {cond}");
        assert_eq!(planned(&query, "patterned"), held, "{counted} counted");
    }

    /// 40,000 rows of a narrow table, each in one of 4 lanes and named "plain" in lanes 0 to 2 but
    /// for every fifth row, and "lane" otherwise, with a B-tree on the lane and a digest of the row
    /// that is wider than the row itself, and a GIN on the name's words.
    fn narrow() {
        Spi::run(
            "CREATE TABLE narrow AS \
             SELECT g AS id, (g % 4)::smallint AS lane, \
                    CASE WHEN g % 4 <> 3 AND g % 5 <> 0 THEN 'plain' ELSE 'lane' END AS name \
             FROM generate_series(1, 40000) g; \
             CREATE INDEX narrow_lane ON narrow (lane, (md5(id::text) || md5(name))) \
                 WITH (deduplicate_items = off); \
             CREATE INDEX narrow_words ON narrow USING gin (to_tsvector('simple', name)); \
             ANALYZE narrow",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_pair_whose_rows_would_pass_the_pages_of_the_table_keeps_each_indexs_share() {
        narrow();
        let lanes = "lane IN (0, 1, 2)";
        let word = format!("{WORDS} @@ 'plain'::tsquery");
        let query = |cond: &str| format!("SELECT id FROM narrow WHERE {cond}");
        let both = format!("{lanes} AND {word}");
        let index_pages = relpages("narrow_lane");
        assert!(
            0.7 * index_pages as f64 > relpages("narrow") as f64,
            "{index_pages} pages of the lane key, {} of the table",
            relpages("narrow")
        );
        Spi::run("CREATE INDEX narrow_order ON narrow USING surveyor (id)").unwrap();
        let tuples = count("SELECT reltuples::bigint FROM pg_class WHERE relname = 'narrow'");
        let each = planned(&query(lanes), "narrow") * planned(&query(&word), "narrow") / tuples;
        let estimate = planned(&query(&both), "narrow");
        let counted = count(&format!("SELECT count(*) FROM narrow WHERE {both}"));
        assert!(
            (estimate - each).abs() <= 1.0,
            "{estimate} planned, {each} from each index"
        );
        assert!(
            (estimate - counted).abs() > 10.0,
            "{estimate} planned, {counted} counted"
        );
        assert_eq!(
            reads(&query(&both)),
            vec![(false, Vec::<String>::new(), None)]
        );
    }

    /// 60,000 rows: half of them marked `a`, half `b`, four in five of the `a` rows among the `b`
    /// rows; in five slots; and named "brick" in three tenths of them and "plate" in three tenths,
    /// a tenth both, every one of those in slot 2. A B-tree each on `a`, `b` and the slot, and a
    /// GIN on the name's words.
    fn paired() {
        Spi::run(
            "CREATE TABLE paired AS \
             SELECT g AS id, g % 2 AS a, \
                    CASE WHEN g % 10 IN (1, 3, 5, 7, 8) THEN 1 ELSE 0 END AS b, \
                    g % 5 AS slot, \
                    concat_ws(' ', CASE WHEN g % 10 IN (0, 1, 2) THEN 'brick' END, \
                                   CASE WHEN g % 10 IN (2, 3, 4) THEN 'plate' END, \
                                   'w' || g % 97) AS name \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX paired_a ON paired (a); \
             CREATE INDEX paired_b ON paired (b); \
             CREATE INDEX paired_slot ON paired (slot); \
             CREATE INDEX paired_words ON paired USING gin (to_tsvector('simple', name)); \
             ANALYZE paired; \
             CREATE INDEX paired_order ON paired USING surveyor (id)",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_pair_whose_rows_would_pass_work_mem_keeps_each_indexs_share() {
        paired();
        let query = |cond: &str| format!("SELECT id FROM paired WHERE {cond}");
        let tuples = count("SELECT reltuples::bigint FROM pg_class WHERE relname = 'paired'");
        let words = format!("{WORDS} @@ 'brick & plate'::tsquery");
        // two B-trees of 30,000 rows each; a GIN whose words list 18,000 rows each, measured at
        // fewer rows than 64kB holds addresses, and a B-tree
        for (first, second, indexes) in [
            ("a = 1", "b = 1".to_string(), ["paired_a", "paired_b"]),
            (
                "slot IN (2, 3)",
                words.clone(),
                ["paired_slot", "paired_words"],
            ),
        ] {
            let both = format!("{first} AND {second}");
            let counted = count(&format!("SELECT count(*) FROM paired WHERE {both}"));
            Spi::run("SET LOCAL work_mem = '64MB'").unwrap();
            assert_eq!(planned(&query(&both), "paired"), counted, "{both}");
            let read = reads(&query(&both));
            let mut matched = read.first().map(|r| r.1.clone()).unwrap_or_default();
            matched.sort();
            assert!(
                read.len() == 1 && !read[0].0 && matched == indexes && read[0].2.is_some(),
                "{both}: {read:?}"
            );
            // past the addresses 64kB holds, each index's share
            Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
            let each =
                planned(&query(first), "paired") * planned(&query(&second), "paired") / tuples;
            let estimate = planned(&query(&both), "paired");
            assert!(
                (estimate - each).abs() <= 1.0,
                "{both}: {estimate} planned, {each} from each index, {counted} counted"
            );
            assert!((estimate - counted).abs() > 0.2 * counted, "{both}");
            assert_eq!(
                reads(&query(&both)),
                vec![(false, Vec::<String>::new(), None)],
                "{both}"
            );
        }
        // the GIN's words' rows pass what 64kB holds; the share it measures does not
        let brick = count(&format!(
            "SELECT count(*) FROM paired WHERE {WORDS} @@ 'brick'::tsquery"
        ));
        assert!(brick > 8192.0, "{brick}");
        assert!(planned(&query(&words), "paired") < 8192.0);
    }

    /// The height of the B-tree `index`, its root's level.
    fn height(index: &str) -> u32 {
        Spi::run("CREATE EXTENSION IF NOT EXISTS pageinspect").unwrap();
        Spi::get_one::<i64>(&format!("SELECT level FROM bt_metap('{index}')"))
            .unwrap()
            .unwrap() as u32
    }

    /// The leaves of the B-tree `index`.
    fn leaves(index: &str) -> u32 {
        Spi::run("CREATE EXTENSION IF NOT EXISTS pageinspect").unwrap();
        Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM bt_multi_page_stats('{index}', 1, -1) WHERE type = 'l'"
        ))
        .unwrap()
        .unwrap() as u32
    }

    /// 60,000 entries of a log, 100 a day over 600 days, each in one of 7 shops; four in five of
    /// the entries of days 100 to 399 in shop 3. A B-tree on the shop and the entry.
    fn logbook() {
        Spi::run(
            "CREATE TABLE logbook AS \
             SELECT g AS id, g / 100 AS day, \
                    CASE WHEN g / 100 BETWEEN 100 AND 399 AND g % 10 < 8 THEN 3 ELSE g % 7 END AS shop \
             FROM generate_series(0, 59999) g; \
             CREATE INDEX logbook_shop ON logbook (shop, id); \
             ANALYZE logbook",
        )
        .unwrap();
    }

    const WIDE: &str = "day BETWEEN 100 AND 399 AND shop = 3";
    const NARROW: &str = "day BETWEEN 200 AND 202 AND shop = 3";

    fn on_logbook(cond: &str) -> String {
        format!("SELECT id FROM logbook WHERE {cond}")
    }

    #[pg_test]
    fn a_condition_on_a_key_column_of_an_index_of_one_width_is_tested_on_five_of_its_leaves() {
        logbook();
        Spi::run(
            "CREATE INDEX logbook_day_shop ON logbook (day, shop) WITH (deduplicate_items = off); \
             ANALYZE logbook; \
             CREATE INDEX logbook_order ON logbook USING surveyor (id)",
        )
        .unwrap();
        let counted = count(&format!("SELECT count(*) FROM logbook WHERE {WIDE}"));
        let estimate = planned(&on_logbook(WIDE), "logbook");
        assert!(
            (estimate - counted).abs() <= 0.1 * counted,
            "{estimate} planned, {counted} counted"
        );
        // the descent, and one from it to each of the block's first and last leaves and three
        // between them
        let most = 2 + 5 * height("logbook_day_shop");
        let read = reads(&on_logbook(WIDE));
        assert!(
            read.len() == 1
                && read[0].0
                && read[0].1 == ["logbook_day_shop", "logbook_shop"]
                && read[0].2.is_some_and(|p| p <= most),
            "{read:?}, at most {most} pages"
        );
        // a block on one or two leaves, read whole
        let counted = count(&format!("SELECT count(*) FROM logbook WHERE {NARROW}"));
        assert_eq!(planned(&on_logbook(NARROW), "logbook"), counted);
    }

    #[pg_test]
    fn a_condition_on_a_column_an_index_only_includes_is_tested_on_every_leaf_of_its_block() {
        logbook();
        Spi::run(
            "CREATE INDEX logbook_day ON logbook (day) INCLUDE (shop); ANALYZE logbook; \
             CREATE INDEX logbook_order ON logbook USING surveyor (id)",
        )
        .unwrap();
        for cond in [WIDE, NARROW] {
            let counted = count(&format!("SELECT count(*) FROM logbook WHERE {cond}"));
            assert_eq!(planned(&on_logbook(cond), "logbook"), counted, "{cond}");
        }
        // every leaf of the block, half the index's
        let read = reads(&on_logbook(WIDE));
        let half = leaves("logbook_day") / 2;
        assert!(
            read.len() == 1 && read[0].0 && read[0].2.is_some_and(|p| p >= half),
            "{read:?}, {half} leaves"
        );
    }

    #[pg_test]
    fn an_expression_another_index_holds_is_matched_by_addresses_not_computed_on_carried_columns() {
        parts();
        Spi::run(
            "DROP INDEX parts_cat; \
             CREATE INDEX parts_cat_named ON parts (part_cat_id, part_num) INCLUDE (name); \
             ANALYZE parts; \
             CREATE INDEX parts_order ON parts USING surveyor (part_num)",
        )
        .unwrap();
        // the category key carries the name, and the words GIN holds the words of it as its key
        let cond = format!("part_cat_id = 27 AND {WORDS} @@ 'helmet'::tsquery");
        let query = format!("SELECT part_num FROM parts WHERE {cond}");
        let counted = count(&format!("SELECT count(*) FROM parts WHERE {cond}"));
        assert_eq!(planned(&query, "parts"), counted);
        let read = reads(&query);
        assert!(
            read.len() == 1 && !read[0].0 && read[0].1.len() == 2,
            "{read:?}"
        );
    }

    #[pg_test]
    fn a_name_numbered_together_is_counted_on_every_leaf_where_a_few_leaves_miss_it() {
        // 40,000 parts in 10 categories; in category 5, the torsos are its parts numbered from a
        // quarter to two fifths of the way through, and no other part of it is one
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_trgm; \
             CREATE TABLE figures AS \
             SELECT g AS id, g % 10 AS cat, lpad(g::text, 6, '0') AS num, \
                    CASE WHEN g % 10 = 5 AND g BETWEEN 10000 AND 16000 THEN 'red torso ' ELSE 'plain ' END \
                        || (g % 89) AS name \
             FROM generate_series(1, 40000) g; \
             CREATE INDEX figures_cat ON figures (cat, num) INCLUDE (name); \
             CREATE INDEX figures_trgm ON figures USING gin (name gin_trgm_ops); \
             ANALYZE figures; \
             CREATE INDEX figures_order ON figures USING surveyor (id)",
        )
        .unwrap();
        // the category key stores the name the pattern compares
        let cond = "cat = 5 AND name ILIKE '%torso%'";
        let query = format!("SELECT id FROM figures WHERE {cond}");
        let counted = count(&format!("SELECT count(*) FROM figures WHERE {cond}"));
        assert!(counted > 500.0, "{counted}");
        assert_eq!(planned(&query, "figures"), counted);
        let read = reads(&query);
        assert!(
            read.len() == 1 && read[0].0 && read[0].1 == ["figures_cat", "figures_trgm"],
            "{read:?}"
        );
    }

    #[pg_test]
    fn an_expression_a_key_stores_is_tested_on_its_stored_value_without_the_columns_it_is_computed_from(
    ) {
        // 60,000 orders every 17 minutes from 2020, each by one of 600 buyers; a B-tree on the buyer
        // and the order's month, keeping no instant, and one on the month and the instant
        Spi::run(
            "CREATE TABLE orders AS \
             SELECT g AS id, g % 600 AS buyer, timestamp '2020-01-01' + g * interval '17 minutes' AS at \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX orders_buyer_month ON orders (buyer, (extract(month FROM at)::smallint), id); \
             CREATE INDEX orders_month_at ON orders ((extract(month FROM at)::smallint), at); \
             ANALYZE orders; \
             CREATE INDEX orders_order ON orders USING surveyor (id)",
        )
        .unwrap();
        let cond = "buyer BETWEEN 100 AND 159 AND extract(month FROM at)::smallint = 12";
        let query = format!("SELECT id FROM orders WHERE {cond}");
        let counted = count(&format!("SELECT count(*) FROM orders WHERE {cond}"));
        let estimate = planned(&query, "orders");
        assert!(
            (estimate - counted).abs() <= 0.1 * counted,
            "{estimate} planned, {counted} counted"
        );
        let read = reads(&query);
        assert!(
            read.len() == 1 && read[0].0 && read[0].1 == ["orders_buyer_month", "orders_month_at"],
            "{read:?}"
        );
    }

    #[pg_test]
    fn a_block_whose_leaves_hold_different_rows_is_read_whole() {
        // 60,000 rows in 10 lanes and 13 kinds, a key of three integers, each row beside a note so
        // that the table holds several times the pages its keys' reads take together; then 20,000
        // more in lane 3, of kind 7, among its entries at random, splitting its leaves in halves
        Spi::run(
            "CREATE TABLE lanes AS SELECT g AS id, g % 10 AS lane, g % 13 AS kind, \
                    repeat('x', 200) AS note \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX lanes_lane ON lanes (lane, kind, id); \
             CREATE INDEX lanes_kind ON lanes (kind, id); \
             ANALYZE lanes; \
             CREATE INDEX lanes_order ON lanes USING surveyor (id)",
        )
        .unwrap();
        let cond = "lane BETWEEN 2 AND 4 AND kind = 7";
        let query = format!("SELECT id FROM lanes WHERE {cond}");
        let most = 2 + 5 * height("lanes_lane");
        // as built, five leaves
        let read = reads(&query);
        assert!(
            read.len() == 1 && read[0].0 && read[0].2.is_some_and(|p| p <= most),
            "{read:?}, at most {most} pages"
        );
        Spi::run(
            "SELECT setseed(0.17); \
             INSERT INTO lanes SELECT (random() * 59999)::int, 3, 7, repeat('x', 200) \
             FROM generate_series(1, 20000); \
             ANALYZE lanes",
        )
        .unwrap();
        let counted = count(&format!("SELECT count(*) FROM lanes WHERE {cond}"));
        assert_eq!(planned(&query, "lanes"), counted);
        let read = reads(&query);
        assert!(
            read.len() == 1 && read[0].0 && read[0].2.is_some_and(|p| p > most),
            "{read:?}"
        );
    }
}
