// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The constant conditions on a relation that one of its B-trees holds, and the rows the B-tree
//! measures for them.
//!
//! A B-tree holds equalities, lists of values and NULL tests on its key columns, and then at most
//! one range on the next column. A key column before the last one a condition holds, and that no
//! condition fixes, is stepped through the values the B-tree holds for it, as a skip scan steps
//! it. Each list value and each value stepped through makes a block of the key, measured on its
//! own, and the blocks' rows are added. A block inside one leaf or two is counted on those leaves,
//! every entry between its ends. Where ANALYZE read every row of the table
//! (`query::every_row_analyzed`), a value of the leading key column that its statistics name among
//! their most common values takes the share they give it, as the planner reads it, and no leaf is
//! read to count it. A block inside one leaf or two whose leaves the pages left to the read do not
//! reach takes the planner's own share of its conditions (an equality or a NULL test for each value
//! it names, and the range), held between none and those pages' rows. A value the planner can fold
//! to a constant counts; a parameter of a generic plan, or a column of another relation, does not.
//!
//! A block spanning whole leaves is counted from a few of its own leaves where every column the
//! index stores is of one width and those leaves hold the same rows. Otherwise, once every block
//! is measured, it is read whole, every entry on every one of its leaves counted, in the key's
//! order while the pages read stay within the pages of the table and what is left of the
//! statement's planning-read budget (`budget`); past them, it keeps the count from its own leaves.
//!
//! Which conditions a B-tree holds is read from the conditions alone, before any page is read. A
//! condition compared under a collation other than the one its key column keeps is not held, as
//! PostgreSQL will not scan the B-tree for it: the key's order is not the condition's. A read
//! stepping through a key column is predicted before it starts: a descent, the index's height, for
//! each block, a stepped column making as many blocks as the planner's statistics give it distinct
//! values. Where the prediction passes the pages of the table, or what is left of the statement's
//! planning-read budget, the read never starts and the conditions are left to the planner.

use crate::leaves;
use crate::measure::{self, End, Part};
use crate::query::{bare, cells, distinct_of};
use pgrx::pg_sys;
use std::ffi::c_void;
use std::ptr::null_mut;

const LESS: i32 = 1;
const LESS_EQUAL: i32 = 2;
const EQUAL: i32 = 3;
const GREATER_EQUAL: i32 = 4;
const GREATER: i32 = 5;

/// What one B-tree holds of a relation's conditions: the rows it measures for them, the pages it
/// read, and the conditions.
#[derive(Clone)]
pub(crate) struct Held {
    pub rows: f64,
    pub pages: u32,
    pub clauses: Vec<*mut pg_sys::RestrictInfo>,
}

/// The conditions on one key column.
#[derive(Default)]
struct OnColumn {
    /// The values an equality, a list or a NULL test leaves, each a block.
    values: Option<Vec<Part>>,
    values_from: Vec<*mut pg_sys::RestrictInfo>,
    /// The bound the range starts at in the key's order, and whether the range lies after the
    /// entries equal to it.
    lower: Option<(Part, bool, *mut pg_sys::RestrictInfo)>,
    /// The bound the range ends at in the key's order, and whether the range takes in the entries
    /// equal to it.
    upper: Option<(Part, bool, *mut pg_sys::RestrictInfo)>,
}

unsafe fn tag(node: *mut pg_sys::Node) -> pg_sys::NodeTag {
    (*node).type_
}

/// Which key column of `index` an expression is, where it is one.
pub(crate) unsafe fn column_of(
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    expr: *mut pg_sys::Node,
) -> Option<usize> {
    let expr = bare(expr);
    if expr.is_null() {
        return None;
    }
    let mut expressions = cells((*index).indexprs).into_iter();
    for i in 0..(*index).nkeycolumns as usize {
        let key = *(*index).indexkeys.add(i);
        if key != 0 {
            if tag(expr) == pg_sys::NodeTag::T_Var {
                let v = expr as *mut pg_sys::Var;
                if (*v).varno == (*rel).relid as i32
                    && (*v).varattno == key as i16
                    && (*v).varlevelsup == 0
                {
                    return Some(i);
                }
            }
        } else {
            let e = expressions.next()? as *mut pg_sys::Node;
            if pg_sys::equal(bare(e) as *const c_void, expr as *const c_void) {
                return Some(i);
            }
        }
    }
    None
}

/// Whether key column `at` of `index` orders its values as a condition compared under `collation`
/// compares them: the column keeps no collation, or keeps that one. A key on text kept in one
/// collation's order cannot count a range or an equality written in another's, as PostgreSQL's own
/// planner will not scan it for one.
pub(crate) unsafe fn compares_as(
    index: *mut pg_sys::IndexOptInfo,
    at: usize,
    collation: pg_sys::Oid,
) -> bool {
    let kept = *(*index).indexcollations.add(at);
    kept == pg_sys::InvalidOid || kept == collation
}

/// The constant an expression folds to while planning, where it folds to one.
pub(crate) unsafe fn constant(
    root: *mut pg_sys::PlannerInfo,
    node: *mut pg_sys::Node,
) -> Option<*mut pg_sys::Const> {
    let mut n = bare(node);
    if n.is_null() {
        return None;
    }
    if tag(n) != pg_sys::NodeTag::T_Const {
        n = bare(pg_sys::estimate_expression_value(root, n));
    }
    (!n.is_null() && tag(n) == pg_sys::NodeTag::T_Const).then_some(n as *mut pg_sys::Const)
}

/// The strategy of `opno` in `family`, and the type of its right side.
pub(crate) unsafe fn strategy(
    opno: pg_sys::Oid,
    family: pg_sys::Oid,
) -> Option<(i32, pg_sys::Oid)> {
    if !pg_sys::op_in_opfamily(opno, family) {
        return None;
    }
    let (mut strategy, mut left, mut right) = (0, pg_sys::InvalidOid, pg_sys::InvalidOid);
    pg_sys::get_op_opfamily_properties(opno, family, false, &mut strategy, &mut left, &mut right);
    Some((strategy, right))
}

/// The distinct values other than NULL of a constant array of `kind`.
unsafe fn elements(array: *mut pg_sys::Const, kind: pg_sys::Oid) -> Vec<Part> {
    let elem = pg_sys::get_element_type((*array).consttype);
    if elem == pg_sys::InvalidOid {
        return Vec::new();
    }
    let (mut len, mut byval, mut align) = (0i16, false, 0 as std::ffi::c_char);
    pg_sys::get_typlenbyvalalign(elem, &mut len, &mut byval, &mut align);
    let a = pg_sys::pg_detoast_datum((*array).constvalue.cast_mut_ptr::<pg_sys::varlena>())
        as *mut pg_sys::ArrayType;
    let (mut values, mut nulls, mut n) = (std::ptr::null_mut(), std::ptr::null_mut(), 0);
    pg_sys::deconstruct_array(
        a,
        elem,
        len.into(),
        byval,
        align,
        &mut values,
        &mut nulls,
        &mut n,
    );
    let mut out: Vec<pg_sys::Datum> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for i in 0..n as usize {
        if *nulls.add(i) {
            continue;
        }
        let v = *values.add(i);
        // each value by its image: the datum itself, or the bytes it points to
        let image = if byval {
            v.value().to_le_bytes().to_vec()
        } else {
            let size = pg_sys::datumGetSize(v, byval, len.into());
            std::slice::from_raw_parts(v.cast_mut_ptr::<u8>(), size).to_vec()
        };
        if seen.insert(image) {
            out.push(v);
        }
    }
    out.into_iter().map(|v| Some((v, kind))).collect()
}

