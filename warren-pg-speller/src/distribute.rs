// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A join distributes over UNION ALL: a relation joined alike in every arm of a UNION ALL is joined
//! once, to the union of the rest of each arm.
//!
//! It applies where the arms, each a plain SELECT over inner joins that reads nothing of a query
//! around it, all read one FROM item as the same table, WITH query or subquery, read alike, as the
//! same role, under the same row security; where every arm keeps another FROM item; where each arm's
//! conditions on that relation alone are the same; where each arm joins it to the rest of the arm by
//! the same equalities; and where each column the arms return reads that relation alone, the same
//! expression in every arm, or does not read it at all.

use crate::levels::{from_items, level, movable, plain, secured_by_sublink};
use crate::nodes::*;
use crate::reading::{reaches_outside, reads, renumber};
use crate::Net;
use pgrx::pg_sys;
use std::ffi::{c_int, c_void, CStr};
use std::ptr::null_mut;

/// An arm of a UNION ALL, read as relations side by side.
pub(crate) struct Arm {
    q: *mut pg_sys::Query,
    items: Vec<c_int>,
    quals: Vec<*mut pg_sys::Node>,
    outputs: Vec<*mut pg_sys::Node>,
}

/// What an arm does with the relation common to every arm.
pub(crate) struct Common {
    /// Its conditions on the relation alone, the relation renumbered to 1.
    own: Vec<*mut pg_sys::Node>,
    /// Each equality joining it to the rest of the arm: the relation's side renumbered to 1, the
    /// rest's side as written, whether the relation's side is on the left, and the equality.
    joins: Vec<(
        *mut pg_sys::Node,
        *mut pg_sys::Node,
        bool,
        *mut pg_sys::OpExpr,
    )>,
    /// The arm's other conditions.
    rest: Vec<*mut pg_sys::Node>,
    /// Each column the arm returns: the relation's expression renumbered to 1, or the rest's.
    outputs: Vec<(bool, *mut pg_sys::Node)>,
    /// The arm's other FROM items.
    others: Vec<c_int>,
}

/// Whether FROM item `xi` of the arm `xq` and FROM item `yi` of the arm `yq` read the same rows: the
/// same table, WITH query or subquery, read alike, as the same role, under the same row security.
pub(crate) unsafe fn same_relation(
    xq: *mut pg_sys::Query,
    xi: c_int,
    yq: *mut pg_sys::Query,
    yi: c_int,
) -> bool {
    let (x, y) = (entry((*xq).rtable, xi), entry((*yq).rtable, yi));
    // the row security conditions of a table name its own entry
    let secured = |q: *mut pg_sys::Query, i: c_int, e: *mut pg_sys::RangeTblEntry| {
        as_common(
            (*e).securityQuals as *mut pg_sys::Node,
            i,
            (*(*q).rtable).length as usize,
        )
    };
    if (*x).rtekind != (*y).rtekind || !movable(x) || !movable(y) {
        return false;
    }
    match (secured(xq, xi, x), secured(yq, yi, y)) {
        (Some(a), Some(b)) if pg_sys::equal(a as *const c_void, b as *const c_void) => {}
        _ => return false,
    }
    let same_reader = || match ((*x).perminfoindex > 0, (*y).perminfoindex > 0) {
        (false, false) => true,
        (true, true) => {
            let a = pg_sys::getRTEPermissionInfo((*xq).rteperminfos, x);
            let b = pg_sys::getRTEPermissionInfo((*yq).rteperminfos, y);
            (*a).checkAsUser == (*b).checkAsUser && (*a).requiredPerms == (*b).requiredPerms
        }
        _ => false,
    };
    match (*x).rtekind {
        pg_sys::RTEKind::RTE_RELATION => {
            (*x).relid == (*y).relid
                && (*x).inh == (*y).inh
                && (*x).tablesample.is_null()
                && (*y).tablesample.is_null()
                && same_reader()
        }
        pg_sys::RTEKind::RTE_CTE => {
            CStr::from_ptr((*x).ctename) == CStr::from_ptr((*y).ctename)
                && (*x).ctelevelsup == (*y).ctelevelsup
        }
        pg_sys::RTEKind::RTE_SUBQUERY => {
            (*x).security_barrier == (*y).security_barrier
                && (*x).relid == (*y).relid
                && same_reader()
                && pg_sys::equal(
                    (*x).subquery as *const c_void,
                    (*y).subquery as *const c_void,
                )
        }
        _ => false,
    }
}

