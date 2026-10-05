// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Group before the join, at the cut: a grouping over a join is taken on each side of the join
//! first, and the grouped sides are joined at the key they meet in.
//!
//! The relations a grouped query reads, its WITH queries and plain subqueries read through, split
//! into two sides that meet only in equalities of one set of equal columns, the key. It applies where
//! every group key and every aggregate reads one side; where each side carries an aggregate of a
//! column of its own; where every aggregate is `count(*)`, `count(x)`, `sum` of an integer or a
//! numeric, `min`, `max`, or `count(DISTINCT u)` with `u` a table's unique NOT NULL column from whose
//! row the key is reached through equalities that each cover a unique NOT NULL key of the table they
//! join; and where every join of the query is an inner join.

use crate::catalog::Facts;
use crate::keep::{held_by, unused_name, with_query};
use crate::levels::{flat, level, movable, Flat};
use crate::nodes::*;
use crate::reading::{deepen, reaches_outside, reads};
use crate::Net;
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::{c_char, c_int, c_void};
use std::ptr::null_mut;

/// How an aggregate is taken on the side it reads and again over the sides joined.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Fold {
    /// `count(*)`
    Rows,
    /// `count(x)`
    Count,
    /// `sum(x)` of an integer or a numeric
    Sum,
    /// `min(x)` or `max(x)`
    Least,
    /// `count(DISTINCT u)`
    Distinct,
}

pub(crate) struct Aggregate {
    node: *mut pg_sys::Aggref,
    fold: Fold,
    reads: Vec<c_int>,
}

/// The aggregates of `f`'s target list and HAVING, where each is one that is taken on a side.
pub(crate) unsafe fn aggregates(f: *mut pg_sys::Query) -> Option<Vec<Aggregate>> {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        if (*node).type_ == pg_sys::NodeTag::T_Aggref {
            (*(context as *mut Vec<*mut pg_sys::Aggref>)).push(node as *mut pg_sys::Aggref);
            return false;
        }
        pg_sys::expression_tree_walker(node, Some(walker), context)
    }
    let mut found: Vec<*mut pg_sys::Aggref> = Vec::new();
    let context = &mut found as *mut Vec<*mut pg_sys::Aggref> as *mut c_void;
    walker((*f).targetList as *mut pg_sys::Node, context);
    walker((*f).havingQual, context);
    let mut out = Vec::new();
    for a in found {
        if (*a).agglevelsup != 0
            || !(*a).aggfilter.is_null()
            || !(*a).aggorder.is_null()
            || !(*a).aggdirectargs.is_null()
            || (*a).aggvariadic
            || (*a).aggkind != b'n' as c_char
            || pg_sys::get_func_namespace((*a).aggfnoid)
                != pg_sys::Oid::from(pg_sys::PG_CATALOG_NAMESPACE)
        {
            return None;
        }
        let name = text(pg_sys::get_func_name((*a).aggfnoid));
        let args: Vec<*mut pg_sys::Node> = cells((*a).args)
            .into_iter()
            .map(|t| (*(t as *mut pg_sys::TargetEntry)).expr as *mut pg_sys::Node)
            .collect();
        let distinct = !(*a).aggdistinct.is_null();
        let exact = |t: pg_sys::Oid| {
            [
                pg_sys::INT2OID,
                pg_sys::INT4OID,
                pg_sys::INT8OID,
                pg_sys::NUMERICOID,
            ]
            .contains(&t)
        };
        let fold = match (name.as_str(), (*a).aggstar, args.len(), distinct) {
            ("count", true, 0, false) => Fold::Rows,
            ("count", false, 1, false) => Fold::Count,
            ("count", false, 1, true) => {
                let u = bare(args[0]);
                if (*u).type_ != pg_sys::NodeTag::T_Var
                    || (*(u as *mut pg_sys::Var)).varlevelsup != 0
                    || (*(u as *mut pg_sys::Var)).varattno <= 0
                {
                    return None;
                }
                Fold::Distinct
            }
            ("sum", false, 1, false) if exact(oids((*a).aggargtypes)[0]) => Fold::Sum,
            ("min" | "max", false, 1, false) => Fold::Least,
            _ => return None,
        };
        let mut r = Vec::new();
        for &arg in &args {
            for v in reads(arg) {
                if !r.contains(&v) {
                    r.push(v);
                }
            }
        }
        r.sort();
        if fold != Fold::Rows && r.is_empty() {
            return None;
        }
        out.push(Aggregate {
            node: a,
            fold,
            reads: r,
        });
    }
    Some(out)
}

