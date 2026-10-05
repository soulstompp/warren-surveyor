// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! How busy the drive holding a tablespace is, read from the operating system while planning, and
//! a statement planned with every page from the disk weighed by it.
//!
//! The drive is the block device holding the tablespace's directory: the device `stat` names for
//! it, found by its major and minor numbers in `/proc/diskstats`, as a partition or, where the
//! partition has no line of its own, as the disk it lies on. Its busy share is the milliseconds it
//! has spent doing I/O over the milliseconds since the machine started, `/proc/uptime`. It is read
//! once for each tablespace in a round of planning, and nothing is kept between rounds. Where the
//! directory or the device cannot be found, or the system is not Linux, the share is 0.
//!
//! The weight of a page the drive reads is 1 over the share of the time the drive is free, the
//! busy share held at `MOST_BUSY` at most.
//!
//! A statement that reads a table carrying a surveyor, over the whole table or partial, read as
//! written, through a view or as an inheritance child or partition of a table it reads without
//! ONLY, is planned with the weight of the busiest of its drives: the drive of the database's own
//! tablespace and of each such table's. The tables are read from the catalog alone, and no table
//! is opened or locked for it: planning waits for no lock the statement does not take itself, and
//! an OID the statement names that is no table it reads (`WHERE oid = '23'`) is never taken for
//! one. Where that weight is above 1, `seq_page_cost`, `random_page_cost` and
//! `warren_surveyor_pg.walked_leaf_page_cost`, where it is set, are each multiplied by it, up to
//! the largest value each takes, for the length of the planning and put back after it, an error
//! included, as PostgreSQL holds a function's SET clause for one call (fmgr.c:704-770, 18.6) and
//! puts settings back after a call with `AtEOXact_GUC(false, …)` (matview.c:194, 370). Every price
//! PostgreSQL and the surveyor make from them weighs its pages from the disk by it; a page in
//! shared buffers keeps its price, and a tablespace that declares its own page costs keeps them. A
//! statement planned while another is being planned takes the settings as they stand.
//!
//! A drive's own page costs, measured on it, are declared on the tablespace that lies on it
//! (`ALTER TABLESPACE … SET (seq_page_cost = …, random_page_cost = …)`), and not in the settings
//! for the whole server. PostgreSQL prices the pages of every table and index in a tablespace by
//! the costs declared on it, the surveyor's price of the DBA's B-trees among them, and prices every
//! spill by the server's `seq_page_cost` and `random_page_cost` alone: a sort's runs, a hashed
//! grouping's and a hash join's batches, and a Material past `work_mem` (costsize.c:1958, 2519,
//! 2834-2836, 4246-4247, 18.6). A spill's temp file is written to the operating system's page cache
//! and is often deleted before the drive ever writes it, so a drive's price of a page read alone,
//! declared on its tablespace, weighs the lookups that read from it and no spill.

use crate::query::surveyor_am;
use crate::round;
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, CString};

/// The busy share the weight holds at most.
pub(crate) const MOST_BUSY: f64 = 0.99;

/// The largest value `seq_page_cost`, `random_page_cost` and the walked-leaf price each take.
const MOST_PAGE_COST: f64 = f64::MAX;

extern "C-unwind" {
    fn find_inheritance_children(
        parent: pg_sys::Oid,
        lockmode: pg_sys::LOCKMODE,
    ) -> *mut pg_sys::List;
}

thread_local! {
    /// The busy share of each tablespace's drive, read once in the round of planning.
    static BUSY: RefCell<HashMap<pg_sys::Oid, f64>> = RefCell::new(HashMap::new());
}

/// Forgets the shares read in the round.
pub(crate) fn forget() {
    BUSY.with(|b| b.borrow_mut().clear());
}

/// The weight of a page read from the drive holding tablespace `space` (0 for the database's).
pub(crate) unsafe fn weight(space: pg_sys::Oid) -> f64 {
    1.0 / (1.0 - busy_share(space).clamp(0.0, MOST_BUSY))
}

/// The weight a statement reading `parse` is planned with: 1 where no table it reads carries a
/// surveyor, and otherwise the highest of the database's own tablespace's drive's and of each
/// such table's.
pub(crate) unsafe fn statement_weight(parse: *mut pg_sys::Query) -> f64 {
    let am = surveyor_am();
    if am == pg_sys::InvalidOid || parse.is_null() {
        return 1.0;
    }
    let mut named: Vec<(pg_sys::Oid, bool)> = Vec::new();
    named_walker(
        parse as *mut pg_sys::Node,
        &mut named as *mut Vec<(pg_sys::Oid, bool)> as *mut c_void,
    );
    let mut spaces = surveyed_spaces(named, am);
    if spaces.is_empty() {
        return 1.0;
    }
    spaces.push(pg_sys::InvalidOid);
    spaces.into_iter().map(|s| weight(s)).fold(1.0, f64::max)
}

unsafe fn oids(list: *mut pg_sys::List) -> Vec<pg_sys::Oid> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).oid_value)
        .collect()
}

