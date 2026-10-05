// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Each side keeps only the keys the other holds: the side the counted table is not on is grouped
//! only over the keys the counted side's grouping holds.
//!
//! It applies wherever a grouping is taken on each side first. The counted side's grouping becomes a
//! WITH query, read by the join of the two sides and by the other side, which keeps only the rows
//! whose key that WITH query holds.

use crate::nodes::*;
use pgrx::pg_sys;
use std::ffi::c_void;
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

/// `key IN (SELECT <column> FROM <cte>)`, for a query level whose WITH queries `cte` stands with:
/// the values of `key` that column of the WITH query holds.
pub(crate) unsafe fn held_by(
    key: *mut pg_sys::Node,
    cte: *mut pg_sys::CommonTableExpr,
    column: i16,
) -> Option<*mut pg_sys::Node> {
    let pstate = pg_sys::make_parsestate(null_mut());
    let read = pg_sys::makeRangeVar(null_mut(), (*cte).ctename, -1);
    pg_sys::addRangeTableEntryForCTE(pstate, cte, 1, read, true);
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
