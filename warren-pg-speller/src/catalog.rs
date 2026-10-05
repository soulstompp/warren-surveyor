// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What the catalog says about each table a statement reads: its unique keys and its NOT NULL
//! columns, read once for each statement.

use crate::nodes::oids;
use pgrx::pg_sys;
use std::collections::HashMap;
use std::ffi::c_int;

pub(crate) struct Table {
    /// The key columns of each valid, immediate, whole-table unique B-tree index on plain columns,
    /// each column under its type's default operator family, and under the column's own collation
    /// where the column's collation is not deterministic.
    unique: Vec<Vec<i16>>,
    /// NOT NULL by a validated constraint, by column number (index 0 unused).
    not_null: Vec<bool>,
    /// Whether the table has inheritance children.
    has_children: bool,
    partitioned: bool,
}

impl Table {
    fn not_null(&self, column: i16) -> bool {
        column > 0 && self.not_null.get(column as usize).copied().unwrap_or(false)
    }

    /// Whether `columns` cover a unique key of the table whose columns are all NOT NULL.
    pub(crate) fn keyed_by(&self, columns: &[i16]) -> bool {
        self.unique.iter().any(|key| {
            key.iter()
                .all(|&c| columns.contains(&c) && self.not_null(c))
        })
    }
}

pub(crate) unsafe fn load(relid: pg_sys::Oid) -> Table {
    // the statement already holds its lock on every table it reads
    let rel = pg_sys::relation_open(relid, pg_sys::NoLock as pg_sys::LOCKMODE);
    let desc = (*rel).rd_att;
    let mut unique = Vec::new();
    for index in oids(pg_sys::RelationGetIndexList(rel)) {
        let idx = pg_sys::index_open(index, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
        let form = (*idx).rd_index;
        if (*form).indisunique
            && (*form).indimmediate
            && (*form).indisvalid
            && (*(*idx).rd_rel).relam == pg_sys::BTREE_AM_OID
            && pg_sys::RelationGetIndexPredicate(idx).is_null()
        {
            let n = (*form).indnkeyatts as usize;
            let columns: Vec<i16> = (0..n)
                .map(|i| *(*form).indkey.values.as_ptr().add(i))
                .collect();
            let plain = columns.iter().enumerate().all(|(i, &c)| {
                if c <= 0 {
                    return false;
                }
                let attribute = pg_sys::TupleDescAttr(desc, c as c_int - 1);
                let default =
                    pg_sys::GetDefaultOpClass((*attribute).atttypid, pg_sys::BTREE_AM_OID);
                let collation = (*attribute).attcollation;
                let collated = *(*idx).rd_indcollation.add(i) == collation
                    || collation == pg_sys::InvalidOid
                    || pg_sys::get_collation_isdeterministic(collation);
                default != pg_sys::InvalidOid
                    && *(*idx).rd_opfamily.add(i) == pg_sys::get_opclass_family(default)
                    && collated
            });
            if plain {
                unique.push(columns);
            }
        }
        pg_sys::index_close(idx, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
    }
    let natts = (*desc).natts as usize;
    let mut not_null = vec![false; natts + 1];
    for i in 0..natts {
        let attribute = pg_sys::TupleDescCompactAttr(desc, i as c_int);
        not_null[i + 1] = (*attribute).attnullability as u8 == pg_sys::ATTNULLABLE_VALID
            && !(*attribute).attisdropped;
    }
    let has_children = (*(*rel).rd_rel).relhassubclass;
    let partitioned = (*(*rel).rd_rel).relkind as u8 == pg_sys::RELKIND_PARTITIONED_TABLE;
    pg_sys::relation_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    Table {
        unique,
        not_null,
        has_children,
        partitioned,
    }
}

#[derive(Default)]
pub(crate) struct Facts {
    pub(crate) tables: HashMap<pg_sys::Oid, Table>,
}

impl Facts {
    pub(crate) unsafe fn table(&mut self, relid: pg_sys::Oid) -> &Table {
        self.tables.entry(relid).or_insert_with(|| load(relid))
    }

    /// The table a FROM item reads, where each of its rows is a row of that table alone: a table,
    /// read without inheritance children, or a partitioned table.
    pub(crate) unsafe fn own_rows(&mut self, e: *mut pg_sys::RangeTblEntry) -> Option<&Table> {
        if (*e).rtekind != pg_sys::RTEKind::RTE_RELATION || !(*e).tablesample.is_null() {
            return None;
        }
        let inh = (*e).inh;
        let t = self.table((*e).relid);
        (!inh || t.partitioned || !t.has_children).then_some(t)
    }
}
