// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A query level read as relations side by side, and a query level made from some of them.

use crate::nodes::*;
use crate::reading::renumber;
use pgrx::pg_sys;
use std::ffi::{c_int, c_void, CStr};
use std::ptr::null_mut;

/// The FROM items of `q`, where every join is an inner join, and the conditions of its joins and
/// WHERE, each read through the joins' merged columns.
pub(crate) unsafe fn from_items(
    q: *mut pg_sys::Query,
) -> Option<(Vec<c_int>, Vec<*mut pg_sys::Node>)> {
    unsafe fn walk(
        node: *mut c_void,
        items: &mut Vec<c_int>,
        quals: &mut Vec<*mut pg_sys::Node>,
    ) -> bool {
        if node.is_null() {
            return true;
        }
        match tag(node) {
            pg_sys::NodeTag::T_FromExpr => {
                let f = node as *mut pg_sys::FromExpr;
                quals.extend(conjuncts((*f).quals));
                cells((*f).fromlist)
                    .into_iter()
                    .all(|item| walk(item, items, quals))
            }
            pg_sys::NodeTag::T_JoinExpr => {
                let j = node as *mut pg_sys::JoinExpr;
                if (*j).jointype != pg_sys::JoinType::JOIN_INNER {
                    return false;
                }
                quals.extend(conjuncts((*j).quals));
                walk((*j).larg as *mut c_void, items, quals)
                    && walk((*j).rarg as *mut c_void, items, quals)
            }
            pg_sys::NodeTag::T_RangeTblRef => {
                items.push((*(node as *mut pg_sys::RangeTblRef)).rtindex);
                true
            }
            _ => false,
        }
    }
    let (mut items, mut quals) = (Vec::new(), Vec::new());
    if !walk((*q).jointree as *mut c_void, &mut items, &mut quals) {
        return None;
    }
    let quals = quals
        .into_iter()
        .map(|c| pg_sys::flatten_join_alias_vars(null_mut(), q, c))
        .collect();
    Some((items, quals))
}

/// Whether `q` is a SELECT of expressions over joined relations, and nothing more: no grouping,
/// aggregate, window, DISTINCT, ORDER BY, LIMIT, set operation, locking, WITH query of its own or
/// volatile function.
pub(crate) unsafe fn plain(q: *mut pg_sys::Query) -> bool {
    (*q).commandType == pg_sys::CmdType::CMD_SELECT
        && (*q).utilityStmt.is_null()
        && !(*q).hasAggs
        && !(*q).hasWindowFuncs
        && !(*q).hasTargetSRFs
        && !(*q).hasForUpdate
        && !(*q).hasModifyingCTE
        && !(*q).hasRecursive
        && !(*q).hasGroupRTE
        && (*q).groupClause.is_null()
        && (*q).groupingSets.is_null()
        && (*q).havingQual.is_null()
        && (*q).distinctClause.is_null()
        && (*q).sortClause.is_null()
        && (*q).limitCount.is_null()
        && (*q).limitOffset.is_null()
        && (*q).setOperations.is_null()
        && (*q).rowMarks.is_null()
        && (*q).cteList.is_null()
        && !pg_sys::contain_volatile_functions(q as *mut pg_sys::Node)
}

/// A FROM item kind a respelling moves: a table, a WITH query, or a subquery that reads nothing
/// beside it.
pub(crate) unsafe fn movable(e: *mut pg_sys::RangeTblEntry) -> bool {
    !(*e).lateral
        && matches!(
            (*e).rtekind,
            pg_sys::RTEKind::RTE_RELATION
                | pg_sys::RTEKind::RTE_CTE
                | pg_sys::RTEKind::RTE_SUBQUERY
        )
        && ((*e).rtekind != pg_sys::RTEKind::RTE_CTE || !(*e).self_reference)
}

/// A query level read as relations side by side.
pub(crate) struct Flat {
    /// A copy of the query, its range table grown by what was read through.
    pub(crate) q: *mut pg_sys::Query,
    /// Its FROM items, as range table indexes.
    pub(crate) items: Vec<c_int>,
    /// Its conditions.
    pub(crate) quals: Vec<*mut pg_sys::Node>,
}

