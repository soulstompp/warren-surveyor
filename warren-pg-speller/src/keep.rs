// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Each side keeps only the keys the other holds: the side the counted table is not on is grouped
//! only over the keys the counted side's grouping holds.
//!
//! It applies wherever a grouping is taken on each side first. The counted side's grouping becomes a
//! WITH query, read by the join of the two sides and by the other side, which keeps only the rows
//! whose key that WITH query holds; where the other side's column of the key is a UNION ALL's, each
//! arm keeps its own rows.

use crate::compat::varno;
use crate::distribute::union_all_leaves;
use crate::levels::plain;
use crate::nodes::*;
use crate::reading::reaches_outside;
use pgrx::pg_sys;
use std::ffi::{c_int, c_void};
use std::ptr::null_mut;

/// `base`, or `base` numbered, whichever no WITH query of `ctes` is named.
pub(crate) unsafe fn unused_name(ctes: *mut pg_sys::List, base: &str) -> String {
    let taken: Vec<String> = cells(ctes)
        .into_iter()
        .map(|c| text((*(c as *mut pg_sys::CommonTableExpr)).ctename))
        .collect();
    let mut name = base.to_string();
    let mut n = 1;
    while taken.contains(&name) {
        n += 1;
        name = format!("{base}_{n}");
    }
    name
}

/// A WITH query `name` that is the query `q`, one level below the query that holds it, not yet
/// read.
pub(crate) unsafe fn with_query(
    name: &str,
    q: *mut pg_sys::Query,
    names: &[String],
) -> *mut pg_sys::CommonTableExpr {
    let cte: *mut pg_sys::CommonTableExpr = made(pg_sys::NodeTag::T_CommonTableExpr);
    (*cte).ctename = name_of(name);
    (*cte).ctematerialized = pg_sys::CTEMaterialize::CTEMaterializeDefault;
    (*cte).ctequery = q as *mut pg_sys::Node;
    (*cte).location = -1;
    let (mut types, mut typmods, mut collations) = (null_mut(), null_mut(), null_mut());
    let mut column_names = Vec::new();
    for (t, name) in cells((*q).targetList).into_iter().zip(names) {
        let expr = (*(t as *mut pg_sys::TargetEntry)).expr as *mut pg_sys::Node;
        types = pg_sys::lappend_oid(types, pg_sys::exprType(expr));
        typmods = pg_sys::lappend_int(typmods, pg_sys::exprTypmod(expr));
        collations = pg_sys::lappend_oid(collations, pg_sys::exprCollation(expr));
        column_names.push(pg_sys::makeString(name_of(name)) as *mut c_void);
    }
    (*cte).ctecolnames = list(&column_names);
    (*cte).ctecoltypes = types;
    (*cte).ctecoltypmods = typmods;
    (*cte).ctecolcollations = collations;
    cte
}

/// `key IN (SELECT <column> FROM <cte>)`, for a query `below` levels under the one whose WITH
/// queries `cte` stands with: the values of `key` that column of the WITH query holds.
pub(crate) unsafe fn held_by(
    key: *mut pg_sys::Node,
    cte: *mut pg_sys::CommonTableExpr,
    column: i16,
    below: pg_sys::Index,
) -> Option<*mut pg_sys::Node> {
    let pstate = pg_sys::make_parsestate(null_mut());
    let read = pg_sys::makeRangeVar(null_mut(), (*cte).ctename, -1);
    pg_sys::addRangeTableEntryForCTE(pstate, cte, 1 + below, read, true);
    let at = column as usize - 1;
    let (type_, typmod, collation) = (
        oids((*cte).ctecoltypes)[at],
        ints((*cte).ctecoltypmods)[at],
        oids((*cte).ctecolcollations)[at],
    );
    let var = pg_sys::makeVar(1, column, type_, typmod, collation, 0);
    let te = pg_sys::makeTargetEntry(var as *mut pg_sys::Expr, 1, name_of("key"), false);
    let sub = select_of(pstate, &[1], null_mut(), list(&[te as *mut c_void]));
    pg_sys::free_parsestate(pstate);
    let param: *mut pg_sys::Param = made(pg_sys::NodeTag::T_Param);
    (*param).paramkind = pg_sys::ParamKind::PARAM_SUBLINK;
    (*param).paramid = 1;
    (*param).paramtype = type_;
    (*param).paramtypmod = typmod;
    (*param).paramcollid = collation;
    (*param).location = -1;
    let link: *mut pg_sys::SubLink = made(pg_sys::NodeTag::T_SubLink);
    (*link).subLinkType = pg_sys::SubLinkType::ANY_SUBLINK;
    (*link).testexpr = equals(copy(key), param as *mut pg_sys::Node)?;
    (*link).operName = list(&[pg_sys::makeString(name_of("=")) as *mut c_void]);
    (*link).subselect = sub as *mut pg_sys::Node;
    (*link).location = -1;
    Some(link as *mut pg_sys::Node)
}

