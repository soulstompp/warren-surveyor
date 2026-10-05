// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The rows of a B-tree's block that also pass another index's conditions, those conditions tested
//! on the block's own entries while planning.
//!
//! A B-tree stores in every entry the columns of its key and of its INCLUDE, and the value of each
//! expression it keys. A condition is answered from an index that stores its value. Where a
//! B-tree stores the value another index's condition compares, the column itself or the expression
//! as one of its key columns, the condition is tested on the entries of the block the B-tree's own
//! conditions fix, by the stored value with nothing computed but the comparison, each entry
//! standing for its row, and no table page is read. Where the B-tree stores only the columns an
//! expression is computed from, the condition is left to the index that holds it.
//!
//! The block is read as the B-tree measures it (`measure`). A block inside one leaf or two is read
//! on those leaves and its passing rows counted. A block spanning whole leaves is read at its first
//! leaf, its last and three between them. Where every tested condition is on a value of the key and
//! every column the index stores is of one width, and the three between hold the same rows, the
//! block lies in the key's own order and its leaves hold the same rows as a build leaves them: the
//! first and last leaves count their passing rows for themselves, and the leaves between them hold
//! the rows the three hold on average, passing at the share the five leaves give. Otherwise every
//! leaf of the block is read and its passing rows counted, while its pages stay within the pages of
//! the table and what is left of the statement's planning-read budget (`budget`). A block whose
//! leaves would pass either is counted from its five leaves.

use crate::conditions::{column_of, compares_as, constant, strategy};
use crate::measure::{self, End, Leaf, Part};
use crate::query::{bare, cells};
use pgrx::pg_sys;
use std::ptr::null_mut;

const LESS: i32 = 1;
const LESS_EQUAL: i32 = 2;
const EQUAL: i32 = 3;
const GREATER_EQUAL: i32 = 4;
const GREATER: i32 = 5;

/// The bounds of a column: the value, and whether the bound lies after the entries equal to it.
type Bound = Option<(Part, bool)>;

/// The ends of the one block of the B-tree `index` that the conditions `clauses` of `rel` fix:
/// equalities and NULL tests on its leading key columns, then bounds on the next. None where they
/// make more than one block, or a condition is not one against a constant.
unsafe fn ends(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    clauses: &[*mut pg_sys::RestrictInfo],
) -> Option<(End, End)> {
    let keys = (*index).nkeycolumns as usize;
    let mut equal: Vec<Option<Part>> = vec![None; keys];
    let (mut lower, mut upper): (Vec<Bound>, Vec<Bound>) = (vec![None; keys], vec![None; keys]);
    for &ri in clauses {
        let clause = (*ri).clause as *mut pg_sys::Node;
        if clause.is_null() {
            return None;
        }
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
                if !compares_as(index, at, (*op).inputcollid) {
                    return None;
                }
                let (number, kind) = strategy(opno, *(*index).opfamily.add(at))?;
                let value = constant(root, other)?;
                if (*value).constisnull {
                    return None;
                }
                let part = Some(((*value).constvalue, kind));
                let descending = *(*index).reverse_sort.add(at);
                match number {
                    EQUAL => {
                        equal[at].get_or_insert(part);
                    }
                    LESS | LESS_EQUAL if !descending => {
                        upper[at] = Some((part, number == LESS_EQUAL))
                    }
                    LESS | LESS_EQUAL => lower[at] = Some((part, number == LESS)),
                    GREATER | GREATER_EQUAL if !descending => {
                        lower[at] = Some((part, number == GREATER))
                    }
                    GREATER | GREATER_EQUAL => upper[at] = Some((part, number == GREATER_EQUAL)),
                    _ => return None,
                }
            }
            pg_sys::NodeTag::T_NullTest => {
                let test = clause as *mut pg_sys::NullTest;
                if (*test).nulltesttype != pg_sys::NullTestType::IS_NULL || (*test).argisrow {
                    return None;
                }
                let at = column_of(rel, index, (*test).arg as *mut pg_sys::Node)?;
                equal[at].get_or_insert(None);
            }
            _ => return None,
        }
    }
    let mut parts = Vec::new();
    while parts.len() < keys {
        let Some(part) = equal[parts.len()] else {
            break;
        };
        parts.push(part);
    }
    let at = parts.len();
    let (lo, hi) = if at < keys {
        (lower[at], upper[at])
    } else {
        (None, None)
    };
    let ranged = lo.is_some() || hi.is_some();
    let after = if ranged { at + 1 } else { at };
    if (after..keys).any(|c| equal[c].is_some() || lower[c].is_some() || upper[c].is_some()) {
        return None;
    }
    if !ranged {
        if parts.is_empty() {
            return None;
        }
        let lower = End {
            parts: parts.clone(),
            after: false,
        };
        return Some((lower, End { parts, after: true }));
    }
    // a range open at one end stops before the column's NULLs
    let nulls_first = *(*index).nulls_first.add(at);
    let at_end = |part: Part, after: bool| {
        let mut p = parts.clone();
        p.push(part);
        End { parts: p, after }
    };
    let lower = match lo {
        Some((part, strict)) => at_end(part, strict),
        None if nulls_first => at_end(None, true),
        None => End {
            parts: parts.clone(),
            after: false,
        },
    };
    let upper = match hi {
        Some((part, inclusive)) => at_end(part, inclusive),
        None if !nulls_first => at_end(None, false),
        None => End {
            parts: parts.clone(),
            after: true,
        },
    };
    Some((lower, upper))
}

