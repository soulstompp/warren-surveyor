// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading PostgreSQL's lists and building the nodes of a statement.

use pgrx::pg_sys;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr::null_mut;

pub(crate) unsafe fn cells(list: *mut pg_sys::List) -> Vec<*mut c_void> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).ptr_value)
        .collect()
}

pub(crate) unsafe fn oids(list: *mut pg_sys::List) -> Vec<pg_sys::Oid> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).oid_value)
        .collect()
}

pub(crate) unsafe fn tag(node: *mut c_void) -> pg_sys::NodeTag {
    (*(node as *mut pg_sys::Node)).type_
}

pub(crate) unsafe fn text(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    CStr::from_ptr(s).to_string_lossy().into_owned()
}

/// A node of `tag`, zeroed.
pub(crate) unsafe fn made<T>(tag: pg_sys::NodeTag) -> *mut T {
    let node = pg_sys::palloc0(std::mem::size_of::<T>()) as *mut T;
    (*(node as *mut pg_sys::Node)).type_ = tag;
    node
}

pub(crate) unsafe fn copy<T>(node: *mut T) -> *mut T {
    pg_sys::copyObjectImpl(node as *const c_void) as *mut T
}

pub(crate) unsafe fn list(items: &[*mut c_void]) -> *mut pg_sys::List {
    let mut list = null_mut();
    for &item in items {
        list = pg_sys::lappend(list, item);
    }
    list
}

pub(crate) unsafe fn name_of(text: &str) -> *mut c_char {
    let owned = CString::new(text).expect("a name holds no NUL");
    pg_sys::pstrdup(owned.as_ptr())
}

/// A qualified name, as a list of its parts.
pub(crate) unsafe fn qualified(parts: &[&str]) -> *mut pg_sys::List {
    let names: Vec<*mut c_void> = parts
        .iter()
        .map(|p| pg_sys::makeString(name_of(p)) as *mut c_void)
        .collect();
    list(&names)
}

/// An alias with its column names.
pub(crate) unsafe fn alias(name: &str, columns: &[String]) -> *mut pg_sys::Alias {
    let names: Vec<*mut c_void> = columns
        .iter()
        .map(|c| pg_sys::makeString(name_of(c)) as *mut c_void)
        .collect();
    pg_sys::makeAlias(name_of(name), list(&names))
}

/// Range table entry `index` of `rtable`.
pub(crate) unsafe fn entry(rtable: *mut pg_sys::List, index: c_int) -> *mut pg_sys::RangeTblEntry {
    (*(*rtable).elements.add(index as usize - 1)).ptr_value as *mut pg_sys::RangeTblEntry
}

/// A SELECT of `targets` over what `pstate` reads, the entries `from` side by side, with `quals`.
pub(crate) unsafe fn select_of(
    pstate: *mut pg_sys::ParseState,
    from: &[c_int],
    quals: *mut pg_sys::Node,
    targets: *mut pg_sys::List,
) -> *mut pg_sys::Query {
    let q: *mut pg_sys::Query = made(pg_sys::NodeTag::T_Query);
    (*q).commandType = pg_sys::CmdType::CMD_SELECT;
    (*q).querySource = pg_sys::QuerySource::QSRC_ORIGINAL;
    (*q).canSetTag = true;
    (*q).rtable = (*pstate).p_rtable;
    (*q).rteperminfos = (*pstate).p_rteperminfos;
    (*q).jointree = pg_sys::makeFromExpr(references(from), quals);
    (*q).targetList = targets;
    q
}

/// A reference to each range table entry of `from`.
pub(crate) unsafe fn references(from: &[c_int]) -> *mut pg_sys::List {
    let mut refs = Vec::new();
    for &index in from {
        let r: *mut pg_sys::RangeTblRef = made(pg_sys::NodeTag::T_RangeTblRef);
        (*r).rtindex = index;
        refs.push(r as *mut c_void);
    }
    list(&refs)
}

