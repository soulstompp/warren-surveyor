// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The catalogue the respelling's laws run on, and how they read what the planner makes of a
//! statement.

use crate::tests::texts;
use pgrx::prelude::*;

/// Which indexes the catalogue's tables carry besides their primary keys.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Indexes {
    /// None.
    KeysAlone,
    /// A B-tree on each key the questions join and filter on.
    BTrees,
    /// Those B-trees, and a surveyor on each table they are on.
    Surveyors,
}

/// A catalogue of sets in the schema `lego`, each fact in the table its key decides, and `lego`
/// first on the search path: a tree of 40 themes under 8 roots; 240 sets; each set's version 1
/// inventory and every fifth set's version 2; sets nested in every seventh set's version 1
/// inventory; ten lines to an inventory, of 150 parts in 12 categories and 20 colours; 60
/// builders in 7 zones; 8 collection rows to a builder, each holding one set; and one to three
/// purchases of each collection row, from 2008 to 2020. `indexes` says which other indexes the
/// tables carry; every index's pages are read into shared buffers.
pub(crate) fn lego(indexes: Indexes) {
    Spi::run(
        "CREATE SCHEMA lego; \
         CREATE FUNCTION lego.clock(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE \
             PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$; \
         CREATE TABLE lego.lego_themes (id int PRIMARY KEY, name varchar(255) NOT NULL, parent_id int); \
         INSERT INTO lego.lego_themes SELECT g, 'theme ' || g, \
             CASE WHEN g <= 8 THEN NULL WHEN g <= 24 THEN 1 + g % 8 ELSE 9 + g % 16 END \
         FROM generate_series(1, 40) g; \
         CREATE TABLE lego.lego_sets (set_num varchar(255) PRIMARY KEY, name varchar(255) NOT NULL, \
                                      year int, theme_id int, num_parts int); \
         INSERT INTO lego.lego_sets SELECT 's' || g, 'set ' || g, 1990 + g % 30, 1 + g % 40, 0 \
         FROM generate_series(1, 240) g; \
         CREATE TABLE lego.lego_inventories (id int PRIMARY KEY, version int NOT NULL, \
                                             set_num varchar(255) NOT NULL); \
         INSERT INTO lego.lego_inventories SELECT g, 1, 's' || g FROM generate_series(1, 240) g; \
         INSERT INTO lego.lego_inventories SELECT 1000 + g, 2, 's' || g FROM generate_series(5, 240, 5) g; \
         CREATE TABLE lego.lego_inventory_sets (inventory_id int NOT NULL, set_num varchar(255) NOT NULL, \
                                                quantity int NOT NULL); \
         INSERT INTO lego.lego_inventory_sets SELECT g, 's' || (1 + (g * 13) % 240), 1 + g % 3 \
         FROM generate_series(7, 240, 7) g; \
         INSERT INTO lego.lego_inventory_sets SELECT g, 's' || (1 + (g * 29) % 240), 2 \
         FROM generate_series(14, 240, 14) g; \
         CREATE TABLE lego.lego_part_categories (id int PRIMARY KEY, name varchar(255) NOT NULL); \
         INSERT INTO lego.lego_part_categories SELECT g, 'category ' || g FROM generate_series(1, 12) g; \
         CREATE TABLE lego.lego_parts (part_num varchar(255) PRIMARY KEY, name text NOT NULL, \
                                       part_cat_id int NOT NULL); \
         INSERT INTO lego.lego_parts SELECT 'p' || g, 'part ' || g, 1 + g % 12 FROM generate_series(1, 150) g; \
         CREATE TABLE lego.lego_colors (id int PRIMARY KEY, name varchar(255) NOT NULL, \
                                        rgb varchar(6) NOT NULL, is_trans char(1) NOT NULL); \
         INSERT INTO lego.lego_colors SELECT g, 'colour ' || g, lpad(to_hex(g * 99991 % 16777216), 6, '0'), 'f' \
         FROM generate_series(1, 20) g; \
         CREATE TABLE lego.lego_inventory_parts (inventory_id int NOT NULL, part_num varchar(255) NOT NULL, \
                                                 color_id int NOT NULL, quantity int NOT NULL, \
                                                 is_spare boolean NOT NULL); \
         INSERT INTO lego.lego_inventory_parts \
         SELECT i.id, 'p' || (1 + (i.id * 7 + g * 11) % 150), 1 + (i.id + g * 3) % 20, 1 + (i.id + g) % 6, g % 9 = 0 \
         FROM lego.lego_inventories i, generate_series(1, 10) g; \
         CREATE TABLE lego.lego_builders (builder_id int PRIMARY KEY, name varchar(255) NOT NULL, \
                                          home_zone varchar(64) NOT NULL); \
         INSERT INTO lego.lego_builders SELECT g, 'builder ' || g, 'zone ' || (g % 7) FROM generate_series(1, 60) g; \
         CREATE TABLE lego.lego_collection (builder_id int NOT NULL, row_no int NOT NULL, \
                                            set_num varchar(255) NOT NULL, typed_set_num varchar(255), \
                                            typed_name varchar(255), PRIMARY KEY (builder_id, row_no)); \
         INSERT INTO lego.lego_collection SELECT b, r, 's' || (1 + (b * 17 + r * 31) % 240), NULL, NULL \
         FROM generate_series(1, 60) b, generate_series(1, 8) r; \
         CREATE TABLE lego.lego_purchases (purchase_id bigint PRIMARY KEY, builder_id int NOT NULL, \
                                           row_no int NOT NULL, store varchar(64) NOT NULL, \
                                           ordered_at timestamptz NOT NULL, ordered_local varchar(32) NOT NULL, \
                                           delivered_at timestamptz); \
         INSERT INTO lego.lego_purchases \
         SELECT row_number() OVER (ORDER BY c.builder_id, c.row_no, g), c.builder_id, c.row_no, 'store', \
                timestamptz '2008-01-01 00:00:00+00' \
                    + ((c.builder_id * 97 + c.row_no * 389 + g * 1231) % 4700) * interval '1 day' \
                    + (g * 5) * interval '1 hour', \
                'local', NULL \
         FROM lego.lego_collection c, generate_series(1, 3) g WHERE g <= 1 + (c.builder_id + c.row_no) % 3",
    )
    .expect("the catalogue could not be made");
    if let Indexes::BTrees | Indexes::Surveyors = indexes {
        Spi::run(
            "CREATE INDEX ON lego.lego_themes (parent_id, id); \
             CREATE INDEX ON lego.lego_sets (theme_id, set_num); \
             CREATE INDEX ON lego.lego_sets (theme_id, year) INCLUDE (set_num); \
             CREATE INDEX ON lego.lego_inventories (set_num, version, id); \
             CREATE INDEX ON lego.lego_inventory_sets (inventory_id, set_num); \
             CREATE INDEX ON lego.lego_inventory_parts (inventory_id, part_num, color_id); \
             CREATE INDEX ON lego.lego_parts (part_cat_id, part_num) INCLUDE (name); \
             CREATE INDEX ON lego.lego_collection (set_num, builder_id, row_no); \
             CREATE INDEX ON lego.lego_collection (builder_id, row_no) INCLUDE (set_num); \
             CREATE INDEX ON lego.lego_purchases \
                 (builder_id, row_no, (extract(month FROM lego.clock(ordered_at))::smallint), lego.clock(ordered_at)) \
                 INCLUDE (ordered_at, purchase_id); \
             CREATE INDEX ON lego.lego_purchases \
                 ((extract(month FROM lego.clock(ordered_at))::smallint), lego.clock(ordered_at)) \
                 INCLUDE (builder_id, row_no, ordered_at, purchase_id)",
        )
        .expect("the indexes could not be made");
    }
    if let Indexes::Surveyors = indexes {
        Spi::run(
            "CREATE INDEX ON lego.lego_themes USING surveyor (parent_id, id); \
             CREATE INDEX ON lego.lego_sets USING surveyor (theme_id, set_num); \
             CREATE INDEX ON lego.lego_inventories USING surveyor (set_num, version, id); \
             CREATE INDEX ON lego.lego_inventory_sets USING surveyor (inventory_id, set_num); \
             CREATE INDEX ON lego.lego_inventory_parts USING surveyor (inventory_id, part_num, color_id); \
             CREATE INDEX ON lego.lego_parts USING surveyor (part_cat_id, part_num); \
             CREATE INDEX ON lego.lego_collection USING surveyor (set_num, builder_id, row_no); \
             CREATE INDEX ON lego.lego_purchases USING surveyor \
                 (builder_id, row_no, (extract(month FROM lego.clock(ordered_at))::smallint), lego.clock(ordered_at))",
        )
        .expect("the surveyors could not be made");
    }
    Spi::run(
        "ANALYZE lego.lego_themes; ANALYZE lego.lego_sets; ANALYZE lego.lego_inventories; \
         ANALYZE lego.lego_inventory_sets; ANALYZE lego.lego_part_categories; ANALYZE lego.lego_parts; \
         ANALYZE lego.lego_colors; ANALYZE lego.lego_inventory_parts; ANALYZE lego.lego_builders; \
         ANALYZE lego.lego_collection; ANALYZE lego.lego_purchases; \
         SET LOCAL search_path = lego, public; SET LOCAL max_parallel_workers_per_gather = 0; \
         CREATE EXTENSION IF NOT EXISTS pg_prewarm; \
         SELECT pg_prewarm(x.indexrelid) FROM pg_index x JOIN pg_class t ON t.oid = x.indrelid \
         WHERE t.relnamespace = 'lego'::regnamespace",
    )
    .unwrap();
}