/// Gathers into the list at `context` each table the queries under `node` name, with whether it is
/// read with its inheritance children or partitions, as it is unless named with ONLY.
#[pg_guard]
unsafe extern "C-unwind" fn named_walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
    if node.is_null() {
        return false;
    }
    match (*node).type_ {
        pg_sys::NodeTag::T_Query => pg_sys::query_tree_walker(
            node as *mut pg_sys::Query,
            Some(named_walker),
            context,
            pg_sys::QTW_EXAMINE_RTES_BEFORE as i32,
        ),
        pg_sys::NodeTag::T_RangeTblEntry => {
            let e = node as *mut pg_sys::RangeTblEntry;
            if (*e).rtekind == pg_sys::RTEKind::RTE_RELATION {
                let named = &mut *(context as *mut Vec<(pg_sys::Oid, bool)>);
                named.push(((*e).relid, (*e).inh));
            }
            false
        }
        _ => pg_sys::expression_tree_walker(node, Some(named_walker), context),
    }
}

/// The tablespaces of the tables in `named` (each a table, and whether it is read with its
/// inheritance children or partitions) that carry a surveyor, and of each child or partition of
/// those read with them that carries one. Each is read from the catalog alone: no table is opened
/// or locked, so planning waits for no lock the statement does not take, and a table dropped
/// meanwhile is passed over, as the planner passes it over.
unsafe fn surveyed_spaces(named: Vec<(pg_sys::Oid, bool)>, am: pg_sys::Oid) -> Vec<pg_sys::Oid> {
    let mut spaces = Vec::new();
    // each table read, and whether its children have been read
    let mut read: HashMap<pg_sys::Oid, bool> = HashMap::new();
    let mut indexes = None;
    let mut next = named;
    while let Some((relid, members)) = next.pop() {
        match read.get(&relid) {
            Some(&true) => continue,
            Some(&false) if !members => continue,
            Some(&false) => {}
            None => match class_of(relid) {
                None => {
                    read.insert(relid, true);
                    continue;
                }
                Some((space, has_indexes)) => {
                    if has_indexes && carries_surveyor(relid, am, &mut indexes) {
                        spaces.push(space);
                    }
                }
            },
        }
        read.insert(relid, members);
        if members {
            next.extend(children_of(relid).into_iter().map(|child| (child, true)));
        }
    }
    if let Some(indexes) = indexes {
        pg_sys::table_close(indexes, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    }
    spaces
}

/// The tablespace of the relation `relid` and whether it has indexes, from `pg_class`; none where
/// it is not there.
unsafe fn class_of(relid: pg_sys::Oid) -> Option<(pg_sys::Oid, bool)> {
    let tuple = pg_sys::SearchSysCache1(
        pg_sys::SysCacheIdentifier::RELOID as i32,
        pg_sys::Datum::from(relid),
    );
    if tuple.is_null() {
        return None;
    }
    let form = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_class>(tuple);
    let class = ((*form).reltablespace, (*form).relhasindex);
    pg_sys::ReleaseSysCache(tuple);
    Some(class)
}

/// Whether one of the table `relid`'s indexes, read from `pg_index`, is a surveyor; `pg_index` is
/// opened into `indexes` the first time.
unsafe fn carries_surveyor(
    relid: pg_sys::Oid,
    am: pg_sys::Oid,
    indexes: &mut Option<pg_sys::Relation>,
) -> bool {
    let catalog = *indexes.get_or_insert_with(|| {
        pg_sys::table_open(
            pg_sys::IndexRelationId,
            pg_sys::AccessShareLock as pg_sys::LOCKMODE,
        )
    });
    let mut key = pg_sys::ScanKeyData::default();
    pg_sys::ScanKeyInit(
        &mut key,
        pg_sys::Anum_pg_index_indrelid as pg_sys::AttrNumber,
        pg_sys::BTEqualStrategyNumber as u16,
        pg_sys::Oid::from(pg_sys::F_OIDEQ),
        pg_sys::Datum::from(relid),
    );
    let scan = pg_sys::systable_beginscan(
        catalog,
        pg_sys::Oid::from(pg_sys::IndexIndrelidIndexId),
        true,
        std::ptr::null_mut(),
        1,
        &mut key,
    );
    let mut carries = false;
    loop {
        let tuple = pg_sys::systable_getnext(scan);
        if tuple.is_null() {
            break;
        }
        let index = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_index>(tuple);
        // an index being dropped is not read
        if (*index).indislive && pg_sys::get_rel_relam((*index).indexrelid) == am {
            carries = true;
            break;
        }
    }
    pg_sys::systable_endscan(scan);
    carries
}

/// The relations that inherit directly from `parent`, from `pg_inherits`, none of them locked; a
/// partition being detached is left out where the statement no longer sees it.
unsafe fn children_of(parent: pg_sys::Oid) -> Vec<pg_sys::Oid> {
    oids(pg_sys::ffi::pg_guard_ffi_boundary(|| {
        find_inheritance_children(parent, pg_sys::NoLock as pg_sys::LOCKMODE)
    }))
}

/// Plans with `seq_page_cost`, `random_page_cost` and the walked-leaf price, where it is set, each
/// multiplied by `heat`, and puts them back after, an error included.
pub(crate) unsafe fn planned_weighed<T>(heat: f64, plan: impl FnOnce() -> T) -> T {
    let nest = pg_sys::NewGUCNestLevel();
    let context = if pg_sys::superuser() {
        pg_sys::GucContext::PGC_SUSET
    } else {
        pg_sys::GucContext::PGC_USERSET
    };
    let mut weighed = vec![
        ("seq_page_cost", pg_sys::seq_page_cost),
        ("random_page_cost", pg_sys::random_page_cost),
    ];
    let walked = crate::price::walked_leaf_page_cost();
    if walked >= 0.0 {
        weighed.push(("warren_surveyor_pg.walked_leaf_page_cost", walked));
    }
    for (name, value) in weighed {
        let name = CString::new(name).expect("a setting's name");
        // a cost set near its largest value is weighed to that value, which the setting takes
        let value =
            CString::new(format!("{}", (value * heat).min(MOST_PAGE_COST))).expect("a number");
        pg_sys::set_config_option(
            name.as_ptr(),
            value.as_ptr(),
            context,
            pg_sys::GucSource::PGC_S_SESSION,
            pg_sys::GucAction::GUC_ACTION_SAVE,
            true,
            0,
            false,
        );
    }
    let planned = PgTryBuilder::new(std::panic::AssertUnwindSafe(plan))
        .catch_others(|e| {
            pg_sys::AtEOXact_GUC(false, nest);
            e.rethrow()
        })
        .execute();
    pg_sys::AtEOXact_GUC(false, nest);
    planned
}

/// The busy share of the drive holding tablespace `space`, read once in the round.
pub(crate) unsafe fn busy_share(space: pg_sys::Oid) -> f64 {
    #[cfg(any(test, feature = "pg_test"))]
    if let Some(share) = tests::BUSY_SHARE.get() {
        return share;
    }
    let space = if space == pg_sys::InvalidOid {
        pg_sys::MyDatabaseTableSpace
    } else {
        space
    };
    if round::depth() > 0 {
        if let Some(kept) = BUSY.with(|b| b.borrow().get(&space).copied()) {
            return kept;
        }
    }
    let share = directory(space).map_or(0.0, |d| read_busy_share(&d));
    if round::depth() > 0 {
        BUSY.with(|b| b.borrow_mut().insert(space, share));
    }
    share
}

/// The directory of tablespace `space` in this database.
unsafe fn directory(space: pg_sys::Oid) -> Option<std::path::PathBuf> {
    if pg_sys::DataDir.is_null() {
        return None;
    }
    let data = std::ffi::CStr::from_ptr(pg_sys::DataDir)
        .to_string_lossy()
        .into_owned();
    let data = std::path::Path::new(&data);
    Some(if space == pg_sys::DEFAULTTABLESPACE_OID {
        data.join("base")
            .join(pg_sys::MyDatabaseId.to_u32().to_string())
    } else if space == pg_sys::GLOBALTABLESPACE_OID {
        data.join("global")
    } else {
        data.join("pg_tblspc").join(space.to_u32().to_string())
    })
}

/// The busy share of the block device holding `dir`.
#[cfg(target_os = "linux")]
fn read_busy_share(dir: &std::path::Path) -> f64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::metadata(dir) else {
        return 0.0;
    };
    let dev = meta.dev();
    let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0x0000_0fff);
    let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0x0000_00ff);
    let Ok(stats) = std::fs::read_to_string("/proc/diskstats") else {
        return 0.0;
    };
    let busy_ms = io_ticks(&stats, major, minor).or_else(|| {
        // a partition with no line of its own: the disk it lies on
        let sys = format!("/sys/dev/block/{major}:{minor}");
        let parent = std::fs::canonicalize(&sys).ok()?.parent()?.join("dev");
        let numbers = std::fs::read_to_string(parent).ok()?;
        let (a, b) = numbers.trim().split_once(':')?;
        io_ticks(&stats, a.parse().ok()?, b.parse().ok()?)
    });
    let up_ms = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .map(|s| s * 1000.0);
    match (busy_ms, up_ms) {
        (Some(busy), Some(up)) if up > 0.0 => (busy / up).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

#[cfg(not(target_os = "linux"))]
fn read_busy_share(_dir: &std::path::Path) -> f64 {
    0.0
}

/// The milliseconds the device `major`:`minor` has spent doing I/O, from `/proc/diskstats`: the
/// thirteenth field of its line.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn io_ticks(stats: &str, major: u64, minor: u64) -> Option<f64> {
    stats.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.len() >= 13
            && fields[0].parse::<u64>().ok()? == major
            && fields[1].parse::<u64>().ok()? == minor)
            .then(|| fields[12].parse::<f64>().ok())
            .flatten()
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
pub(crate) mod tests {
    use crate::query::cells;
    use pgrx::prelude::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::{CStr, CString};

