// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a GiST of cubes measures of a box, of a distance from a point, or of both, read from the
//! union keys while planning.
//!
//! The box is a condition `&&` or `<@` against a constant cube. The distance is a bound, `<` or
//! `<=`, on the distance from a constant point: earthdistance's `earth_distance` on a GiST of its
//! earths, or the cube's `cube_distance` and `<->` against the same kind of point, written either
//! way round. Both are counted together, so a box beside its distance is counted once with it, and
//! each key across their edge takes the share of its entries they take (region.rs). A box or a
//! distance compared under a collation other than the one the key column keeps is left to the
//! planner, as PostgreSQL will not scan the GiST for it.
//!
//! From the root, the read follows the children whose union keys reach the question, level by
//! level, as a scan of the index does, and stops at the first level naming at least twenty of
//! them, or at the level over the leaves. Each child named there counts its share times the rows
//! under one page of the level below: the table's rows over that level's pages. Where the level
//! below is the leaves, the leaves of the children across the question's edge are read and their
//! entries inside it counted, since there a share stands for a single page. Where it is not, the
//! pages named are read, and each child counts the pages under it, since the pages a question names
//! hold more or fewer of them than the level's average.
//!
//! A GiST's pages are filled by its splits, unevenly, and in no order a few of them could stand
//! for, so the rows its leaves hold are read from the leaves themselves: every leaf wholly inside
//! the question, and under a child named above the level over the leaves across the question's
//! edge, every one of its own leaves. A child named there wholly inside counts its own pages under
//! it at the rows under one page of the level under them. Where the leaves to read cannot all be
//! read within the pages the read may read, the first, the quarter, the middle, the three-quarter
//! and the last of them, in the order the read names them, are read, and each leaf counts at the
//! entries those hold on average; and where even those cannot be, at the table's rows over the
//! leaves' pages.
//!
//! No page stores a level's pages. The level under a level whose every page was read is their
//! downlinks, so the level under the root is the root's downlinks, and a level of at most twenty
//! pages, all known, is read whole, the pages the question does not reach with it. Under the
//! deepest level counted so, the index's other pages are shared out over the levels down to the
//! leaves, each level the one above times the downlinks a page read on the one above holds on
//! average. Under the level the read stops at, the first, the middle and the last child it names
//! are read for theirs, with as many levels under them as bring the level below the stop nearest
//! the level above it times the downlinks a page read there holds. That level is the leaves where
//! the pages not yet counted are fewer than that estimate times the square root of one more than
//! those downlinks; the level under it is shared out the same way from the pages the stop names. A
//! root that is itself a leaf has its entries in the question counted.
//!
//! The read stops once its pages would pass the pages of the table, or what is left of the
//! statement's planning-read budget (`budget`), and the question is left to the planner. At the
//! stop, the children it has not read by then count as they would unread, and leaves whose pages
//! would pass what is left are not read.

use crate::conditions::{column_of, compares_as, constant, strategy, Held};
use crate::query::{bare, cells};
use crate::reading::{block_of, on_first_column, read, support_name, OnFirstColumn, PageCopy};
use crate::region::{Ball, Cube, Region};
use pgrx::pg_sys;
use pgrx::FromDatum;
use pgrx::IntoDatum;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;

const ROOT: pg_sys::BlockNumber = 0;
/// The children a level must name for the read to stop there.
pub(crate) const CHILDREN: usize = 20;
/// The most levels under the level below the stop that the pages not yet counted are shared over.
const LEVELS: i32 = 8;
const F_LEAF: u16 = 1 << 0;
const F_DELETED: u16 = 1 << 1;
const F_FOLLOW_RIGHT: u16 = 1 << 3;
const GIST_PAGE_ID: u16 = 0xFF81;
const CONSISTENT_PROC: i16 = 1;
const CUBE_CONSISTENT: &str = "g_cube_consistent";
/// `gist_cube_ops`'s `&&`.
const OVERLAP: i32 = 3;
/// `gist_cube_ops`'s `<@` and its older spelling `~`.
const CONTAINED_BY: [i32; 2] = [8, 14];
/// The B-tree operator family of `real` and `double precision`, and its `<` and `<=`.
const FLOAT_OPS: u32 = 1970;
const LESS: i32 = 1;
const LESS_EQUAL: i32 = 2;
const CUBE_DISTANCE: &str = "cube_distance";
/// earthdistance's domain of points on the earth, and its functions giving the earth's radius and
/// turning a distance over the earth into the chord under it and back.
const EARTH: &str = "earth";
const RADIUS: &CStr = c"earth";
const ARC_TO_CHORD: &CStr = c"gc_to_sec";
const CHORD_TO_ARC: &CStr = c"sec_to_gc";

/// What a read measured: the rows in the question; the depth of the level whose children it
/// counted; the pages it read; the pages of the level below the one it stopped at and whether they
/// were counted exactly; the deepest level counted exactly; the children named there, and the
/// shares added of those counted at the rows under one page of the level below; the leaves the
/// descent read; the children across the question's edge whose entries were counted, and their
/// entries inside it; the children wholly inside whose leaves were read whole, and their entries;
/// the children wholly inside counted at the entries five of their leaves hold, and those entries
/// on average; the children counted by their own pages under them, those of them whose own leaves
/// were read whole, those counted at five of their own leaves, those wholly inside, and the pages
/// taken for the level those lie on; and the pages read for leaves' entries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Measured {
    pub rows: f64,
    pub depth: u32,
    pub pages: u32,
    pub below: f64,
    pub exact: bool,
    pub exact_depth: u32,
    pub named: usize,
    pub share: f64,
    pub leaves_read: u32,
    pub counted: usize,
    pub entries: f64,
    pub whole: usize,
    pub whole_entries: f64,
    pub inside: usize,
    pub per_leaf: f64,
    pub own: usize,
    pub own_read: usize,
    pub own_sampled: usize,
    pub own_inside: usize,
    pub under: f64,
    pub leaf_pages: u32,
}

/// A child a level names: its block, whether the question's edge crosses it, the share of its
/// entries the question takes, and the WAL position of the page holding its downlink.
struct Named {
    block: pg_sys::BlockNumber,
    key: Cube,
    across: bool,
    lsn: u64,
}

impl Named {
    /// The share of the child's entries the question `region` takes.
    fn share(&self, region: &Region) -> f64 {
        if self.across {
            region.share(&self.key)
        } else {
            1.0
        }
    }
}

/// The pages a read has read, each kept once read, and the most it may read.
struct Pages {
    kept: HashMap<pg_sys::BlockNumber, PageCopy>,
    read: u32,
    most: u32,
}

impl Pages {
    /// Block `block` of `index`, read where it is not kept; None where reading it would pass the
    /// most the read may read, or what is left of the statement's planning-read budget.
    unsafe fn get(
        &mut self,
        index: pg_sys::Relation,
        block: pg_sys::BlockNumber,
    ) -> Option<&PageCopy> {
        if !self.kept.contains_key(&block) {
            if self.read >= self.most {
                return None;
            }
            let page = read(index, block)?;
            self.kept.insert(block, page);
            self.read += 1;
        }
        self.kept.get(&block)
    }

    /// Whether `n` pages more of `index` lie within the most the read may read, and within what is
    /// left of the statement's planning-read budget.
    unsafe fn room(&self, index: pg_sys::Relation, n: usize) -> bool {
        n <= self.most.saturating_sub(self.read) as usize
            && crate::budget::fits((*index).rd_id, n as f64)
    }
}

unsafe fn opaque(page: &PageCopy) -> &pg_sys::GISTPageOpaqueData {
    page.special::<pg_sys::GISTPageOpaqueData>()
}

unsafe fn is_leaf(page: &PageCopy) -> bool {
    opaque(page).flags & F_LEAF != 0
}

unsafe fn nsn(page: &PageCopy) -> u64 {
    let n = opaque(page).nsn;
    ((n.xlogid as u64) << 32) | n.xrecoff as u64
}

/// The live items of a page, each a cube and the block or heap address it points to. A row with no
/// value has a NULL key on its leaf, and a page whose rows all have none a NULL key above it: those
/// keys are None, since no box or distance takes such a row.
unsafe fn items(
    index: pg_sys::Relation,
    page: &PageCopy,
) -> Vec<(Option<Cube>, pg_sys::BlockNumber)> {
    (1..=page.last())
        .filter(|&o| !page.dead(o))
        .map(|o| {
            let tuple = page.item(o);
            let key = crate::measure::value(tuple, 1, (*index).rd_att).and_then(|k| Cube::of(k));
            (key, block_of(tuple))
        })
        .collect()
}

/// The entries of a leaf `page` with a key: the rows a question can take.
unsafe fn keyed(index: pg_sys::Relation, page: &PageCopy) -> f64 {
    items(index, page)
        .iter()
        .filter(|(key, _)| key.is_some())
        .count() as f64
}

/// The entries of a leaf `page` the question `region` takes.
unsafe fn taken(index: pg_sys::Relation, page: &PageCopy, region: &Region) -> f64 {
    items(index, page)
        .iter()
        .filter(|(key, _)| key.as_ref().is_some_and(|k| region.takes(k)))
        .count() as f64
}