/// Reads one condition into the column it is on.
unsafe fn read_condition(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    ri: *mut pg_sys::RestrictInfo,
    columns: &mut [OnColumn],
) {
    let clause = (*ri).clause as *mut pg_sys::Node;
    if (*ri).pseudoconstant || clause.is_null() {
        return;
    }
    match tag(clause) {
        pg_sys::NodeTag::T_OpExpr => {
            let op = clause as *mut pg_sys::OpExpr;
            let args = cells((*op).args);
            if args.len() != 2 {
                return;
            }
            let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
            let (i, other, opno) = if let Some(i) = column_of(rel, index, l) {
                (i, r, (*op).opno)
            } else if let Some(i) = column_of(rel, index, r) {
                (i, l, pg_sys::get_commutator((*op).opno))
            } else {
                return;
            };
            if opno == pg_sys::InvalidOid || !compares_as(index, i, (*op).inputcollid) {
                return;
            }
            let Some((strategy, kind)) = strategy(opno, *(*index).opfamily.add(i)) else {
                return;
            };
            let Some(c) = constant(root, other) else {
                return;
            };
            if (*c).constisnull {
                return;
            }
            let part = Some(((*c).constvalue, kind));
            // a descending column holds its larger values first
            let descending = *(*index).reverse_sort.add(i);
            let on = &mut columns[i];
            match strategy {
                EQUAL => {
                    if on.values.as_ref().is_none_or(|v| v.len() > 1) {
                        on.values = Some(vec![part]);
                    }
                    on.values_from.push(ri);
                }
                LESS | LESS_EQUAL if !descending && on.upper.is_none() => {
                    on.upper = Some((part, strategy == LESS_EQUAL, ri));
                }
                LESS | LESS_EQUAL if descending && on.lower.is_none() => {
                    on.lower = Some((part, strategy == LESS, ri));
                }
                GREATER | GREATER_EQUAL if !descending && on.lower.is_none() => {
                    on.lower = Some((part, strategy == GREATER, ri));
                }
                GREATER | GREATER_EQUAL if descending && on.upper.is_none() => {
                    on.upper = Some((part, strategy == GREATER_EQUAL, ri));
                }
                _ => {}
            }
        }
        pg_sys::NodeTag::T_ScalarArrayOpExpr => {
            let op = clause as *mut pg_sys::ScalarArrayOpExpr;
            let args = cells((*op).args);
            if !(*op).useOr || args.len() != 2 {
                return;
            }
            let Some(i) = column_of(rel, index, args[0] as *mut pg_sys::Node) else {
                return;
            };
            if !compares_as(index, i, (*op).inputcollid) {
                return;
            }
            let Some((EQUAL, kind)) = strategy((*op).opno, *(*index).opfamily.add(i)) else {
                return;
            };
            let Some(c) = constant(root, args[1] as *mut pg_sys::Node) else {
                return;
            };
            if (*c).constisnull {
                return;
            }
            let on = &mut columns[i];
            if on.values.is_none() {
                on.values = Some(elements(c, kind));
            }
            on.values_from.push(ri);
        }
        pg_sys::NodeTag::T_NullTest => {
            let test = clause as *mut pg_sys::NullTest;
            if (*test).nulltesttype != pg_sys::NullTestType::IS_NULL || (*test).argisrow {
                return;
            }
            let Some(i) = column_of(rel, index, (*test).arg as *mut pg_sys::Node) else {
                return;
            };
            let on = &mut columns[i];
            if on.values.is_none() {
                on.values = Some(vec![None]);
            }
            on.values_from.push(ri);
        }
        _ => {}
    }
}

/// One key column up to the last one a condition holds: the values its conditions leave, or none
/// where no condition fixes it and it is stepped through.
enum Level {
    Values(Vec<Part>),
    Stepped,
}

impl OnColumn {
    fn holds_something(&self) -> bool {
        self.values.is_some() || self.lower.is_some() || self.upper.is_some()
    }
}

/// What one B-tree would hold of a relation's conditions, read from the conditions alone, before any
/// page of the index is read: the conditions, the blocks of the key they make, and the range closing
/// each block.
pub(crate) struct Holding {
    pub clauses: Vec<*mut pg_sys::RestrictInfo>,
    levels: Vec<Level>,
    range: Option<(Bounded, Bounded)>,
    /// Whether a list of NULLs alone leaves the conditions holding no row.
    empty: bool,
}

impl Holding {
    /// The key columns no condition fixes that the read would step through.
    pub(crate) fn stepped(&self) -> usize {
        self.levels
            .iter()
            .filter(|l| matches!(l, Level::Stepped))
            .count()
    }
}