/// One condition against a constant, read for the value it compares: the expression or column
/// compared, and how it is compared.
enum Compared {
    /// An operator between the value and a constant, the value on its left where `left`.
    Operator {
        value: *mut pg_sys::Node,
        constant: *mut pg_sys::Const,
        left: bool,
        func: pg_sys::Oid,
        collation: pg_sys::Oid,
    },
    /// An operator between the value and any element of a constant array.
    Any {
        value: *mut pg_sys::Node,
        array: *mut pg_sys::Const,
        func: pg_sys::Oid,
        collation: pg_sys::Oid,
    },
    /// A test whether the value is NULL, or is not.
    Null { value: *mut pg_sys::Node, is: bool },
}

impl Compared {
    fn value(&self) -> *mut pg_sys::Node {
        match *self {
            Compared::Operator { value, .. }
            | Compared::Any { value, .. }
            | Compared::Null { value, .. } => value,
        }
    }
}

/// The condition `ri` read for the value it compares, where it compares one with a constant by a
/// strict operator, or tests it for NULL.
unsafe fn compared(
    root: *mut pg_sys::PlannerInfo,
    ri: *mut pg_sys::RestrictInfo,
) -> Option<Compared> {
    let clause = (*ri).clause as *mut pg_sys::Node;
    if clause.is_null() {
        return None;
    }
    match (*clause).type_ {
        pg_sys::NodeTag::T_OpExpr => {
            let op = clause as *mut pg_sys::OpExpr;
            let args = cells((*op).args);
            if args.len() != 2 || !pg_sys::func_strict((*op).opfuncid) {
                return None;
            }
            let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
            let (value, constant, left) = match (constant(root, l), constant(root, r)) {
                (None, Some(c)) => (l, c, true),
                (Some(c), None) => (r, c, false),
                _ => return None,
            };
            Some(Compared::Operator {
                value,
                constant,
                left,
                func: (*op).opfuncid,
                collation: (*op).inputcollid,
            })
        }
        pg_sys::NodeTag::T_ScalarArrayOpExpr => {
            let op = clause as *mut pg_sys::ScalarArrayOpExpr;
            let args = cells((*op).args);
            if !(*op).useOr || args.len() != 2 || !pg_sys::func_strict((*op).opfuncid) {
                return None;
            }
            Some(Compared::Any {
                value: args[0] as *mut pg_sys::Node,
                array: constant(root, args[1] as *mut pg_sys::Node)?,
                func: (*op).opfuncid,
                collation: (*op).inputcollid,
            })
        }
        pg_sys::NodeTag::T_NullTest => {
            let test = clause as *mut pg_sys::NullTest;
            if (*test).argisrow {
                return None;
            }
            Some(Compared::Null {
                value: (*test).arg as *mut pg_sys::Node,
                is: (*test).nulltesttype == pg_sys::NullTestType::IS_NULL,
            })
        }
        _ => None,
    }
}