/// A group key: how it is grouped, its expression, and what it reads.
pub(crate) struct Group {
    clause: *mut pg_sys::SortGroupClause,
    expr: *mut pg_sys::Node,
    reads: Vec<c_int>,
}

/// A column one side of an equality names.
#[derive(Clone, Copy)]
pub(crate) struct Member {
    var: *mut pg_sys::Var,
}

impl Member {
    unsafe fn at(&self) -> (c_int, i16) {
        ((*self.var).varno, (*self.var).varattno)
    }
}

/// Where a grouped query is cut: the equal columns its sides meet in, and each side's FROM items.
pub(crate) struct Cut {
    key: Vec<Member>,
    sides: [Vec<c_int>; 2],
    /// For each aggregate, the side it reads, and for a `count(DISTINCT u)`, whether its side holds
    /// one row for each row of `u`'s table.
    side_of: Vec<usize>,
    one_per_row: Vec<bool>,
}

pub(crate) fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

pub(crate) fn union(parent: &mut [usize], a: usize, b: usize) {
    let (a, b) = (find(parent, a), find(parent, b));
    if a != b {
        parent[b.max(a)] = a.min(b);
    }
}

/// The side `reads` lies on, if one.
pub(crate) fn side_of(sides: &[Vec<c_int>; 2], reads: &[c_int]) -> Option<usize> {
    (0..2).find(|&s| reads.iter().all(|r| sides[s].contains(r)))
}

/// Whether `u`, a column of a table on side `side`, is a unique NOT NULL key of that table from
/// whose row the key is reached through equalities that each cover a unique NOT NULL key of the
/// table they join; and then whether the side holds one row for each row of that table.
pub(crate) unsafe fn reached_from(
    f: &Flat,
    facts: &mut Facts,
    u: *mut pg_sys::Var,
    side: &[c_int],
    equalities: &[(usize, Member, Member)],
    key: &[Member],
) -> Option<bool> {
    let grain = (*u).varno;
    let t = facts.own_rows(entry((*f.q).rtable, grain))?;
    if !t.keyed_by(&[(*u).varattno]) {
        return None;
    }
    let key_on_side: Vec<(c_int, i16)> = key
        .iter()
        .map(|m| m.at())
        .filter(|(v, _)| side.contains(v))
        .collect();
    let mut reached = vec![grain];
    loop {
        let before = reached.len();
        for &y in side {
            if reached.contains(&y) {
                continue;
            }
            let mut columns = Vec::new();
            for (_, l, r) in equalities {
                for (this, that) in [(l.at(), r.at()), (r.at(), l.at())] {
                    if this.0 == y && reached.contains(&that.0) && side.contains(&that.0) {
                        columns.push(this.1);
                    }
                }
            }
            // the key's columns on this side are equal to one another
            if key_on_side.iter().any(|(v, _)| reached.contains(v)) {
                columns.extend(key_on_side.iter().filter(|(v, _)| *v == y).map(|(_, a)| *a));
            }
            let e = entry((*f.q).rtable, y);
            if let Some(t) = facts.own_rows(e) {
                if t.keyed_by(&columns) {
                    reached.push(y);
                }
            }
        }
        if reached.len() == before {
            break;
        }
    }
    key_on_side
        .iter()
        .any(|(v, _)| reached.contains(v))
        .then_some(reached.len() == side.len())
}