    thread_local! {
        /// The busy share every drive is taken to have, where a test gives one; none reads the
        /// drive.
        pub(crate) static BUSY_SHARE: Cell<Option<f64>> = const { Cell::new(Some(0.0)) };
    }

    #[pg_test]
    fn the_busy_share_of_the_databases_drive_is_read_from_its_line_of_the_system_statistics() {
        BUSY_SHARE.set(None);
        let share = unsafe { super::busy_share(pg_sys::InvalidOid) };
        BUSY_SHARE.set(Some(0.0));
        if cfg!(target_os = "linux") {
            assert!(share > 0.0 && share < 1.0, "{share}");
        } else {
            assert_eq!(share, 0.0);
        }
        // a tablespace whose directory is not there
        BUSY_SHARE.set(None);
        let none = unsafe { super::busy_share(pg_sys::Oid::from(4_000_000_000u32)) };
        BUSY_SHARE.set(Some(0.0));
        assert_eq!(none, 0.0);
    }

    #[pg_test]
    fn the_thirteenth_field_of_a_devices_line_is_its_time_doing_io() {
        let stats = "   7       0 loop0 19653 0 106428 113 0 0 0 0 0 166 113 0 0 0 0 0 0\n \
                     259       2 nvme0n1p2 60285285 27334539 7764779706 4078888870 173715283 \
                     115100211 23843293280 2869220084 0 14544920 2653392414 1066471 0";
        assert_eq!(super::io_ticks(stats, 259, 2), Some(14544920.0));
        assert_eq!(super::io_ticks(stats, 7, 0), Some(166.0));
        assert_eq!(super::io_ticks(stats, 259, 1), None);
        let weight = |share: f64| {
            BUSY_SHARE.set(Some(share));
            let w = unsafe { super::weight(pg_sys::InvalidOid) };
            BUSY_SHARE.set(Some(0.0));
            w
        };
        assert_eq!(weight(0.0), 1.0);
        assert_eq!(weight(0.5), 2.0);
        assert_eq!(weight(0.75), 4.0);
        assert!((weight(1.0) - 100.0).abs() < 1e-9);
    }