/// The rows the GiST `index` of cubes measures in the question `region`, for a table of `tuples`
/// rows, reading at most `most` pages. None where the index's root is not a GiST page, or where the
/// read would read more, or more than the statement's planning-read budget has left.
pub(crate) unsafe fn rows(
    index: pg_sys::Relation,
    region: &Region,
    tuples: f64,
    most: u32,
) -> Option<Measured> {
    let all =
        pg_sys::RelationGetNumberOfBlocksInFork(index, pg_sys::ForkNumber::MAIN_FORKNUM) as f64;
    if most == 0 {
        return None;
    }
    let root = read(index, ROOT)?;
    if opaque(&root).gist_page_id != GIST_PAGE_ID {
        return None;
    }
    let mut pages = 1;
    if is_leaf(&root) {
        let inside = taken(index, &root, region);
        return Some(Measured {
            rows: inside,
            depth: 0,
            pages,
            below: 0.0,
            exact: true,
            exact_depth: 0,
            named: inside as usize,
            share: inside,
            leaves_read: 0,
            counted: 0,
            entries: inside,
            whole: 0,
            whole_entries: 0.0,
            inside: 0,
            per_leaf: 0.0,
            own: 0,
            own_read: 0,
            own_sampled: 0,
            own_inside: 0,
            under: 0.0,
            leaf_pages: 0,
        });
    }
    let mut level = vec![root];
    // the level read's pages taken as the level above times the downlinks a page read there
    // holds, and every level's at and above it
    let (mut level_pages, mut counted) = (1.0, 1.0);
    // the deepest level whose pages are counted exactly, and the pages of it and every level above
    let (mut exact_depth, mut exact_counted) = (0usize, 1.0);
    // the downlinks a page read at each depth holds
    let mut fanouts = Vec::new();
    let mut depth = 0usize;
    loop {
        let mut named = Vec::new();
        let mut every = Vec::new();
        for page in &level {
            let lsn = page.lsn();
            for (key, child) in items(index, page) {
                every.push((child, lsn));
                if let Some(key) = key.filter(|k| region.reaches(k)) {
                    named.push(Named {
                        block: child,
                        across: !region.holds(&key),
                        key,
                        lsn,
                    });
                }
            }
        }
        let downlinks = every.len();
        let fanout = downlinks as f64 / level.len() as f64;
        fanouts.push(fanout);
        let next = level_pages * fanout;
        let next_exact = exact_depth == depth && level.len() as f64 == level_pages;
        let rest = all - counted;
        let next_is_leaves = rest < (1.0 + fanout).sqrt() * next;
        let read_fanouts = &fanouts[exact_depth + 1..];
        let uncounted = all - exact_counted;
        // the rows the children named stand for, the level below the stop having `below` pages,
        // with the pages `at` read so far
        let finish = |below: f64, exact: bool, leaves_below: bool, mut at: Pages, leaves_read| {
            let t = tally(
                index,
                region,
                &named,
                tuples,
                below,
                leaves_below,
                counted,
                all,
                &mut at,
            );
            Measured {
                rows: t.rows,
                depth: depth as u32,
                pages: at.read,
                below,
                exact,
                exact_depth: if exact {
                    depth as u32 + 1
                } else {
                    exact_depth as u32
                },
                named: named.len(),
                share: t.share,
                leaves_read,
                counted: t.counted,
                entries: t.entries,
                whole: t.whole,
                whole_entries: t.whole_entries,
                inside: t.inside,
                per_leaf: t.per_leaf,
                own: t.own,
                own_read: t.own_read,
                own_sampled: t.own_sampled,
                own_inside: t.own_inside,
                under: t.under,
                leaf_pages: t.leaf_pages,
            }
        };
        if named.len() >= CHILDREN || next_is_leaves || named.is_empty() {
            let mut at = Pages {
                kept: HashMap::new(),
                read: pages,
                most,
            };
            if next_exact || named.is_empty() {
                return Some(finish(next, next_exact, next_is_leaves, at, 0));
            }
            if next_is_leaves {
                let below = shared(uncounted, read_fanouts, None, next);
                return Some(finish(below, read_fanouts.is_empty(), true, at, 0));
            }
            let (under, leaves) = sample(index, &named, &mut at)?;
            let below = shared(uncounted, read_fanouts, under, next);
            return Some(finish(below, false, leaves > 0, at, 0));
        }
        // the children named, and any page a split has put on a child's right since its parent
        // was read; every child, where the level below is counted exactly and has at most
        // `CHILDREN` pages, so that the level under it is counted exactly too
        let whole = next_exact && next <= CHILDREN as f64;
        let to_read: Vec<(pg_sys::BlockNumber, u64)> = if whole {
            every
        } else {
            named.iter().map(|n| (n.block, n.lsn)).collect()
        };
        let mut seen = HashSet::new();
        let mut children = Vec::new();
        let mut kept = HashMap::new();
        for (block, parent_lsn) in to_read {
            let mut block = block;
            while seen.insert(block) {
                if pages >= most {
                    return None;
                }
                let page = read(index, block)?;
                pages += 1;
                let o = opaque(&page);
                let (flags, right) = (o.flags, o.rightlink);
                let follow = flags & F_FOLLOW_RIGHT != 0 || parent_lsn < nsn(&page);
                if flags & F_DELETED == 0 {
                    kept.insert(block, page.clone());
                    children.push(page);
                }
                if !follow || right == pg_sys::InvalidBlockNumber {
                    break;
                }
                block = right;
            }
        }
        let leaves_read = children.iter().filter(|p| is_leaf(p)).count() as u32;
        if leaves_read > 0 || children.is_empty() {
            // the level read is the leaves
            let below = if next_exact {
                next
            } else {
                shared(uncounted, read_fanouts, None, next)
            };
            let at = Pages {
                kept,
                read: pages,
                most,
            };
            return Some(finish(below, next_exact, true, at, leaves_read));
        }
        if next_exact {
            exact_depth = depth + 1;
            exact_counted += next;
        }
        counted += next;
        level_pages = next;
        level = children;
        depth += 1;
    }
}

/// The downlinks a page of the level under the children `named` holds, from the first, the middle
/// and the last of them, and the leaves among them. The first is None where one is a leaf; the
/// whole is None where reading them would pass the pages `at` may read.
unsafe fn sample(
    index: pg_sys::Relation,
    named: &[Named],
    at: &mut Pages,
) -> Option<(Option<f64>, u32)> {
    let n = named.len();
    let mut picks = vec![0, n / 2, n - 1];
    picks.dedup();
    let (mut downlinks, mut leaves) = (0usize, 0u32);
    for &i in &picks {
        let page = at.get(index, named[i].block)?;
        if is_leaf(page) {
            leaves += 1;
        } else {
            downlinks += items(index, page).len();
        }
    }
    let under = (leaves == 0).then(|| downlinks as f64 / picks.len() as f64);
    Some((under, leaves))
}

/// What lies under a page: the entries of a leaf, inside the question where one is given, or the
/// downlinks of a page above the leaves with the WAL position of the page holding each; on the page
/// and any page a split has put on its right since its parent was read.
enum Under {
    Entries(f64),
    Downlinks(Vec<(pg_sys::BlockNumber, u64)>),
}

/// What lies under the page `block`, whose parent was read at WAL position `lsn`, counting a
/// leaf's entries inside `region` where it is given and all of them with a key otherwise; None
/// where reading it would pass the pages `at` may read.
unsafe fn under(
    index: pg_sys::Relation,
    region: Option<&Region>,
    block: pg_sys::BlockNumber,
    lsn: u64,
    at: &mut Pages,
) -> Option<Under> {
    let mut block = block;
    let mut seen = HashSet::new();
    let (mut entries, mut downlinks, mut leaf) = (0.0, Vec::new(), false);
    while seen.insert(block) {
        let page = at.get(index, block)?;
        let o = opaque(page);
        let (flags, right) = (o.flags, o.rightlink);
        let follow = flags & F_FOLLOW_RIGHT != 0 || lsn < nsn(page);
        if flags & F_DELETED == 0 {
            if is_leaf(page) {
                leaf = true;
                entries += match region {
                    Some(r) => taken(index, page, r),
                    None => keyed(index, page),
                };
            } else {
                let page_lsn = page.lsn();
                downlinks.extend(
                    items(index, page)
                        .into_iter()
                        .map(|(_, child)| (child, page_lsn)),
                );
            }
        }
        if !follow || right == pg_sys::InvalidBlockNumber {
            break;
        }
        block = right;
    }
    Some(if leaf {
        Under::Entries(entries)
    } else {
        Under::Downlinks(downlinks)
    })
}

/// The places of the first, the quarter, the middle, the three-quarter and the last of `n` things,
/// each once: every one of them where there are five or fewer.
fn picks(n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let mut p = vec![0, n / 4, n / 2, 3 * n / 4, n - 1];
    p.dedup();
    p
}

/// The entries the leaves `leaves` (each a block and the WAL position of the page naming it) hold
/// on average, from those at `picks`, counting inside `region` where it is given; None where one is
/// not a leaf, or reading them would pass the pages `at` may read.
unsafe fn sampled(
    index: pg_sys::Relation,
    region: Option<&Region>,
    leaves: &[(pg_sys::BlockNumber, u64)],
    at: &mut Pages,
) -> Option<f64> {
    let picked = picks(leaves.len());
    let mut entries = 0.0;
    for &i in &picked {
        let (block, lsn) = leaves[i];
        match under(index, region, block, lsn, at)? {
            Under::Entries(e) => entries += e,
            Under::Downlinks(_) => return None,
        }
    }
    (!picked.is_empty()).then(|| entries / picked.len() as f64)
}

/// The entries the leaves `leaves` (each a block and the WAL position of the page naming it) hold
/// together, every one read; None where one is not a leaf, or reading them would pass the pages
/// `at` may read.
unsafe fn held(
    index: pg_sys::Relation,
    leaves: &[(pg_sys::BlockNumber, u64)],
    at: &mut Pages,
) -> Option<f64> {
    if !at.room(index, leaves.len()) {
        return None;
    }
    let mut entries = 0.0;
    for &(block, lsn) in leaves {
        match under(index, None, block, lsn, at)? {
            Under::Entries(e) => entries += e,
            Under::Downlinks(_) => return None,
        }
    }
    Some(entries)
}

/// The rows children stand for: the rows; the shares added of the children counted at the rows
/// under one page of the level below; the children across the question's edge whose entries were
/// counted, and their entries inside it; the children wholly inside whose leaves were read whole,
/// and their entries; the children wholly inside counted at the entries five of their leaves hold,
/// and those entries on average; the children counted by their own pages under them, those of them
/// whose own leaves were read whole, those counted at five of their own leaves, those wholly
/// inside, and the pages taken for the level those lie on; and the pages read for leaves' entries.
#[derive(Default)]
struct Tally {
    rows: f64,
    share: f64,
    counted: usize,
    entries: f64,
    whole: usize,
    whole_entries: f64,
    inside: usize,
    per_leaf: f64,
    own: usize,
    own_read: usize,
    own_sampled: usize,
    own_inside: usize,
    under: f64,
    leaf_pages: u32,
}