/// Where the grouped query `f` is cut, if anywhere: the cut whose side holding the first
/// `count(DISTINCT)`, or else the first aggregate of a column, reads the fewest FROM items.
pub(crate) unsafe fn cut(
    f: &Flat,
    facts: &mut Facts,
    aggs: &[Aggregate],
    groups: &[Group],
) -> Option<Cut> {
    // equalities of two columns of two FROM items
    let mut equalities: Vec<(usize, Member, Member)> = Vec::new();
    for (i, &q) in f.quals.iter().enumerate() {
        let Some((l, r)) = equality_args(q) else {
            continue;
        };
        let (l, r) = (bare(l), bare(r));
        if (*l).type_ != pg_sys::NodeTag::T_Var || (*r).type_ != pg_sys::NodeTag::T_Var {
            continue;
        }
        let (lv, rv) = (l as *mut pg_sys::Var, r as *mut pg_sys::Var);
        if (*lv).varlevelsup != 0
            || (*rv).varlevelsup != 0
            || (*lv).varattno <= 0
            || (*rv).varattno <= 0
            || (*lv).varno == (*rv).varno
        {
            continue;
        }
        equalities.push((i, Member { var: lv }, Member { var: rv }));
    }
    // the sets of equal columns
    unsafe fn id(m: Member, columns: &mut Vec<(c_int, i16)>, members: &mut Vec<Member>) -> usize {
        let at = m.at();
        match columns.iter().position(|&c| c == at) {
            Some(p) => p,
            None => {
                columns.push(at);
                members.push(m);
                columns.len() - 1
            }
        }
    }
    let mut columns: Vec<(c_int, i16)> = Vec::new();
    let mut members: Vec<Member> = Vec::new();
    let mut pairs = Vec::new();
    for &(_, l, r) in &equalities {
        let a = id(l, &mut columns, &mut members);
        let b = id(r, &mut columns, &mut members);
        pairs.push((a, b));
    }
    let mut parent: Vec<usize> = (0..columns.len()).collect();
    for &(a, b) in &pairs {
        union(&mut parent, a, b);
    }
    let mut classes: Vec<(usize, Vec<usize>)> = Vec::new();
    for c in 0..columns.len() {
        let root = find(&mut parent, c);
        match classes.iter_mut().find(|(r, _)| *r == root) {
            Some((_, k)) => k.push(c),
            None => classes.push((root, vec![c])),
        }
    }
    let position = |varno: c_int| f.items.iter().position(|&i| i == varno);
    let mut best: Option<(usize, Cut)> = None;
    for (_, class) in classes {
        let mut relations: Vec<c_int> = class.iter().map(|&c| columns[c].0).collect();
        relations.sort();
        relations.dedup();
        if relations.len() < 2 {
            continue;
        }
        let in_class = |i: usize| {
            let (a, b) = pairs[i];
            class.contains(&a) && class.contains(&b)
        };
        // the FROM items joined by anything but an equality of the class, or read together by one
        // group key or aggregate
        let mut parts: Vec<usize> = (0..f.items.len()).collect();
        let together = |r: &[c_int], parts: &mut Vec<usize>| -> bool {
            let mut at = Vec::new();
            for &v in r {
                match position(v) {
                    Some(p) => at.push(p),
                    None => return false,
                }
            }
            for w in at.windows(2) {
                union(parts, w[0], w[1]);
            }
            true
        };
        let mut readable = true;
        for (i, &q) in f.quals.iter().enumerate() {
            if equalities
                .iter()
                .position(|e| e.0 == i)
                .is_some_and(in_class)
            {
                continue;
            }
            readable &= together(&reads(q), &mut parts);
        }
        for g in groups {
            readable &= together(&g.reads, &mut parts);
        }
        for a in aggs {
            readable &= together(&a.reads, &mut parts);
        }
        if !readable {
            return None;
        }
        let roots: Vec<usize> = (0..f.items.len()).map(|p| find(&mut parts, p)).collect();
        let mut distinct_roots: Vec<usize> = roots.clone();
        distinct_roots.sort();
        distinct_roots.dedup();
        if distinct_roots.len() < 2 {
            continue;
        }
        let root_of = |varno: c_int| roots[position(varno).expect("a FROM item")];
        let counted: Vec<usize> = {
            let mut r: Vec<usize> = aggs
                .iter()
                .filter(|a| a.fold == Fold::Distinct)
                .map(|a| root_of(a.reads[0]))
                .collect();
            if r.is_empty() {
                r = aggs
                    .iter()
                    .find(|a| !a.reads.is_empty())
                    .map(|a| vec![root_of(a.reads[0])])
                    .unwrap_or_default();
            }
            r
        };
        if counted.is_empty() {
            return None;
        }
        let a_side: Vec<c_int> = f
            .items
            .iter()
            .copied()
            .filter(|&i| counted.contains(&root_of(i)))
            .collect();
        let b_side: Vec<c_int> = f
            .items
            .iter()
            .copied()
            .filter(|&i| !counted.contains(&root_of(i)))
            .collect();
        let sides = [a_side, b_side];
        let side_of_agg: Vec<Option<usize>> =
            aggs.iter().map(|a| side_of(&sides, &a.reads)).collect();
        // each side carries an aggregate of a column of its own
        let carries = |s: usize| {
            aggs.iter()
                .zip(&side_of_agg)
                .any(|(a, &at)| !a.reads.is_empty() && at == Some(s))
        };
        if !carries(0) || !carries(1) {
            continue;
        }
        let key: Vec<Member> = class.iter().map(|&c| members[c]).collect();
        if (0..2).any(|s| !key.iter().any(|m| sides[s].contains(&m.at().0))) {
            continue;
        }
        // each side groups by the key
        if !key.iter().all(|m| ordered((*m.var).vartype)) {
            continue;
        }
        // each count(DISTINCT u) reaches the key from u's row
        let mut one_per_row = vec![false; aggs.len()];
        let mut sides_of = Vec::new();
        let mut valid = true;
        for (i, a) in aggs.iter().enumerate() {
            let s = match (a.fold, side_of_agg[i]) {
                (Fold::Rows, _) => 0,
                (_, Some(s)) => s,
                _ => {
                    valid = false;
                    break;
                }
            };
            sides_of.push(s);
            if a.fold == Fold::Distinct {
                let u = bare(
                    (*(cells((*a.node).args)[0] as *mut pg_sys::TargetEntry)).expr
                        as *mut pg_sys::Node,
                ) as *mut pg_sys::Var;
                match reached_from(f, facts, u, &sides[s], &equalities, &key) {
                    Some(one) => one_per_row[i] = one,
                    None => {
                        valid = false;
                        break;
                    }
                }
            }
        }
        if !valid {
            continue;
        }
        let size = sides[0].len();
        if best.as_ref().is_none_or(|(b, _)| size < *b) {
            best = Some((
                size,
                Cut {
                    key,
                    sides,
                    side_of: sides_of,
                    one_per_row,
                },
            ));
        }
    }
    best.map(|(_, c)| c)
}