/// Each row `sql` returns, its columns' text joined by " | ".
pub(crate) fn rows(sql: &str) -> Vec<String> {
    Spi::connect(|client| {
        let mut out = Vec::new();
        for row in client.select(sql, None, &[])? {
            let mut columns = Vec::new();
            for i in 1..=row.columns() {
                let type_ = row.get_datum_by_ordinal(i)?.oid();
                let value = if type_ == pg_sys::INT8OID {
                    row.get::<i64>(i)?.map(|v| v.to_string())
                } else if type_ == pg_sys::INT4OID {
                    row.get::<i32>(i)?.map(|v| v.to_string())
                } else if type_ == pg_sys::NUMERICOID {
                    row.get::<AnyNumeric>(i)?.map(|v| v.to_string())
                } else if type_ == pg_sys::TIMESTAMPTZOID {
                    row.get::<TimestampWithTimeZone>(i)?.map(|v| v.to_string())
                } else {
                    row.get::<String>(i)?
                };
                columns.push(value.unwrap_or_else(|| "NULL".to_string()));
            }
            out.push(columns.join(" | "));
        }
        Ok::<_, pgrx::spi::SpiError>(out)
    })
    .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

pub(crate) fn explain(sql: &str) -> String {
    texts(&format!("EXPLAIN (COSTS OFF) {sql}")).join("\n")
}

/// Whether `sql` is planned through a respelling: its plan through the planner hooks differs from
/// the standard planner's plan of it as written.
pub(crate) fn respelled(sql: &str) -> bool {
    use std::ffi::{c_void, CStr, CString};
    use std::ptr::null_mut;
    unsafe {
        let text = CString::new(sql).unwrap();
        let raw = pg_sys::pg_parse_query(text.as_ptr());
        let stmt = (*(*raw).elements).ptr_value as *mut pg_sys::RawStmt;
        let queries = pg_sys::pg_analyze_and_rewrite_fixedparams(
            stmt,
            text.as_ptr(),
            std::ptr::null(),
            0,
            null_mut(),
        );
        let query = (*(*queries).elements).ptr_value as *mut pg_sys::Query;
        let copy = || pg_sys::copyObjectImpl(query as *const c_void) as *mut pg_sys::Query;
        let options = pg_sys::CURSOR_OPT_PARALLEL_OK as i32;
        #[cfg(not(feature = "pg19"))]
        let (hooked, written) = (
            pg_sys::planner(copy(), text.as_ptr(), options, null_mut()),
            pg_sys::standard_planner(copy(), text.as_ptr(), options, null_mut()),
        );
        #[cfg(feature = "pg19")]
        let (hooked, written) = (
            pg_sys::planner(copy(), text.as_ptr(), options, null_mut(), null_mut()),
            pg_sys::standard_planner(copy(), text.as_ptr(), options, null_mut(), null_mut()),
        );
        let tree = |s: *mut pg_sys::PlannedStmt| {
            CStr::from_ptr(pg_sys::nodeToString((*s).planTree as *const c_void))
                .to_string_lossy()
                .into_owned()
        };
        tree(hooked) != tree(written)
    }
}

/// Whether a scan whose line holds `own` lies under a grouping that reads no scan whose line
/// holds `other`.
pub(crate) fn grouped_apart(plan: &str, own: &str, other: &str) -> bool {
    grouped_with(plan, own, other, "")
}

/// Whether a scan whose line holds `own` lies under a grouping that reads no scan whose line
/// holds `other`, and reads one whose line holds `with`.
pub(crate) fn grouped_with(plan: &str, own: &str, other: &str, with: &str) -> bool {
    let nodes: Vec<(usize, &str)> = plan
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            if i == 0 {
                Some((0, l.trim()))
            } else {
                l.find("->").map(|p| (p + 1, l[p + 2..].trim()))
            }
        })
        .collect();
    (0..nodes.len())
        .filter(|&i| nodes[i].1.contains(own))
        .any(|i| {
            let mut indent = nodes[i].0;
            (0..i).rev().any(|j| {
                if nodes[j].0 >= indent {
                    return false;
                }
                indent = nodes[j].0;
                let below = || nodes[j + 1..].iter().take_while(|n| n.0 > nodes[j].0);
                nodes[j].1.contains("Aggregate")
                    && !below().any(|n| n.1.contains(other))
                    && below().any(|n| n.1.contains(with))
            })
        })
}