/// `node`, read in an arm where `a` is the common relation, with `a` renumbered to 1; none where it
/// reads another relation of the arm.
pub(crate) unsafe fn as_common(
    node: *mut pg_sys::Node,
    a: c_int,
    size: usize,
) -> Option<*mut pg_sys::Node> {
    let mut map = vec![0; size + 1];
    map[a as usize] = 1;
    let c = copy(node);
    renumber(c, &map).then_some(c)
}

/// What arm `arm` does with its FROM item `a`, where every condition that reads `a` and another item
/// is an equality of an expression of `a` alone and an expression of the other items alone, and
/// every column it returns reads `a` alone or not at all.
pub(crate) unsafe fn common(arm: &Arm, a: c_int) -> Option<Common> {
    let size = (*(*arm.q).rtable).length as usize;
    let mut c = Common {
        own: Vec::new(),
        joins: Vec::new(),
        rest: Vec::new(),
        outputs: Vec::new(),
        others: arm.items.iter().copied().filter(|&i| i != a).collect(),
    };
    if c.others.is_empty() {
        return None;
    }
    for &q in &arm.quals {
        let r = reads(q);
        if !r.contains(&a) {
            c.rest.push(q);
        } else if r == [a] {
            c.own.push(as_common(q, a, size)?);
        } else {
            let (l, rr) = equality_args(q)?;
            let (lr, rrr) = (reads(l), reads(rr));
            if lr == [a] && !rrr.is_empty() && !rrr.contains(&a) {
                c.joins
                    .push((as_common(l, a, size)?, rr, true, q as *mut pg_sys::OpExpr));
            } else if rrr == [a] && !lr.is_empty() && !lr.contains(&a) {
                c.joins
                    .push((as_common(rr, a, size)?, l, false, q as *mut pg_sys::OpExpr));
            } else {
                return None;
            }
        }
    }
    for &o in &arm.outputs {
        let r = reads(o);
        if r == [a] {
            c.outputs.push((true, as_common(o, a, size)?));
        } else if r.contains(&a) {
            return None;
        } else {
            c.outputs.push((false, o));
        }
    }
    Some(c)
}

/// Whether two arms do alike with their common relation.
pub(crate) unsafe fn alike(x: &Common, y: &Common) -> bool {
    let same = |a: *mut pg_sys::Node, b: *mut pg_sys::Node| {
        pg_sys::equal(a as *const c_void, b as *const c_void)
    };
    x.own.len() == y.own.len()
        && x.own.iter().zip(&y.own).all(|(&a, &b)| same(a, b))
        && x.joins.len() == y.joins.len()
        && x.joins.iter().zip(&y.joins).all(|(a, b)| {
            same(a.0, b.0)
                && a.2 == b.2
                && (*a.3).opno == (*b.3).opno
                && (*a.3).inputcollid == (*b.3).inputcollid
                && pg_sys::exprType(a.1) == pg_sys::exprType(b.1)
                && pg_sys::exprTypmod(a.1) == pg_sys::exprTypmod(b.1)
                && pg_sys::exprCollation(a.1) == pg_sys::exprCollation(b.1)
        })
        && x.outputs.len() == y.outputs.len()
        && x.outputs
            .iter()
            .zip(&y.outputs)
            .all(|(a, b)| a.0 == b.0 && (!a.0 || same(a.1, b.1)))
}

/// The leaves of a tree of UNION ALLs, left to right; none where another set operation is in it.
pub(crate) unsafe fn union_all_leaves(node: *mut c_void, out: &mut Vec<c_int>) -> bool {
    match tag(node) {
        pg_sys::NodeTag::T_SetOperationStmt => {
            let s = node as *mut pg_sys::SetOperationStmt;
            (*s).op == pg_sys::SetOperation::SETOP_UNION
                && (*s).all
                && union_all_leaves((*s).larg as *mut c_void, out)
                && union_all_leaves((*s).rarg as *mut c_void, out)
        }
        pg_sys::NodeTag::T_RangeTblRef => {
            out.push((*(node as *mut pg_sys::RangeTblRef)).rtindex);
            true
        }
        _ => false,
    }
}