/// The operators and aggregates the grouping over the sides is spelled with.
pub(crate) struct Spelling {
    multiply: pg_sys::Oid,
    sum: pg_sys::Oid,
    count: pg_sys::Oid,
}

pub(crate) unsafe fn spelling() -> Option<Spelling> {
    let multiply = pg_sys::OpernameGetOprid(
        qualified(&["pg_catalog", "*"]),
        pg_sys::NUMERICOID,
        pg_sys::NUMERICOID,
    );
    let numeric = [pg_sys::NUMERICOID];
    let sum = pg_sys::LookupFuncName(qualified(&["pg_catalog", "sum"]), 1, numeric.as_ptr(), true);
    let count = pg_sys::LookupFuncName(
        qualified(&["pg_catalog", "count"]),
        0,
        std::ptr::null(),
        true,
    );
    (multiply != pg_sys::InvalidOid && sum != pg_sys::InvalidOid && count != pg_sys::InvalidOid)
        .then_some(Spelling {
            multiply,
            sum,
            count,
        })
}

impl Spelling {
    /// An aggregate `fnoid` of `arg`, or of every row, returning `result`.
    unsafe fn aggregate(
        &self,
        fnoid: pg_sys::Oid,
        result: pg_sys::Oid,
        arg: Option<*mut pg_sys::Node>,
    ) -> *mut pg_sys::Node {
        let a: *mut pg_sys::Aggref = made(pg_sys::NodeTag::T_Aggref);
        (*a).aggfnoid = fnoid;
        (*a).aggtype = result;
        match arg {
            Some(e) => {
                (*a).aggargtypes = pg_sys::lappend_oid(null_mut(), pg_sys::exprType(e));
                let te = pg_sys::makeTargetEntry(e as *mut pg_sys::Expr, 1, null_mut(), false);
                (*a).args = list(&[te as *mut c_void]);
            }
            None => (*a).aggstar = true,
        }
        (*a).aggkind = b'n' as c_char;
        (*a).aggsplit = pg_sys::AggSplit::AGGSPLIT_SIMPLE;
        (*a).aggno = -1;
        (*a).aggtransno = -1;
        (*a).location = -1;
        a as *mut pg_sys::Node
    }