/// The rows the children `named` stand for in a table of `tuples` rows, the level below them having
/// `below` pages and being the leaves where `leaves_below`; the levels down to theirs holding
/// `through` pages of the index's `all`. The pages are read through `at`, and a child left unread
/// there counts its share of the rows under one page of the level below: the table's rows over
/// that level's pages.
///
/// Where the level below is the leaves, a child across the question's edge counts its entries
/// inside the question, and a child wholly inside counts the entries its leaf holds: every leaf
/// named is read, where they all lie within the pages `at` may read. Otherwise the children wholly
/// inside count at the entries the first, the quarter, the middle, the three-quarter and the last
/// of them, in the order named, hold on average, read first; and those across, as far as the pages
/// left allow.
///
/// Where it is not, a child wholly inside counts the pages under it at the rows under one page of
/// the level under the level below. A child across the edge counts its share of the pages under
/// it, at the entries its own leaves hold: every such child's leaves read, where they all lie
/// within the pages `at` may read, and otherwise five of each one's taken the same way, on
/// average, until a child's five cannot be read; and past that, at the rows under one page of the
/// level under the level below.
#[allow(clippy::too_many_arguments)]
unsafe fn tally(
    index: pg_sys::Relation,
    region: &Region,
    named: &[Named],
    tuples: f64,
    below: f64,
    leaves_below: bool,
    through: f64,
    all: f64,
    at: &mut Pages,
) -> Tally {
    let per_page = if below > 0.0 { tuples / below } else { 0.0 };
    let mut t = Tally::default();
    if leaves_below {
        let inside: Vec<(pg_sys::BlockNumber, u64)> = named
            .iter()
            .filter(|n| !n.across)
            .map(|n| (n.block, n.lsn))
            .collect();
        let before = at.read;
        let every = at.room(index, named.len());
        let mut whole = Vec::with_capacity(inside.len());
        let mut per_leaf = None;
        if every {
            for &(block, lsn) in &inside {
                whole.push(match under(index, Some(region), block, lsn, at) {
                    Some(Under::Entries(e)) => Some(e),
                    _ => None,
                });
            }
        } else {
            per_leaf = sampled(index, Some(region), &inside, at);
        }
        t.leaf_pages = at.read - before;
        let mut whole = whole.into_iter();
        for n in named {
            if n.across {
                if let Some(Under::Entries(e)) = under(index, Some(region), n.block, n.lsn, at) {
                    t.rows += e;
                    t.counted += 1;
                    t.entries += e;
                    continue;
                }
            } else if let Some(Some(e)) = whole.next() {
                t.rows += e;
                t.whole += 1;
                t.whole_entries += e;
                continue;
            } else if let Some(e) = per_leaf {
                t.rows += e;
                t.inside += 1;
                t.per_leaf = e;
                continue;
            }
            let share = n.share(region);
            t.share += share;
            t.rows += share * per_page;
        }
        return t;
    }
    let mut got = Vec::with_capacity(named.len());
    for n in named {
        match under(index, Some(region), n.block, n.lsn, at) {
            Some(u) => got.push(Some(u)),
            None => break,
        }
    }
    got.resize_with(named.len(), || None);
    let downlinks: Vec<usize> = got
        .iter()
        .filter_map(|g| match g {
            Some(Under::Downlinks(d)) => Some(d.len()),
            _ => None,
        })
        .collect();
    let mean = downlinks.iter().sum::<usize>() as f64 / downlinks.len().max(1) as f64;
    let remaining = all - through - below;
    let under_pages = if downlinks.is_empty() || mean <= 1.0 || remaining <= 0.0 {
        None
    } else if remaining < (1.0 + mean).sqrt() * below * mean {
        Some(remaining)
    } else {
        Some(shared(remaining, &[], Some(mean), below * mean))
    };
    // the leaves of the children across the edge read whole where they all lie within the pages
    // left
    let own_leaves: usize = named
        .iter()
        .zip(&got)
        .filter(|(n, _)| n.across)
        .map(|(_, g)| match g {
            Some(Under::Downlinks(d)) => d.len(),
            _ => 0,
        })
        .sum();
    let every = at.room(index, own_leaves);
    let mut room = true;
    for (n, g) in named.iter().zip(&got) {
        match (g, under_pages) {
            (Some(Under::Downlinks(d)), Some(p)) if p > 0.0 && !n.across => {
                t.rows += d.len() as f64 * tuples / p;
                t.own += 1;
                t.own_inside += 1;
            }
            (Some(Under::Downlinks(d)), Some(p)) if p > 0.0 => {
                let share = n.share(region);
                let before = at.read;
                let entries = if every { held(index, d, at) } else { None };
                let per_leaf = match entries {
                    None if room => sampled(index, None, d, at),
                    _ => None,
                };
                t.leaf_pages += at.read - before;
                match (entries, per_leaf) {
                    (Some(e), _) => {
                        t.rows += share * e;
                        t.own_read += 1;
                    }
                    (None, Some(e)) => {
                        t.rows += share * d.len() as f64 * e;
                        t.own_sampled += 1;
                    }
                    (None, None) => {
                        room = false;
                        t.rows += share * d.len() as f64 * tuples / p;
                    }
                }
                t.own += 1;
            }
            (Some(Under::Entries(e)), _) if n.across => {
                t.rows += e;
                t.counted += 1;
                t.entries += e;
            }
            _ => {
                let share = n.share(region);
                t.share += share;
                t.rows += share * per_page;
            }
        }
    }
    t.under = under_pages.unwrap_or(0.0);
    t
}

/// The pages of the level below the stop. `uncounted` pages lie under the deepest level counted
/// exactly, and are shared out over the levels from there down: each level the one above times the
/// downlinks a page of the one above holds, `read` for the levels down to the stop, then `under` for
/// each level below it, with as many of those as bring the level below the stop nearest
/// `estimate`. With `under` None the level below the stop is the leaves.
fn shared(uncounted: f64, read: &[f64], under: Option<f64>, estimate: f64) -> f64 {
    // each level's pages over the first's, from the first level under the one counted exactly
    // down to the level below the stop
    let mut through = 1.0;
    let mut above = 1.0;
    for r in read {
        through *= r;
        above += through;
    }
    let Some(under) = under else {
        return uncounted / above * through;
    };
    if !under.is_finite() || under <= 1.0 {
        return estimate;
    }
    (1..=LEVELS)
        .map(|levels| {
            let below: f64 = (1..=levels).map(|j| through * under.powi(j)).sum();
            uncounted / (above + below) * through
        })
        .min_by(|a, b| {
            (a / estimate)
                .ln()
                .abs()
                .total_cmp(&(b / estimate).ln().abs())
        })
        .unwrap_or(estimate)
}

/// Whether `index` is a GiST of cubes over the whole table, on one key column.
unsafe fn of_cubes(index: *mut pg_sys::IndexOptInfo) -> bool {
    (*index).relam == pg_sys::GIST_AM_OID
        && (*index).indpred.is_null()
        && (*index).nkeycolumns == 1
        && support_name(index, CONSISTENT_PROC).as_deref() == Some(CUBE_CONSISTENT)
}

/// earthdistance's earths, where they are the values of an index: the earth's radius, and the
/// functions turning a distance over the earth into the chord under it and back.
struct Earth {
    radius: f64,
    arc_to_chord: pg_sys::Oid,
    chord_to_arc: pg_sys::Oid,
}

/// The type of the first key column of `index` of the base relation `rel`.
unsafe fn column_type(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
) -> Option<pg_sys::Oid> {
    let key = *(*index).indexkeys;
    if key != 0 {
        if (*root).simple_rte_array.is_null() {
            return None;
        }
        let rte = *(*root).simple_rte_array.add((*rel).relid as usize);
        return (!rte.is_null()).then(|| pg_sys::get_atttype((*rte).relid, key as i16));
    }
    let expression = *cells((*index).indexprs).first()?;
    Some(pg_sys::exprType(expression as *const pg_sys::Node))
}

/// The function `name` of `nargs` arguments of `double precision` in the schema `namespace`.
unsafe fn function_in(namespace: pg_sys::Oid, name: &CStr, nargs: i32) -> Option<pg_sys::Oid> {
    let schema = pg_sys::get_namespace_name(namespace);
    if schema.is_null() {
        return None;
    }
    let mut names = std::ptr::null_mut();
    names = pg_sys::lappend(names, pg_sys::makeString(schema) as *mut std::ffi::c_void);
    names = pg_sys::lappend(
        names,
        pg_sys::makeString(pg_sys::pstrdup(name.as_ptr())) as *mut std::ffi::c_void,
    );
    let args = [pg_sys::FLOAT8OID];
    let oid = pg_sys::LookupFuncName(names, nargs, args.as_ptr(), true);
    (oid != pg_sys::InvalidOid).then_some(oid)
}

/// The value the function `function` of `double precision` gives for `args`, folded while
/// planning.
unsafe fn folded(
    root: *mut pg_sys::PlannerInfo,
    function: pg_sys::Oid,
    args: &[f64],
) -> Option<f64> {
    let mut list = std::ptr::null_mut();
    for &a in args {
        let c = pg_sys::makeConst(
            pg_sys::FLOAT8OID,
            -1,
            pg_sys::InvalidOid,
            8,
            a.into_datum()?,
            false,
            true,
        );
        list = pg_sys::lappend(list, c as *mut std::ffi::c_void);
    }
    let call = pg_sys::makeFuncExpr(
        function,
        pg_sys::FLOAT8OID,
        list,
        pg_sys::InvalidOid,
        pg_sys::InvalidOid,
        pg_sys::CoercionForm::COERCE_EXPLICIT_CALL,
    );
    let out = constant(root, call as *mut pg_sys::Node)?;
    if (*out).constisnull || (*out).consttype != pg_sys::FLOAT8OID {
        return None;
    }
    f64::from_datum((*out).constvalue, false)
}

/// earthdistance's earths, where they are the values of `index` of the base relation `rel`: its
/// key column's type is the domain `earth` over the cube, and the earth's radius and functions are
/// in the domain's schema.
unsafe fn earth_of(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
) -> Option<Earth> {
    let kind = column_type(root, rel, index)?;
    let tuple = pg_sys::SearchSysCache1(
        pg_sys::SysCacheIdentifier::TYPEOID as i32,
        pg_sys::Datum::from(kind.to_u32()),
    );
    if tuple.is_null() {
        return None;
    }
    let form = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_type>(tuple);
    let is_earth = (*form).typtype as u8 == b'd'
        && (*form).typbasetype == *(*index).opcintype
        && CStr::from_ptr((*form).typname.data.as_ptr()).to_bytes() == EARTH.as_bytes();
    let namespace = (*form).typnamespace;
    pg_sys::ReleaseSysCache(tuple);
    if !is_earth {
        return None;
    }
    let radius = folded(root, function_in(namespace, RADIUS, 0)?, &[])?;
    Some(Earth {
        radius,
        arc_to_chord: function_in(namespace, ARC_TO_CHORD, 1)?,
        chord_to_arc: function_in(namespace, CHORD_TO_ARC, 1)?,
    })
}

/// A call or an operator: its function, its arguments, and the collation it compares them under.
struct Call {
    function: pg_sys::Oid,
    args: Vec<*mut pg_sys::Node>,
    collation: pg_sys::Oid,
}

/// The call or operator `node` is, where it is one.
unsafe fn call_of(node: *mut pg_sys::Node) -> Option<Call> {
    let node = bare(node);
    if node.is_null() {
        return None;
    }
    let (function, args, collation) = match (*node).type_ {
        pg_sys::NodeTag::T_FuncExpr => {
            let f = node as *mut pg_sys::FuncExpr;
            ((*f).funcid, (*f).args, (*f).inputcollid)
        }
        pg_sys::NodeTag::T_OpExpr => {
            let o = node as *mut pg_sys::OpExpr;
            (pg_sys::get_opcode((*o).opno), (*o).args, (*o).inputcollid)
        }
        _ => return None,
    };
    let args = cells(args)
        .into_iter()
        .map(|a| a as *mut pg_sys::Node)
        .collect();
    Some(Call {
        function,
        args,
        collation,
    })
}