/// For each home zone and part category, the purchases that reached it and the bricks of their
/// sets' version 1 lines, the purchases, sets and lines joined as written.
pub(crate) const BY_ZONE_AND_CATEGORY: &str = "SELECT b.home_zone, pc.name AS category, \
        count(DISTINCT p.purchase_id) AS purchases, sum(ip.quantity) AS bricks \
    FROM lego_purchases p \
    JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
    JOIN lego_builders b ON b.builder_id = p.builder_id \
    JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
    JOIN lego_inventory_parts ip ON ip.inventory_id = i.id \
    JOIN lego_parts pt ON pt.part_num = ip.part_num \
    JOIN lego_part_categories pc ON pc.id = pt.part_cat_id \
    GROUP BY 1, 2 ORDER BY bricks DESC, 1, 2";

/// The same, with the purchases and the lines each grouped per set before the join.
pub(crate) const BY_ZONE_AND_CATEGORY_PER_SET: &str = "WITH z AS ( \
        SELECT c.set_num, b.home_zone, count(*) AS purchases \
        FROM lego_purchases p \
        JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
        JOIN lego_builders b ON b.builder_id = p.builder_id \
        GROUP BY 1, 2), \
    k AS ( \
        SELECT i.set_num, pc.name AS category, sum(ip.quantity) AS bricks \
        FROM lego_inventories i \
        JOIN lego_inventory_parts ip ON ip.inventory_id = i.id \
        JOIN lego_parts pt ON pt.part_num = ip.part_num \
        JOIN lego_part_categories pc ON pc.id = pt.part_cat_id \
        WHERE i.version = 1 GROUP BY 1, 2) \
    SELECT z.home_zone, k.category, sum(z.purchases)::bigint AS purchases, \
           sum(z.purchases * k.bricks)::bigint AS bricks \
    FROM z JOIN k ON k.set_num = z.set_num GROUP BY 1, 2 ORDER BY bricks DESC, 1, 2";

/// `BY_ZONE_AND_CATEGORY` with `from` replaced by `to`.
pub(crate) fn by_zone_and_category_with(from: &str, to: &str) -> String {
    assert!(BY_ZONE_AND_CATEGORY.contains(from), "{from}");
    BY_ZONE_AND_CATEGORY.replace(from, to)
}