    unsafe fn numeric(&self, e: *mut pg_sys::Node) -> Option<*mut pg_sys::Node> {
        let t = pg_sys::exprType(e);
        if t == pg_sys::NUMERICOID {
            return Some(e);
        }
        let c = pg_sys::coerce_to_target_type(
            null_mut(),
            e,
            t,
            pg_sys::NUMERICOID,
            -1,
            pg_sys::CoercionContext::COERCION_IMPLICIT,
            pg_sys::CoercionForm::COERCE_IMPLICIT_CAST,
            -1,
        );
        (!c.is_null()).then_some(c)
    }

    /// `sum(l × r)` as a numeric, or `sum(l)`, returned as `result`.
    unsafe fn summed(
        &self,
        l: *mut pg_sys::Node,
        r: Option<*mut pg_sys::Node>,
        result: pg_sys::Oid,
    ) -> Option<*mut pg_sys::Node> {
        let mut arg = self.numeric(l)?;
        if let Some(r) = r {
            let product = pg_sys::make_opclause(
                self.multiply,
                pg_sys::NUMERICOID,
                false,
                arg as *mut pg_sys::Expr,
                self.numeric(r)? as *mut pg_sys::Expr,
                pg_sys::InvalidOid,
                pg_sys::InvalidOid,
            ) as *mut pg_sys::OpExpr;
            pg_sys::set_opfuncid(product);
            arg = product as *mut pg_sys::Node;
        }
        let total = self.aggregate(self.sum, pg_sys::NUMERICOID, Some(arg));
        if result == pg_sys::NUMERICOID {
            return Some(total);
        }
        let c = pg_sys::coerce_to_target_type(
            null_mut(),
            total,
            pg_sys::NUMERICOID,
            result,
            -1,
            pg_sys::CoercionContext::COERCION_EXPLICIT,
            pg_sys::CoercionForm::COERCE_EXPLICIT_CAST,
            -1,
        );
        (!c.is_null()).then_some(c)
    }
}

/// The target list entries and HAVING of the grouping over the sides: each group key read from its
/// side, and each aggregate from the sides' own.
pub(crate) struct Outer {
    groups: Vec<(*mut pg_sys::Node, *mut pg_sys::Node)>,
    aggs: Vec<(*mut pg_sys::Aggref, *mut pg_sys::Node)>,
    stray: bool,
}

