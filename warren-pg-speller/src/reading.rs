// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What an expression reads, and moving it between query levels.

use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::{c_int, c_void};

/// A walker's place: how many queries below the level it was started at.
pub(crate) struct Depth<T> {
    depth: u32,
    state: T,
}

/// The range table entries of its own level an expression reads, sorted.
pub(crate) unsafe fn reads(node: *mut pg_sys::Node) -> Vec<c_int> {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        let c = &mut *(context as *mut Depth<Vec<c_int>>);
        match (*node).type_ {
            pg_sys::NodeTag::T_Var => {
                let var = node as *mut pg_sys::Var;
                if (*var).varlevelsup == c.depth && !c.state.contains(&(*var).varno) {
                    c.state.push((*var).varno);
                }
                false
            }
            pg_sys::NodeTag::T_Query => {
                c.depth += 1;
                let r =
                    pg_sys::query_tree_walker(node as *mut pg_sys::Query, Some(walker), context, 0);
                c.depth -= 1;
                r
            }
            _ => pg_sys::expression_tree_walker(node, Some(walker), context),
        }
    }
    let mut c = Depth {
        depth: 0,
        state: Vec::new(),
    };
    walker(node, &mut c as *mut Depth<Vec<c_int>> as *mut c_void);
    c.state.sort();
    c.state
}

/// Whether a query reads a column or an aggregate of a query around it.
pub(crate) unsafe fn reaches_outside(q: *mut pg_sys::Query) -> bool {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        let c = &mut *(context as *mut Depth<()>);
        match (*node).type_ {
            pg_sys::NodeTag::T_Var => (*(node as *mut pg_sys::Var)).varlevelsup > c.depth,
            pg_sys::NodeTag::T_Aggref if (*(node as *mut pg_sys::Aggref)).agglevelsup > c.depth => {
                true
            }
            pg_sys::NodeTag::T_GroupingFunc
                if (*(node as *mut pg_sys::GroupingFunc)).agglevelsup > c.depth =>
            {
                true
            }
            pg_sys::NodeTag::T_Query => {
                c.depth += 1;
                let r =
                    pg_sys::query_tree_walker(node as *mut pg_sys::Query, Some(walker), context, 0);
                c.depth -= 1;
                r
            }
            _ => pg_sys::expression_tree_walker(node, Some(walker), context),
        }
    }
    let mut c = Depth {
        depth: 0,
        state: (),
    };
    pg_sys::query_tree_walker(q, Some(walker), &mut c as *mut Depth<()> as *mut c_void, 0)
}

/// Gives each column of its own level `node` reads the range table index `map` gives its entry;
/// false where an entry has none.
pub(crate) unsafe fn renumber(node: *mut pg_sys::Node, map: &[c_int]) -> bool {
    struct State<'a> {
        map: &'a [c_int],
        missing: bool,
    }
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        let c = &mut *(context as *mut Depth<State>);
        match (*node).type_ {
            pg_sys::NodeTag::T_Var => {
                let var = node as *mut pg_sys::Var;
                if (*var).varlevelsup == c.depth {
                    let to = c.state.map.get((*var).varno as usize).copied().unwrap_or(0);
                    if to == 0 {
                        c.state.missing = true;
                    } else {
                        (*var).varno = to;
                        (*var).varnosyn = to as pg_sys::Index;
                        (*var).varattnosyn = (*var).varattno;
                    }
                }
                false
            }
            pg_sys::NodeTag::T_Query => {
                c.depth += 1;
                let r =
                    pg_sys::query_tree_walker(node as *mut pg_sys::Query, Some(walker), context, 0);
                c.depth -= 1;
                r
            }
            _ => pg_sys::expression_tree_walker(node, Some(walker), context),
        }
    }
    let mut c = Depth {
        depth: 0,
        state: State {
            map,
            missing: false,
        },
    };
    walker(node, &mut c as *mut Depth<State> as *mut c_void);
    !c.state.missing
}

/// Makes `q`, built from what a query level read, a query one level below that level, whose WITH
/// queries stay where they were: every reference past `q`'s own level, to a column, an aggregate or
/// a WITH query, reaches one level further out.
pub(crate) unsafe fn deepen(q: *mut pg_sys::Query) {
    #[pg_guard]
    unsafe extern "C-unwind" fn walker(node: *mut pg_sys::Node, context: *mut c_void) -> bool {
        if node.is_null() {
            return false;
        }
        let c = &mut *(context as *mut Depth<()>);
        match (*node).type_ {
            pg_sys::NodeTag::T_Var => {
                let var = node as *mut pg_sys::Var;
                if (*var).varlevelsup > c.depth {
                    (*var).varlevelsup += 1;
                }
                false
            }
            pg_sys::NodeTag::T_RangeTblEntry => {
                let e = node as *mut pg_sys::RangeTblEntry;
                if (*e).rtekind == pg_sys::RTEKind::RTE_CTE && (*e).ctelevelsup >= c.depth {
                    (*e).ctelevelsup += 1;
                }
                false
            }
            pg_sys::NodeTag::T_Query => {
                c.depth += 1;
                let r = pg_sys::query_tree_walker(
                    node as *mut pg_sys::Query,
                    Some(walker),
                    context,
                    pg_sys::QTW_EXAMINE_RTES_BEFORE as c_int,
                );
                c.depth -= 1;
                r
            }
            _ => {
                if (*node).type_ == pg_sys::NodeTag::T_Aggref {
                    let a = node as *mut pg_sys::Aggref;
                    if (*a).agglevelsup > c.depth {
                        (*a).agglevelsup += 1;
                    }
                } else if (*node).type_ == pg_sys::NodeTag::T_GroupingFunc {
                    let g = node as *mut pg_sys::GroupingFunc;
                    if (*g).agglevelsup > c.depth {
                        (*g).agglevelsup += 1;
                    }
                }
                pg_sys::expression_tree_walker(node, Some(walker), context)
            }
        }
    }
    let mut c = Depth {
        depth: 0,
        state: (),
    };
    pg_sys::query_tree_walker(
        q,
        Some(walker),
        &mut c as *mut Depth<()> as *mut c_void,
        pg_sys::QTW_EXAMINE_RTES_BEFORE as c_int,
    );
}