/// Where `key` is a column of a UNION ALL that `q` reads, each of whose arms is a plain SELECT
/// returning that column with the union's own type: for each arm, its range table index in the
/// union and the values of the arm's own expression of the column that `column` of `cte` holds,
/// for the arm, two levels under `q`, whose WITH queries `cte` stands with. None where any arm
/// is not such a SELECT.
pub(crate) unsafe fn held_in_arms(
    q: *mut pg_sys::Query,
    key: *mut pg_sys::Var,
    cte: *mut pg_sys::CommonTableExpr,
    column: i16,
) -> Option<Vec<(c_int, *mut pg_sys::Node)>> {
    if (*key).varlevelsup != 0 {
        return None;
    }
    let e = entry((*q).rtable, varno(key));
    if (*e).rtekind != pg_sys::RTEKind::RTE_SUBQUERY || (*e).security_barrier {
        return None;
    }
    let u = (*e).subquery;
    if (*u).setOperations.is_null()
        || !(*u).limitCount.is_null()
        || !(*u).limitOffset.is_null()
        || !(*u).rowMarks.is_null()
    {
        return None;
    }
    let mut leaves = Vec::new();
    if !union_all_leaves((*u).setOperations as *mut c_void, &mut leaves) {
        return None;
    }
    let (type_, typmod, collation) = (
        pg_sys::exprType(key as *mut pg_sys::Node),
        pg_sys::exprTypmod(key as *mut pg_sys::Node),
        pg_sys::exprCollation(key as *mut pg_sys::Node),
    );
    let mut held = Vec::new();
    for leaf in leaves {
        let arm = entry((*u).rtable, leaf);
        if (*arm).rtekind != pg_sys::RTEKind::RTE_SUBQUERY
            || (*arm).lateral
            || (*arm).security_barrier
        {
            return None;
        }
        let a = (*arm).subquery;
        if !plain(a) || reaches_outside(a) {
            return None;
        }
        let te = cells((*a).targetList)
            .into_iter()
            .map(|t| t as *mut pg_sys::TargetEntry)
            .find(|&t| (*t).resno == (*key).varattno && !(*t).resjunk)?;
        let own = (*te).expr as *mut pg_sys::Node;
        if pg_sys::exprType(own) != type_
            || pg_sys::exprTypmod(own) != typmod
            || pg_sys::exprCollation(own) != collation
        {
            return None;
        }
        held.push((leaf, held_by(own, cte, column, 2)?));
    }
    Some(held)
}

/// Each arm of the UNION ALL at `index` in `q`'s range table kept to its condition from
/// `held_in_arms`.
pub(crate) unsafe fn hold_arms(
    q: *mut pg_sys::Query,
    index: c_int,
    held: &[(c_int, *mut pg_sys::Node)],
) {
    let u = (*entry((*q).rtable, index)).subquery;
    for &(leaf, condition) in held {
        let a = (*entry((*u).rtable, leaf)).subquery;
        let quals = (*(*a).jointree).quals;
        (*(*a).jointree).quals = if quals.is_null() {
            condition
        } else {
            and_of(&[quals, condition])
        };
        (*a).hasSubLinks = true;
    }
}
