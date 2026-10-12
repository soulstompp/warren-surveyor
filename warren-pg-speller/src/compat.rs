// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What differs between the PostgreSQL versions the speller builds for, each behind one function.

use pgrx::pg_sys;
use std::ffi::{c_int, c_void};
#[cfg(not(any(feature = "pg14", feature = "pg15")))]
use std::ptr::null_mut;

/// Column `i` of `desc`, counted from 0.
pub(crate) unsafe fn attribute(
    desc: pg_sys::TupleDesc,
    i: c_int,
) -> *mut pg_sys::FormData_pg_attribute {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::TupleDescAttr(desc, i)
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        (*desc).attrs.as_mut_ptr().add(i as usize)
    }
}

/// Whether column `i` of `desc`, counted from 0, is a column NOT NULL by a validated constraint.
pub(crate) unsafe fn not_null(desc: pg_sys::TupleDesc, i: c_int) -> bool {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        let attribute = pg_sys::TupleDescCompactAttr(desc, i);
        (*attribute).attnullability as u8 == pg_sys::ATTNULLABLE_VALID && !(*attribute).attisdropped
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        let attribute = attribute(desc, i);
        (*attribute).attnotnull && !(*attribute).attisdropped
    }
}

/// Whether `q`'s grouped columns are read through a range table entry of their own.
pub(crate) unsafe fn has_group_rte(q: *mut pg_sys::Query) -> bool {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        (*q).hasGroupRTE
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        let _ = q;
        false
    }
}

/// `node`, a part of `q`, with each grouped column read as what it stands for.
pub(crate) unsafe fn flatten_group_exprs(
    q: *mut pg_sys::Query,
    node: *mut pg_sys::Node,
) -> *mut pg_sys::Node {
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::flatten_group_exprs(null_mut(), q, node)
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        let _ = q;
        node
    }
}

/// `node`, a part of `q`, with each merged join column read as what it stands for.
pub(crate) unsafe fn flatten_join_alias_vars(
    q: *mut pg_sys::Query,
    node: *mut pg_sys::Node,
) -> *mut pg_sys::Node {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        pg_sys::flatten_join_alias_vars(q, node)
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        pg_sys::flatten_join_alias_vars(null_mut(), q, node)
    }
}

/// `node` with each column of the range table entry `varno` read as the expression `targets`
/// gives it, raising an ERROR where it gives none.
pub(crate) unsafe fn replace_vars(
    node: *mut pg_sys::Node,
    varno: c_int,
    e: *mut pg_sys::RangeTblEntry,
    targets: *mut pg_sys::List,
    sublinks: &mut bool,
) -> *mut pg_sys::Node {
    let report = pg_sys::ReplaceVarsNoMatchOption::REPLACEVARS_REPORT_ERROR;
    #[cfg(any(feature = "pg18", feature = "pg19"))]
    {
        pg_sys::ReplaceVarsFromTargetList(node, varno, 0, e, targets, 0, report, 0, sublinks)
    }
    #[cfg(not(any(feature = "pg18", feature = "pg19")))]
    {
        pg_sys::ReplaceVarsFromTargetList(node, varno, 0, e, targets, report, 0, sublinks)
    }
}

/// A copy of `node` with `mutator` applied to each node directly below it.
pub(crate) unsafe fn expression_tree_mutator(
    node: *mut pg_sys::Node,
    mutator: unsafe extern "C-unwind" fn(*mut pg_sys::Node, *mut c_void) -> *mut pg_sys::Node,
    context: *mut c_void,
) -> *mut pg_sys::Node {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        // the mutator's type names no arguments on these versions
        let mutator = std::mem::transmute::<
            unsafe extern "C-unwind" fn(*mut pg_sys::Node, *mut c_void) -> *mut pg_sys::Node,
            unsafe extern "C-unwind" fn() -> *mut pg_sys::Node,
        >(mutator);
        pg_sys::expression_tree_mutator(node, Some(mutator), context)
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        pg_sys::expression_tree_mutator_impl(node, Some(mutator), context)
    }
}

/// The range table entry a column reads.
pub(crate) unsafe fn varno(var: *mut pg_sys::Var) -> c_int {
    #[cfg(feature = "pg14")]
    {
        (*var).varno as c_int
    }
    #[cfg(not(feature = "pg14"))]
    {
        (*var).varno
    }
}

/// Makes `var` read range table entry `varno`.
pub(crate) unsafe fn set_varno(var: *mut pg_sys::Var, varno: c_int) {
    #[cfg(feature = "pg14")]
    {
        (*var).varno = varno as pg_sys::Index;
    }
    #[cfg(not(feature = "pg14"))]
    {
        (*var).varno = varno;
    }
}