    fn texts(sql: &str) -> Vec<String> {
        crate::tests::texts(sql)
    }

    /// Runs `f` with every drive taken to be busy for `share` of the time.
    fn busy<T>(share: f64, f: impl FnOnce() -> T) -> T {
        BUSY_SHARE.set(Some(share));
        let out = f();
        BUSY_SHARE.set(Some(0.0));
        out
    }

    thread_local! {
        static OWN: RefCell<Vec<(String, f64)>> = const { RefCell::new(Vec::new()) };
    }
    static mut NEXT_JOIN: pg_sys::set_join_pathlist_hook_type = None;
    static mut NEXT_UPPER: pg_sys::create_upper_paths_hook_type = None;

    /// The name of the first table under `path`'s relation.
    unsafe fn named(root: *mut pg_sys::PlannerInfo, path: *mut pg_sys::Path) -> String {
        let at = pg_sys::bms_next_member((*(*path).parent).relids, -1);
        let rte = *(*root).simple_rte_array.add(at as usize);
        CStr::from_ptr(pg_sys::get_rel_name((*rte).relid))
            .to_string_lossy()
            .into_owned()
    }

    /// Records each spilling hash join of a join relation once the hooks before it have run: what
    /// it is, and its price less its inputs'.
    #[pg_guard]
    unsafe extern "C-unwind" fn record_joins(
        root: *mut pg_sys::PlannerInfo,
        joinrel: *mut pg_sys::RelOptInfo,
        outerrel: *mut pg_sys::RelOptInfo,
        innerrel: *mut pg_sys::RelOptInfo,
        jointype: pg_sys::JoinType::Type,
        extra: *mut pg_sys::JoinPathExtraData,
    ) {
        if let Some(next) = NEXT_JOIN {
            next(root, joinrel, outerrel, innerrel, jointype, extra);
        }
        for p in cells((*joinrel).pathlist) {
            let path = p as *mut pg_sys::Path;
            if (*path).type_ != pg_sys::NodeTag::T_HashPath {
                continue;
            }
            let hash = path as *mut pg_sys::HashPath;
            let (outer, inner) = ((*hash).jpath.outerjoinpath, (*hash).jpath.innerjoinpath);
            let kind = format!(
                "hash of {} probed by {}, {} batches",
                named(root, inner),
                named(root, outer),
                (*hash).num_batches
            );
            let own = (*path).total_cost - (*outer).total_cost - (*inner).total_cost;
            OWN.with(|o| o.borrow_mut().push((kind, own)));
        }
    }

    /// Records each sort and grouping of an upper stage once the hooks before it have run: what it
    /// is, and its price less its input's.
    #[pg_guard]
    unsafe extern "C-unwind" fn record_upper(
        root: *mut pg_sys::PlannerInfo,
        stage: pg_sys::UpperRelationKind::Type,
        input_rel: *mut pg_sys::RelOptInfo,
        output_rel: *mut pg_sys::RelOptInfo,
        extra: *mut std::ffi::c_void,
    ) {
        if let Some(next) = NEXT_UPPER {
            next(root, stage, input_rel, output_rel, extra);
        }
        for p in cells((*output_rel).pathlist) {
            let path = p as *mut pg_sys::Path;
            let (kind, under) = match (*path).type_ {
                pg_sys::NodeTag::T_SortPath => (
                    "sort".to_string(),
                    (*(path as *mut pg_sys::SortPath)).subpath,
                ),
                pg_sys::NodeTag::T_AggPath => {
                    let agg = path as *mut pg_sys::AggPath;
                    let strategy = match (*agg).aggstrategy {
                        pg_sys::AggStrategy::AGG_HASHED => "hashed",
                        pg_sys::AggStrategy::AGG_SORTED => "sorted",
                        _ => "other",
                    };
                    (format!("grouping {strategy}"), (*agg).subpath)
                }
                _ => continue,
            };
            let own = (*path).total_cost - (*under).total_cost;
            OWN.with(|o| {
                o.borrow_mut()
                    .push((format!("{kind} over {:?}", (*under).pathtype), own))
            });
        }
    }