#[pg_guard]
unsafe extern "C-unwind" fn outer_mutator(
    node: *mut pg_sys::Node,
    context: *mut c_void,
) -> *mut pg_sys::Node {
    if node.is_null() {
        return null_mut();
    }
    let o = &mut *(context as *mut Outer);
    for &(g, v) in &o.groups {
        if pg_sys::equal(node as *const c_void, g as *const c_void) {
            return copy(v);
        }
    }
    match (*node).type_ {
        pg_sys::NodeTag::T_Aggref => {
            match o.aggs.iter().find(|(a, _)| *a as *mut pg_sys::Node == node) {
                Some(&(_, r)) => copy(r),
                None => {
                    o.stray = true;
                    node
                }
            }
        }
        pg_sys::NodeTag::T_Var | pg_sys::NodeTag::T_Query | pg_sys::NodeTag::T_SubLink => {
            o.stray = true;
            node
        }
        _ => pg_sys::expression_tree_mutator_impl(node, Some(outer_mutator), context),
    }
}

/// A grouping over a join, taken on each side of a cut first.
pub(crate) unsafe fn fold(
    q: *mut pg_sys::Query,
    facts: &mut Facts,
    net: &Net,
) -> Option<*mut pg_sys::Query> {
    if (*q).commandType != pg_sys::CmdType::CMD_SELECT
        || (*q).groupClause.is_null()
        || !(*q).groupingSets.is_null()
        || !(*q).hasAggs
        || (*q).hasWindowFuncs
        || (*q).hasTargetSRFs
        || !(*q).setOperations.is_null()
        || !(*q).rowMarks.is_null()
        || (*q).hasForUpdate
        || (*q).hasModifyingCTE
        || (*q).jointree.is_null()
        || reaches_outside(q)
        || pg_sys::contain_volatile_functions(q as *mut pg_sys::Node)
    {
        return None;
    }
    let f = flat(q)?;
    if f.items.len() < 2
        || f.items.iter().any(|&i| !movable(entry((*f.q).rtable, i)))
        || pg_sys::checkExprHasSubLink((*f.q).targetList as *mut pg_sys::Node)
        || pg_sys::checkExprHasSubLink((*f.q).havingQual)
    {
        return None;
    }
    let aggs = aggregates(f.q)?;
    let mut groups = Vec::new();
    for g in cells((*f.q).groupClause) {
        let g = g as *mut pg_sys::SortGroupClause;
        let te = pg_sys::get_sortgroupref_tle((*g).tleSortGroupRef, (*f.q).targetList);
        let expr = (*te).expr as *mut pg_sys::Node;
        let r = reads(expr);
        if r.is_empty() {
            return None;
        }
        groups.push(Group {
            clause: g,
            expr,
            reads: r,
        });
    }
    let cut = cut(&f, facts, &aggs, &groups)?;
    net.begin();
    folded(q, &f, &aggs, &groups, &cut)
}