/// The columns a query returns: name, type and type modifier, in order.
pub(crate) unsafe fn columns(q: *mut pg_sys::Query) -> Vec<(String, pg_sys::Oid, i32)> {
    cells((*q).targetList)
        .into_iter()
        .map(|te| te as *mut pg_sys::TargetEntry)
        .filter(|te| !(**te).resjunk)
        .map(|te| {
            let expr = (*te).expr as *const pg_sys::Node;
            (
                text((*te).resname),
                pg_sys::exprType(expr),
                pg_sys::exprTypmod(expr),
            )
        })
        .collect()
}

/// `replacement` in the place of `original`, when it returns the same columns.
pub(crate) unsafe fn in_place_of(
    original: *mut pg_sys::Query,
    replacement: *mut pg_sys::Query,
) -> Option<*mut pg_sys::Query> {
    if columns(original) != columns(replacement) {
        return None;
    }
    (*replacement).querySource = (*original).querySource;
    (*replacement).canSetTag = (*original).canSetTag;
    (*replacement).hasRowSecurity = (*original).hasRowSecurity;
    (*replacement).queryId = (*original).queryId;
    (*replacement).stmt_location = (*original).stmt_location;
    (*replacement).stmt_len = (*original).stmt_len;
    Some(replacement)
}

/// The conditions ANDed in `node`.
pub(crate) unsafe fn conjuncts(node: *mut pg_sys::Node) -> Vec<*mut pg_sys::Node> {
    if node.is_null() {
        return Vec::new();
    }
    if (*node).type_ == pg_sys::NodeTag::T_BoolExpr
        && (*(node as *mut pg_sys::BoolExpr)).boolop == pg_sys::BoolExprType::AND_EXPR
    {
        return cells((*(node as *mut pg_sys::BoolExpr)).args)
            .into_iter()
            .flat_map(|c| conjuncts(c as *mut pg_sys::Node))
            .collect();
    }
    vec![node]
}

/// `quals` ANDed, or none.
pub(crate) unsafe fn and_of(quals: &[*mut pg_sys::Node]) -> *mut pg_sys::Node {
    match quals.len() {
        0 => null_mut(),
        1 => quals[0],
        _ => {
            let items: Vec<*mut c_void> = quals.iter().map(|&q| q as *mut c_void).collect();
            pg_sys::make_andclause(list(&items)) as *mut pg_sys::Node
        }
    }
}

/// The expression beneath any relabelling.
pub(crate) unsafe fn bare(mut node: *mut pg_sys::Node) -> *mut pg_sys::Node {
    while !node.is_null() && (*node).type_ == pg_sys::NodeTag::T_RelabelType {
        node = (*(node as *mut pg_sys::RelabelType)).arg as *mut pg_sys::Node;
    }
    node
}

/// The type cache's entry for `type_`, with the default ordering and equality operators a GROUP BY
/// reads, and whether the type hashes by that equality.
unsafe fn operators(type_: pg_sys::Oid) -> *mut pg_sys::TypeCacheEntry {
    pg_sys::lookup_type_cache(
        type_,
        (pg_sys::TYPECACHE_LT_OPR
            | pg_sys::TYPECACHE_EQ_OPR
            | pg_sys::TYPECACHE_GT_OPR
            | pg_sys::TYPECACHE_HASH_PROC) as c_int,
    )
}

/// The default equality operator of `type_`; none where the type has none.
pub(crate) unsafe fn equality(type_: pg_sys::Oid) -> pg_sys::Oid {
    (*operators(type_)).eq_opr
}

/// Whether `type_` has a default ordering and equality, to be sorted and grouped by.
pub(crate) unsafe fn ordered(type_: pg_sys::Oid) -> bool {
    let entry = operators(type_);
    (*entry).lt_opr != pg_sys::InvalidOid && (*entry).eq_opr != pg_sys::InvalidOid
}

/// When `clause` is an equality between two expressions, under a collation that tells apart what
/// it does not call equal, those two.
pub(crate) unsafe fn equality_args(
    clause: *mut pg_sys::Node,
) -> Option<(*mut pg_sys::Node, *mut pg_sys::Node)> {
    if (*clause).type_ != pg_sys::NodeTag::T_OpExpr {
        return None;
    }
    let op = clause as *mut pg_sys::OpExpr;
    let args = cells((*op).args);
    if args.len() != 2 {
        return None;
    }
    let (l, r) = (args[0] as *mut pg_sys::Node, args[1] as *mut pg_sys::Node);
    let left_type = pg_sys::exprType(bare(l));
    let right_type = pg_sys::exprType(bare(r));
    // the operator is the equality both sides' type sorts and groups by
    let eq = equality(left_type);
    let collation = (*op).inputcollid;
    (eq != pg_sys::InvalidOid
        && eq == (*op).opno
        && equality(right_type) == eq
        && (collation == pg_sys::InvalidOid || pg_sys::get_collation_isdeterministic(collation)))
    .then_some((l, r))
}