/// A column `attno` of range table entry `varno`, of the type, type modifier and collation given,
/// `levelsup` query levels out.
pub(crate) unsafe fn make_var(
    varno: c_int,
    attno: i16,
    type_: pg_sys::Oid,
    typmod: i32,
    collation: pg_sys::Oid,
    levelsup: pg_sys::Index,
) -> *mut pg_sys::Var {
    #[cfg(feature = "pg14")]
    let varno = varno as pg_sys::Index;
    pg_sys::makeVar(varno, attno, type_, typmod, collation, levelsup)
}

/// Whether the FROM items `x` of `xq` and `y` of `yq` are read as the same role, for the same
/// rights.
pub(crate) unsafe fn same_reader(
    xq: *mut pg_sys::Query,
    x: *mut pg_sys::RangeTblEntry,
    yq: *mut pg_sys::Query,
    y: *mut pg_sys::RangeTblEntry,
) -> bool {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        let _ = (xq, yq);
        (*x).checkAsUser == (*y).checkAsUser && (*x).requiredPerms == (*y).requiredPerms
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        match ((*x).perminfoindex > 0, (*y).perminfoindex > 0) {
            (false, false) => true,
            (true, true) => {
                let a = pg_sys::getRTEPermissionInfo((*xq).rteperminfos, x);
                let b = pg_sys::getRTEPermissionInfo((*yq).rteperminfos, y);
                (*a).checkAsUser == (*b).checkAsUser && (*a).requiredPerms == (*b).requiredPerms
            }
            _ => false,
        }
    }
}

/// Gives `e`, a copy of the range table entry `original` of `source`, a copy of the permissions
/// `original` is read with, added to `perminfos`. Where each entry holds its own, `e` has them.
pub(crate) unsafe fn copy_permissions(
    source: *mut pg_sys::Query,
    original: *mut pg_sys::RangeTblEntry,
    e: *mut pg_sys::RangeTblEntry,
    perminfos: &mut *mut pg_sys::List,
) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        let _ = (source, original, e, perminfos);
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        if (*original).perminfoindex > 0 {
            let info = pg_sys::copyObjectImpl(pg_sys::getRTEPermissionInfo(
                (*source).rteperminfos,
                original,
            ) as *const c_void);
            *perminfos = pg_sys::lappend(*perminfos, info);
            (*e).perminfoindex = (**perminfos).length as pg_sys::Index;
        }
    }
}

/// Makes `perminfos` the permissions `q`'s range table entries are read with. Where each entry
/// holds its own, there is nothing to set.
pub(crate) unsafe fn set_permissions(q: *mut pg_sys::Query, perminfos: *mut pg_sys::List) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        let _ = (q, perminfos);
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        (*q).rteperminfos = perminfos;
    }
}

/// The permissions the range table entries `pstate` added are read with.
pub(crate) unsafe fn parsed_permissions(pstate: *mut pg_sys::ParseState) -> *mut pg_sys::List {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        let _ = pstate;
        std::ptr::null_mut()
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        (*pstate).p_rteperminfos
    }
}

/// Adds the range table of `sub` to the end of `f`'s, with the permissions its entries are read
/// with.
pub(crate) unsafe fn combine_range_tables(f: *mut pg_sys::Query, sub: *mut pg_sys::Query) {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        (*f).rtable = pg_sys::list_concat((*f).rtable, (*sub).rtable);
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        pg_sys::CombineRangeTables(
            &mut (*f).rtable,
            &mut (*f).rteperminfos,
            (*sub).rtable,
            (*sub).rteperminfos,
        );
    }
}

/// Whether the subquery FROM item `e` is a view.
pub(crate) unsafe fn is_view(e: *mut pg_sys::RangeTblEntry) -> bool {
    #[cfg(any(feature = "pg14", feature = "pg15"))]
    {
        // a view keeps its relation, and its query holds the view first, where its permissions
        // are checked
        if (*e).relid != pg_sys::InvalidOid {
            return true;
        }
        let sub = (*e).subquery;
        if sub.is_null() || (*sub).rtable.is_null() {
            return false;
        }
        let first = (*(*(*sub).rtable).elements).ptr_value as *mut pg_sys::RangeTblEntry;
        (*first).rtekind == pg_sys::RTEKind::RTE_RELATION
            && (*first).relkind as u8 == pg_sys::RELKIND_VIEW
    }
    #[cfg(not(any(feature = "pg14", feature = "pg15")))]
    {
        (*e).perminfoindex != 0
    }
}
