// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! Grouping one block at a time. A block is what one value, given as a parameter or as a column of
//! an outer row (as in a LATERAL over the values), reaches through the joins of a query level:
//! - a table surveyed for the question, one of whose B-trees leads with a column equated with the
//!   value, or joined to a column of a table already in the block, is entered at that column's
//!   values;
//! - a table joined on its own one-column unique key to a table already in the block, or equated
//!   with the value on it, is read one row at a time;
//! - a subquery read within one value's block lies in it.
//!
//! A table read with its partitions is surveyed through its partitioned surveyor and entered by its
//! partitioned B-trees. A plain inheritance parent read with its children lists no index of its
//! own, so it is read through its members: it is surveyed where every member it reads that holds
//! rows carries a surveyor, and entered at a column where every such member's B-trees lead with
//! that member's own column for it. A member holding no row, as an empty parent holds none, needs
//! neither.
//!
//! Where every table and subquery a GROUP BY reads lies in one value's block, and some table is
//! surveyed for the question, its groups are made by hashing, and its rows are never sorted to group
//! them:
//! - a grouping that sorts its input is left out of the plans the planner chooses from;
//! - a hashed grouping takes its place, made here when the planner kept none;
//! - a grouping that reads its rows already in its order is kept.
//!
//! A value equated with a constant, `enable_hashagg = off`, grouping sets, and aggregates that cannot
//! be hashed leave the plan as the planner chose it. Relations of other kinds (a WITH query, a
//! function) are not asked about.

use crate::query::{bare, btree_leading_columns, cells, members, surveyed, surveyor_am};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;

static mut NEXT_UPPER: pg_sys::create_upper_paths_hook_type = None;

extern "C-unwind" {
    fn get_agg_clause_costs(
        root: *mut pg_sys::PlannerInfo,
        aggsplit: pg_sys::AggSplit::Type,
        costs: *mut pg_sys::AggClauseCosts,
    );
}