/// The query `q`, read as `f`, grouped on each side of `cut` first: the first side's grouping a WITH
/// query, and the second side grouped only over the keys it holds.
pub(crate) unsafe fn folded(
    q: *mut pg_sys::Query,
    f: &Flat,
    aggs: &[Aggregate],
    groups: &[Group],
    cut: &Cut,
) -> Option<*mut pg_sys::Query> {
    let spell = spelling()?;
    let other = |s: usize| 1 - s;
    // the rows of a side are counted where the other side's counts and sums need them, where the
    // query counts every row, and for a count(DISTINCT) whose side holds one row per row counted
    let mut rows_needed = [false; 2];
    for (i, a) in aggs.iter().enumerate() {
        let s = cut.side_of[i];
        match a.fold {
            Fold::Rows => rows_needed = [true, true],
            Fold::Count | Fold::Sum => rows_needed[other(s)] = true,
            Fold::Distinct if cut.one_per_row[i] => rows_needed[s] = true,
            _ => {}
        }
    }
    let mut pseudo = Vec::new();
    let mut side_quals: [Vec<*mut pg_sys::Node>; 2] = [Vec::new(), Vec::new()];
    for &c in &f.quals {
        let r = reads(c);
        if r.is_empty() {
            pseudo.push(c);
            continue;
        }
        match side_of(&cut.sides, &r) {
            Some(s) => side_quals[s].push(c),
            // an equality of the key's columns across the cut is the join of the sides
            None => {
                let (l, r) = equality_args(c)?;
                let is_key = |n: *mut pg_sys::Node| {
                    let n = bare(n);
                    (*n).type_ == pg_sys::NodeTag::T_Var
                        && cut
                            .key
                            .iter()
                            .any(|m| pg_sys::equal(m.var as *const c_void, n as *const c_void))
                };
                if !is_key(l) || !is_key(r) {
                    return None;
                }
            }
        }
    }
    let mut group_columns: Vec<(usize, i16)> = Vec::new();
    let mut agg_columns: Vec<Option<i16>> = vec![None; aggs.len()];
    let mut rows_column = [0i16; 2];
    let mut key_column = [0i16; 2];
    let mut queries = [null_mut(), null_mut()];
    let mut names_of: [Vec<String>; 2] = [Vec::new(), Vec::new()];
    let mut first: *mut pg_sys::CommonTableExpr = null_mut();
    for s in 0..2 {
        let side = &cut.sides[s];
        let on_side: Vec<Member> = cut
            .key
            .iter()
            .copied()
            .filter(|m| side.contains(&m.at().0))
            .collect();
        // the key's columns on this side are equal to one another
        let mut quals = side_quals[s].clone();
        for m in &on_side[1..] {
            quals.push(equals(
                on_side[0].var as *mut pg_sys::Node,
                m.var as *mut pg_sys::Node,
            )?);
        }
        // the second side, only at the keys the first side's grouping holds
        if s == 1 {
            quals.push(held_by(
                on_side[0].var as *mut pg_sys::Node,
                first,
                key_column[0],
            )?);
        }
        let mut targets: Vec<(*mut pg_sys::Node, String, pg_sys::Index)> = Vec::new();
        let mut clauses = Vec::new();
        for (gi, g) in groups.iter().enumerate() {
            if side_of(&cut.sides, &g.reads) != Some(s) {
                continue;
            }
            let reference = targets.len() as pg_sys::Index + 1;
            let clause = copy(g.clause);
            (*clause).tleSortGroupRef = reference;
            clauses.push(clause);
            targets.push((g.expr, format!("group{}", gi + 1), reference));
            group_columns.push((gi, reference as i16));
        }
        let reference = targets.len() as pg_sys::Index + 1;
        let key_var = on_side[0].var as *mut pg_sys::Node;
        clauses.push(sort_group(pg_sys::exprType(key_var), reference)?);
        targets.push((key_var, "key".to_string(), reference));
        key_column[s] = reference as i16;
        if rows_needed[s] {
            targets.push((
                spell.aggregate(spell.count, pg_sys::INT8OID, None),
                "rows".to_string(),
                0,
            ));
            rows_column[s] = targets.len() as i16;
        }
        for (i, a) in aggs.iter().enumerate() {
            if cut.side_of[i] != s
                || a.fold == Fold::Rows
                || (a.fold == Fold::Distinct && cut.one_per_row[i])
            {
                continue;
            }
            targets.push((a.node as *mut pg_sys::Node, format!("side{}", i + 1), 0));
            agg_columns[i] = Some(targets.len() as i16);
        }
        let sq = level(f.q, side, &quals, &targets, &clauses)?;
        deepen(sq);
        names_of[s] = targets.iter().map(|t| t.1.clone()).collect();
        queries[s] = sq;
        if s == 0 {
            let name = unused_name((*f.q).cteList, "side_a");
            first = with_query(&name, sq, &names_of[0]);
        }
    }
    // the sides, joined at the key
    let pstate = pg_sys::make_parsestate(null_mut());
    let read = pg_sys::makeRangeVar(null_mut(), (*first).ctename, -1);
    pg_sys::addRangeTableEntryForCTE(pstate, first, 0, read, true);
    pg_sys::addRangeTableEntryForSubquery(
        pstate,
        queries[1],
        alias("side_b", &names_of[1]),
        false,
        true,
    );
    let side_var = |s: usize, resno: i16| -> *mut pg_sys::Node {
        let te = cells((*queries[s]).targetList)[resno as usize - 1] as *mut pg_sys::TargetEntry;
        column_var(s as c_int + 1, resno, (*te).expr as *mut pg_sys::Node)
    };
    let mut quals = vec![equals(
        side_var(0, key_column[0]),
        side_var(1, key_column[1]),
    )?];
    quals.extend(pseudo);
    let mut outer = Outer {
        groups: Vec::new(),
        aggs: Vec::new(),
        stray: false,
    };
    for &(gi, resno) in &group_columns {
        let s = side_of(&cut.sides, &groups[gi].reads)?;
        outer.groups.push((groups[gi].expr, side_var(s, resno)));
    }
    for (i, a) in aggs.iter().enumerate() {
        let s = cut.side_of[i];
        let result = (*a.node).aggtype;
        let own = || agg_columns[i].map(|c| side_var(s, c));
        let rows = |side: usize| side_var(side, rows_column[side]);
        let replacement = match a.fold {
            Fold::Rows => spell.summed(rows(0), Some(rows(1)), result)?,
            Fold::Count | Fold::Sum => spell.summed(own()?, Some(rows(other(s))), result)?,
            Fold::Distinct => {
                let counted = if cut.one_per_row[i] { rows(s) } else { own()? };
                spell.summed(counted, None, result)?
            }
            Fold::Least => {
                let again = copy(a.node);
                let te = pg_sys::makeTargetEntry(own()? as *mut pg_sys::Expr, 1, null_mut(), false);
                (*again).args = list(&[te as *mut c_void]);
                again as *mut pg_sys::Node
            }
        };
        outer.aggs.push((a.node, replacement));
    }
    let context = &mut outer as *mut Outer as *mut c_void;
    let targets = outer_mutator((*f.q).targetList as *mut pg_sys::Node, context);
    let having = outer_mutator((*f.q).havingQual, context);
    if outer.stray {
        pg_sys::free_parsestate(pstate);
        return None;
    }
    let o = select_of(
        pstate,
        &[1, 2],
        and_of(&quals),
        targets as *mut pg_sys::List,
    );
    pg_sys::free_parsestate(pstate);
    (*o).havingQual = having;
    (*o).groupClause = copy((*f.q).groupClause);
    (*o).distinctClause = copy((*f.q).distinctClause);
    (*o).hasDistinctOn = (*f.q).hasDistinctOn;
    (*o).sortClause = copy((*f.q).sortClause);
    (*o).limitCount = copy((*f.q).limitCount);
    (*o).limitOffset = copy((*f.q).limitOffset);
    (*o).limitOption = (*f.q).limitOption;
    // the first side's grouping is read by the join and by the second side, after the WITH queries
    // it may read
    (*o).cteList = pg_sys::lappend((*f.q).cteList, first as *mut c_void);
    (*o).hasRecursive = (*f.q).hasRecursive;
    (*o).constraintDeps = (*f.q).constraintDeps;
    (*o).hasAggs = true;
    (*o).hasSubLinks = pg_sys::checkExprHasSubLink((*o).targetList as *mut pg_sys::Node)
        || pg_sys::checkExprHasSubLink((*(*o).jointree).quals)
        || pg_sys::checkExprHasSubLink((*o).limitCount)
        || pg_sys::checkExprHasSubLink((*o).limitOffset);
    in_place_of(q, o)
}