/// `q` read as relations side by side: its grouped columns and merged join columns read as what
/// they stand for, each WITH query it alone reads once read in place, and each plain subquery in its
/// FROM read as the relations it joins.
pub(crate) unsafe fn flat(q: *mut pg_sys::Query) -> Option<Flat> {
    let f = copy(q);
    if (*f).hasGroupRTE {
        (*f).targetList =
            pg_sys::flatten_group_exprs(null_mut(), f, (*f).targetList as *mut pg_sys::Node)
                as *mut pg_sys::List;
        (*f).havingQual = pg_sys::flatten_group_exprs(null_mut(), f, (*f).havingQual);
    }
    (*f).targetList =
        pg_sys::flatten_join_alias_vars(null_mut(), f, (*f).targetList as *mut pg_sys::Node)
            as *mut pg_sys::List;
    (*f).havingQual = pg_sys::flatten_join_alias_vars(null_mut(), f, (*f).havingQual);
    let (mut items, mut quals) = from_items(f)?;
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < items.len() {
            let index = items[i];
            let e = entry((*f).rtable, index);
            if (*e).rtekind == pg_sys::RTEKind::RTE_CTE && (*e).ctelevelsup == 0 && in_place(f, e) {
                changed = true;
            }
            if (*e).rtekind == pg_sys::RTEKind::RTE_SUBQUERY {
                if let Some((more, conditions)) = pull_up(f, index, &mut quals) {
                    items.splice(i..=i, more);
                    quals.extend(conditions);
                    changed = true;
                    continue;
                }
            }
            i += 1;
        }
        if !changed {
            break;
        }
    }
    Some(Flat { q: f, items, quals })
}

/// Reads the WITH query of `f` that `e` names in `e`'s place, where `e` is its only reader and it
/// is a SELECT that is not recursive, not kept MATERIALIZED and calls no volatile function.
pub(crate) unsafe fn in_place(f: *mut pg_sys::Query, e: *mut pg_sys::RangeTblEntry) -> bool {
    let name = CStr::from_ptr((*e).ctename);
    let ctes: Vec<*mut pg_sys::CommonTableExpr> = cells((*f).cteList)
        .into_iter()
        .map(|c| c as *mut pg_sys::CommonTableExpr)
        .collect();
    let Some(&cte) = ctes.iter().find(|&&c| CStr::from_ptr((*c).ctename) == name) else {
        return false;
    };
    let cq = (*cte).ctequery as *mut pg_sys::Query;
    if (*cte).cterecursive
        || (*cte).cterefcount != 1
        || (*cte).ctematerialized == pg_sys::CTEMaterialize::CTEMaterializeAlways
        || cq.is_null()
        || tag(cq as *mut c_void) != pg_sys::NodeTag::T_Query
        || (*cq).commandType != pg_sys::CmdType::CMD_SELECT
        || (*cq).hasModifyingCTE
        || pg_sys::contain_volatile_functions(cq as *mut pg_sys::Node)
    {
        return false;
    }
    (*e).rtekind = pg_sys::RTEKind::RTE_SUBQUERY;
    (*e).subquery = copy(cq);
    (*e).security_barrier = false;
    (*e).ctename = null_mut();
    (*e).ctelevelsup = 0;
    (*e).self_reference = false;
    (*e).coltypes = null_mut();
    (*e).coltypmods = null_mut();
    (*e).colcollations = null_mut();
    let kept: Vec<*mut c_void> = ctes
        .iter()
        .filter(|&&c| c != cte)
        .map(|&c| c as *mut c_void)
        .collect();
    (*f).cteList = list(&kept);
    true
}

/// The FROM item `index` of `f`, a plain subquery that reads nothing beside it and is not a view,
/// read as the relations it joins: its range table added to `f`'s, each column of it read as the
/// expression it returns. Returns its items and conditions.
pub(crate) unsafe fn pull_up(
    f: *mut pg_sys::Query,
    index: c_int,
    quals: &mut [*mut pg_sys::Node],
) -> Option<(Vec<c_int>, Vec<*mut pg_sys::Node>)> {
    let e = entry((*f).rtable, index);
    if (*e).lateral
        || (*e).security_barrier
        || !(*e).securityQuals.is_null()
        || (*e).perminfoindex != 0
        || (*e).subquery.is_null()
        || !plain((*e).subquery)
    {
        return None;
    }
    let sub = copy((*e).subquery);
    if cells((*sub).rtable)
        .into_iter()
        .any(|x| (*(x as *mut pg_sys::RangeTblEntry)).lateral)
    {
        return None;
    }
    pg_sys::IncrementVarSublevelsUp(sub as *mut pg_sys::Node, -1, 1);
    (*sub).targetList =
        pg_sys::flatten_join_alias_vars(null_mut(), sub, (*sub).targetList as *mut pg_sys::Node)
            as *mut pg_sys::List;
    let (items, conditions) = from_items(sub)?;
    let offset = (*(*f).rtable).length;
    // the row security conditions of a table name its own entry
    for x in cells((*sub).rtable) {
        let x = x as *mut pg_sys::RangeTblEntry;
        pg_sys::OffsetVarNodes((*x).securityQuals as *mut pg_sys::Node, offset, 0);
    }
    pg_sys::CombineRangeTables(
        &mut (*f).rtable,
        &mut (*f).rteperminfos,
        (*sub).rtable,
        (*sub).rteperminfos,
    );
    pg_sys::OffsetVarNodes((*sub).targetList as *mut pg_sys::Node, offset, 0);
    for &c in &conditions {
        pg_sys::OffsetVarNodes(c, offset, 0);
    }
    let mut sublinks = (*f).hasSubLinks;
    let replace = |node: *mut pg_sys::Node, sublinks: &mut bool| {
        pg_sys::ReplaceVarsFromTargetList(
            node,
            index,
            0,
            e,
            (*sub).targetList,
            0,
            pg_sys::ReplaceVarsNoMatchOption::REPLACEVARS_REPORT_ERROR,
            0,
            sublinks,
        )
    };
    (*f).targetList =
        replace((*f).targetList as *mut pg_sys::Node, &mut sublinks) as *mut pg_sys::List;
    (*f).havingQual = replace((*f).havingQual, &mut sublinks);
    for c in quals.iter_mut() {
        *c = replace(*c, &mut sublinks);
    }
    (*f).hasSubLinks = sublinks || (*sub).hasSubLinks;
    Some((items.into_iter().map(|i| i + offset).collect(), conditions))
}