/// Every set operation node of a tree.
pub(crate) unsafe fn set_operations(
    node: *mut c_void,
    out: &mut Vec<*mut pg_sys::SetOperationStmt>,
) {
    if tag(node) == pg_sys::NodeTag::T_SetOperationStmt {
        let s = node as *mut pg_sys::SetOperationStmt;
        out.push(s);
        set_operations((*s).larg as *mut c_void, out);
        set_operations((*s).rarg as *mut c_void, out);
    }
}

/// A UNION ALL whose arms join one relation alike, as that relation joined once to the union of the
/// rest of each arm. `outer` holds the queries around `s`, the outermost first.
pub(crate) unsafe fn distribute(
    s: *mut pg_sys::Query,
    outer: &[*mut pg_sys::Query],
    net: &Net,
) -> Option<*mut pg_sys::Query> {
    if (*s).setOperations.is_null() || !(*s).rowMarks.is_null() {
        return None;
    }
    let mut leaves = Vec::new();
    if !union_all_leaves((*s).setOperations as *mut c_void, &mut leaves) || leaves.len() < 2 {
        return None;
    }
    let mut arms = Vec::new();
    for &leaf in &leaves {
        let e = entry((*s).rtable, leaf);
        if (*e).rtekind != pg_sys::RTEKind::RTE_SUBQUERY || (*e).lateral {
            return None;
        }
        let q = (*e).subquery;
        if !plain(q) || reaches_outside(q) {
            return None;
        }
        let targets: Vec<*mut pg_sys::TargetEntry> = cells((*q).targetList)
            .into_iter()
            .map(|t| t as *mut pg_sys::TargetEntry)
            .collect();
        if targets.iter().any(|&t| (*t).resjunk) {
            return None;
        }
        let (items, quals) = from_items(q)?;
        if items.iter().any(|&i| !movable(entry((*q).rtable, i))) {
            return None;
        }
        let outputs = targets
            .iter()
            .map(|&t| {
                pg_sys::flatten_join_alias_vars(null_mut(), q, (*t).expr as *mut pg_sys::Node)
            })
            .collect();
        arms.push(Arm {
            q,
            items,
            quals,
            outputs,
        });
    }
    // the first relation of the first arm that each arm reads once, alike
    'candidates: for &x in &arms[0].items {
        let mut picks = Vec::new();
        for arm in &arms {
            let matching: Vec<c_int> = arm
                .items
                .iter()
                .copied()
                .filter(|&y| same_relation(arms[0].q, x, arm.q, y))
                .collect();
            if matching.len() != 1 {
                continue 'candidates;
            }
            picks.push(matching[0]);
        }
        let mut commons = Vec::new();
        for (arm, &a) in arms.iter().zip(&picks) {
            match common(arm, a) {
                Some(c) => commons.push(c),
                None => continue 'candidates,
            }
        }
        if commons[1..].iter().any(|c| !alike(&commons[0], c)) {
            continue;
        }
        net.begin();
        if let Some(j) = distributed(s, &arms, &picks, &commons, outer) {
            return Some(j);
        }
    }
    None
}