/// The column of the B-tree `index` (from 1) that stores `value` of `rel` as it is: a column of its
/// key or its INCLUDE, or an expression as one of its key columns.
unsafe fn stored(
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    value: *mut pg_sys::Node,
    desc: pg_sys::TupleDesc,
) -> Option<usize> {
    let at = match column_of(rel, index, value) {
        Some(i) => i,
        None => {
            let v = bare(value);
            if v.is_null() || (*v).type_ != pg_sys::NodeTag::T_Var {
                return None;
            }
            let var = v as *mut pg_sys::Var;
            if (*var).varno != (*rel).relid as i32 || (*var).varlevelsup != 0 {
                return None;
            }
            (0..(*index).ncolumns as usize)
                .find(|&i| *(*index).indexkeys.add(i) == (*var).varattno as i32)?
        }
    };
    let kept = (*pg_sys::TupleDescAttr(desc, at as i32)).atttypid;
    (kept == pg_sys::exprType(bare(value))).then_some(at + 1)
}

/// The condition `ri`'s test on the B-tree `index`'s entries: the value the index stores, at its
/// column (from 1), and how it is compared. None where the index does not store the value.
unsafe fn tested(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    desc: pg_sys::TupleDesc,
    ri: *mut pg_sys::RestrictInfo,
) -> Option<(usize, Compared)> {
    let c = compared(root, ri)?;
    let at = stored(rel, index, c.value(), desc)?;
    Some((at, c))
}

/// Whether the conditions `clauses` of `rel` can each be tested on the entries of the B-tree
/// `index`, by the value the index stores.
pub(crate) unsafe fn testable(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    clauses: &[*mut pg_sys::RestrictInfo],
) -> bool {
    if (*index).relam != pg_sys::BTREE_AM_OID || clauses.is_empty() {
        return false;
    }
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let desc = (*rel_index).rd_att;
    let all = clauses
        .iter()
        .all(|&ri| tested(root, rel, index, desc, ri).is_some());
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    all
}

/// A condition tested on the value an index stores, at its column (from 1).
struct Stored {
    at: usize,
    compared: Compared,
    func: pg_sys::FmgrInfo,
    elements: Vec<pg_sys::Datum>,
}

impl Stored {
    unsafe fn new(at: usize, compared: Compared) -> Stored {
        let mut func = pg_sys::FmgrInfo::default();
        let mut elements = Vec::new();
        match compared {
            Compared::Operator { func: f, .. } => pg_sys::fmgr_info(f, &mut func),
            Compared::Any { func: f, array, .. } => {
                pg_sys::fmgr_info(f, &mut func);
                if !(*array).constisnull {
                    elements = elements_of(array);
                }
            }
            Compared::Null { .. } => {}
        }
        Stored {
            at,
            compared,
            func,
            elements,
        }
    }

    /// Whether the entry `tuple` passes: its stored value compared, and nothing computed.
    unsafe fn passes(&mut self, tuple: pg_sys::IndexTuple, desc: pg_sys::TupleDesc) -> bool {
        let value = measure::value(tuple, self.at, desc);
        match self.compared {
            Compared::Null { is, .. } => value.is_none() == is,
            Compared::Operator {
                constant,
                left,
                collation,
                ..
            } => {
                let (Some(v), false) = (value, (*constant).constisnull) else {
                    return false;
                };
                let c = (*constant).constvalue;
                let (a, b) = if left { (v, c) } else { (c, v) };
                pg_sys::FunctionCall2Coll(&mut self.func, collation, a, b).value() & 0xff != 0
            }
            Compared::Any { collation, .. } => {
                let Some(v) = value else {
                    return false;
                };
                let func = &mut self.func as *mut pg_sys::FmgrInfo;
                self.elements
                    .iter()
                    .any(|&e| pg_sys::FunctionCall2Coll(func, collation, v, e).value() & 0xff != 0)
            }
        }
    }
}