/// `node` as an argument of type `type_`, where its own type is another name for it.
pub(crate) unsafe fn as_argument(node: *mut pg_sys::Node, type_: pg_sys::Oid) -> *mut pg_sys::Expr {
    let own = pg_sys::exprType(node);
    if own == type_
        || type_ == pg_sys::InvalidOid
        || pg_sys::get_typtype(type_) as u8 == pg_sys::TYPTYPE_PSEUDO
    {
        return node as *mut pg_sys::Expr;
    }
    pg_sys::makeRelabelType(
        node as *mut pg_sys::Expr,
        type_,
        -1,
        pg_sys::exprCollation(node),
        pg_sys::CoercionForm::COERCE_IMPLICIT_CAST,
    ) as *mut pg_sys::Expr
}

/// `left = right` by the default equality of `left`'s type.
pub(crate) unsafe fn equals(
    left: *mut pg_sys::Node,
    right: *mut pg_sys::Node,
) -> Option<*mut pg_sys::Node> {
    let type_ = pg_sys::exprType(left);
    let eq = equality(type_);
    if eq == pg_sys::InvalidOid {
        return None;
    }
    let (mut left_type, mut right_type) = (pg_sys::InvalidOid, pg_sys::InvalidOid);
    pg_sys::op_input_types(eq, &mut left_type, &mut right_type);
    let clause = pg_sys::make_opclause(
        eq,
        pg_sys::BOOLOID,
        false,
        as_argument(left, left_type),
        as_argument(right, right_type),
        pg_sys::InvalidOid,
        pg_sys::exprCollation(left),
    ) as *mut pg_sys::OpExpr;
    pg_sys::set_opfuncid(clause);
    Some(clause as *mut pg_sys::Node)
}

/// How a column of `type_` is sorted and grouped by, for the target list entry `reference`; none
/// where the type has no default ordering or equality.
pub(crate) unsafe fn sort_group(
    type_: pg_sys::Oid,
    reference: pg_sys::Index,
) -> Option<*mut pg_sys::SortGroupClause> {
    let entry = operators(type_);
    let (lt, eq) = ((*entry).lt_opr, (*entry).eq_opr);
    if lt == pg_sys::InvalidOid || eq == pg_sys::InvalidOid {
        return None;
    }
    let sgc: *mut pg_sys::SortGroupClause = made(pg_sys::NodeTag::T_SortGroupClause);
    (*sgc).tleSortGroupRef = reference;
    (*sgc).eqop = eq;
    (*sgc).sortop = lt;
    (*sgc).nulls_first = false;
    (*sgc).hashable = (*entry).hash_proc != pg_sys::InvalidOid;
    Some(sgc)
}

/// A column of range table entry `varno` returned at `resno` by `target`'s expression.
pub(crate) unsafe fn column_var(
    varno: c_int,
    resno: i16,
    target: *mut pg_sys::Node,
) -> *mut pg_sys::Node {
    pg_sys::makeVar(
        varno,
        resno,
        pg_sys::exprType(target),
        pg_sys::exprTypmod(target),
        pg_sys::exprCollation(target),
        0,
    ) as *mut pg_sys::Node
}

pub(crate) unsafe fn ints(list: *mut pg_sys::List) -> Vec<i32> {
    if list.is_null() {
        return Vec::new();
    }
    (0..(*list).length as usize)
        .map(|i| (*(*list).elements.add(i)).int_value)
        .collect()
}

pub(crate) unsafe fn copy_name(name: *mut c_char) -> *mut c_char {
    if name.is_null() {
        null_mut()
    } else {
        pg_sys::pstrdup(name)
    }
}