/// The point a distance between the first column of `index` and a constant point is taken from,
/// where `node` is the cube's distance between them, either way round, under the collation the
/// column keeps.
unsafe fn distance_from(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    node: *mut pg_sys::Node,
) -> Option<Vec<f64>> {
    let Call {
        function,
        args,
        collation,
    } = call_of(node)?;
    if !compares_as(index, 0, collation) {
        return None;
    }
    let name = pg_sys::get_func_name(function);
    let consistent = pg_sys::get_opfamily_proc(
        *(*index).opfamily,
        *(*index).opcintype,
        *(*index).opcintype,
        CONSISTENT_PROC,
    );
    if name.is_null()
        || CStr::from_ptr(name).to_bytes() != CUBE_DISTANCE.as_bytes()
        || pg_sys::get_func_namespace(function) != pg_sys::get_func_namespace(consistent)
        || args.len() != 2
    {
        return None;
    }
    let other = if column_of(rel, index, args[0]) == Some(0) {
        args[1]
    } else if column_of(rel, index, args[1]) == Some(0) {
        args[0]
    } else {
        return None;
    };
    let value = constant(root, other)?;
    if (*value).constisnull {
        return None;
    }
    let point = Cube::of((*value).constvalue)?;
    (point.dim() == 3 && point.lo == point.hi).then_some(point.lo)
}

/// The number a constant of `real` or `double precision` holds.
unsafe fn number(root: *mut pg_sys::PlannerInfo, node: *mut pg_sys::Node) -> Option<f64> {
    let value = constant(root, node)?;
    if (*value).constisnull {
        return None;
    }
    match (*value).consttype {
        pg_sys::FLOAT8OID => f64::from_datum((*value).constvalue, false),
        pg_sys::FLOAT4OID => f32::from_datum((*value).constvalue, false).map(f64::from),
        _ => None,
    }
}