/// Whether the row security conditions of an entry of `rtable` hold a subquery.
pub(crate) unsafe fn secured_by_sublink(rtable: *mut pg_sys::List) -> bool {
    cells(rtable).into_iter().any(|e| {
        let e = e as *mut pg_sys::RangeTblEntry;
        pg_sys::checkExprHasSubLink((*e).securityQuals as *mut pg_sys::Node)
    })
}

/// A query level holding copies of the range table entries `items` of `source`, with their
/// permissions, in that order: it reads them side by side under `quals`, and returns `targets`,
/// each expression of `source`'s level. Group keys are given by `groups`, each the reference of a
/// target and how it is grouped.
pub(crate) unsafe fn level(
    source: *mut pg_sys::Query,
    items: &[c_int],
    quals: &[*mut pg_sys::Node],
    targets: &[(*mut pg_sys::Node, String, pg_sys::Index)],
    groups: &[*mut pg_sys::SortGroupClause],
) -> Option<*mut pg_sys::Query> {
    let mut map = vec![0; (*(*source).rtable).length as usize + 1];
    for (j, &i) in items.iter().enumerate() {
        map[i as usize] = j as c_int + 1;
    }
    let (mut rtable, mut perminfos) = (null_mut(), null_mut());
    for &i in items {
        let original = entry((*source).rtable, i);
        let e = copy(original);
        if (*original).perminfoindex > 0 {
            let info = copy(pg_sys::getRTEPermissionInfo(
                (*source).rteperminfos,
                original,
            ));
            perminfos = pg_sys::lappend(perminfos, info as *mut c_void);
            (*e).perminfoindex = (*perminfos).length as pg_sys::Index;
        }
        // the row security conditions of a table name its own entry
        if !renumber((*e).securityQuals as *mut pg_sys::Node, &map) {
            return None;
        }
        rtable = pg_sys::lappend(rtable, e as *mut c_void);
    }
    let moved = |node: *mut pg_sys::Node| -> Option<*mut pg_sys::Node> {
        let c = copy(node);
        renumber(c, &map).then_some(c)
    };
    let mut conditions = Vec::new();
    for &c in quals {
        conditions.push(moved(c)?);
    }
    let mut entries = Vec::new();
    for (resno, (expr, name, reference)) in targets.iter().enumerate() {
        let te = pg_sys::makeTargetEntry(
            moved(*expr)? as *mut pg_sys::Expr,
            resno as i16 + 1,
            name_of(name),
            false,
        );
        (*te).ressortgroupref = *reference;
        entries.push(te as *mut c_void);
    }
    let q: *mut pg_sys::Query = made(pg_sys::NodeTag::T_Query);
    (*q).commandType = pg_sys::CmdType::CMD_SELECT;
    (*q).querySource = pg_sys::QuerySource::QSRC_ORIGINAL;
    (*q).canSetTag = true;
    (*q).rtable = rtable;
    (*q).rteperminfos = perminfos;
    let refs: Vec<c_int> = (1..=items.len() as c_int).collect();
    (*q).jointree = pg_sys::makeFromExpr(references(&refs), and_of(&conditions));
    (*q).targetList = list(&entries);
    let group_items: Vec<*mut c_void> = groups.iter().map(|&g| g as *mut c_void).collect();
    (*q).groupClause = list(&group_items);
    (*q).hasAggs = !groups.is_empty()
        || pg_sys::contain_aggs_of_level((*q).targetList as *mut pg_sys::Node, 0);
    (*q).hasSubLinks = pg_sys::checkExprHasSubLink((*q).targetList as *mut pg_sys::Node)
        || pg_sys::checkExprHasSubLink((*(*q).jointree).quals)
        || secured_by_sublink(rtable);
    (*q).hasRowSecurity = (*source).hasRowSecurity;
    Some(q)
}