/// Puts the hook in place, after any hook already there.
pub fn init() {
    unsafe {
        NEXT_UPPER = pg_sys::create_upper_paths_hook;
        pg_sys::create_upper_paths_hook = Some(upper);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn upper(
    root: *mut pg_sys::PlannerInfo,
    stage: pg_sys::UpperRelationKind::Type,
    input_rel: *mut pg_sys::RelOptInfo,
    output_rel: *mut pg_sys::RelOptInfo,
    extra: *mut c_void,
) {
    if let Some(next) = NEXT_UPPER {
        next(root, stage, input_rel, output_rel, extra);
    }
    if stage == pg_sys::UpperRelationKind::UPPERREL_GROUP_AGG && !extra.is_null() {
        hash_at_one_value(
            root,
            input_rel,
            output_rel,
            extra as *mut pg_sys::GroupPathExtraData,
        );
    }
    // the last stage of a statement's top level: a planning outside a round reads nothing after it
    if stage == pg_sys::UpperRelationKind::UPPERREL_FINAL {
        crate::budget::planned(root);
    }
}

/// A value a column is equated with at a query level that the planner does not see: a parameter,
/// or a column of an outer row.
unsafe fn is_parameter(node: *mut pg_sys::Node) -> bool {
    !node.is_null() && (*node).type_ == pg_sys::NodeTag::T_Param
}

/// Columns an equivalence class equates, as (range table index, column), and the parameter it
/// equates them with, if any.
type Class = (Vec<(u32, i16)>, Option<*mut pg_sys::Node>);

/// The columns of `root`'s level each equivalence class equates, as (range table index, column),
/// and the parameter it equates them with, if any.
unsafe fn classes(root: *mut pg_sys::PlannerInfo) -> Vec<Class> {
    let mut out = Vec::new();
    for ec in cells((*root).eq_classes) {
        let ec = ec as *mut pg_sys::EquivalenceClass;
        if (*ec).ec_has_volatile || (*ec).ec_broken {
            continue;
        }
        let mut columns = Vec::new();
        let mut value = None;
        for m in cells((*ec).ec_members) {
            let m = m as *mut pg_sys::EquivalenceMember;
            let e = bare((*m).em_expr as *mut pg_sys::Node);
            if (*m).em_is_const {
                if value.is_none() && is_parameter(e) {
                    value = Some(e);
                }
                continue;
            }
            if e.is_null() || (*e).type_ != pg_sys::NodeTag::T_Var {
                continue;
            }
            let var = e as *mut pg_sys::Var;
            if (*var).varlevelsup == 0 && (*var).varattno > 0 {
                columns.push(((*var).varno as u32, (*var).varattno));
            }
        }
        out.push((columns, value));
    }
    out
}

/// The leading key column of each of a table's valid B-trees over the whole table, and the columns
/// a valid one-column unique index over the whole table covers.
#[derive(Clone)]
struct Keys {
    leading: Vec<i16>,
    unique: Vec<i16>,
}

impl Keys {
    /// Whether a row is found by `column` within a block: a B-tree it leads, on a table `surveyed`,
    /// is entered at the block's values of it, or it is the table's own unique key.
    fn enters(&self, column: i16, surveyed: bool) -> bool {
        (surveyed && self.leading.contains(&column)) || self.unique.contains(&column)
    }
}

unsafe fn keys(relid: pg_sys::Oid) -> Keys {
    Keys {
        leading: btree_leading_columns(relid),
        unique: unique_columns(relid),
    }
}

/// The keys of the plain inheritance parent at `varno` of `root`'s range table, read with its
/// children, and whether it is surveyed for the question: it is where every member it reads that
/// holds rows carries a surveyor, and it is entered at a column where every such member's B-trees
/// lead with that member's own column for it. A member holding no row, as an empty parent holds
/// none, adds no row to any block and needs neither. No unique key holds across its members.
unsafe fn through_members(root: *mut pg_sys::PlannerInfo, varno: pg_sys::Index) -> (Keys, bool) {
    let members = members(root, varno);
    let holding: Vec<_> = members.iter().filter(|m| (*m.rel).tuples > 0.0).collect();
    let surveyed = !holding.is_empty() && holding.iter().all(|m| surveyed(m.rel, true));
    let mut leading: Option<Vec<i16>> = None;
    for m in holding {
        let own = btree_leading_columns((**(*root).simple_rte_array.add(m.varno as usize)).relid);
        let columns: Vec<i16> = (1..=m.columns.len() as i16)
            .filter(|&at| m.column(at).is_some_and(|c| own.contains(&c)))
            .collect();
        leading = Some(match leading {
            None => columns,
            Some(l) => l.into_iter().filter(|c| columns.contains(c)).collect(),
        });
    }
    (
        Keys {
            leading: leading.unwrap_or_default(),
            unique: Vec::new(),
        },
        surveyed,
    )
}

/// How the relations `relids` of `root`'s level are read.
enum Read {
    /// None of them is surveyed, and no subquery among them is read at one value.
    Plain,
    /// Each is read within the block of one of these values: entered at the blocks of a B-tree's
    /// leading column that its joins connect to the value, or reached by its own unique column from
    /// a row already in the block.
    At(Vec<*mut pg_sys::Node>),
    /// Some relation is read outside any one value's block.
    Whole,
}

/// Whether `value` is a parameter the level of the subquery `rel` sets for it from its own rows.
unsafe fn passed_down(rel: *mut pg_sys::RelOptInfo, value: *mut pg_sys::Node) -> bool {
    let param = value as *mut pg_sys::Param;
    (*param).paramkind == pg_sys::ParamKind::PARAM_EXEC
        && cells((*rel).subplan_params)
            .into_iter()
            .any(|item| (*(item as *mut pg_sys::PlannerParamItem)).paramId == (*param).paramid)
}

unsafe fn read_at(
    root: *mut pg_sys::PlannerInfo,
    relids: *mut pg_sys::Bitmapset,
    known: &mut HashMap<pg_sys::Oid, Keys>,
) -> Read {
    // the tables of this level, whether each is surveyed, and the subqueries with the values each
    // is read at
    let mut tables: Vec<(u32, Keys, bool)> = Vec::new();
    let mut subqueries: Vec<(u32, Vec<*mut pg_sys::Node>)> = Vec::new();
    let mut i = pg_sys::bms_next_member(relids, -1);
    while i >= 0 {
        let index = i as usize;
        i = pg_sys::bms_next_member(relids, i);
        if index >= (*root).simple_rel_array_size as usize {
            continue;
        }
        let rel = *(*root).simple_rel_array.add(index);
        let rte = *(*root).simple_rte_array.add(index);
        if rel.is_null() || rte.is_null() {
            continue;
        }
        match (*rte).rtekind {
            pg_sys::RTEKind::RTE_RELATION => {
                let (keys, surveyed) = if (*rte).inh
                    && (*rte).relkind != pg_sys::RELKIND_PARTITIONED_TABLE as std::ffi::c_char
                {
                    // a plain inheritance parent lists no index of its own: it is read
                    // through its members
                    through_members(root, index as pg_sys::Index)
                } else {
                    let keys = known
                        .entry((*rte).relid)
                        .or_insert_with(|| keys((*rte).relid))
                        .clone();
                    (keys, surveyed(rel, true))
                };
                tables.push((index as u32, keys, surveyed));
            }
            pg_sys::RTEKind::RTE_SUBQUERY if !(*rel).subroot.is_null() => {
                let sub = (*rel).subroot;
                match read_at(sub, (*sub).all_baserels, known) {
                    Read::Plain => {}
                    // a parameter this level passes the subquery changes with this level's rows
                    Read::At(values) => subqueries.push((
                        index as u32,
                        values
                            .into_iter()
                            .filter(|&v| !passed_down(rel, v))
                            .collect(),
                    )),
                    Read::Whole => return Read::Whole,
                }
            }
            _ => {}
        }
    }
    if subqueries.is_empty() && tables.iter().all(|(_, _, surveyed)| !surveyed) {
        return Read::Plain;
    }
    let classes = classes(root);
    let enters = |rti: u32, column: i16| {
        tables
            .iter()
            .any(|(t, keys, surveyed)| *t == rti && keys.enters(column, *surveyed))
    };
    // every value a column is equated with, or a subquery is read at
    let mut values: Vec<*mut pg_sys::Node> = Vec::new();
    for v in classes
        .iter()
        .filter_map(|(_, value)| *value)
        .chain(subqueries.iter().flat_map(|(_, vs)| vs.iter().copied()))
    {
        if !values
            .iter()
            .any(|&w| pg_sys::equal(v as *const c_void, w as *const c_void))
        {
            values.push(v);
        }
    }
    let mut at = Vec::new();
    for v in values {
        let same = |w: Option<*mut pg_sys::Node>| {
            w.is_some_and(|w| pg_sys::equal(v as *const c_void, w as *const c_void))
        };
        // the block begins at what is equated with the value, and follows the joins
        let mut block: Vec<u32> = subqueries
            .iter()
            .filter(|(_, vs)| vs.iter().any(|&w| same(Some(w))))
            .map(|&(rti, _)| rti)
            .collect();
        for (columns, value) in &classes {
            if same(*value) {
                for &(rti, column) in columns {
                    if enters(rti, column) && !block.contains(&rti) {
                        block.push(rti);
                    }
                }
            }
        }
        loop {
            let before = block.len();
            for (columns, value) in &classes {
                if value.is_some() || !columns.iter().any(|(rti, _)| block.contains(rti)) {
                    continue;
                }
                for &(rti, column) in columns {
                    if !block.contains(&rti) && enters(rti, column) {
                        block.push(rti);
                    }
                }
            }
            if block.len() == before {
                break;
            }
        }
        let whole = tables.iter().any(|(rti, _, _)| !block.contains(rti))
            || subqueries.iter().any(|(rti, _)| !block.contains(rti));
        if !whole {
            at.push(v);
        }
    }
    if at.is_empty() {
        Read::Whole
    } else {
        Read::At(at)
    }
}

/// The columns of `table` that a valid, immediate, one-column unique index over the whole table
/// covers.
unsafe fn unique_columns(table: pg_sys::Oid) -> Vec<i16> {
    let rel = pg_sys::relation_open(table, pg_sys::NoLock as pg_sys::LOCKMODE);
    let mut unique = Vec::new();
    for index in cells_oid(pg_sys::RelationGetIndexList(rel)) {
        let idx = pg_sys::index_open(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        let form = (*idx).rd_index;
        if (*form).indisunique
            && (*form).indimmediate
            && (*form).indisvalid
            && (*form).indnkeyatts == 1
            && pg_sys::RelationGetIndexPredicate(idx).is_null()
        {
            let column = *(*form).indkey.values.as_ptr();
            if column > 0 {
                unique.push(column);
            }
        }
        pg_sys::index_close(idx, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    }
    pg_sys::relation_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    unique
}

/// The OIDs of a list of them.
unsafe fn cells_oid(list: *mut pg_sys::List) -> Vec<pg_sys::Oid> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).oid_value)
        .collect()
}

/// Whether `path` groups rows it sorts first, the rows workers sort and gather in order included.
unsafe fn sorts_to_group(path: *mut pg_sys::Path) -> bool {
    let subpath = match (*path).type_ {
        pg_sys::NodeTag::T_AggPath => {
            let agg = path as *mut pg_sys::AggPath;
            if (*agg).aggstrategy != pg_sys::AggStrategy::AGG_SORTED {
                return false;
            }
            (*agg).subpath
        }
        pg_sys::NodeTag::T_GroupPath => (*(path as *mut pg_sys::GroupPath)).subpath,
        _ => return false,
    };
    sorted(subpath)
}

/// Whether `path` sorts its rows, or gathers in order rows that workers sort or group sorted.
unsafe fn sorted(path: *mut pg_sys::Path) -> bool {
    if path.is_null() {
        return false;
    }
    match (*path).pathtype {
        pg_sys::NodeTag::T_Sort | pg_sys::NodeTag::T_IncrementalSort => true,
        pg_sys::NodeTag::T_GatherMerge if (*path).type_ == pg_sys::NodeTag::T_GatherMergePath => {
            let under = (*(path as *mut pg_sys::GatherMergePath)).subpath;
            sorted(under) || (!under.is_null() && sorts_to_group(under))
        }
        _ => false,
    }
}

unsafe fn is_hashed(path: *mut pg_sys::Path) -> bool {
    (*path).type_ == pg_sys::NodeTag::T_AggPath
        && (*(path as *mut pg_sys::AggPath)).aggstrategy == pg_sys::AggStrategy::AGG_HASHED
}

unsafe fn hash_at_one_value(
    root: *mut pg_sys::PlannerInfo,
    input_rel: *mut pg_sys::RelOptInfo,
    grouped_rel: *mut pg_sys::RelOptInfo,
    extra: *mut pg_sys::GroupPathExtraData,
) {
    let parse = (*root).parse;
    if !pg_sys::enable_hashagg
        || (*extra).flags & pg_sys::GROUPING_CAN_USE_HASH as i32 == 0
        || (*extra).patype != pg_sys::PartitionwiseAggregateType::PARTITIONWISE_AGGREGATE_NONE
        || (*root).processed_groupClause.is_null()
        || !(*parse).groupingSets.is_null()
        || (*input_rel).cheapest_total_path.is_null()
    {
        return;
    }
    let paths: Vec<*mut pg_sys::Path> = cells((*grouped_rel).pathlist)
        .into_iter()
        .map(|p| p as *mut pg_sys::Path)
        .collect();
    let sorting: Vec<_> = paths
        .iter()
        .copied()
        .filter(|&p| sorts_to_group(p))
        .collect();
    if sorting.is_empty() {
        return;
    }
    if surveyor_am() == pg_sys::InvalidOid {
        return;
    }
    let mut leading = HashMap::new();
    if !matches!(
        read_at(root, (*input_rel).relids, &mut leading),
        Read::At(_)
    ) {
        return;
    }
    let mut kept: *mut pg_sys::List = null_mut();
    for &p in &paths {
        if !sorts_to_group(p) {
            kept = pg_sys::lappend(kept, p as *mut c_void);
        }
    }
    let hashed = paths.iter().any(|&p| is_hashed(p));
    (*grouped_rel).pathlist = kept;
    if !hashed {
        let groups = sorting
            .iter()
            .find(|&&p| (*p).type_ == pg_sys::NodeTag::T_AggPath)
            .map(|&p| (*(p as *mut pg_sys::AggPath)).numGroups)
            .unwrap_or((*sorting[0]).rows);
        let mut costs = pg_sys::AggClauseCosts::default();
        get_agg_clause_costs(root, pg_sys::AggSplit::AGGSPLIT_SIMPLE, &mut costs);
        let path = pg_sys::create_agg_path(
            root,
            grouped_rel,
            (*input_rel).cheapest_total_path,
            (*grouped_rel).reltarget,
            pg_sys::AggStrategy::AGG_HASHED,
            pg_sys::AggSplit::AGGSPLIT_SIMPLE,
            (*root).processed_groupClause,
            (*extra).havingQual as *mut pg_sys::List,
            &costs,
            groups,
        );
        pg_sys::add_path(grouped_rel, path as *mut pg_sys::Path);
    }
}