/// The distance condition on the first column of the GiST `index` of earths of the base relation
/// `rel`, other than those in `counted`: a bound, `<` or `<=`, written either way round, on the
/// distance over the earth or the cube's distance from a constant point; and the condition.
unsafe fn ball_condition(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    earth: &Earth,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<(Ball, *mut pg_sys::RestrictInfo)> {
    for ri in cells((*rel).baserestrictinfo) {
        let ri = ri as *mut pg_sys::RestrictInfo;
        if counted.contains(&ri) || (*ri).pseudoconstant || (*ri).clause.is_null() {
            continue;
        }
        let clause = (*ri).clause as *mut pg_sys::Node;
        if (*clause).type_ != pg_sys::NodeTag::T_OpExpr {
            continue;
        }
        let op = clause as *mut pg_sys::OpExpr;
        let args = cells((*op).args);
        if args.len() != 2 {
            continue;
        }
        let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
        for (distance, bound, opno) in [
            (l, r, (*op).opno),
            (r, l, pg_sys::get_commutator((*op).opno)),
        ] {
            if opno == pg_sys::InvalidOid {
                continue;
            }
            let Some((s, _)) = strategy(opno, pg_sys::Oid::from(FLOAT_OPS)) else {
                continue;
            };
            if s != LESS && s != LESS_EQUAL {
                continue;
            }
            // over the earth, the cube's distance under earthdistance's
            let (inner, over_earth) = match call_of(distance) {
                Some(c) if c.function == earth.chord_to_arc && c.args.len() == 1 => {
                    (c.args[0], true)
                }
                _ => (distance, false),
            };
            let Some(centre) = distance_from(root, rel, index, inner) else {
                continue;
            };
            let Some(bound) = number(root, bound) else {
                continue;
            };
            let chord = if over_earth {
                match folded(root, earth.arc_to_chord, &[bound]) {
                    Some(c) => c,
                    None => continue,
                }
            } else {
                bound
            };
            if chord.is_nan() {
                continue;
            }
            let ball = Ball {
                centre,
                chord,
                closed: s == LESS_EQUAL,
            };
            return Some((ball, ri));
        }
    }
    None
}

/// The box condition on the first column of the GiST `index` of cubes of the base relation `rel`,
/// other than those in `counted`, compared under the collation the column keeps.
unsafe fn box_condition(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<OnFirstColumn> {
    on_first_column(root, rel, index, counted)
        .into_iter()
        .find(|c| {
            let op = (*c.clause).clause as *mut pg_sys::OpExpr;
            (c.strategy == OVERLAP || CONTAINED_BY.contains(&c.strategy))
                && compares_as(index, 0, (*op).inputcollid)
        })
}

/// The question the GiST `index` of the base relation `rel` holds of its conditions other than
/// those in `counted`, and the conditions: a box, a distance, or both. None where it is not a GiST
/// of cubes over the whole table, or holds neither.
unsafe fn question(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<(Region, Vec<*mut pg_sys::RestrictInfo>)> {
    if !of_cubes(index) {
        return None;
    }
    let boxed = box_condition(root, rel, index, counted);
    let earth = earth_of(root, rel, index);
    let ball = earth
        .as_ref()
        .and_then(|e| ball_condition(root, rel, index, e, counted));
    let mut clauses = Vec::new();
    let boxed = boxed.and_then(|c| {
        let cube = Cube::of((*c.value).constvalue)?;
        clauses.push(c.clause);
        Some((cube, CONTAINED_BY.contains(&c.strategy)))
    });
    let ball = ball.map(|(b, ri)| {
        clauses.push(ri);
        b
    });
    let region = Region::new(boxed, ball, earth.map(|e| e.radius))?;
    Some((region, clauses))
}

/// What the GiST `index` of the base relation `rel` measures of a box or a distance on its first
/// column, and the conditions. None where it is not a GiST of cubes over the whole table, or holds
/// no such condition.
pub(crate) unsafe fn survey(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
) -> Option<(Measured, Vec<*mut pg_sys::RestrictInfo>)> {
    survey_except(root, rel, index, &[])
}

/// The conditions the GiST `index` of the base relation `rel` would hold, other than those in
/// `counted`, read from the conditions alone, before any page of the index is read.
pub(crate) unsafe fn holds(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Vec<*mut pg_sys::RestrictInfo> {
    question(root, rel, index, counted).map_or_else(Vec::new, |(_, clauses)| clauses)
}

/// As `survey`, leaving out the conditions in `counted`.
unsafe fn survey_except(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<(Measured, Vec<*mut pg_sys::RestrictInfo>)> {
    let (region, clauses) = question(root, rel, index, counted)?;
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let measured = rows(rel_index, &region, (*rel).tuples, (*rel).pages);
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    if let Some(m) = measured {
        if pg_sys::message_level_is_interesting(pg_sys::DEBUG1 as i32) {
            pgrx::debug1!(
                "surveyor: {} read {} pages for {} condition(s), stopping at depth {} on {} \
                 children ({:.2} in shares) over {:.0} pages below; {} across counted at {:.0} \
                 entries, {} inside read whole at {:.0} entries, {} inside at {:.1} a leaf, {} by their \
                 own pages over {:.0} ({} read whole, {} at five of their own leaves, {} wholly inside), \
                 {} pages for leaves' entries",
                CStr::from_ptr(pg_sys::get_rel_name((*index).indexoid)).to_string_lossy(),
                m.pages,
                clauses.len(),
                m.depth,
                m.named,
                m.share,
                m.below,
                m.counted,
                m.entries,
                m.whole,
                m.whole_entries,
                m.inside,
                m.per_leaf,
                m.own,
                m.under,
                m.own_read,
                m.own_sampled,
                m.own_inside,
                m.leaf_pages
            );
        }
    }
    measured.map(|m| (m, clauses))
}

/// What the GiST `index` of the base relation `rel` holds of `rel`'s constant conditions other
/// than those in `counted`: a box on a cube key, `&&` or `<@`, and a distance from a constant
/// point, each written either way round.
pub(crate) unsafe fn held_except(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<Held> {
    survey_except(root, rel, index, counted).map(|(m, clauses)| Held {
        rows: m.rows,
        pages: m.pages,
        clauses,
    })
}
#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::{Measured, CHILDREN};
    use crate::planned::planning;
    use pgrx::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 60,000 homes: a quarter round London, a quarter round a point on the antimeridian, and the
    /// rest over the whole earth, with a GiST on their places.
    fn homes() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; \
             CREATE EXTENSION IF NOT EXISTS earthdistance; \
             CREATE EXTENSION IF NOT EXISTS pageinspect; \
             SELECT setseed(0.42); \
             CREATE TABLE homes AS \
             SELECT g AS id, \
                    CASE g % 4 WHEN 0 THEN 51.5 + (u - 0.5) * 0.6 \
                               WHEN 1 THEN -16.8 + (u - 0.5) * 0.6 \
                               ELSE degrees(asin(2 * u - 1)) END AS latitude, \
                    CASE g % 4 WHEN 0 THEN -0.1 + (v - 0.5) \
                               WHEN 1 THEN CASE WHEN v < 0.5 THEN 179.5 + v ELSE v - 180.5 END \
                               ELSE 360 * v - 180 END AS longitude \
             FROM (SELECT g, random() AS u, random() AS v FROM generate_series(1, 60000) g) s; \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX homes_earth ON homes USING gist (ll_to_earth(latitude, longitude)); \
             ANALYZE homes",
        )
        .unwrap();
    }

    /// What the GiST measured of `query`'s box while it was planned.
    fn surveyed(query: &str) -> Measured {
        let seen = Rc::new(RefCell::new(None));
        let s = seen.clone();
        planning(query, move |root, rel, index| unsafe {
            if let Some((m, _)) = super::survey(root, rel, index) {
                *s.borrow_mut() = Some(m);
            }
        });
        let out = *seen.borrow();
        out.unwrap_or_else(|| panic!("{query}: no box held"))
    }

    const KEY: &str = r#"(substring(i.keys FROM '=\("(.*)"\)$'))::cube"#;
    const CHILD: &str = "((i.ctid::text::point)[0])::int";

    /// The pages of each level of `index`, read by pageinspect from the root down.
    fn census(index: &str) -> Vec<f64> {
        Spi::connect(|client| {
            let sql = format!(
                "WITH RECURSIVE l(depth, b) AS (SELECT 0, 0 UNION ALL \
                     SELECT l.depth + 1, {CHILD} FROM l \
                     CROSS JOIN LATERAL gist_page_items(get_raw_page('{index}', l.b), '{index}') i \
                     WHERE NOT ('leaf' = ANY ((gist_page_opaque_info(get_raw_page('{index}', l.b))).flags))) \
                 SELECT count(*)::float8 FROM l GROUP BY depth ORDER BY depth"
            );
            let mut out = Vec::new();
            for row in client.select(&sql, None, &[])? {
                out.push(row.get::<f64>(1)?.unwrap());
            }
            Ok::<_, pgrx::spi::SpiError>(out)
        })
        .unwrap()
    }

    /// The children `bx` overlaps at `depth` of `index`, reached through the union keys it
    /// overlaps above, read by pageinspect: their shares of volume inside it added, and how many.
    fn named_at(index: &str, bx: &str, depth: u32) -> (f64, i64, i64) {
        Spi::get_three::<f64, i64, i64>(&format!(
            "WITH RECURSIVE w(depth, b) AS (SELECT 0, 0 UNION ALL \
                 SELECT w.depth + 1, {CHILD} FROM w \
                 CROSS JOIN LATERAL gist_page_items(get_raw_page('{index}', w.b), '{index}') i \
                 WHERE w.depth < {depth} AND {KEY} && {bx}) \
             SELECT coalesce(sum(CASE WHEN cube_size(k) > 0 \
                        THEN cube_size(cube_inter(k, {bx})) / cube_size(k) ELSE 1 END), 0)::float8, \
                    count(*), count(*) FILTER (WHERE k <@ {bx}) \
             FROM (SELECT {KEY} AS k FROM w \
                   CROSS JOIN LATERAL gist_page_items(get_raw_page('{index}', w.b), '{index}') i \
                   WHERE w.depth = {depth} AND NOT i.dead) z \
             WHERE k && {bx}"
        ))
        .map(|(a, b, c)| (a.unwrap(), b.unwrap(), c.unwrap()))
        .unwrap()
    }

    fn count(sql: &str) -> f64 {
        Spi::get_one::<i64>(sql).unwrap().unwrap() as f64
    }

    /// The box measured from the level the read stops at, as pageinspect reads it: the same
    /// children; where the read counted the entries of the children across the box's edge or read
    /// the leaves of those inside it, every one of them, the others inside the box; where it
    /// counted none of them by their own pages, the same shares; that level the coarsest naming
    /// twenty children, or the one over the leaves; no leaf read on the way down; every level
    /// counted exactly down to the first of more than twenty pages, or to the level below the stop;
    /// the level below's pages the census's where they were counted exactly, and within `within` of
    /// it where they were estimated. The rows measured, and the rows counted in the box.
    fn holds(index: &str, table: &str, cond: &str, bx: &str, within: f64) -> (Measured, f64) {
        let m = surveyed(&format!("SELECT id FROM {table} WHERE {cond}"));
        let levels = census(index);
        let leaves = levels.len() as u32 - 1;
        let (share, named, inside) = named_at(index, bx, m.depth);
        assert_eq!(m.named as i64, named, "{cond}: {m:?}");
        if m.counted > 0 || m.inside > 0 || m.whole > 0 {
            assert!(
                m.counted as i64 == named - inside
                    && (m.whole + m.inside) as f64 + m.share == inside as f64,
                "{cond}: {m:?}, pageinspect's {named} children, {inside} inside"
            );
        } else if m.own == 0 {
            assert!(
                (m.share - share).abs() <= 1e-9 * share.max(1.0),
                "{cond}: {m:?}, pageinspect's {named} children holding {share}"
            );
        }
        assert_eq!(m.leaves_read, 0, "{cond}: {m:?}");
        assert!(
            m.named >= CHILDREN || m.named == 0 || m.depth + 1 == leaves,
            "{cond}: stopped early, {m:?}, {levels:?}"
        );
        if m.depth > 0 {
            let (_, above, _) = named_at(index, bx, m.depth - 1);
            assert!(above < CHILDREN as i64, "{cond}: stopped late, {m:?}");
        }
        let census_below = levels[m.depth as usize + 1];
        // every level down to the first of more than twenty pages is read whole, so the level
        // under it is counted exactly
        let whole = levels
            .iter()
            .skip(1)
            .position(|&p| p > CHILDREN as f64)
            .map_or(leaves, |k| k as u32 + 1);
        assert!(
            m.exact_depth >= whole.min(m.depth + 1),
            "{cond}: counted exactly to depth {}, {m:?}, {levels:?}",
            m.exact_depth
        );
        let within = if m.exact { 1e-9 } else { within };
        assert!(
            (m.below - census_below).abs() <= within * census_below,
            "{cond}: {} pages below, {census_below} counted, {m:?}, {levels:?}",
            m.below
        );
        let counted = count(&format!("SELECT count(*) FROM {table} WHERE {cond}"));
        (m, counted)
    }

    fn earth_box(lat: f64, lon: f64, m: f64) -> String {
        format!("earth_box(ll_to_earth({lat}, {lon}), {m})")
    }

    #[pg_test]
    fn a_box_is_measured_from_the_union_keys_above_the_leaves() {
        homes();
        for (what, lat, lon, half, band) in [
            ("where the homes are few", -60.0, -120.0, 50_000.0, None),
            ("small, where the homes are many", 51.5, -0.1, 300.0, None),
            (
                "across many leaves",
                51.5,
                -0.1,
                10_000.0,
                Some((0.25, 2.0)),
            ),
            (
                "across the antimeridian",
                -16.8,
                180.0,
                20_000.0,
                Some((0.25, 2.0)),
            ),
            (
                "over much of the earth",
                20.0,
                60.0,
                2_000_000.0,
                Some((0.25, 2.0)),
            ),
            ("the whole earth", 0.0, 0.0, 20_100_000.0, Some((0.6, 1.6))),
        ] {
            let bx = earth_box(lat, lon, half);
            let cond = format!("{bx} @> ll_to_earth(latitude, longitude)");
            let (m, counted) = holds("homes_earth", "homes", &cond, &bx, 0.03);
            if let Some((low, high)) = band {
                assert!(
                    m.rows >= low * counted && m.rows <= high * counted,
                    "{what}: {} rows measured, {counted} counted, {m:?}",
                    m.rows
                );
            }
            if half == 50_000.0 {
                assert!(m.named < CHILDREN, "{what}: {m:?}");
            }
        }
        let east = count(&format!(
            "SELECT count(*) FROM homes WHERE {} @> ll_to_earth(latitude, longitude) AND longitude > 0",
            earth_box(-16.8, 180.0, 20_000.0)
        ));
        let west = count(&format!(
            "SELECT count(*) FROM homes WHERE {} @> ll_to_earth(latitude, longitude) AND longitude < 0",
            earth_box(-16.8, 180.0, 20_000.0)
        ));
        assert!(east > 0.0 && west > 0.0, "{east} east, {west} west");
    }

    #[pg_test]
    fn a_level_of_at_most_twenty_pages_is_read_whole_and_the_level_under_it_counted_exactly() {
        // sixteen clusters along the diagonal, so the upper levels part them
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; \
             CREATE EXTENSION IF NOT EXISTS pageinspect; \
             SELECT setseed(0.17); \
             CREATE TABLE spread AS SELECT g AS id, \
                 cube(ARRAY(SELECT (g % 16) * 10 + random() FROM generate_series(1, 16))) AS c \
             FROM generate_series(1, 40000) g; \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX spread_c ON spread USING gist (c); \
             ANALYZE spread",
        )
        .unwrap();
        let levels = census("spread_c");
        assert!(levels.len() >= 4, "{levels:?}");
        for (low, high) in [
            (-1.0, 200.0),
            (-1.0, 11.0),
            (-1.0, 41.0),
            (25.0, 75.0),
            (95.0, 200.0),
        ] {
            let bx = format!(
                "cube(array_fill({low}::float8, ARRAY[16]), array_fill({high}::float8, ARRAY[16]))"
            );
            let cond = format!("c <@ {bx}");
            // an estimate of a level over pages this uneven is held only to twice the census
            let (m, counted) = holds("spread_c", "spread", &cond, &bx, 1.0);
            assert!(counted > 0.0, "{cond}");
            // a box holding every point names every page above the level it stops at
            assert!(high < 200.0 || low > 0.0 || m.exact, "{m:?}");
        }
    }

    #[pg_test]
    fn a_gist_read_stops_once_its_pages_would_pass_the_pages_of_its_table() {
        // 600 numbers on a few pages, each a point of a hundred dimensions in a GiST, so the GiST
        // stands many times the table; and the same numbers each beside a note that fills a page
        // with a few of them
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; SELECT setseed(0.61); \
             CREATE TABLE dots AS SELECT g AS id, random() AS x FROM generate_series(1, 600) g; \
             CREATE TABLE dots_padded (id int, x float8, note text); \
             ALTER TABLE dots_padded ALTER note SET STORAGE PLAIN; \
             INSERT INTO dots_padded SELECT id, x, repeat('x', 2000) FROM dots; \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX dots_cube ON dots USING gist (cube(array_fill(x, ARRAY[100]))); \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX dots_padded_cube ON dots_padded \
                 USING gist (cube(array_fill(x, ARRAY[100]))); \
             ANALYZE dots; ANALYZE dots_padded",
        )
        .unwrap();
        let measured = |table: &str| {
            let seen = Rc::new(RefCell::new(None));
            let s = seen.clone();
            planning(
                &format!(
                    "SELECT id FROM {table} WHERE cube(array_fill(x, ARRAY[100])) <@ \
                     cube(array_fill(0.2::float8, ARRAY[100]), array_fill(0.6::float8, ARRAY[100]))"
                ),
                move |root, rel, index| unsafe {
                    if let Some((m, _)) = super::survey(root, rel, index) {
                        *s.borrow_mut() = Some(m);
                    }
                },
            );
            let out = *seen.borrow();
            out
        };
        let table_pages =
            Spi::get_one::<i32>("SELECT relpages FROM pg_class WHERE relname = 'dots'")
                .unwrap()
                .unwrap() as u32;
        let wide = measured("dots_padded").expect("the box is measured beside the notes");
        assert!(wide.pages > table_pages, "{wide:?}, {table_pages} pages");
        assert!(measured("dots").is_none());
    }

    #[pg_test]
    fn pages_not_yet_counted_are_shared_out_over_the_levels_down_to_the_leaves() {
        let near = |a: f64, b: f64| (a - b).abs() <= 1e-9 * b;
        // the leaves under one level read: all but that level
        let leaves = super::shared(21_126.0, &[54.0], None, 500.0);
        assert!(near(leaves, 21_126.0 / 55.0 * 54.0), "{leaves}");
        // the leaves under two levels read
        let leaves = super::shared(10_000.0, &[4.0, 9.0], None, 1.0);
        assert!(
            near(leaves, 10_000.0 / (1.0 + 4.0 + 36.0) * 36.0),
            "{leaves}"
        );
        // a level over the leaves, from the downlinks under it
        let level = super::shared(21_126.0, &[], Some(54.0), 510.0);
        assert!(near(level, 21_126.0 / 55.0), "{level}");
        // a level with two under it: the number of levels nearest the estimate
        let level = super::shared(10_000.0, &[], Some(10.0), 100.0);
        assert!(near(level, 10_000.0 / 111.0), "{level}");
        let level = super::shared(10_000.0, &[], Some(10.0), 900.0);
        assert!(near(level, 10_000.0 / 11.0), "{level}");
        // under a level read, a level sampled, two under it
        let level = super::shared(10_000.0, &[3.0], Some(10.0), 200.0);
        assert!(
            near(level, 10_000.0 / (1.0 + 3.0 + 30.0 + 300.0) * 3.0),
            "{level}"
        );
        // no downlinks to share by: the estimate
        assert_eq!(super::shared(10_000.0, &[], Some(1.0), 123.0), 123.0);
    }

    /// 60,000 builders round London: 10,000 on streets running east and west, each street on one
    /// latitude; 10,000 on streets running north and south; 10,000 on streets running across; and
    /// 30,000 off the streets; written in no order, with a GiST on their places packed to a third
    /// of a page, so that it stands four levels.
    fn streets() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; \
             CREATE EXTENSION IF NOT EXISTS earthdistance; \
             CREATE EXTENSION IF NOT EXISTS pageinspect; \
             SELECT setseed(0.31); \
             CREATE TABLE builders AS \
             SELECT row_number() OVER () AS id, latitude, longitude, repeat('x', 60) AS note FROM ( \
               SELECT 51.45 + 0.005 * k AS latitude, -0.25 + 0.0006 * i AS longitude \
               FROM generate_series(0, 19) k, generate_series(0, 499) i \
               UNION ALL \
               SELECT 51.45 + 0.0002 * i, -0.247 + 0.015 * j \
               FROM generate_series(0, 19) j, generate_series(0, 499) i \
               UNION ALL \
               SELECT 51.45 + 0.0002 * i, -0.25 + 0.015 * j + 0.0002 * i \
               FROM generate_series(0, 19) j, generate_series(0, 499) i \
               UNION ALL \
               SELECT 51.45 + 0.1 * random(), -0.25 + 0.3 * random() FROM generate_series(1, 30000) \
             ) s ORDER BY random(); \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX builders_earth ON builders \
                 USING gist (ll_to_earth(latitude, longitude)) WITH (fillfactor = 30); \
             ANALYZE builders",
        )
        .unwrap();
    }

    /// 16,000 builders on 40 streets running east and west, each on one latitude, written in no
    /// order, with a GiST on their places.
    fn lines() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; \
             CREATE EXTENSION IF NOT EXISTS earthdistance; \
             CREATE EXTENSION IF NOT EXISTS pageinspect; \
             SELECT setseed(0.53); \
             CREATE TABLE lines AS \
             SELECT row_number() OVER () AS id, latitude, longitude FROM ( \
               SELECT 51.45 + 0.0025 * k AS latitude, -0.2 + 0.0005 * i AS longitude \
               FROM generate_series(0, 39) k, generate_series(0, 399) i \
             ) s ORDER BY random(); \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX lines_earth ON lines USING gist (ll_to_earth(latitude, longitude)); \
             ANALYZE lines",
        )
        .unwrap();
    }

    /// What the GiST measured of `query`'s question while it was planned, reading at most `most`
    /// pages where given and the table's pages otherwise, and the conditions it held.
    fn read_within(query: &str, most: Option<u32>) -> (Measured, usize) {
        let seen = Rc::new(RefCell::new(None));
        let s = seen.clone();
        planning(query, move |root, rel, index| unsafe {
            if let Some((region, clauses)) = super::question(root, rel, index, &[]) {
                let lock = pg_sys::AccessShareLock as pg_sys::LOCKMODE;
                let ix = pg_sys::index_open((*index).indexoid, lock);
                let m = super::rows(ix, &region, (*rel).tuples, most.unwrap_or((*rel).pages));
                pg_sys::index_close(ix, lock);
                if let Some(m) = m {
                    *s.borrow_mut() = Some((m, clauses.len()));
                }
            }
        });
        let out = *seen.borrow();
        out.unwrap_or_else(|| panic!("{query}: nothing held"))
    }

    /// The rows EXPLAIN gives the scan of `table` in `query`'s plan.
    fn planned_rows(query: &str, table: &str) -> f64 {
        crate::tests::texts(&format!("EXPLAIN {query}"))
            .iter()
            .find(|l| l.contains(&format!("on {table}")))
            .and_then(|l| l.split(" rows=").nth(1))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("{query}: no estimate"))
    }

    /// The SQL of every key at `depth` of `index` that `reaches` (an expression of `c.k`, the
    /// key) holds, reached through keys it holds above: the key `k` and its child's block `child`.
    fn keys_at(index: &str, depth: u32, reaches: &str) -> String {
        let items = format!(
            "SELECT {KEY} AS k, {CHILD} AS child, i.dead \
             FROM gist_page_items(get_raw_page('{index}', w.b), '{index}') i"
        );
        format!(
            "WITH RECURSIVE w(depth, b) AS (SELECT 0, 0 UNION ALL \
                 SELECT w.depth + 1, c.child FROM w CROSS JOIN LATERAL ({items}) c \
                 WHERE w.depth < {depth} AND NOT c.dead AND {reaches}) \
             SELECT c.k, c.child FROM w CROSS JOIN LATERAL ({items}) c \
             WHERE w.depth = {depth} AND NOT c.dead AND {reaches}"
        )
    }

    const LONDON: &str = "ll_to_earth(51.5, -0.1)";

    fn ball(metres: f64) -> String {
        format!("earth_distance({LONDON}, ll_to_earth(latitude, longitude)) < {metres}")
    }

    fn boxed(metres: f64) -> String {
        format!("earth_box({LONDON}, {metres}) @> ll_to_earth(latitude, longitude)")
    }

    #[pg_test]
    fn a_distance_is_counted_from_the_keys_with_and_without_its_box() {
        streets();
        let base = "SELECT id FROM builders WHERE";
        let mut before = Vec::new();
        for metres in [1_000.0, 4_000.0] {
            let truth = count(&format!(
                "SELECT count(*) FROM builders WHERE {}",
                ball(metres)
            ));
            let (alone, held) = read_within(&format!("{base} {}", ball(metres)), None);
            assert_eq!(held, 1, "{metres}");
            assert!(
                alone.rows >= 0.75 * truth && alone.rows <= 1.33 * truth,
                "{metres}: {} measured, {truth} counted, {alone:?}",
                alone.rows
            );
            // the box beside its distance, and the distance spelled every other way: the same
            // question, counted the same
            let chord = format!("gc_to_sec({metres})");
            for (cond, conditions) in [
                (format!("{} AND {}", boxed(metres), ball(metres)), 2),
                (
                    format!("ll_to_earth(latitude, longitude) <-> {LONDON} < {chord}"),
                    1,
                ),
                (
                    format!("cube_distance({LONDON}, ll_to_earth(latitude, longitude)) < {chord}"),
                    1,
                ),
                (
                    format!(
                        "{metres} >= earth_distance(ll_to_earth(latitude, longitude), {LONDON})"
                    ),
                    1,
                ),
            ] {
                let (m, held) = read_within(&format!("{base} {cond}"), None);
                assert_eq!(held, conditions, "{cond}");
                assert_eq!(m.rows, alone.rows, "{cond}: {m:?} against {alone:?}");
                assert_eq!(m.pages, alone.pages, "{cond}");
            }
            for query in [
                format!("{base} {}", ball(metres)),
                format!("{base} {} AND {}", boxed(metres), ball(metres)),
            ] {
                before.push((query.clone(), planned_rows(&query, "builders"), truth));
            }
        }
        // the plan takes the count, the distance not taken again at the planner's own share
        Spi::run("CREATE INDEX builders_order ON builders USING surveyor (id)").unwrap();
        for (query, alone, truth) in before {
            let rows = planned_rows(&query, "builders");
            assert!(
                rows >= 0.75 * truth && rows <= 1.33 * truth,
                "{query}: planned {rows}, {truth} counted, {alone} without the surveyor"
            );
            assert!(
                (rows - truth).abs() < (alone - truth).abs(),
                "{query}: planned {rows}, {alone} without the surveyor, {truth} counted"
            );
        }
    }

    #[pg_test]
    fn the_leaves_across_the_edge_are_read_and_their_entries_counted() {
        streets();
        // the first distance whose read stops on the leaves' keys; the tree's shape is the build's
        let levels = census("builders_earth").len();
        let (question, m, metres) = [250.0, 500.0, 1_000.0]
            .into_iter()
            .find_map(|metres| {
                let question = format!("SELECT id FROM builders WHERE {}", ball(metres));
                let (m, _) = read_within(&question, None);
                (m.depth as usize + 2 == levels).then_some((question, m, metres))
            })
            .expect("a distance whose read stops on the leaves' keys");
        let (point, chord) = (format!("{LONDON}::cube"), format!("gc_to_sec({metres})"));
        let reaches = format!("cube_distance(c.k, {point}) < {chord}");
        // each key's farthest corner from the point
        let farthest = format!(
            "sqrt((SELECT sum(greatest((cube_ll_coord({point}, d) - cube_ll_coord(n.k, d)) ^ 2, \
                                    (cube_ur_coord(n.k, d) - cube_ll_coord({point}, d)) ^ 2)) \
                   FROM generate_series(1, 3) d))"
        );
        let (named, across, entries) = Spi::get_three::<i64, i64, i64>(&format!(
            "WITH n AS ({}), f AS (SELECT n.k, n.child, {farthest} AS far FROM n) \
             SELECT count(*), count(*) FILTER (WHERE far >= {chord}), \
                    (SELECT count(*) FROM f, gist_page_items(get_raw_page('builders_earth', f.child), \
                                                             'builders_earth') i \
                     WHERE f.far >= {chord} AND NOT i.dead \
                       AND cube_distance({KEY}, {point}) < {chord}) \
             FROM f",
            keys_at("builders_earth", m.depth, &reaches)
        ))
        .map(|(a, b, c)| (a.unwrap() as f64, b.unwrap() as f64, c.unwrap() as f64))
        .unwrap();
        assert!(across > 0.0, "{m:?}");
        assert_eq!(m.named as f64, named, "{m:?}");
        assert_eq!(m.counted as f64, across, "{m:?}");
        assert_eq!(m.entries, entries, "{m:?}");
        // the keys wholly inside, their leaves read whole: every entry in the distance counted
        let tuples = count("SELECT count(*) FROM builders");
        assert_eq!(m.whole as f64, named - across, "{m:?}");
        assert!(
            (m.rows - (entries + m.whole_entries)).abs() <= 1e-9 * m.rows,
            "{m:?}"
        );
        let truth = count(&format!(
            "{} AND true",
            question.replace("SELECT id", "SELECT count(*)")
        ));
        assert_eq!(m.rows, truth, "{m:?}");
        // past the pages it may read, the keys across the edge take their shares, and those inside
        // the rows under a leaf
        let tight = m.pages - m.counted as u32 - m.leaf_pages;
        let (shared, _) = read_within(&question, Some(tight));
        assert_eq!(
            (shared.counted, shared.whole, shared.inside, shared.pages),
            (0, 0, 0, tight),
            "{shared:?}"
        );
        assert!(
            (shared.rows - shared.share * tuples / shared.below).abs() <= 1e-9 * shared.rows,
            "{shared:?}"
        );
    }

    #[pg_test]
    fn a_key_on_one_latitude_across_a_box_counts_the_stretch_of_its_latitude_inside() {
        lines();
        let bx = format!("earth_box({LONDON}, 1500)");
        let question =
            format!("SELECT id FROM lines WHERE {bx} @> ll_to_earth(latitude, longitude)");
        let truth = count(&format!(
            "SELECT count(*) FROM lines WHERE {bx} @> ll_to_earth(latitude, longitude)"
        ));
        let (full, _) = read_within(&question, None);
        // the keys across the edge left to their shares, and no leaf read
        let tight = full.pages - full.counted as u32 - full.leaf_pages;
        let (m, _) = read_within(&question, Some(tight));
        assert_eq!(
            (m.counted, m.whole, m.inside, m.leaf_pages),
            (0, 0, 0, 0),
            "{m:?}"
        );
        // the keys named, and the share each would take were a key of no volume counted whole
        let (flat_across, whole) = Spi::get_two::<i64, f64>(&format!(
            "WITH n AS ({}) \
             SELECT count(*) FILTER (WHERE cube_ll_coord(n.k, 3) = cube_ur_coord(n.k, 3) \
                                       AND cube_size(n.k) = 0 AND NOT n.k <@ {bx}), \
                    sum(CASE WHEN cube_size(n.k) > 0 \
                             THEN cube_size(cube_inter(n.k, {bx})) / cube_size(n.k) ELSE 1 END)::float8 \
             FROM n",
            keys_at("lines_earth", m.depth, &format!("c.k && {bx}"))
        ))
        .map(|(a, b)| (a.unwrap(), b.unwrap()))
        .unwrap();
        assert!(
            flat_across >= 5,
            "{flat_across} keys on one latitude across the edge"
        );
        let tuples = count("SELECT count(*) FROM lines");
        let whole = whole * tuples / m.below;
        assert!(
            m.rows >= 0.75 * truth && m.rows <= 1.33 * truth,
            "{} measured, {truth} counted, {whole} with such keys whole, {m:?}",
            m.rows
        );
        assert!(
            (m.rows - truth).abs() < 0.5 * (whole - truth).abs(),
            "{} measured, {truth} counted, {whole} with such keys whole",
            m.rows
        );
        // the plan takes the count read in full
        Spi::run("CREATE INDEX lines_order ON lines USING surveyor (id)").unwrap();
        let rows = planned_rows(&question, "lines");
        assert!(
            rows >= 0.75 * truth && rows <= 1.33 * truth,
            "planned {rows}, {truth} counted, {full:?}"
        );
    }

    #[pg_test]
    fn a_child_named_above_the_level_over_the_leaves_counts_its_own_pages_under_it() {
        streets();
        // the first box whose read stops two levels over the leaves; the tree's shape is the
        // build's
        let levels = census("builders_earth");
        let (bx, question, m) = [4000, 3000, 6000]
            .into_iter()
            .find_map(|metres| {
                let bx = format!("earth_box({LONDON}, {metres})");
                let question = format!(
                    "SELECT id FROM builders WHERE {bx} @> ll_to_earth(latitude, longitude)"
                );
                let (m, _) = read_within(&question, None);
                (m.depth as usize + 3 == levels.len()).then_some((bx, question, m))
            })
            .expect("a box whose read stops two levels over the leaves");
        let truth = count(&format!(
            "SELECT count(*) FROM builders WHERE {bx} @> ll_to_earth(latitude, longitude)"
        ));
        // every child the stop names is read; one wholly inside counts its own pages, and one
        // across the edge its own leaves, all of them or five; five leaves of a child stand for
        // its leaves only to their spread
        let across = m.named - m.own_inside;
        assert!(
            m.own == m.named && (m.own_read == across || m.own_sampled == across),
            "{m:?}"
        );
        let band = if m.own_read == across {
            (0.85, 1.15)
        } else {
            (0.75, 1.33)
        };
        assert!(
            m.rows >= band.0 * truth && m.rows <= band.1 * truth,
            "{} measured, {truth} counted",
            m.rows
        );
        // with no room for the leaves, each child at the rows under a page of the level under it
        let full = m;
        let (m, _) = read_within(&question, Some(full.pages - full.leaf_pages));
        assert_eq!(
            (m.own, m.own_read, m.own_sampled, m.leaf_pages),
            (m.named, 0, 0, 0),
            "{m:?}"
        );
        assert!(
            (m.under - levels[m.depth as usize + 2]).abs() <= 0.03 * m.under,
            "{m:?} {levels:?}"
        );
        // each child's share times the pages under it
        let (weighted, solid) = Spi::get_two::<f64, bool>(&format!(
            "WITH n AS ({}) \
             SELECT sum(cube_size(cube_inter(n.k, {bx})) / cube_size(n.k) \
                        * (SELECT count(*) FROM gist_page_items(get_raw_page('builders_earth', n.child), \
                                                                 'builders_earth') i WHERE NOT i.dead))::float8, \
                    bool_and(cube_size(n.k) > 0) \
             FROM n",
            keys_at("builders_earth", m.depth, &format!("c.k && {bx}"))
        ))
        .map(|(a, b)| (a.unwrap(), b.unwrap()))
        .unwrap();
        assert!(solid);
        let tuples = count("SELECT count(*) FROM builders");
        let expected = weighted * tuples / m.under;
        assert!(
            (m.rows - expected).abs() <= 1e-9 * expected,
            "{m:?}: {expected}"
        );
        assert!(
            m.rows >= 0.85 * truth && m.rows <= 1.15 * truth,
            "{} measured, {truth} counted",
            m.rows
        );
        // past the pages it may read, each child at the rows under a page of the level below
        let tight = m.pages - m.own as u32;
        let (unread, _) = read_within(&question, Some(tight));
        assert_eq!((unread.own, unread.pages), (0, tight), "{unread:?}");
        assert!(
            (unread.rows - unread.share * tuples / unread.below).abs() <= 1e-9 * unread.rows,
            "{unread:?}"
        );
        // the plan takes the count
        Spi::run("CREATE INDEX builders_order ON builders USING surveyor (id)").unwrap();
        assert_eq!(planned_rows(&question, "builders"), full.rows.round());
    }

    /// 88,000 builders round London: 8,000 spread over the whole of it and written in the order of
    /// their places, with a GiST on their places packed to a tenth of a page; then 80,000 more in
    /// its east only, which fill the east's leaves while the west's stay a tenth full.
    fn sparse() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS cube; \
             CREATE EXTENSION IF NOT EXISTS earthdistance; \
             CREATE EXTENSION IF NOT EXISTS pageinspect; \
             SELECT setseed(0.71); \
             CREATE TABLE sparse AS SELECT g AS id, latitude, longitude FROM ( \
               SELECT g, 51.45 + 0.1 * random() AS latitude, -0.25 + 0.3 * random() AS longitude \
               FROM generate_series(1, 8000) g) s \
             ORDER BY (SELECT sum(((floor((latitude - 51.45) / 0.1 * 1024))::int >> b & 1) \
                                  * (4 ^ b)::bigint \
                                + ((floor((longitude + 0.25) / 0.3 * 1024))::int >> b & 1) \
                                  * (2 * 4 ^ b)::bigint) \
                       FROM generate_series(0, 9) b), id; \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX sparse_earth ON sparse \
                 USING gist (ll_to_earth(latitude, longitude)) WITH (fillfactor = 10); \
             INSERT INTO sparse SELECT g, 51.45 + 0.1 * random(), -0.1 + 0.15 * random() \
             FROM generate_series(8001, 88000) g; \
             ANALYZE sparse",
        )
        .unwrap();
    }

    /// The SQL of every key at `depth` of `index` that `reaches` (an expression of `c.k`, the
    /// key) holds, reached through keys it holds above: the key `k`, its child's block `child`, and
    /// its place on the way down, `path`, whose order is the order the read names them.
    fn keys_in_order(index: &str, depth: u32, reaches: &str) -> String {
        let items = format!(
            "SELECT {KEY} AS k, {CHILD} AS child, i.dead, i.itemoffset::int AS at \
             FROM gist_page_items(get_raw_page('{index}', w.b), '{index}') i"
        );
        format!(
            "WITH RECURSIVE w(depth, b, path) AS (SELECT 0, 0, '{{}}'::int[] UNION ALL \
                 SELECT w.depth + 1, c.child, w.path || c.at FROM w CROSS JOIN LATERAL ({items}) c \
                 WHERE w.depth < {depth} AND NOT c.dead AND {reaches}) \
             SELECT c.k, c.child, w.path || c.at AS path FROM w CROSS JOIN LATERAL ({items}) c \
             WHERE w.depth = {depth} AND NOT c.dead AND {reaches}"
        )
    }

    const WEST: &str = "ll_to_earth(51.5, -0.2)";

    /// The SQL of the leaves wholly inside the box `bx` among the keys at `depth` of `sparse_earth`
    /// that reach it, in the order the read names them: how many, the entries the first, the
    /// quarter, the middle, the three-quarter and the last hold on average, and the entries they
    /// all hold.
    fn inside_leaves(depth: u32, bx: &str) -> String {
        format!(
            "WITH n AS ({}), \
                  o AS (SELECT row_number() OVER (ORDER BY n.path) - 1 AS r, count(*) OVER () AS c, \
                               (SELECT count(*) FROM gist_page_items(get_raw_page('sparse_earth', \
                                                                                  n.child), \
                                                                     'sparse_earth') e \
                                WHERE NOT e.dead) AS e \
                        FROM n WHERE n.k <@ {bx}) \
             SELECT max(c), (avg(e) FILTER (WHERE r IN (0, c / 4, c / 2, 3 * c / 4, c - 1)))::float8, \
                    sum(e)::float8 \
             FROM o",
            keys_in_order("sparse_earth", depth, &format!("c.k && {bx}"))
        )
    }

    #[pg_test]
    fn the_leaves_wholly_inside_are_read_whole_and_past_the_limit_count_at_five_of_them() {
        sparse();
        let levels = census("sparse_earth");
        // the first box whose read stops on the leaves' keys with more than five leaves inside;
        // the tree's shape is the build's
        let (bx, question, m) = [800, 900, 700, 1000, 600]
            .into_iter()
            .find_map(|metres| {
                let bx = format!("earth_box({WEST}, {metres})");
                let question =
                    format!("SELECT id FROM sparse WHERE {bx} @> ll_to_earth(latitude, longitude)");
                let (m, _) = read_within(&question, None);
                (m.depth as usize + 2 == levels.len() && m.whole > 5).then_some((bx, question, m))
            })
            .expect("a box whose read stops on the leaves' keys with more than five inside");
        let (inside, sampled, held) = Spi::get_three::<i64, f64, f64>(&inside_leaves(m.depth, &bx))
            .map(|(a, b, c)| (a.unwrap(), b.unwrap(), c.unwrap()))
            .unwrap();
        // every leaf inside read whole, and with the leaves across the edge, the box's count
        assert_eq!((m.whole as i64, m.inside), (inside, 0), "{m:?}");
        assert_eq!(m.whole_entries, held, "{m:?}");
        let truth = count(&format!(
            "SELECT count(*) FROM sparse WHERE {bx} @> ll_to_earth(latitude, longitude)"
        ));
        assert_eq!(m.rows, truth, "{m:?}");
        // the leaves inside hold far fewer entries than the index's average
        let tuples = count("SELECT count(*) FROM sparse");
        let average = tuples / levels[levels.len() - 1];
        assert!(
            held / (inside as f64) < 0.5 * average,
            "{held} on {inside} leaves, {average} a leaf on average"
        );
        // with no room to read every leaf named, the leaves inside at five of them
        let (s, _) = read_within(&question, Some(m.pages - 1));
        assert_eq!((s.whole, s.inside as i64), (0, inside), "{s:?}");
        assert!(
            (s.per_leaf - sampled).abs() <= 1e-9 * sampled,
            "{s:?}: {sampled}"
        );
        let per_page = tuples / s.below;
        let expected = s.entries + s.inside as f64 * s.per_leaf + s.share * per_page;
        assert!(
            (s.rows - expected).abs() <= 1e-9 * expected,
            "{s:?}: {expected}"
        );
        let at_average = s.entries + (s.inside as f64 + s.share) * per_page;
        assert!(
            s.rows >= 0.6 * truth && s.rows <= 1.6 * truth,
            "{} measured, {truth} counted, {at_average} at the average",
            s.rows
        );
        assert!(
            (s.rows - truth).abs() < 0.25 * (at_average - truth).abs(),
            "{} measured, {truth} counted, {at_average} at the average",
            s.rows
        );
        // the plan takes the count
        Spi::run("CREATE INDEX sparse_order ON sparse USING surveyor (id)").unwrap();
        assert_eq!(planned_rows(&question, "sparse"), m.rows.round());
    }

    #[pg_test]
    fn a_child_named_above_the_level_over_the_leaves_across_the_edge_counts_its_leaves_read_whole()
    {
        sparse();
        let levels = census("sparse_earth");
        // the first box whose read stops two levels over the leaves, names children across its
        // edge and wholly inside it, and reads the leaves under those across
        let (bx, question, m) = [1500, 2000, 3000, 4000, 1200, 6000]
            .into_iter()
            .find_map(|metres| {
                let bx = format!("earth_box({WEST}, {metres})");
                let question =
                    format!("SELECT id FROM sparse WHERE {bx} @> ll_to_earth(latitude, longitude)");
                let (m, _) = read_within(&question, None);
                (m.depth as usize + 3 == levels.len()
                    && m.own == m.named
                    && m.own_read > 0
                    && m.own_inside > 0
                    && m.own_read + m.own_inside == m.named)
                    .then_some((bx, question, m))
            })
            .expect("a box whose read stops two levels over the leaves, across and inside");
        let tuples = count("SELECT count(*) FROM sparse");
        let per_leaf = tuples / m.under;
        // over the children named, those wholly inside the box at their leaves and the index's
        // average, and those across it: at their share times the entries their leaves all hold;
        // at their share times their leaves and the entries five of them hold on average (the
        // first, the quarter, the middle, the three-quarter and the last); and at their share
        // times their leaves and the index's average. Then the children across and their leaves,
        // the children inside, and the entries their leaves hold
        let found = Spi::get_one::<Vec<f64>>(&format!(
            "WITH n AS ({}), \
                  c AS (SELECT n.child AS page, n.k, ((i.ctid::text::point)[0])::int AS leaf, \
                               row_number() OVER (PARTITION BY n.child ORDER BY i.itemoffset) - 1 AS r, \
                               count(*) OVER (PARTITION BY n.child) AS d \
                        FROM n CROSS JOIN LATERAL gist_page_items(get_raw_page('sparse_earth', \
                                                                               n.child), \
                                                                  'sparse_earth') i \
                        WHERE NOT i.dead), \
                  e AS (SELECT c.*, (SELECT count(*) FROM gist_page_items(get_raw_page('sparse_earth', \
                                                                                        c.leaf), \
                                                                           'sparse_earth') x \
                                     WHERE NOT x.dead) AS entries \
                        FROM c), \
                  per AS (SELECT page, k, max(d) AS d, \
                                 avg(entries) FILTER (WHERE r IN (0, d / 4, d / 2, 3 * d / 4, d - 1)) AS mean, \
                                 sum(entries) AS held, \
                                 cube_size(cube_inter(k, {bx})) / cube_size(k) AS share, \
                                 k <@ {bx} AS inside \
                          FROM e GROUP BY page, k) \
             SELECT ARRAY[sum(CASE WHEN inside THEN d * {per_leaf} ELSE share * held END), \
                          sum(CASE WHEN inside THEN d * {per_leaf} ELSE share * d * mean END), \
                          sum(share * d * {per_leaf}), \
                          count(*) FILTER (WHERE NOT inside), sum(d) FILTER (WHERE NOT inside), \
                          count(*) FILTER (WHERE inside), \
                          sum(d * {per_leaf}) FILTER (WHERE inside), \
                          sum(held) FILTER (WHERE inside)]::float8[] \
             FROM per",
            keys_in_order("sparse_earth", m.depth, &format!("c.k && {bx}"))
        ))
        .unwrap()
        .unwrap();
        let (whole, five, at_average) = (found[0], found[1], found[2]);
        let (across, across_leaves, inside) = (found[3], found[4], found[5]);
        let (inside_by_pages, inside_held) = (found[6], found[7]);
        assert_eq!(
            (m.own_read as f64, m.own_inside as f64),
            (across, inside),
            "{m:?}"
        );
        // the children inside counted by their own pages, which their leaves read would not give
        assert!(
            (inside_by_pages - inside_held).abs() > 1.0,
            "{inside_by_pages} by their pages, {inside_held} held"
        );
        assert!((m.rows - whole).abs() <= 1e-9 * whole, "{m:?}: {whole}");
        // the leaves read are those of the children across the edge
        assert_eq!(m.leaf_pages as f64, across_leaves, "{m:?}");
        // with room for five of each such child's leaves and not for them all
        let across = m.own_read as u32;
        assert!(
            across_leaves > 5.0 * across as f64,
            "{across_leaves} leaves under {across}"
        );
        let (s, _) = read_within(&question, Some(m.pages - m.leaf_pages + 5 * across));
        assert_eq!(
            (s.own_read, s.own_sampled, s.own_inside),
            (0, m.own_read, m.own_inside),
            "{s:?}"
        );
        assert!((s.rows - five).abs() <= 1e-9 * five, "{s:?}: {five}");
        // with no room for the leaves, the index's average
        let (u, _) = read_within(&question, Some(m.pages - m.leaf_pages));
        assert_eq!(
            (u.own_read, u.own_sampled, u.leaf_pages, u.under),
            (0, 0, 0, m.under),
            "{u:?}"
        );
        assert!(
            (u.rows - at_average).abs() <= 1e-9 * at_average,
            "{u:?}: {at_average}"
        );
        let truth = count(&format!(
            "SELECT count(*) FROM sparse WHERE {bx} @> ll_to_earth(latitude, longitude)"
        ));
        assert!(
            (m.rows - truth).abs() < (u.rows - truth).abs(),
            "{} measured, {truth} counted, {} at the average",
            m.rows,
            u.rows
        );
        // the plan takes the count
        Spi::run("CREATE INDEX sparse_order ON sparse USING surveyor (id)").unwrap();
        assert_eq!(planned_rows(&question, "sparse"), m.rows.round());
    }

    /// `rows` homes round London, every third and the last `trailing` with no place, with a GiST
    /// on their places: NULL keys on its leaves, and pages above them whose union keys are NULL.
    fn unplaced(table: &str, rows: u32, trailing: u32) {
        Spi::run(&format!(
            "CREATE EXTENSION IF NOT EXISTS cube; \
             CREATE EXTENSION IF NOT EXISTS earthdistance; \
             SELECT setseed(0.37); \
             CREATE TABLE {table} AS \
             SELECT g AS id, \
                    CASE WHEN g % 3 <> 0 AND g <= {placed} THEN 51.5 + (random() - 0.5) * 0.6 END \
                        AS latitude, \
                    CASE WHEN g % 3 <> 0 AND g <= {placed} THEN -0.1 + (random() - 0.5) END \
                        AS longitude \
             FROM generate_series(1, {rows}) g; \
             SELECT tests.same_gist_every_run(); \
             CREATE INDEX {table}_earth ON {table} USING gist (ll_to_earth(latitude, longitude)); \
             ANALYZE {table}",
            placed = rows - trailing
        ))
        .unwrap();
    }

    #[pg_test]
    fn rows_with_no_place_are_rows_no_box_or_distance_takes() {
        unplaced("unplaced", 60_000, 15_000);
        let placed = count("SELECT count(*) FROM unplaced WHERE latitude IS NOT NULL");
        assert!(placed < 0.6 * count("SELECT count(*) FROM unplaced"));
        Spi::run("CREATE INDEX unplaced_order ON unplaced USING surveyor (id)").unwrap();
        for cond in [boxed(15_000.0), ball(15_000.0), boxed(500.0)] {
            let question = format!("SELECT id FROM unplaced WHERE {cond}");
            let truth = count(&format!("SELECT count(*) FROM unplaced WHERE {cond}"));
            let (m, _) = read_within(&question, None);
            assert!(
                m.rows >= 0.75 * truth && m.rows <= 1.33 * truth,
                "{cond}: {} measured, {truth} counted, {m:?}",
                m.rows
            );
            // the plan takes the count
            let rows = planned_rows(&question, "unplaced");
            assert!(
                rows >= 0.75 * truth && rows <= 1.33 * truth,
                "{cond}: planned {rows}, {truth} counted"
            );
        }
        // a GiST whose root is a leaf holding NULL keys counts the rows with a place exactly
        unplaced("few_unplaced", 60, 10);
        let cond = boxed(50_000.0);
        let (m, _) = read_within(&format!("SELECT id FROM few_unplaced WHERE {cond}"), None);
        assert_eq!(m.depth, 0, "{m:?}");
        assert_eq!(
            m.rows,
            count(&format!("SELECT count(*) FROM few_unplaced WHERE {cond}")),
            "{m:?}"
        );
    }

    #[pg_test]
    fn a_box_or_a_distance_on_a_key_kept_under_another_collation_is_left_to_the_planner() {
        lines();
        let question = format!(
            "SELECT id FROM lines WHERE {} AND {}",
            boxed(1500.0),
            ball(1500.0)
        );
        let held = Rc::new(RefCell::new(Vec::new()));
        let h = held.clone();
        planning(&question, move |root, rel, index| unsafe {
            let kept = *(*index).indexcollations;
            let own = super::question(root, rel, index, &[]).map(|(_, c)| c.len());
            // the same key, kept under a collation the conditions are not compared under
            *(*index).indexcollations = pg_sys::C_COLLATION_OID;
            let other = super::question(root, rel, index, &[]).map(|(_, c)| c.len());
            *(*index).indexcollations = kept;
            h.borrow_mut().push((own, other));
        });
        assert_eq!(*held.borrow(), vec![(Some(2), None)]);
    }
}