/// The elements other than NULL of a constant array.
unsafe fn elements_of(array: *mut pg_sys::Const) -> Vec<pg_sys::Datum> {
    let elem = pg_sys::get_element_type((*array).consttype);
    if elem == pg_sys::InvalidOid {
        return Vec::new();
    }
    let (mut len, mut byval, mut align) = (0i16, false, 0 as std::ffi::c_char);
    pg_sys::get_typlenbyvalalign(elem, &mut len, &mut byval, &mut align);
    let a = pg_sys::pg_detoast_datum((*array).constvalue.cast_mut_ptr::<pg_sys::varlena>())
        as *mut pg_sys::ArrayType;
    let (mut values, mut nulls, mut n) = (null_mut(), null_mut(), 0);
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
    (0..n as usize)
        .filter(|&i| !*nulls.add(i))
        .map(|i| *values.add(i))
        .collect()
}
/// The rows of the block that the conditions `own` fix in the B-tree `index` of `rel` that also
/// pass the conditions `others`, tested on the block's own entries by the values the index stores,
/// the pages read, and whether every leaf of the block was read. The block is read as the B-tree
/// measures it: a block inside one leaf or two on those leaves, a block spanning whole leaves at
/// its first, its last and three between them. Those five stand for the block where every
/// condition is tested on a value of the index's key and the leaves read can stand for it; every
/// leaf is read where they cannot, while the pages allow. None where `own` make more than one
/// block, the index does not store a value a condition compares, or the read would read more than
/// `most` pages before it has a count.
pub(crate) unsafe fn rows(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    index: *mut pg_sys::IndexOptInfo,
    own: &[*mut pg_sys::RestrictInfo],
    others: &[*mut pg_sys::RestrictInfo],
    most: u32,
) -> Option<(f64, u32, bool)> {
    let (lower, upper) = ends(root, rel, index, own)?;
    let rel_index = pg_sys::index_open(
        (*index).indexoid,
        pg_sys::AccessShareLock as pg_sys::LOCKMODE,
    );
    let desc = (*rel_index).rd_att;
    let found = (|| {
        let mut stored = others
            .iter()
            .map(|&ri| tested(root, rel, index, desc, ri).map(|(at, c)| Stored::new(at, c)))
            .collect::<Option<Vec<Stored>>>()?;
        let keys = (*index).nkeycolumns as usize;
        let keyed = stored.iter().all(|s| s.at <= keys);
        // the rows read, and those passing: on the end leaves, on the leaves between that stand
        // for the rest, and on every leaf of a block read whole
        let (mut ends, mut between, mut every) =
            ((0.0f64, 0.0f64), (0.0f64, 0.0f64), (0.0f64, 0.0f64));
        let m = measure::on_leaves(
            rel_index,
            &lower,
            &upper,
            (*rel).tuples,
            true,
            most,
            &mut |leaf, tuple, n| {
                let n = n as f64;
                let passes = stored.iter_mut().all(|s| s.passes(tuple, desc));
                let at = match leaf {
                    Leaf::End => &mut ends,
                    Leaf::Between => &mut between,
                    Leaf::Every => &mut every,
                };
                at.0 += n;
                at.1 += if passes { n } else { 0.0 };
            },
        )?;
        if m.counted {
            return Some((every.1, m.pages, true));
        }
        // every leaf, where the leaves read cannot stand for the block and the pages allow
        let mut pages = m.pages;
        let left = most.saturating_sub(pages);
        if (!keyed || m.uneven)
            && m.whole_pages() <= left as f64
            && crate::budget::fits((*rel_index).rd_id, m.whole_pages())
        {
            let mut passing = 0.0f64;
            let (counted, read) =
                measure::every_leaf(rel_index, &lower, &upper, left, &mut |_, tuple, n| {
                    if stored.iter_mut().all(|s| s.passes(tuple, desc)) {
                        passing += n as f64;
                    }
                });
            pages += read;
            if counted.is_some() {
                return Some((passing, pages, true));
            }
        }
        // the ends for themselves, and the leaves between them at the rows the measure gives
        // them, passing at the share the leaves read give
        if between.0 <= 0.0 {
            return None;
        }
        let read = ends.0 + between.0;
        let share = (ends.1 + between.1) / read;
        Some((ends.1 + (m.rows - ends.0).max(0.0) * share, pages, false))
    })();
    pg_sys::index_close(rel_index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    found
}