/// The UNION ALL `s` with the relation `picks` names in each arm joined once, to the union of the
/// rest of each arm.
pub(crate) unsafe fn distributed(
    s: *mut pg_sys::Query,
    arms: &[Arm],
    picks: &[c_int],
    commons: &[Common],
    outer: &[*mut pg_sys::Query],
) -> Option<*mut pg_sys::Query> {
    let first = &commons[0];
    let top = (*s).setOperations as *mut pg_sys::SetOperationStmt;
    let (types, typmods, collations) = (
        oids((*top).colTypes),
        ints((*top).colTypmods),
        oids((*top).colCollations),
    );
    let s_targets: Vec<*mut pg_sys::TargetEntry> = cells((*s).targetList)
        .into_iter()
        .map(|t| t as *mut pg_sys::TargetEntry)
        .filter(|&t| !(*t).resjunk)
        .collect();
    if s_targets.len() != first.outputs.len() || types.len() != first.outputs.len() {
        return None;
    }
    // the columns of the new union: the rest's side of each join, then what the arms return of the
    // rest
    let mut kinds: Vec<(pg_sys::Oid, i32, pg_sys::Oid, String)> = Vec::new();
    for (j, join) in first.joins.iter().enumerate() {
        kinds.push((
            pg_sys::exprType(join.1),
            pg_sys::exprTypmod(join.1),
            pg_sys::exprCollation(join.1),
            format!("key{}", j + 1),
        ));
    }
    let mut rest_at = Vec::new();
    for (j, output) in first.outputs.iter().enumerate() {
        if output.0 {
            continue;
        }
        rest_at.push(j);
        kinds.push((
            types[j],
            typmods[j],
            collations[j],
            text((*s_targets[j]).resname),
        ));
    }
    let names: Vec<String> = kinds.iter().map(|k| k.3.clone()).collect();
    // each arm without the common relation
    let u = copy(s);
    for (n, (arm, c)) in arms.iter().zip(commons).enumerate() {
        let mut targets = Vec::new();
        for (join, name) in c.joins.iter().zip(&names) {
            targets.push((join.1, name.clone(), 0));
        }
        for (&j, name) in rest_at.iter().zip(&names[c.joins.len()..]) {
            targets.push((c.outputs[j].1, name.clone(), 0));
        }
        let rest = level(arm.q, &c.others, &c.rest, &targets, &[])?;
        pg_sys::IncrementVarSublevelsUp(rest as *mut pg_sys::Node, 1, 1);
        let e = entry((*u).rtable, arms_index(s, n)?);
        (*e).subquery = rest;
        (*e).eref = alias(&text((*(*e).eref).aliasname), &names);
    }
    let (mut new_types, mut new_typmods, mut new_collations) = (null_mut(), null_mut(), null_mut());
    for k in &kinds {
        new_types = pg_sys::lappend_oid(new_types, k.0);
        new_typmods = pg_sys::lappend_int(new_typmods, k.1);
        new_collations = pg_sys::lappend_oid(new_collations, k.2);
    }
    let mut nodes = Vec::new();
    set_operations((*u).setOperations as *mut c_void, &mut nodes);
    for n in nodes {
        (*n).colTypes = new_types;
        (*n).colTypmods = new_typmods;
        (*n).colCollations = new_collations;
        (*n).groupClauses = null_mut();
    }
    let leftmost = arms_index(s, 0)?;
    let mut u_targets = Vec::new();
    for (i, k) in kinds.iter().enumerate() {
        let var = pg_sys::makeVar(leftmost, i as i16 + 1, k.0, k.1, k.2, 0);
        u_targets.push(pg_sys::makeTargetEntry(
            var as *mut pg_sys::Expr,
            i as i16 + 1,
            name_of(&k.3),
            false,
        ) as *mut c_void);
    }
    (*u).targetList = list(&u_targets);
    (*u).cteList = null_mut();
    (*u).hasRecursive = false;
    (*u).hasModifyingCTE = false;
    (*u).sortClause = null_mut();
    (*u).limitCount = null_mut();
    (*u).limitOffset = null_mut();
    (*u).limitOption = pg_sys::LimitOption::LIMIT_OPTION_COUNT;

    // the common relation, one level out, joined to the union
    let arm = arms[0].q;
    let original = entry((*arm).rtable, picks[0]);
    let a = copy(original);
    let mut perminfos = null_mut();
    if (*original).perminfoindex > 0 {
        let info = copy(pg_sys::getRTEPermissionInfo((*arm).rteperminfos, original));
        perminfos = pg_sys::lappend(perminfos, info as *mut c_void);
        (*a).perminfoindex = 1;
    }
    // the row security conditions of a table name its own entry
    let size = (*(*arm).rtable).length as usize;
    (*a).securityQuals = match (*a).securityQuals.is_null() {
        true => null_mut(),
        false => {
            as_common((*a).securityQuals as *mut pg_sys::Node, picks[0], size)? as *mut pg_sys::List
        }
    };
    let a_list = list(&[a as *mut c_void]);
    pg_sys::IncrementVarSublevelsUp_rtable(a_list, -1, 1);
    let pstate = pg_sys::make_parsestate(null_mut());
    let item = pg_sys::addRangeTableEntryForSubquery(pstate, u, alias("arms", &names), false, true);
    let u_entry = (*item).p_rte;
    pg_sys::free_parsestate(pstate);
    let out_level = |node: *mut pg_sys::Node| {
        let c = copy(node);
        pg_sys::IncrementVarSublevelsUp(c, -1, 1);
        c
    };
    let mut quals: Vec<*mut pg_sys::Node> = first.own.iter().map(|&q| out_level(q)).collect();
    for (j, join) in first.joins.iter().enumerate() {
        let key = pg_sys::makeVar(2, j as i16 + 1, kinds[j].0, kinds[j].1, kinds[j].2, 0);
        let clause = copy(join.3);
        let (l, r) = if join.2 {
            (out_level(join.0), key as *mut pg_sys::Node)
        } else {
            (key as *mut pg_sys::Node, out_level(join.0))
        };
        (*clause).args = list(&[l as *mut c_void, r as *mut c_void]);
        quals.push(clause as *mut pg_sys::Node);
    }
    let mut targets = Vec::new();
    let mut rest_column = first.joins.len() as i16;
    for (j, output) in first.outputs.iter().enumerate() {
        let original_target = s_targets[j];
        let expr = if output.0 {
            let e = out_level(output.1);
            if pg_sys::exprType(e) != types[j] || pg_sys::exprTypmod(e) != typmods[j] {
                return None;
            }
            e
        } else {
            rest_column += 1;
            pg_sys::makeVar(2, rest_column, types[j], typmods[j], collations[j], 0)
                as *mut pg_sys::Node
        };
        let te = pg_sys::makeTargetEntry(
            expr as *mut pg_sys::Expr,
            (*original_target).resno,
            copy_name((*original_target).resname),
            false,
        );
        (*te).ressortgroupref = (*original_target).ressortgroupref;
        targets.push(te as *mut c_void);
    }
    let j: *mut pg_sys::Query = made(pg_sys::NodeTag::T_Query);
    (*j).commandType = pg_sys::CmdType::CMD_SELECT;
    (*j).rtable = list(&[a as *mut c_void, u_entry as *mut c_void]);
    (*j).rteperminfos = perminfos;
    (*j).jointree = pg_sys::makeFromExpr(references(&[1, 2]), and_of(&quals));
    (*j).targetList = list(&targets);
    (*j).sortClause = copy((*s).sortClause);
    (*j).limitCount = copy((*s).limitCount);
    (*j).limitOffset = copy((*s).limitOffset);
    (*j).limitOption = (*s).limitOption;
    (*j).cteList = (*s).cteList;
    (*j).hasRecursive = (*s).hasRecursive;
    (*j).hasModifyingCTE = (*s).hasModifyingCTE;
    (*j).hasSubLinks = pg_sys::checkExprHasSubLink((*j).targetList as *mut pg_sys::Node)
        || pg_sys::checkExprHasSubLink((*(*j).jointree).quals)
        || pg_sys::checkExprHasSubLink((*j).limitCount)
        || pg_sys::checkExprHasSubLink((*j).limitOffset)
        || secured_by_sublink((*j).rtable);
    let j = in_place_of(s, j)?;
    // a WITH query the arms read is read once
    if (*original).rtekind == pg_sys::RTEKind::RTE_CTE {
        let up = (*original).ctelevelsup as usize;
        let owner = if up == 1 {
            Some(j)
        } else {
            outer.len().checked_sub(up - 1).map(|i| outer[i])
        };
        if let Some(owner) = owner {
            let name = CStr::from_ptr((*original).ctename);
            for c in cells((*owner).cteList) {
                let c = c as *mut pg_sys::CommonTableExpr;
                if CStr::from_ptr((*c).ctename) == name {
                    (*c).cterefcount -= arms.len() as c_int - 1;
                }
            }
        }
    }
    Some(j)
}

/// The range table index of arm `n` of the UNION ALL `s`, left to right.
pub(crate) unsafe fn arms_index(s: *mut pg_sys::Query, n: usize) -> Option<c_int> {
    let mut leaves = Vec::new();
    union_all_leaves((*s).setOperations as *mut c_void, &mut leaves);
    leaves.get(n).copied()
}