/// What the B-tree `index` of the base relation `rel` would hold of `rel`'s constant conditions
/// other than those in `counted`, read from the conditions alone. None where it holds none, or where
/// it is not a B-tree over the whole table.
pub(crate) unsafe fn holding(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<Holding> {
    if (*index).relam != pg_sys::BTREE_AM_OID
        || !(*index).indpred.is_null()
        || (*index).nkeycolumns < 1
    {
        return None;
    }
    let keys = (*index).nkeycolumns as usize;
    let mut columns: Vec<OnColumn> = (0..keys).map(|_| OnColumn::default()).collect();
    for ri in cells((*rel).baserestrictinfo) {
        let ri = ri as *mut pg_sys::RestrictInfo;
        if counted.contains(&ri) {
            continue;
        }
        read_condition(root, rel, index, ri, &mut columns);
    }
    let deepest = columns.iter().rposition(OnColumn::holds_something)?;
    let mut levels = Vec::new();
    let mut clauses = Vec::new();
    let mut range = None;
    for on in columns.iter().take(deepest + 1) {
        if let Some(values) = &on.values {
            if values.is_empty() {
                return Some(Holding {
                    clauses: on.values_from.clone(),
                    levels: Vec::new(),
                    range: None,
                    empty: true,
                });
            }
            levels.push(Level::Values(values.clone()));
            clauses.extend(on.values_from.iter().copied());
            continue;
        }
        if on.lower.is_some() || on.upper.is_some() {
            clauses.extend(on.lower.iter().map(|b| b.2));
            clauses.extend(on.upper.iter().map(|b| b.2));
            range = Some((on.lower, on.upper));
            break;
        }
        levels.push(Level::Stepped);
    }
    if clauses.is_empty() {
        return None;
    }
    Some(Holding {
        clauses,
        levels,
        range,
        empty: false,
    })
}

/// What the B-tree `index` of the base relation `rel` holds of `rel`'s constant conditions. None
/// where it holds none, where it is not a B-tree over the whole table, where a read stepping through
/// a key column is predicted to read more pages than the table holds or than the statement's
/// planning-read budget has left, or where the read would read more pages of the index than either,
/// every page it reads counted, and every block its lists and the values it steps through make
/// reading one page at least.
pub(crate) unsafe fn held(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
) -> Option<Held> {
    held_except(root, rel, index, &[])
}

/// As `held`, leaving out the conditions in `counted`.
pub(crate) unsafe fn held_except(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<Held> {
    held_on_leaves(root, rel, index, counted).map(|(held, _)| held)
}

/// As `held_except`, with the leaves the blocks it measured lie on, where it read any.
pub(crate) unsafe fn held_on_leaves(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    counted: &[*mut pg_sys::RestrictInfo],
) -> Option<(Held, Option<f64>)> {
    let Holding {
        clauses,
        levels,
        range,
        empty,
    } = holding(root, rel, index, counted)?;
    if empty {
        // a list of NULLs alone: no row holds it
        return Some((
            Held {
                rows: 0.0,
                pages: 0,
                clauses,
            },
            None,
        ));
    }
    if levels.iter().any(|l| matches!(l, Level::Stepped)) {
        let predicted = stepped_pages(root, rel, index, &levels);
        if predicted > (*rel).pages as f64 || !crate::budget::fits((*index).indexoid, predicted) {
            #[cfg(any(test, feature = "pg_test"))]
            tests::note((*index).indexoid, None);
            return None;
        }
    }
    let ranged: Vec<*mut pg_sys::RestrictInfo> = range
        .iter()
        .flat_map(|(lo, hi)| lo.iter().chain(hi.iter()).map(|b| b.2))
        .collect();
    let planner = |block: &[Part]| block_share(root, rel, index, block, &ranged);
    let listed = |block: &[Part]| match block {
        [value] if range.is_none() => listed_share(root, rel, index, *value),
        _ => None,
    };
    let table = (**(*root).simple_rte_array.add((*rel).relid as usize)).relid;
    let exact = crate::query::every_row_analyzed(table);
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let measured = measure_blocks(
        rel_index,
        &levels,
        range,
        (*rel).tuples,
        (*rel).pages,
        exact,
        &listed,
        &planner,
    );
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    #[cfg(any(test, feature = "pg_test"))]
    tests::note((*index).indexoid, Some(measured.map_or(0, |m| m.1)));
    let (rows, pages, leaves) = measured?;
    Some((
        Held {
            rows,
            pages,
            clauses,
        },
        Some(leaves),
    ))
}

/// The pages a read of `index` over the blocks `levels` make is predicted to read before it starts:
/// a descent, the index's height, for each block, a list making as many blocks as its values and a
/// stepped column as many as its distinct values, from the planner's statistics; at most a block for
/// each row of the table.
unsafe fn stepped_pages(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    levels: &[Level],
) -> f64 {
    let mut blocks = 1.0f64;
    for (at, level) in levels.iter().enumerate() {
        blocks *= match level {
            Level::Values(values) => values.len() as f64,
            Level::Stepped => distinct_of(root, index, at),
        };
    }
    let blocks = blocks.min((*rel).tuples.max(1.0));
    blocks * (*index).tree_height.max(1) as f64
}

/// The expression key column `at` (from 0) of `index` holds, on the base relation `rel`.
unsafe fn key_expression(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    at: usize,
) -> Option<*mut pg_sys::Expr> {
    let key = *(*index).indexkeys.add(at);
    if key != 0 {
        let rte = *(*root).simple_rte_array.add((*rel).relid as usize);
        let (mut kind, mut typmod, mut collation) = (pg_sys::InvalidOid, -1, pg_sys::InvalidOid);
        pg_sys::get_atttypetypmodcoll(
            (*rte).relid,
            key as pg_sys::AttrNumber,
            &mut kind,
            &mut typmod,
            &mut collation,
        );
        return Some(pg_sys::makeVar(
            (*rel).relid as i32,
            key as pg_sys::AttrNumber,
            kind,
            typmod,
            collation,
            0,
        ) as *mut pg_sys::Expr);
    }
    let before = (0..at).filter(|&i| *(*index).indexkeys.add(i) == 0).count();
    cells((*index).indexprs)
        .get(before)
        .map(|&e| e as *mut pg_sys::Expr)
}

/// The planner's own share of the rows of one block of the B-tree `index`: an equality for each
/// value the block names, a NULL test for each NULL, and the conditions `ranged` of its range.
/// None where the index's operator family has no equality for a value's type.
unsafe fn block_share(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    block: &[Part],
    ranged: &[*mut pg_sys::RestrictInfo],
) -> Option<f64> {
    let mut list: *mut pg_sys::List = null_mut();
    for (at, part) in block.iter().enumerate() {
        let key = key_expression(root, rel, index, at)?;
        let clause = match *part {
            None => {
                let test = pg_sys::palloc0(std::mem::size_of::<pg_sys::NullTest>())
                    as *mut pg_sys::NullTest;
                (*test).xpr.type_ = pg_sys::NodeTag::T_NullTest;
                (*test).arg = key;
                (*test).nulltesttype = pg_sys::NullTestType::IS_NULL;
                (*test).location = -1;
                test as *mut c_void
            }
            Some((value, kind)) => {
                let equality = pg_sys::get_opfamily_member(
                    *(*index).opfamily.add(at),
                    *(*index).opcintype.add(at),
                    kind,
                    EQUAL as i16,
                );
                if equality == pg_sys::InvalidOid {
                    return None;
                }
                let (mut len, mut byval) = (0i16, false);
                pg_sys::get_typlenbyval(kind, &mut len, &mut byval);
                let constant = pg_sys::makeConst(
                    kind,
                    -1,
                    pg_sys::get_typcollation(kind),
                    len as i32,
                    value,
                    false,
                    byval,
                );
                pg_sys::make_opclause(
                    equality,
                    pg_sys::BOOLOID,
                    false,
                    key,
                    constant as *mut pg_sys::Expr,
                    pg_sys::InvalidOid,
                    *(*index).indexcollations.add(at),
                ) as *mut c_void
            }
        };
        list = pg_sys::lappend(list, clause);
    }
    for &ri in ranged {
        list = pg_sys::lappend(list, ri as *mut c_void);
    }
    Some(pg_sys::clauselist_selectivity(
        root,
        list,
        0,
        pg_sys::JoinType::JOIN_INNER,
        null_mut(),
    ))
}

/// The share of the rows ANALYZE's statistics for the leading key column of the B-tree `index` give
/// the value `part`, where their most common values name it, read as the planner reads them for an
/// equality: the table's statistics for a column, the index's own for an expression.
unsafe fn listed_share(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    part: Part,
) -> Option<f64> {
    let (value, kind) = part?;
    let equality =
        pg_sys::get_opfamily_member(*(*index).opfamily, *(*index).opcintype, kind, EQUAL as i16);
    if equality == pg_sys::InvalidOid {
        return None;
    }
    let key = *(*index).indexkeys;
    let (gathered, readable) = if key != 0 {
        let rte = *(*root).simple_rte_array.add((*rel).relid as usize);
        (
            leaves::gathered_statistics((*rte).relid, key as pg_sys::AttrNumber),
            pg_sys::all_rows_selectable(
                root,
                (*rel).relid,
                pg_sys::bms_make_singleton(key - pg_sys::FirstLowInvalidHeapAttributeNumber),
            ),
        )
    } else {
        (
            leaves::gathered_statistics((*index).indexoid, 1),
            pg_sys::all_rows_selectable(root, (*rel).relid, null_mut()),
        )
    };
    if gathered.is_null() {
        return None;
    }
    let share = leaves::listed_share(
        gathered,
        equality,
        *(*index).indexcollations,
        value,
        readable,
    );
    pg_sys::ReleaseSysCache(gathered);
    share
}

/// The bound of a range: its value, whether it lies after the entries equal to it, and the
/// condition it came from.
type Bounded = Option<(Part, bool, *mut pg_sys::RestrictInfo)>;

/// The rows the B-tree `index` measures over the blocks `levels` make, each closed by `range` on
/// the next column where there is one, for a table of `tuples` rows, the pages read, and the leaves
/// the blocks lie on. Where ANALYZE read every row of the table (`exact`), a block takes the share
/// `listed` gives it, where it gives one, and no leaf of it is read to count it. A block inside one
/// leaf or two is counted on those leaves; where the pages left do not reach them, it takes the
/// share `planner` gives it, held to those pages' rows. Every block is measured first; then each
/// block spanning whole leaves that the leaves read cannot stand for, and that takes no share from
/// `listed`, is read whole, in the key's order, while the pages read stay within `most` and what is
/// left of the statement's planning-read budget. None where the first measure of every block would
/// read more than either, every block reading one page at least.
#[allow(clippy::too_many_arguments)]
unsafe fn measure_blocks(
    index: pg_sys::Relation,
    levels: &[Level],
    range: Option<(Bounded, Bounded)>,
    tuples: f64,
    most: u32,
    exact: bool,
    listed: &dyn Fn(&[Part]) -> Option<f64>,
    planner: &dyn Fn(&[Part]) -> Option<f64>,
) -> Option<(f64, u32, f64)> {
    let mut pages = 0;
    let mut blocks: Vec<Vec<Part>> = vec![Vec::new()];
    for (column, level) in levels.iter().enumerate() {
        match level {
            Level::Values(values) => {
                let made = blocks.len() * values.len();
                if made > most.saturating_sub(pages) as usize
                    || !crate::budget::fits((*index).rd_id, made as f64)
                {
                    return None;
                }
                blocks = blocks
                    .iter()
                    .flat_map(|b| {
                        values.iter().map(move |v| {
                            let mut next = b.clone();
                            next.push(*v);
                            next
                        })
                    })
                    .collect();
            }
            Level::Stepped => {
                let mut next = Vec::new();
                for b in &blocks {
                    let left = most.saturating_sub(pages);
                    let places = (left as usize).saturating_sub(next.len());
                    let (places, read) = measure::steps(index, b, column, places, left)?;
                    pages += read;
                    let places = places?;
                    for place in places {
                        let mut stepped = b.clone();
                        stepped.push(place);
                        next.push(stepped);
                    }
                }
                blocks = next;
            }
        }
    }
    // where the range's column keeps its NULLs: a range open at that end stops before them
    let nulls_first = range.is_some()
        && (*(*index).rd_indoption.add(levels.len()) as u32) & pg_sys::INDOPTION_NULLS_FIRST != 0;
    let mut measured = Vec::with_capacity(blocks.len());
    for block in blocks {
        let named = block.clone();
        let (lower, upper) = match range {
            Some((lo, hi)) => {
                let at = |part: Part, after: bool| {
                    let mut parts = block.clone();
                    parts.push(part);
                    End { parts, after }
                };
                let lower = match lo {
                    Some((part, strict, _)) => at(part, strict),
                    None if nulls_first => at(None, true),
                    None => End {
                        parts: block.clone(),
                        after: false,
                    },
                };
                let upper = match hi {
                    Some((part, inclusive, _)) => at(part, inclusive),
                    None if !nulls_first => at(None, false),
                    None => End {
                        parts: block.clone(),
                        after: true,
                    },
                };
                (lower, upper)
            }
            None => (
                End {
                    parts: block.clone(),
                    after: false,
                },
                End {
                    parts: block,
                    after: true,
                },
            ),
        };
        // where ANALYZE read every row, the share its most common values give a value, and no
        // leaf read to count it
        let analyzed = if exact { listed(&named) } else { None };
        let m = measure::rows_at(
            index,
            &lower,
            &upper,
            tuples,
            analyzed.is_none(),
            most.saturating_sub(pages),
        )?;
        pages += m.pages;
        measured.push((named, lower, upper, m, analyzed));
    }
    // the blocks whose leaves the leaves read cannot stand for, each read whole while the pages
    // read stay within `most` and what is left of the budget
    for (_, lower, upper, m, analyzed) in measured.iter_mut() {
        if !m.uneven
            || m.bracketed()
            || analyzed.is_some()
            || m.whole_pages() > most.saturating_sub(pages) as f64
            || !crate::budget::fits((*index).rd_id, m.whole_pages())
        {
            continue;
        }
        let (counted, read) = measure::every_leaf(
            index,
            lower,
            upper,
            most.saturating_sub(pages),
            &mut |_, _, _| {},
        );
        pages += read;
        if let Some((counted, on)) = counted {
            m.rows = counted;
            m.leaves = on;
            m.uneven = false;
            m.counted = true;
        }
    }
    let (mut rows, mut leaves) = (0.0, 0.0);
    for (named, _, _, m, analyzed) in &measured {
        rows += match analyzed {
            Some(share) => share * tuples,
            None => m.rows_within(tuples, || planner(named))?,
        };
        leaves += m.leaves;
    }
    Some((rows, pages, leaves))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
pub(crate) mod tests {
    use crate::query::cells;
    use pgrx::prelude::*;
    use std::cell::RefCell;
    use std::ffi::CStr;

    thread_local! {
        static SEEN: RefCell<Vec<(String, f64, usize)>> = const { RefCell::new(Vec::new()) };
        /// Each read of a B-tree's conditions: the index, and the pages it read; 0 where it stopped
        /// before it was done, none where it never started.
        static NOTED: RefCell<Vec<(pg_sys::Oid, Option<u32>)>> = const { RefCell::new(Vec::new()) };
    }
    static mut NEXT: pg_sys::set_rel_pathlist_hook_type = None;

    /// Notes a read of `index`'s conditions: the pages it read; 0 where it stopped before it was
    /// done, none where it never started.
    pub(crate) fn note(index: pg_sys::Oid, pages: Option<u32>) {
        NOTED.with(|n| n.borrow_mut().push((index, pages)));
    }

    /// Forgets the reads noted.
    pub(crate) fn clear_noted() {
        NOTED.with(|n| n.borrow_mut().clear());
    }

    /// The reads noted since they were last forgotten, by the index's name.
    pub(crate) fn noted() -> Vec<(String, Option<u32>)> {
        NOTED.with(|n| {
            n.borrow()
                .iter()
                .map(|&(oid, pages)| {
                    let name = unsafe { CStr::from_ptr(pg_sys::get_rel_name(oid)) };
                    (name.to_string_lossy().into_owned(), pages)
                })
                .collect()
        })
    }

    /// Each read of a B-tree's conditions made while `query` was planned.
    pub(crate) fn reads(query: &str) -> Vec<(String, Option<u32>)> {
        clear_noted();
        let planned = Spi::run(&format!("EXPLAIN {query}"));
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        noted()
    }

    /// Records what each B-tree of a base relation holds of its conditions while it is planned.
    #[pg_guard]
    unsafe extern "C-unwind" fn record(
        root: *mut pg_sys::PlannerInfo,
        rel: *mut pg_sys::RelOptInfo,
        rti: pg_sys::Index,
        rte: *mut pg_sys::RangeTblEntry,
    ) {
        if let Some(next) = NEXT {
            next(root, rel, rti, rte);
        }
        if (*rel).reloptkind != pg_sys::RelOptKind::RELOPT_BASEREL
            || (*rte).rtekind != pg_sys::RTEKind::RTE_RELATION
        {
            return;
        }
        for index in cells((*rel).indexlist) {
            let index = index as *mut pg_sys::IndexOptInfo;
            if let Some(h) = crate::budget::tests::outside(|| super::held(root, rel, index)) {
                let name = CStr::from_ptr(pg_sys::get_rel_name((*index).indexoid))
                    .to_string_lossy()
                    .into_owned();
                SEEN.with(|s| s.borrow_mut().push((name, h.rows, h.clauses.len())));
            }
        }
    }

    /// What each B-tree held of `query`'s conditions while `query` was planned: the index, the
    /// rows it measured and the conditions it held.
    fn held_by(query: &str) -> Vec<(String, f64, usize)> {
        SEEN.with(|s| s.borrow_mut().clear());
        unsafe {
            NEXT = pg_sys::set_rel_pathlist_hook;
            pg_sys::set_rel_pathlist_hook = Some(record);
        }
        let planned = Spi::run(&format!("EXPLAIN {query}"));
        unsafe { pg_sys::set_rel_pathlist_hook = NEXT };
        planned.unwrap_or_else(|e| panic!("{query}: {e}"));
        SEEN.with(|s| s.borrow().clone())
    }

    /// 60,000 purchases every 17 minutes from 2020, each in one of 7 shops or none, with a key on
    /// the month and the instant, a key on the shop, and one on the shop and the purchase.
    fn bought() {
        Spi::run(
            "CREATE TABLE bought AS \
             SELECT g AS id, timestamp '2020-01-01' + g * interval '17 minutes' AS at, \
                    CASE WHEN g % 40 = 0 THEN NULL ELSE g % 7 END AS shop \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX bought_month_at ON bought ((extract(month FROM at)::smallint), at); \
             CREATE INDEX bought_shop ON bought (shop); \
             CREATE INDEX bought_shop_id ON bought (shop, id); \
             ANALYZE bought",
        )
        .unwrap();
    }

    /// The rows under one leaf of `index`, on average.
    fn under_a_leaf(index: &str) -> f64 {
        Spi::get_one::<f64>(&format!(
            "SELECT 60000.0::float8 / (relpages - 2) FROM pg_class WHERE relname = '{index}'"
        ))
        .unwrap()
        .unwrap()
    }

    /// `query`'s conditions held by `index`, the rows within one leaf of each block's ends of
    /// `count`, and `conditions` of them held.
    fn holds(query: &str, index: &str, blocks: f64, conditions: usize, count: &str) {
        let seen = held_by(query);
        let (_, rows, held) = seen
            .iter()
            .find(|(name, _, _)| name == index)
            .unwrap_or_else(|| panic!("{index} held nothing of {query}: {seen:?}"))
            .clone();
        let counted = Spi::get_one::<i64>(count).unwrap().unwrap() as f64;
        let under = under_a_leaf(index);
        assert!(
            (rows - counted).abs() <= 2.0 * blocks * under,
            "{query}: {rows} measured, {counted} counted, {under} under a leaf"
        );
        assert_eq!(held, conditions, "{query}");
    }

    fn holds_nothing(query: &str, index: &str) {
        let seen = held_by(query);
        assert!(
            seen.iter().all(|(name, _, _)| name != index),
            "{query}: {seen:?}"
        );
    }

    /// The rows `index` measured of `query`'s conditions.
    fn rows_held(query: &str, index: &str) -> f64 {
        let seen = held_by(query);
        seen.iter()
            .find(|(name, _, _)| name == index)
            .unwrap_or_else(|| panic!("{index} held nothing of {query}: {seen:?}"))
            .1
    }

    /// The rows the planner estimates for `query`, from the first line of its plan.
    fn planned(query: &str) -> f64 {
        let line = Spi::get_one::<String>(&format!("EXPLAIN {query}"))
            .unwrap()
            .unwrap();
        line.split(" rows=")
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("no estimate in {line}"))
    }

    /// The purchases of `bought`, each instant also as a number of seconds, with a key on the month
    /// and the number.
    fn numbered() {
        Spi::run(
            "CREATE TABLE numbered AS \
             SELECT g AS id, timestamp '2020-01-01' + g * interval '17 minutes' AS at, \
                    extract(epoch FROM timestamp '2020-01-01' + g * interval '17 minutes') AS stamp \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX numbered_month_stamp ON numbered ((extract(month FROM at)::smallint), stamp); \
             ANALYZE numbered",
        )
        .unwrap();
    }

    /// The seconds of `instant` as a number.
    fn seconds(instant: &str) -> String {
        format!("extract(epoch FROM timestamp '{instant}')")
    }

    #[pg_test]
    fn a_block_inside_one_page_on_a_number_or_at_the_key_end_is_counted_on_it() {
        numbered();
        // a month's purchases over thirty hours, inside one page of the leaves, on a number
        let cond = format!(
            "{MONTH} = 3 AND stamp >= {} AND stamp < {}",
            seconds("2021-03-10 00:00"),
            seconds("2021-03-11 06:00")
        );
        let rows = rows_held(
            &format!("SELECT id FROM numbered WHERE {cond}"),
            "numbered_month_stamp",
        );
        let counted = Spi::get_one::<i64>(&format!("SELECT count(*) FROM numbered WHERE {cond}"))
            .unwrap()
            .unwrap() as f64;
        assert_eq!(rows, counted);
        // the last purchases on a key with no statistics, which the planner takes for a third of
        // the table
        Spi::run(
            "CREATE TABLE ended AS SELECT g AS id, g AS other FROM generate_series(1, 60000) g; \
             ALTER TABLE ended ALTER id SET STATISTICS 0; \
             CREATE INDEX ended_id ON ended (id); ANALYZE ended",
        )
        .unwrap();
        let query = "SELECT id FROM ended WHERE id > 59995";
        assert_eq!(rows_held(query, "ended_id"), 5.0);
        assert!(planned(query) > 1000.0, "{}", planned(query));
    }

    #[pg_test]
    fn a_range_inside_a_page_on_an_integer_or_a_date_is_counted_on_it() {
        // a key on an integer, one on a bigint and one on a date, none with statistics, so the
        // planner takes a range for a two-hundredth of the table, more than the range holds
        Spi::run(
            "CREATE TABLE stamped AS SELECT g AS id, date '2000-01-01' + g / 7 AS d, \
                    5000000000::int8 + g AS big \
             FROM generate_series(0, 69999) g; \
             ALTER TABLE stamped ALTER id SET STATISTICS 0; \
             ALTER TABLE stamped ALTER d SET STATISTICS 0; \
             ALTER TABLE stamped ALTER big SET STATISTICS 0; \
             CREATE INDEX stamped_id ON stamped (id); \
             CREATE INDEX stamped_big ON stamped (big); \
             CREATE INDEX stamped_d ON stamped (d) WITH (deduplicate_items = off); \
             ANALYZE stamped",
        )
        .unwrap();
        for (cond, index) in [
            ("id BETWEEN 30000 AND 30020", "stamped_id"),
            ("id > 41000 AND id < 41010", "stamped_id"),
            ("big BETWEEN 5000030000 AND 5000030020", "stamped_big"),
            ("big > 5000041000 AND big < 5000041010", "stamped_big"),
            ("d BETWEEN '2010-01-01' AND '2010-01-03'", "stamped_d"),
            ("d >= '2012-06-01' AND d < '2012-06-02'", "stamped_d"),
        ] {
            let query = format!("SELECT id FROM stamped WHERE {cond}");
            let rows = rows_held(&query, index);
            let counted = Spi::get_one::<i64>(&format!("SELECT count(*) FROM stamped WHERE {cond}"))
                .unwrap()
                .unwrap() as f64;
            assert_eq!(rows, counted, "{cond}");
            assert!(planned(&query) > 5.0 * counted, "{cond}");
        }
    }

    #[pg_test]
    fn a_stretch_across_one_page_or_two_on_a_number_is_counted_on_them() {
        numbered();
        // windows of twenty hours through March, a fraction of a page each, a fifth of them
        // crossing from one page of the leaves into the next
        for start in 0..40 {
            let from = format!("timestamp '2021-03-01' + {start} * interval '17 hours'");
            let cond = format!(
                "{MONTH} = 3 AND stamp >= extract(epoch FROM {from}) \
                 AND stamp < extract(epoch FROM {from} + interval '20 hours')"
            );
            let rows = rows_held(
                &format!("SELECT id FROM numbered WHERE {cond}"),
                "numbered_month_stamp",
            );
            let counted =
                Spi::get_one::<i64>(&format!("SELECT count(*) FROM numbered WHERE {cond}"))
                    .unwrap()
                    .unwrap() as f64;
            assert_eq!(rows, counted, "{cond}");
        }
    }

    #[pg_test]
    fn a_range_inside_a_page_or_two_on_a_timestamp_is_counted_on_them() {
        bought();
        let leaf = under_a_leaf("bought_month_at");
        // two and eight hours inside a page, and windows of twenty hours through March 2021 after its
        // first days, some of them crossing from one page of the leaves into the next
        let mut windows = vec![
            ("timestamp '2021-03-10 10:00'".to_string(), 2),
            ("timestamp '2021-03-10 08:00'".to_string(), 8),
        ];
        for start in 0..40 {
            windows.push((
                format!("timestamp '2021-03-03' + {start} * interval '13 hours'"),
                20,
            ));
        }
        for (from, hours) in windows {
            let cond =
                format!("{MONTH} = 3 AND at >= {from} AND at < {from} + interval '{hours} hours'");
            let query = format!("SELECT id FROM bought WHERE {cond}");
            let rows = rows_held(&query, "bought_month_at");
            let counted = Spi::get_one::<i64>(&format!("SELECT count(*) FROM bought WHERE {cond}"))
                .unwrap()
                .unwrap() as f64;
            assert!(
                counted <= 2.0 * leaf,
                "{cond}: {counted} counted, {leaf} a page"
            );
            assert_eq!(rows, counted, "{cond}");
            if hours == 2 {
                // where the planner is several times off
                let planner = planned(&query);
                assert!(planner < counted / 3.0, "{cond}: {planner} planned");
            }
        }
    }

    const MONTH: &str = "extract(month FROM at)::smallint";

    /// 20,003 parts in 30 categories, the 15th holding 3 of them and each other about 690, with a
    /// key on the category and the part carrying the part's name, so that a page of the leaves
    /// holds about a hundred parts and the 15th's lie among its neighbours'. The category keeps no
    /// statistics.
    pub(crate) fn listed() {
        Spi::run(
            "CREATE TABLE listed AS SELECT g AS id, \
                 CASE WHEN g <= 3 THEN 15 WHEN g % 29 < 14 THEN 1 + g % 29 ELSE 2 + g % 29 END AS cat, \
                 md5(g::text) AS name \
             FROM generate_series(1, 20003) g; \
             ALTER TABLE listed ALTER cat SET STATISTICS 0; \
             CREATE INDEX listed_cat_id ON listed (cat, id) INCLUDE (name); \
             ANALYZE listed",
        )
        .unwrap();
    }

    /// Whether ANALYZE's most common values of the parts' category name the 15th.
    fn fifteenth_named() -> bool {
        Spi::get_one::<bool>(
            "SELECT coalesce((SELECT 15 = ANY (most_common_vals::text::int[]) FROM pg_stats \
                 WHERE tablename = 'listed' AND attname = 'cat'), false)",
        )
        .unwrap()
        .unwrap()
    }

    #[pg_test]
    fn a_rare_category_inside_a_leaf_among_others_is_counted_on_it() {
        listed();
        let query = "SELECT id FROM listed WHERE cat = 15";
        let counted = Spi::get_one::<i64>("SELECT count(*) FROM listed WHERE cat = 15")
            .unwrap()
            .unwrap() as f64;
        Spi::run("CREATE INDEX listed_order ON listed USING surveyor (id)").unwrap();
        // with no statistics on the category: its own entries on the leaf
        assert!(!fifteenth_named());
        assert_eq!(rows_held(query, "listed_cat_id"), counted);
        // with statistics naming ten other categories: the same
        Spi::run("ALTER TABLE listed ALTER cat SET STATISTICS 10; ANALYZE listed").unwrap();
        assert!(!fifteenth_named());
        assert_eq!(rows_held(query, "listed_cat_id"), counted);
        assert_eq!(planned(query), counted);
    }

    /// 40,000 parts in 20 categories of 2,000 each, with a key on the category and the part carrying
    /// a note: 320 bytes on the 7th category's parts and 8 on every other's, so that a page of the
    /// leaves holds a few dozen of the 7th's parts and a few hundred of any other's.
    pub(crate) fn annotated() {
        Spi::run(
            "CREATE TABLE annotated AS SELECT g AS id, 1 + g % 20 AS cat, \
                 CASE WHEN g % 20 = 6 \
                      THEN (SELECT string_agg(md5(g::text || '.' || i), '') FROM generate_series(1, 10) i) \
                      ELSE left(md5(g::text), 8) END AS note \
             FROM generate_series(1, 40000) g; \
             CREATE INDEX annotated_cat_id ON annotated (cat, id) INCLUDE (note); \
             ANALYZE annotated",
        )
        .unwrap();
    }

    /// Three blocks of 12,000 rows keyed by the block and a number, each carrying a note of 384
    /// bytes, but the second block's numbers 1,000 to 2,999, whose note is 64 bytes: a stretch of
    /// its leaves near its start holds several times the rows of any other leaf, and none of the
    /// leaves at the quarter, the middle and the three-quarter of the block lies in it. Each row
    /// also keeps 800 bytes the index does not carry, so that the table holds about twice the
    /// index's pages.
    pub(crate) fn banded() {
        Spi::run(
            "CREATE TABLE banded AS SELECT a, n, \
                 (SELECT string_agg(md5(a || '.' || n || '.' || i), '') \
                  FROM generate_series(1, CASE WHEN a = 2 AND n >= 1000 AND n < 3000 \
                                               THEN 2 ELSE 12 END) i) AS note, \
                 repeat('x', 800) AS kept \
             FROM generate_series(1, 3) a, generate_series(0, 11999) n; \
             CREATE INDEX banded_an ON banded (a, n) INCLUDE (note); \
             CREATE INDEX banded_order ON banded USING surveyor (a, n); \
             ANALYZE banded",
        )
        .unwrap();
    }

    /// Three blocks of 12,000 rows keyed by the block and a number, carrying a whole number: every
    /// column the index stores is of one width.
    fn even() {
        Spi::run(
            "CREATE TABLE even AS SELECT a, n, (a * 100000 + n)::int8 AS v \
             FROM generate_series(1, 3) a, generate_series(0, 23998, 2) n; \
             CREATE INDEX even_an ON even (a, n) INCLUDE (v); \
             CREATE INDEX even_order ON even USING surveyor (a, n); \
             ANALYZE even",
        )
        .unwrap();
    }

    /// The pages read for `query`'s conditions on `index`.
    fn pages_read(query: &str, index: &str) -> u32 {
        let read = reads(query);
        read.iter()
            .find(|(name, _)| name == index)
            .and_then(|(_, pages)| *pages)
            .unwrap_or_else(|| panic!("{index} read nothing for {query}: {read:?}"))
    }

    /// The leaves the block `a = 2` of `index` lies on, counted with pageinspect.
    fn block_leaves(index: &str) -> u32 {
        Spi::run("CREATE EXTENSION IF NOT EXISTS pageinspect").unwrap();
        Spi::get_one::<i64>(&format!(
            "SELECT count(DISTINCT s.blkno) \
             FROM bt_multi_page_stats('{index}', 1, -1) s, \
                  LATERAL bt_page_items('{index}', s.blkno::int) i \
             WHERE s.type = 'l' AND i.data LIKE '02 00 00 00%' \
               AND NOT (s.btpo_next <> 0 AND i.itemoffset = 1)"
        ))
        .unwrap()
        .unwrap() as u32
    }

    #[pg_test]
    fn a_block_of_entries_of_one_width_built_in_one_pass_is_counted_from_a_few_of_its_leaves() {
        even();
        let query = "SELECT n FROM even WHERE a = 2";
        let rows = rows_held(query, "even_an");
        assert_eq!(rows, 12000.0, "{rows} measured");
        // the descent and five leaves, far fewer than the block's
        let (pages, leaves) = (pages_read(query, "even_an"), block_leaves("even_an"));
        assert!(leaves > 30, "{leaves} leaves");
        assert!(pages <= 12, "{pages} pages read, {leaves} leaves");
    }

    #[pg_test]
    fn a_block_of_entries_of_one_width_split_since_it_was_built_is_read_whole() {
        even();
        // the odd numbers of the second block, written in no order after the index was built, so
        // that its leaves split and hold different rows
        Spi::run(
            "INSERT INTO even SELECT 2, n, (200000 + n)::int8 \
             FROM generate_series(1, 23999, 2) n ORDER BY md5(n::text); \
             ANALYZE even",
        )
        .unwrap();
        let query = "SELECT n FROM even WHERE a = 2";
        let rows = rows_held(query, "even_an");
        assert_eq!(rows, 24000.0, "{rows} measured");
        let (pages, leaves) = (pages_read(query, "even_an"), block_leaves("even_an"));
        assert!(pages > leaves, "{pages} pages read, {leaves} leaves");
    }

    #[pg_test]
    fn a_block_of_an_index_storing_text_is_read_whole() {
        banded();
        let query = "SELECT n FROM banded WHERE a = 2";
        let rows = rows_held(query, "banded_an");
        assert_eq!(rows, 12000.0, "{rows} measured");
        assert_eq!(planned(query), 12000.0);
        let (pages, leaves) = (pages_read(query, "banded_an"), block_leaves("banded_an"));
        assert!(pages > leaves, "{pages} pages read, {leaves} leaves");
        // a block of the same index whose entries are of one length holds its count too
        assert_eq!(
            rows_held("SELECT n FROM banded WHERE a = 3", "banded_an"),
            12000.0
        );
    }

    #[pg_test]
    fn a_value_whose_entries_are_wider_than_the_rest_is_counted_from_its_own_leaves() {
        annotated();
        Spi::run("CREATE INDEX annotated_order ON annotated USING surveyor (id)").unwrap();
        // the rows the leaves hold on average, against those a leaf of the 7th holds
        let leaf = Spi::get_one::<f64>(
            "SELECT reltuples::float8 / (relpages - 2) FROM pg_class WHERE relname = 'annotated_cat_id'",
        )
        .unwrap()
        .unwrap();
        Spi::run("CREATE EXTENSION IF NOT EXISTS pageinspect").unwrap();
        let wide = Spi::get_one::<f64>(
            "SELECT 2000.0::float8 / count(DISTINCT s.blkno) \
             FROM bt_multi_page_stats('annotated_cat_id', 1, -1) s, \
                  LATERAL bt_page_items('annotated_cat_id', s.blkno::int) i \
             WHERE s.type = 'l' AND i.data LIKE '07 00 00 00%' \
               AND NOT (s.btpo_next <> 0 AND i.itemoffset = 1)",
        )
        .unwrap()
        .unwrap();
        assert!(
            leaf > 2.0 * wide,
            "{leaf} a leaf on average, {wide} a leaf of the 7th's"
        );
        for cat in [7, 3, 20] {
            let query = format!("SELECT id FROM annotated WHERE cat = {cat}");
            let counted =
                Spi::get_one::<i64>(&format!("SELECT count(*) FROM annotated WHERE cat = {cat}"))
                    .unwrap()
                    .unwrap() as f64;
            let estimate = planned(&query);
            assert!(
                (estimate - counted).abs() <= 0.03 * counted,
                "category {cat}: {estimate} planned, {counted} counted"
            );
        }
    }

    #[pg_test]
    fn the_conditions_a_b_tree_holds_are_measured_on_it() {
        bought();
        let base = "SELECT id FROM bought WHERE";
        let count = "SELECT count(*) FROM bought WHERE";
        for (cond, blocks, held) in [
            (format!("{MONTH} = 12"), 1.0, 1),
            (
                format!("{MONTH} = 12 AND at >= '2020-12-01' AND at < '2020-12-15'"),
                1.0,
                3,
            ),
            (format!("{MONTH} IN (1, 2, 12)"), 3.0, 1),
            (format!("12 = {MONTH} AND '2020-12-15' > at"), 1.0, 2),
            (
                format!("{MONTH} = extract(month FROM now() - interval '1 month')::smallint"),
                1.0,
                1,
            ),
        ] {
            holds(
                &format!("{base} {cond}"),
                "bought_month_at",
                blocks,
                held,
                &format!("{count} {cond}"),
            );
        }
        holds(
            &format!("{base} shop IS NULL"),
            "bought_shop",
            1.0,
            1,
            &format!("{count} shop IS NULL"),
        );
    }

    #[pg_test]
    fn a_range_is_measured_in_the_keys_order_and_stops_at_its_columns_nulls() {
        // a third of the values NULL, on keys ascending and descending, NULLs last and first
        Spi::run(
            "CREATE TABLE spans AS \
             SELECT g AS id, CASE WHEN g % 3 = 0 THEN NULL ELSE g END AS v \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX spans_v ON spans (v) WITH (deduplicate_items = off); \
             CREATE INDEX spans_v_first ON spans (v NULLS FIRST) WITH (deduplicate_items = off); \
             CREATE INDEX spans_v_desc ON spans (v DESC) WITH (deduplicate_items = off); \
             CREATE INDEX spans_v_desc_last ON spans (v DESC NULLS LAST) \
                 WITH (deduplicate_items = off); \
             ANALYZE spans",
        )
        .unwrap();
        for (cond, held) in [
            ("v > 50000", 1),
            ("v >= 50000", 1),
            ("v < 10000", 1),
            ("v <= 10000", 1),
            ("v BETWEEN 20000 AND 30000", 2),
        ] {
            for index in [
                "spans_v",
                "spans_v_first",
                "spans_v_desc",
                "spans_v_desc_last",
            ] {
                holds(
                    &format!("SELECT id FROM spans WHERE {cond}"),
                    index,
                    1.0,
                    held,
                    &format!("SELECT count(*) FROM spans WHERE {cond}"),
                );
            }
        }
    }

    #[pg_test]
    fn a_range_compared_under_another_collation_than_a_b_trees_is_not_counted_on_that_b_tree() {
        // 40,000 names, a quarter of them starting with each of "A", "a", "B" and "b", under a
        // linguistic collation; a key on the names in the C collation's order, which puts every
        // upper-case letter before every lower-case one
        Spi::run(
            "CREATE TABLE lettered (id int, name text COLLATE \"und-x-icu\"); \
             INSERT INTO lettered \
             SELECT g, (ARRAY['A', 'a', 'B', 'b'])[1 + g % 4] || md5(g::text) \
             FROM generate_series(1, 40000) g; \
             CREATE INDEX lettered_c ON lettered (name COLLATE \"C\"); \
             CREATE INDEX lettered_order ON lettered USING surveyor (id); \
             ANALYZE lettered",
        )
        .unwrap();
        let count = |cond: &str| {
            Spi::get_one::<i64>(&format!("SELECT count(*) FROM lettered WHERE {cond}"))
                .unwrap()
                .unwrap() as f64
        };
        let query = |cond: &str| format!("SELECT id FROM lettered WHERE {cond}");
        let (linguistic, in_c) = ("name < 'b'", "name < 'b' COLLATE \"C\"");
        assert_eq!((count(linguistic), count(in_c)), (20000.0, 30000.0));
        // the key in the C order counts the range in its own order, and only there
        assert_eq!(rows_held(&query(in_c), "lettered_c"), count(in_c));
        holds_nothing(&query(linguistic), "lettered_c");
        assert!(
            (planned(&query(linguistic)) - count(linguistic)).abs() < 0.1 * count(linguistic),
            "{} planned",
            planned(&query(linguistic))
        );
        // a key in the column's own order counts it
        Spi::run("CREATE INDEX lettered_name ON lettered (name); ANALYZE lettered").unwrap();
        assert_eq!(
            rows_held(&query(linguistic), "lettered_name"),
            count(linguistic)
        );
        assert_eq!(planned(&query(linguistic)), count(linguistic));
    }

    #[pg_test]
    fn a_condition_with_no_constant_is_left_to_the_planner() {
        bought();
        holds_nothing("SELECT id FROM bought WHERE shop = id", "bought_shop");
        Spi::run(&format!(
            "PREPARE in_month(int) AS SELECT id FROM bought WHERE {MONTH} = $1; \
             SET LOCAL plan_cache_mode = force_generic_plan"
        ))
        .unwrap();
        holds_nothing("EXECUTE in_month(12)", "bought_month_at");
        Spi::run("DEALLOCATE in_month").unwrap();
    }

    #[pg_test]
    fn a_condition_under_a_leading_column_no_condition_fixes_is_measured_at_each_of_its_values() {
        bought();
        let base = "SELECT id FROM bought WHERE";
        let count = "SELECT count(*) FROM bought WHERE";
        // the month's 12 values; the shop's 7 and NULL
        for (cond, index, blocks, held) in [
            ("at >= '2021-01-01'", "bought_month_at", 12.0, 1),
            (
                "at >= '2020-03-01' AND at < '2020-03-08'",
                "bought_month_at",
                12.0,
                2,
            ),
            ("id > 59000", "bought_shop_id", 8.0, 1),
            ("id BETWEEN 1000 AND 20000", "bought_shop_id", 8.0, 2),
            ("id = 777", "bought_shop_id", 8.0, 1),
        ] {
            holds(
                &format!("{base} {cond}"),
                index,
                blocks,
                held,
                &format!("{count} {cond}"),
            );
        }
        // each value stepped through is the block a list of it makes
        let rows_of = |query: &str, index: &str| {
            held_by(query)
                .iter()
                .find(|(name, _, _)| name == index)
                .map(|(_, rows, _)| *rows)
                .unwrap_or_else(|| panic!("{index} held nothing of {query}"))
        };
        let stepped = rows_of(&format!("{base} at >= '2021-01-01'"), "bought_month_at");
        let listed = rows_of(
            &format!(
                "{base} {MONTH} IN (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12) AND at >= '2021-01-01'"
            ),
            "bought_month_at",
        );
        assert_eq!(stepped, listed);
        let stepped = rows_of(&format!("{base} id > 59000"), "bought_shop_id");
        let listed = rows_of(
            &format!("{base} shop IN (0, 1, 2, 3, 4, 5, 6) AND id > 59000"),
            "bought_shop_id",
        ) + rows_of(
            &format!("{base} shop IS NULL AND id > 59000"),
            "bought_shop_id",
        );
        assert_eq!(stepped, listed);
        // a value listed twice is one block
        assert_eq!(
            rows_of(
                &format!("{base} {MONTH} IN (1, 12, 12, 1)"),
                "bought_month_at"
            ),
            rows_of(&format!("{base} {MONTH} IN (1, 12)"), "bought_month_at")
        );
        // one row, under each shop inside a page between two others of its block: the page's rows
        // over the values it can hold
        let seen = held_by(&format!("{base} id = 30001"));
        let rows = seen
            .iter()
            .find(|(name, _, _)| name == "bought_shop_id")
            .unwrap()
            .1;
        assert!((0.5..=2.0).contains(&rows), "{rows}");
    }

    /// Purchases on each day of 1990 to 2019, the day's rows growing with its year from 1 to 30,
    /// each row its own entry in a key on the day.
    fn days() {
        Spi::run(
            "CREATE TABLE days AS SELECT d::date AS d, n \
             FROM generate_series(date '1990-01-01', date '2019-12-31', interval '1 day') d, \
                  generate_series(1, extract(year FROM d)::int - 1989) n; \
             CREATE INDEX days_d ON days (d) WITH (deduplicate_items = off); \
             ANALYZE days",
        )
        .unwrap();
    }

    /// The Mondays of the years `from` to `to` in `months`, as a list of dates.
    fn mondays(from: i32, to: i32, months: &str) -> String {
        Spi::get_one::<String>(&format!(
            "SELECT string_agg(quote_literal(g::date), ', ') \
             FROM generate_series(date '{from}-01-01', date '{to}-12-31', interval '1 day') g \
             WHERE extract(isodow FROM g) = 1 AND extract(month FROM g) IN ({months})"
        ))
        .unwrap()
        .unwrap()
    }

    #[pg_test]
    fn a_list_of_days_is_measured_day_by_day_in_a_stretch_and_across_the_whole_line() {
        days();
        let every_month = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12";
        for list in [
            // thin: each day inside one page
            mondays(1991, 1991, every_month),
            // thick: each day a fifth of a page or more
            mondays(2018, 2018, every_month),
            // across the line, every year's December
            mondays(1990, 2019, "12"),
        ] {
            let query = format!("SELECT n FROM days WHERE d IN ({list})");
            let seen = held_by(&query);
            let (_, rows, held) = seen
                .iter()
                .find(|(name, _, _)| name == "days_d")
                .unwrap_or_else(|| panic!("days_d held nothing: {seen:?}"))
                .clone();
            assert_eq!(held, 1);
            let counted =
                Spi::get_one::<i64>(&format!("SELECT count(*) FROM days WHERE d IN ({list})"))
                    .unwrap()
                    .unwrap() as f64;
            assert_eq!(rows, counted, "{list}");
        }
    }

    /// Whether ANALYZE's most common values of `column` of `table` name `value`.
    fn named(table: &str, column: &str, value: &str) -> bool {
        Spi::get_one::<bool>(&format!(
            "SELECT coalesce((SELECT {value} = ANY (most_common_vals::text::text[]) FROM pg_stats \
                 WHERE tablename = '{table}' AND attname = '{column}'), false)"
        ))
        .unwrap()
        .unwrap()
    }

    #[pg_test]
    fn a_day_inside_a_leaf_of_a_sampled_table_is_counted_on_it_whatever_analyze_names() {
        // the purchases of `days`, and 260 more on one Monday of 2018, which ANALYZE's sample of
        // 30,000 of the 170,000 rows names among its most common days; at first ANALYZE keeps no
        // statistics on the day
        Spi::run(
            "CREATE TABLE heavy AS SELECT d::date AS d, n \
             FROM generate_series(date '1990-01-01', date '2019-12-31', interval '1 day') d, \
                  generate_series(1, extract(year FROM d)::int - 1989) n; \
             INSERT INTO heavy SELECT date '2018-06-04', g FROM generate_series(100, 359) g; \
             CREATE INDEX heavy_d ON heavy (d) WITH (deduplicate_items = off); \
             ALTER TABLE heavy ALTER d SET STATISTICS 0; ANALYZE heavy",
        )
        .unwrap();
        let query = "SELECT n FROM heavy WHERE d = '2018-06-04'";
        let counted = Spi::get_one::<i64>("SELECT count(*) FROM heavy WHERE d = '2018-06-04'")
            .unwrap()
            .unwrap() as f64;
        // with no statistics on the day, and with the day named: its own entries on the leaves
        assert!(!named("heavy", "d", "'2018-06-04'"));
        assert_eq!(rows_held(query, "heavy_d"), counted);
        Spi::run("ALTER TABLE heavy ALTER d SET STATISTICS 100; ANALYZE heavy").unwrap();
        assert!(named("heavy", "d", "'2018-06-04'"));
        assert_eq!(rows_held(query, "heavy_d"), counted);
    }

    #[pg_test]
    fn a_text_value_inside_a_leaf_of_a_sampled_table_is_counted_on_it() {
        // 60,000 codes, every one its own but one held by 40 rows, inside a page or two of a key on
        // the code; ANALYZE samples 30,000 of the rows and keeps no statistics on the code
        Spi::run(
            "CREATE TABLE coded AS SELECT g AS id, \
                 CASE WHEN g % 1500 = 0 THEN 'shared' ELSE md5(g::text) END AS code \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX coded_code ON coded (code); \
             CREATE INDEX coded_order ON coded USING surveyor (id); \
             ALTER TABLE coded ALTER code SET STATISTICS 0; ANALYZE coded",
        )
        .unwrap();
        let query = "SELECT id FROM coded WHERE code = 'shared'";
        assert_eq!(rows_held(query, "coded_code"), 40.0);
        assert_eq!(planned(query), 40.0);
    }

    #[pg_test]
    fn a_single_instant_held_by_many_rows_inside_a_leaf_is_counted_on_it() {
        // 60,000 purchases every minute but 40 at one instant, inside a page or two of a key on the
        // instant; ANALYZE samples 30,000 of the rows
        Spi::run(
            "CREATE TABLE instants AS SELECT g AS id, \
                 CASE WHEN g % 1500 = 0 THEN timestamp '2021-06-01 12:00' \
                      ELSE timestamp '2020-01-01' + g * interval '1 minute' END AS at \
             FROM generate_series(1, 60000) g; \
             CREATE INDEX instants_at ON instants (at); \
             ANALYZE instants",
        )
        .unwrap();
        let query = "SELECT id FROM instants WHERE at = '2021-06-01 12:00'";
        assert_eq!(rows_held(query, "instants_at"), 40.0);
    }

    /// 120,000 entries of a log, 200 a day over 600 days, each in one of 50 slots, with a note
    /// filling a page with about 40 of them; B-trees on the day, and on the day and the slot.
    /// `logged_narrow` holds the same entries with no note, and a B-tree on the day.
    fn logged() {
        Spi::run(
            "CREATE TABLE logged AS SELECT g AS id, g / 200 AS day, g % 50 AS slot, \
                 repeat('x', 150) AS note FROM generate_series(0, 119999) g; \
             CREATE INDEX logged_day ON logged (day); \
             CREATE INDEX logged_day_slot ON logged (day, slot) WITH (deduplicate_items = off); \
             CREATE TABLE logged_narrow AS SELECT id, day, slot FROM logged; \
             CREATE INDEX logged_narrow_day ON logged_narrow (day); \
             ANALYZE logged; ANALYZE logged_narrow",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_read_while_planning_stops_once_its_pages_would_pass_the_pages_of_its_table() {
        logged();
        let pages = |table: &str| {
            Spi::get_one::<i32>(&format!(
                "SELECT relpages FROM pg_class WHERE relname = '{table}'"
            ))
            .unwrap()
            .unwrap() as f64
        };
        // 400 days, each a block of the key
        let days: Vec<String> = (0..400).map(|d| (d * 3 / 2).to_string()).collect();
        let days = days.join(", ");
        assert!(pages("logged") > 1600.0, "{}", pages("logged"));
        assert!(pages("logged_narrow") < 800.0, "{}", pages("logged_narrow"));
        let wide = format!("SELECT id FROM logged WHERE day IN ({days})");
        let rows = rows_held(&wide, "logged_day");
        let counted = Spi::get_one::<i64>(&format!(
            "SELECT count(*) FROM logged WHERE day IN ({days})"
        ))
        .unwrap()
        .unwrap() as f64;
        assert!(
            (rows - counted).abs() <= 0.1 * counted,
            "{rows} measured, {counted} counted"
        );
        // the same blocks would read more pages than the table without the note holds
        holds_nothing(
            &format!("SELECT id FROM logged_narrow WHERE day IN ({days})"),
            "logged_narrow_day",
        );
        // the days where a page of the leaves begins, more than 200, stepped through for the slot
        let rows = rows_held("SELECT id FROM logged WHERE slot = 7", "logged_day_slot");
        assert!(rows > 0.0, "{rows}");
    }

    #[pg_test]
    fn a_key_whose_steps_are_predicted_to_pass_the_pages_of_its_table_is_never_read() {
        // 100,000 values of the leading column, two rows each, and a condition on the second
        Spi::run(
            "CREATE EXTENSION IF NOT EXISTS pageinspect; \
             CREATE TABLE spread AS SELECT g / 2 AS k, g % 1000 AS v FROM generate_series(1, 200000) g; \
             CREATE INDEX spread_kv ON spread (k, v); ANALYZE spread",
        )
        .unwrap();
        let query = "SELECT k FROM spread WHERE v = 7";
        let alone = planned(query);
        Spi::run("CREATE INDEX spread_order ON spread USING surveyor (k)").unwrap();
        let (pages, height) = Spi::get_two::<i32, i32>(
            "SELECT (SELECT relpages FROM pg_class WHERE relname = 'spread'), \
                    (SELECT level FROM bt_metap('spread_kv'))::int",
        )
        .unwrap();
        // a descent a value would pass the table many times over
        assert!(
            100_000 * height.unwrap().max(1) > 10 * pages.unwrap(),
            "{pages:?}"
        );
        assert_eq!(reads(query), vec![("spread_kv".to_string(), None)]);
        assert_eq!(planned(query), alone);
    }
}