    /// What each recorded path cost beyond its inputs while `query` was planned after `settings`,
    /// numbered among its kind.
    fn own_of(settings: &str, query: &str) -> Vec<(String, f64)> {
        OWN.with(|o| o.borrow_mut().clear());
        unsafe {
            NEXT_JOIN = pg_sys::set_join_pathlist_hook;
            pg_sys::set_join_pathlist_hook = Some(record_joins);
            NEXT_UPPER = pg_sys::create_upper_paths_hook;
            pg_sys::create_upper_paths_hook = Some(record_upper);
        }
        let planned = Spi::run(&format!("{settings}; EXPLAIN {query}"));
        unsafe {
            pg_sys::set_join_pathlist_hook = NEXT_JOIN;
            pg_sys::create_upper_paths_hook = NEXT_UPPER;
        }
        Spi::run(
            "RESET seq_page_cost; RESET random_page_cost; RESET enable_mergejoin; \
             RESET enable_nestloop",
        )
        .unwrap();
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        let mut seen: Vec<(String, f64)> = Vec::new();
        for (kind, own) in OWN.with(|o| o.borrow().clone()) {
            let n = seen.iter().filter(|(k, _)| k.starts_with(&kind)).count();
            seen.push((format!("{kind} #{n}"), own));
        }
        seen
    }

    /// 20,000 purchases of 200,000 parts, a surveyor on them, and 60,000 parts written all-visible
    /// with a primary key in shared buffers and a surveyor.
    fn parts() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_prewarm; \
             CREATE TABLE bought AS SELECT (g * 7919) % 200000 + 1 AS id FROM generate_series(1, 20000) g; \
             CREATE INDEX bought_order ON bought USING surveyor (id); \
             CREATE TABLE parts (id int, pad int DEFAULT 0); \
             COPY parts (id) FROM PROGRAM 'seq 1 60000' WITH (FREEZE); \
             ALTER TABLE parts ADD PRIMARY KEY (id); \
             CREATE INDEX parts_order ON parts USING surveyor (id); \
             ANALYZE bought; ANALYZE parts; \
             SELECT pg_prewarm('parts_pkey'); \
             SET LOCAL max_parallel_workers_per_gather = 0; SET LOCAL hash_mem_multiplier = 1; \
             SET LOCAL work_mem = '700kB'",
        )
        .unwrap();
    }

    /// Each recorded path's price beyond its inputs, at the page costs and with the pages free, the
    /// drive idle and half busy: each one found in all four, with the part the page costs make.
    fn doubled(settings: &str, query: &str) -> Vec<String> {
        let free =
            format!("{settings}; SET LOCAL seq_page_cost = 0; SET LOCAL random_page_cost = 0");
        let (priced, nothing) = (own_of(settings, query), own_of(&free, query));
        let (priced_busy, nothing_busy) =
            busy(0.5, || (own_of(settings, query), own_of(&free, query)));
        let at = |paths: &[(String, f64)], kind: &str| {
            paths.iter().find(|(k, _)| k == kind).map(|(_, t)| *t)
        };
        let mut weighed = Vec::new();
        for (kind, own) in &priced {
            let (Some(f), Some(pb), Some(fb)) = (
                at(&nothing, kind),
                at(&priced_busy, kind),
                at(&nothing_busy, kind),
            ) else {
                continue;
            };
            let pages = own - f;
            // the rest of the price stays, and the part the page costs make doubles
            assert!(
                (fb - f).abs() <= 1e-9 * f.abs().max(1.0),
                "{kind}: {f} {fb}"
            );
            assert!(
                (pb - fb - 2.0 * pages).abs() <= 1e-9 * pb.abs().max(1.0),
                "{kind}: {own} {f} {pb} {fb}"
            );
            if pages > 1e-6 * own.abs().max(1.0) {
                weighed.push(kind.clone());
            }
        }
        weighed
    }

    #[pg_test]
    fn a_busy_drive_weighs_exactly_the_temporary_pages_of_a_hash_join_and_nothing_else() {
        parts();
        let query = "SELECT count(*) FROM bought b JOIN parts p ON p.id = b.id";
        // with hash joins alone, which a busy drive would otherwise drop
        let settings = "SET LOCAL enable_mergejoin = off; SET LOCAL enable_nestloop = off";
        let weighed = doubled(settings, query);
        for inner in ["hash of bought", "hash of parts"] {
            assert!(
                weighed
                    .iter()
                    .any(|k| k.contains(inner) && !k.contains(", 1 batches")),
                "{inner}: {weighed:?}"
            );
        }
    }

    #[pg_test]
    fn a_busy_drive_weighs_exactly_the_spill_of_a_sort_and_of_a_hashed_grouping() {
        parts();
        Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
        let sorted = doubled("SELECT 1", "SELECT id FROM bought ORDER BY id");
        assert!(sorted.iter().any(|k| k.starts_with("sort")), "{sorted:?}");
        Spi::run("SET LOCAL enable_sort = off").unwrap();
        let grouped = doubled("SELECT 1", "SELECT id, count(*) FROM bought GROUP BY id");
        assert!(
            grouped.iter().any(|k| k.starts_with("grouping hashed")),
            "{grouped:?}"
        );
    }

    #[pg_test]
    fn a_busy_drive_moves_the_choice_to_a_merge_join_that_does_not_spill() {
        parts();
        // 20,000 purchases of 50 parts spread over all of them
        Spi::run(
            "TRUNCATE bought; INSERT INTO bought SELECT (g % 50) * 1201 + 1 FROM generate_series(1, 20000) g; \
             ANALYZE bought; SET LOCAL enable_nestloop = off",
        )
        .unwrap();
        let query = "SELECT b.id, count(*) FROM bought b JOIN parts p ON p.id = b.id GROUP BY b.id";
        let chosen = |share: f64| {
            busy(share, || {
                let plan = texts(&format!("EXPLAIN {query}")).join("\n");
                if plan.contains("Merge Join") {
                    "merge"
                } else if plan.contains("Hash Join") {
                    "hash"
                } else {
                    panic!("{plan}")
                }
            })
        };
        // idle, the hash join that spills; busy, the merge join that does not
        assert_eq!(chosen(0.0), "hash");
        assert_eq!(chosen(0.5), "merge");
        assert_eq!(chosen(0.75), "merge");
    }

    thread_local! {
        static SEEN_COSTS: RefCell<Vec<(f64, f64, f64)>> = const { RefCell::new(Vec::new()) };
    }
    static mut NEXT_PATHLIST: pg_sys::set_rel_pathlist_hook_type = None;

    /// Records the page costs in force while a relation's paths are made.
    #[pg_guard]
    unsafe extern "C-unwind" fn record_costs(
        root: *mut pg_sys::PlannerInfo,
        rel: *mut pg_sys::RelOptInfo,
        rti: pg_sys::Index,
        rte: *mut pg_sys::RangeTblEntry,
    ) {
        if let Some(next) = NEXT_PATHLIST {
            next(root, rel, rti, rte);
        }
        SEEN_COSTS.with(|s| {
            s.borrow_mut().push((
                pg_sys::seq_page_cost,
                pg_sys::random_page_cost,
                crate::price::walked_leaf_page_cost(),
            ))
        });
    }

    /// The page costs in force while `query` was planned.
    fn costs_while_planning(query: &str) -> Vec<(f64, f64, f64)> {
        SEEN_COSTS.with(|s| s.borrow_mut().clear());
        unsafe {
            NEXT_PATHLIST = pg_sys::set_rel_pathlist_hook;
            pg_sys::set_rel_pathlist_hook = Some(record_costs);
        }
        let planned = Spi::run(&format!("EXPLAIN {query}"));
        unsafe { pg_sys::set_rel_pathlist_hook = NEXT_PATHLIST };
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        SEEN_COSTS.with(|s| s.borrow().clone())
    }

    /// The page costs in force now.
    fn costs_now() -> (f64, f64, f64) {
        unsafe {
            (
                pg_sys::seq_page_cost,
                pg_sys::random_page_cost,
                crate::price::walked_leaf_page_cost(),
            )
        }
    }

    #[pg_test]
    fn a_statement_on_a_busy_drive_is_planned_at_weighed_page_costs_put_back_after() {
        Spi::run(
            "CREATE TABLE watched AS SELECT g AS id FROM generate_series(1, 1000) g; \
             CREATE INDEX watched_order ON watched USING surveyor (id); \
             CREATE TABLE unwatched AS SELECT g AS id FROM generate_series(1, 1000) g; \
             ANALYZE watched; ANALYZE unwatched; \
             SET LOCAL seq_page_cost = 1.5; SET LOCAL random_page_cost = 6; \
             SET LOCAL warren_surveyor_pg.walked_leaf_page_cost = 0.75",
        )
        .unwrap();
        let before = costs_now();
        assert_eq!(before, (1.5, 6.0, 0.75));
        for (share, query, heat) in [
            (0.0, "SELECT id FROM watched WHERE id < 10", 1.0),
            (0.5, "SELECT id FROM watched WHERE id < 10", 2.0),
            (
                0.75,
                "SELECT u.id FROM unwatched u JOIN watched w ON w.id = u.id",
                4.0,
            ),
            (0.75, "SELECT id FROM unwatched WHERE id < 10", 1.0),
            (
                0.75,
                "SELECT id FROM unwatched WHERE id IN (SELECT id FROM watched)",
                4.0,
            ),
        ] {
            let seen = busy(share, || costs_while_planning(query));
            assert!(!seen.is_empty(), "{query}");
            for costs in seen {
                assert_eq!(
                    costs,
                    (1.5 * heat, 6.0 * heat, 0.75 * heat),
                    "{share} {query}"
                );
            }
            assert_eq!(costs_now(), before, "{share} {query}");
            assert_eq!(
                texts("SELECT current_setting('seq_page_cost') || ' ' || current_setting('random_page_cost')"),
                vec!["1.5 6".to_string()]
            );
        }
        // with the walked-leaf price unset, it stays unset
        Spi::run("RESET warren_surveyor_pg.walked_leaf_page_cost").unwrap();
        for costs in busy(0.5, || costs_while_planning("SELECT id FROM watched")) {
            assert_eq!(costs, (3.0, 12.0, -1.0));
        }
    }

    #[pg_test]
    fn a_statement_naming_an_oid_that_is_not_a_table_is_planned_and_runs() {
        Spi::run(
            "CREATE TABLE watched AS SELECT g AS id FROM generate_series(1, 1000) g; \
             CREATE INDEX watched_order ON watched USING surveyor (id); \
             ANALYZE watched",
        )
        .unwrap();
        // a type's OID, as psql's \dx+ and many catalog queries write one
        assert_eq!(
            texts("SELECT typname::text FROM pg_type WHERE oid = '23'"),
            vec!["int4"]
        );
        // a large object's OID, as a restore writes one
        assert_eq!(
            texts("SELECT lo_create('4000000001')::text"),
            vec!["4000000001"]
        );
        // an OID naming nothing, written as a table's name type
        assert!(
            texts("SELECT relname::text FROM pg_class WHERE oid = '4000000002'::regclass")
                .is_empty()
        );
        // beside a table that carries a surveyor
        assert_eq!(
            texts(
                "SELECT count(*)::text FROM watched w JOIN pg_type t ON t.oid = '23' \
                 WHERE w.id < 10 AND w.tableoid = 'watched'::regclass"
            ),
            vec!["9"]
        );
    }

    #[pg_test]
    fn a_statement_is_weighed_by_the_children_and_partitions_it_reads_and_by_none_under_only() {
        Spi::run(
            "CREATE TABLE kin (id int); \
             CREATE TABLE kin_a () INHERITS (kin); \
             INSERT INTO kin_a SELECT g FROM generate_series(1, 1000) g; \
             CREATE INDEX kin_a_order ON kin_a USING surveyor (id); \
             CREATE TABLE lots (id int) PARTITION BY RANGE (id); \
             CREATE TABLE lots_low PARTITION OF lots FOR VALUES FROM (1) TO (1001); \
             CREATE TABLE lots_high PARTITION OF lots FOR VALUES FROM (1001) TO (2001); \
             INSERT INTO lots SELECT g FROM generate_series(1, 2000) g; \
             CREATE INDEX lots_high_order ON lots_high USING surveyor (id); \
             CREATE VIEW kin_seen AS SELECT id FROM kin; \
             ANALYZE kin; ANALYZE kin_a; ANALYZE lots; \
             SET LOCAL seq_page_cost = 1.5; SET LOCAL random_page_cost = 6",
        )
        .unwrap();
        for (query, heat) in [
            ("SELECT id FROM kin WHERE id < 10", 2.0),
            ("SELECT id FROM kin_seen WHERE id < 10", 2.0),
            ("SELECT id FROM lots", 2.0),
            ("SELECT id FROM ONLY kin WHERE id < 10", 1.0),
            // a partition read alone, beside the one that carries a surveyor
            ("SELECT id FROM lots_low WHERE id < 10", 1.0),
        ] {
            let seen = busy(0.5, || costs_while_planning(query));
            assert!(!seen.is_empty(), "{query}");
            for (seq, random, _) in seen {
                assert_eq!((seq, random), (1.5 * heat, 6.0 * heat), "{query}");
            }
        }
    }

    #[pg_test]
    fn a_page_cost_near_its_largest_value_is_weighed_to_its_largest_value() {
        Spi::run(
            "CREATE TABLE watched AS SELECT g AS id FROM generate_series(1, 1000) g; \
             CREATE INDEX watched_order ON watched USING surveyor (id); \
             ANALYZE watched; \
             SET LOCAL seq_page_cost = 1.5; SET LOCAL random_page_cost = 1e308; \
             SET LOCAL warren_surveyor_pg.walked_leaf_page_cost = 1e308",
        )
        .unwrap();
        let before = costs_now();
        let seen = busy(0.5, || {
            costs_while_planning("SELECT id FROM watched WHERE id < 10")
        });
        assert!(!seen.is_empty());
        for costs in seen {
            assert_eq!(costs, (3.0, f64::MAX, f64::MAX));
        }
        assert_eq!(costs_now(), before);
    }

    #[pg_test]
    fn the_page_costs_are_put_back_when_planning_fails() {
        Spi::run(
            "CREATE TABLE watched AS SELECT g AS id FROM generate_series(1, 1000) g; \
             CREATE INDEX watched_order ON watched USING surveyor (id); \
             ANALYZE watched",
        )
        .unwrap();
        let before = costs_now();
        // a statement whose planning fails, folding a division by zero
        let failed = busy(0.5, || unsafe {
            let text = CString::new("SELECT 1 / 0 FROM watched").unwrap();
            let raw = pg_sys::pg_parse_query(text.as_ptr());
            let stmt = (*(*raw).elements).ptr_value as *mut pg_sys::RawStmt;
            let queries = pg_sys::pg_analyze_and_rewrite_fixedparams(
                stmt,
                text.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
            );
            let query = (*(*queries).elements).ptr_value as *mut pg_sys::Query;
            let context = pg_sys::CurrentMemoryContext;
            let failed = PgTryBuilder::new(|| {
                #[cfg(not(feature = "pg19"))]
                pg_sys::planner(query, text.as_ptr(), 0, std::ptr::null_mut());
                #[cfg(feature = "pg19")]
                pg_sys::planner(
                    query,
                    text.as_ptr(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                false
            })
            .catch_others(|_| true)
            .execute();
            pg_sys::CurrentMemoryContext = context;
            failed
        });
        assert!(failed);
        assert_eq!(costs_now(), before);
        assert_eq!(crate::round::depth(), 0);
    }

    /// 20,000 purchases of parts 1 to 60,000, a surveyor on them, and 60,000 parts written
    /// all-visible with a primary key in shared buffers and a surveyor.
    fn spread() {
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pg_prewarm; \
             CREATE TABLE ta AS SELECT (g * 7919) % 60000 + 1 AS id FROM generate_series(1, 20000) g; \
             CREATE INDEX ta_order ON ta USING surveyor (id); \
             CREATE TABLE tb (id int, pad int DEFAULT 0); \
             COPY tb (id) FROM PROGRAM 'seq 1 60000' WITH (FREEZE); \
             ALTER TABLE tb ADD PRIMARY KEY (id); \
             CREATE INDEX tb_order ON tb USING surveyor (id); \
             ANALYZE ta; ANALYZE tb; \
             SELECT pg_prewarm('tb_pkey'); \
             SET LOCAL max_parallel_workers_per_gather = 0; SET LOCAL hash_mem_multiplier = 1; \
             SET LOCAL work_mem = '700kB'",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_busy_drive_chooses_a_merge_join_the_join_search_would_otherwise_drop() {
        spread();
        let query = "SELECT count(*) FROM ta JOIN tb ON tb.id = ta.id";
        let chosen = |share: f64| {
            busy(share, || {
                let plan = texts(&format!("EXPLAIN {query}")).join("\n");
                if plan.contains("Merge Join") {
                    "merge"
                } else if plan.contains("Hash Join") {
                    "hash"
                } else {
                    panic!("{plan}")
                }
            })
        };
        // idle, the hash join that spills, priced lower; busy, the merge join, which neither
        // spills nor reads its key from the drive
        assert_eq!(chosen(0.0), "hash");
        assert_eq!(chosen(0.5), "merge");
        assert_eq!(chosen(0.75), "merge");
    }
}

/// Tests that need sessions of their own, outside a test's transaction.
#[cfg(test)]
mod sessions {
    use crate::sessions::session;
    use std::time::{Duration, Instant};

    /// `{name}_parts`, a partitioned table of two partitions, and `{name}_kinds`, an inheritance
    /// parent with no rows of its own over two children, each partition and child carrying a
    /// surveyor, written and committed.
    fn family(db: &mut postgres::Client, name: &str) {
        db.batch_execute(&format!(
            "DROP TABLE IF EXISTS {name}_parts, {name}_kinds CASCADE; \
             CREATE TABLE {name}_parts (id int) PARTITION BY RANGE (id); \
             CREATE TABLE {name}_parts_low PARTITION OF {name}_parts FOR VALUES FROM (1) TO (1001); \
             CREATE TABLE {name}_parts_high PARTITION OF {name}_parts FOR VALUES FROM (1001) TO (2001); \
             INSERT INTO {name}_parts SELECT g FROM generate_series(1, 2000) g; \
             CREATE INDEX {name}_parts_order ON {name}_parts USING surveyor (id); \
             CREATE TABLE {name}_kinds (id int); \
             CREATE TABLE {name}_kinds_a () INHERITS ({name}_kinds); \
             CREATE TABLE {name}_kinds_b () INHERITS ({name}_kinds); \
             INSERT INTO {name}_kinds_a SELECT g FROM generate_series(1, 1000) g; \
             INSERT INTO {name}_kinds_b SELECT g FROM generate_series(1001, 2000) g; \
             CREATE INDEX {name}_kinds_a_order ON {name}_kinds_a USING surveyor (id); \
             CREATE INDEX {name}_kinds_b_order ON {name}_kinds_b USING surveyor (id); \
             ANALYZE {name}_parts; ANALYZE {name}_kinds"
        ))
        .unwrap();
    }

    /// The server's message for `e`.
    fn message(e: &postgres::Error) -> String {
        e.as_db_error()
            .map_or_else(|| e.to_string(), |d| d.message().to_string())
    }

    /// A session with the library loaded, that gives up on a statement after `timeout`.
    fn reader(timeout: &str) -> postgres::Client {
        let mut db = session();
        db.batch_execute(&format!(
            "LOAD 'warren_surveyor_pg'; SET statement_timeout = '{timeout}'"
        ))
        .unwrap();
        db
    }

    #[test]
    fn a_statement_waits_for_no_lock_on_a_table_it_does_not_read() {
        let mut holder = session();
        family(&mut holder, "held");
        holder
            .batch_execute(
                "BEGIN; LOCK TABLE held_parts_high, held_kinds_b IN ACCESS EXCLUSIVE MODE",
            )
            .unwrap();
        let mut db = reader("3s");
        let waited = [
            // a partition the planner prunes
            "SELECT count(*) FROM held_parts WHERE id = 5",
            // the parent alone
            "SELECT count(*) FROM ONLY held_kinds",
            // the lock itself, looked up by the table's name
            "SELECT count(*) FROM pg_locks WHERE relation = 'held_kinds_b'::regclass",
        ]
        .into_iter()
        .filter_map(|query| {
            db.query_one(query, &[])
                .err()
                .map(|e| format!("{query}: {}", message(&e)))
        })
        .collect::<Vec<_>>();
        holder
            .batch_execute("COMMIT; DROP TABLE held_parts, held_kinds CASCADE")
            .unwrap();
        assert!(waited.is_empty(), "{waited:#?}");
    }

    #[test]
    fn a_statement_over_an_inheritance_parent_runs_when_a_child_is_dropped_while_it_waits() {
        let mut dropper = session();
        family(&mut dropper, "dropped");
        dropper
            .batch_execute("BEGIN; DROP TABLE dropped_kinds_b")
            .unwrap();
        let mut db = reader("60s");
        let pid: i32 = db.query_one("SELECT pg_backend_pid()", &[]).unwrap().get(0);
        let read = std::thread::spawn(move || {
            db.query_one("SELECT count(*) FROM dropped_kinds", &[])
                .map(|row| row.get::<_, i64>(0))
                .map_err(|e| message(&e))
        });
        // the read waits for the dropped child's lock, and the drop is committed
        let mut watcher = session();
        let start = Instant::now();
        while !watcher
            .query_one(
                "SELECT coalesce(bool_or(wait_event_type = 'Lock'), false) \
                 FROM pg_stat_activity WHERE pid = $1",
                &[&pid],
            )
            .unwrap()
            .get::<_, bool>(0)
        {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "the read never waited"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        dropper.batch_execute("COMMIT").unwrap();
        let counted = read.join().unwrap();
        dropper
            .batch_execute("DROP TABLE dropped_parts, dropped_kinds CASCADE")
            .unwrap();
        assert_eq!(counted, Ok(1000));
    }
}
