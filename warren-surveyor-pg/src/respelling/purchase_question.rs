// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The purchase question, which reaches all three rules of the respelling: its purchases are
//! joined alike in both arms of a UNION ALL, its grouping reads the purchases on one side of the
//! set and the lines on the other, and the lines are grouped only for the sets bought.

#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use pgrx::prelude::*;

    /// The purchase question as written, over the purchases `{purchases}` selects: for each home
    /// zone, root theme, part category and colour, the purchases that reached it and the bricks they
    /// brought, the twenty with the most bricks. A purchase brings its set's version 1 lines, and
    /// each nested set's version 1 lines times the nesting quantity.
    const PURCHASES_AS_WRITTEN: &str = "WITH RECURSIVE root (theme_id, root_id) AS ( \
            SELECT id, id FROM lego_themes WHERE parent_id IS NULL \
          UNION ALL \
            SELECT t.id, r.root_id FROM lego_themes t JOIN root r ON t.parent_id = r.theme_id \
        ), \
        bought AS ( \
            SELECT p.purchase_id, b.home_zone, c.set_num \
            FROM ({purchases}) p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
            JOIN lego_builders b   ON b.builder_id = p.builder_id \
        ), \
        inventory AS ( \
            SELECT bt.purchase_id, bt.home_zone, bt.set_num, i.id AS inventory_id, 1 AS copies \
            FROM bought bt JOIN lego_inventories i ON i.set_num = bt.set_num AND i.version = 1 \
          UNION ALL \
            SELECT bt.purchase_id, bt.home_zone, bt.set_num, ci.id, sub.quantity \
            FROM bought bt \
            JOIN lego_inventories i       ON i.set_num = bt.set_num AND i.version = 1 \
            JOIN lego_inventory_sets sub  ON sub.inventory_id = i.id \
            JOIN lego_inventories ci      ON ci.set_num = sub.set_num AND ci.version = 1 \
        ) \
        SELECT inv.home_zone, rt.name AS root_theme, pc.name AS category, col.name AS colour, \
               count(DISTINCT inv.purchase_id) AS purchases, \
               sum(ip.quantity * inv.copies)    AS bricks \
        FROM inventory inv \
        JOIN lego_sets s              ON s.set_num = inv.set_num \
        JOIN root r                   ON r.theme_id = s.theme_id \
        JOIN lego_themes rt           ON rt.id = r.root_id \
        JOIN lego_inventory_parts ip  ON ip.inventory_id = inv.inventory_id \
        JOIN lego_parts pt            ON pt.part_num = ip.part_num \
        JOIN lego_part_categories pc  ON pc.id = pt.part_cat_id \
        JOIN lego_colors col          ON col.id = ip.color_id \
        GROUP BY 1, 2, 3, 4 \
        ORDER BY bricks DESC, 1, 2, 3, 4 \
        LIMIT 20";

    /// The purchase question with both sides grouped per set before the join: each set's
    /// purchases by home zone, and each bought set's bricks by root theme, part category and colour,
    /// joined on the set.
    const PURCHASES_PER_SET: &str = "WITH RECURSIVE root (theme_id, root_id) AS ( \
            SELECT id, id FROM lego_themes WHERE parent_id IS NULL \
          UNION ALL \
            SELECT t.id, r.root_id FROM lego_themes t JOIN root r ON t.parent_id = r.theme_id \
        ), \
        per_set_zone AS ( \
            SELECT c.set_num, b.home_zone, count(*) AS purchases \
            FROM ({purchases}) p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
            JOIN lego_builders b   ON b.builder_id = p.builder_id \
            GROUP BY 1, 2 \
        ), \
        set_inv AS ( \
            SELECT i.set_num, i.id AS inventory_id, 1 AS copies \
            FROM lego_inventories i \
            WHERE i.version = 1 AND i.set_num IN (SELECT set_num FROM per_set_zone) \
          UNION ALL \
            SELECT i.set_num, ci.id, sub.quantity \
            FROM lego_inventories i \
            JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
            JOIN lego_inventories ci     ON ci.set_num = sub.set_num AND ci.version = 1 \
            WHERE i.version = 1 AND i.set_num IN (SELECT set_num FROM per_set_zone) \
        ), \
        set_key AS ( \
            SELECT si.set_num, rt.name AS root_theme, pc.name AS category, col.name AS colour, \
                   sum(ip.quantity * si.copies) AS bricks \
            FROM set_inv si \
            JOIN lego_sets s              ON s.set_num = si.set_num \
            JOIN root r                   ON r.theme_id = s.theme_id \
            JOIN lego_themes rt           ON rt.id = r.root_id \
            JOIN lego_inventory_parts ip  ON ip.inventory_id = si.inventory_id \
            JOIN lego_parts pt            ON pt.part_num = ip.part_num \
            JOIN lego_part_categories pc  ON pc.id = pt.part_cat_id \
            JOIN lego_colors col          ON col.id = ip.color_id \
            GROUP BY 1, 2, 3, 4 \
        ) \
        SELECT z.home_zone, k.root_theme, k.category, k.colour, \
               sum(z.purchases)::bigint AS purchases, sum(z.purchases * k.bricks) AS bricks \
        FROM per_set_zone z \
        JOIN set_key k ON k.set_num = z.set_num \
        GROUP BY 1, 2, 3, 4 \
        ORDER BY bricks DESC, 1, 2, 3, 4 \
        LIMIT 20";

    /// The purchases each form of the purchase question asks about.
    const PURCHASE_FORMS: [(&str, &str); 4] = [
        (
            "every purchase",
            "SELECT p.purchase_id, p.builder_id, p.row_no, p.ordered_at FROM lego_purchases p",
        ),
        (
            "every December",
            "SELECT p.purchase_id, p.builder_id, p.row_no, p.ordered_at FROM lego_purchases p \
             WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12",
        ),
        (
            "2015",
            "SELECT p.purchase_id, p.builder_id, p.row_no, p.ordered_at FROM lego_purchases p \
             WHERE lego.clock(p.ordered_at) >= '2015-01-01' AND lego.clock(p.ordered_at) < '2016-01-01'",
        ),
        (
            "the Decembers of 2010 to 2019",
            "SELECT p.purchase_id, p.builder_id, p.row_no, p.ordered_at FROM lego_purchases p \
             WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12 \
               AND lego.clock(p.ordered_at) >= '2010-01-01' AND lego.clock(p.ordered_at) < '2020-01-01'",
        ),
    ];

    /// The purchase question as written, over each form of purchases, returns the rows of the
    /// question grouped per set, and its plan groups the purchases and the lines each before the
    /// join.
    fn the_purchase_question_is_respelled(indexes: Indexes) {
        lego(indexes);
        for (form, purchases) in PURCHASE_FORMS {
            let written = PURCHASES_AS_WRITTEN.replace("{purchases}", purchases);
            let per_set = PURCHASES_PER_SET.replace("{purchases}", purchases);
            let answer = rows(&written);
            assert_eq!(answer.len(), 20, "{indexes:?}, {form}: {answer:?}");
            assert_eq!(answer, rows(&per_set), "{indexes:?}, {form}");
            assert!(respelled(&written), "{indexes:?}, {form}");
            let plan = explain(&written);
            assert!(
                grouped_apart(&plan, " on lego_purchases", " on lego_inventory_parts"),
                "{indexes:?}, {form}\n{plan}"
            );
            assert!(
                grouped_apart(&plan, " on lego_inventory_parts", " on lego_purchases"),
                "{indexes:?}, {form}\n{plan}"
            );
            // the lines are grouped only for the sets the purchases' grouping holds
            assert!(
                grouped_with(
                    &plan,
                    " on lego_inventory_parts",
                    " on lego_purchases",
                    "CTE Scan on side_a"
                ),
                "{indexes:?}, {form}\n{plan}"
            );
        }
    }

    #[pg_test]
    fn the_purchase_question_is_respelled_on_surveyors() {
        the_purchase_question_is_respelled(Indexes::Surveyors);
    }

    #[pg_test]
    fn the_purchase_question_is_respelled_on_btrees() {
        the_purchase_question_is_respelled(Indexes::BTrees);
    }

    #[pg_test]
    fn the_purchase_question_is_respelled_on_primary_keys_alone() {
        the_purchase_question_is_respelled(Indexes::KeysAlone);
    }

    #[pg_test]
    fn without_the_collections_key_the_purchases_and_lines_are_joined_before_they_are_grouped() {
        lego(Indexes::Surveyors);
        Spi::run("ALTER TABLE lego.lego_collection DROP CONSTRAINT lego_collection_pkey").unwrap();
        for (form, purchases) in PURCHASE_FORMS {
            let written = PURCHASES_AS_WRITTEN.replace("{purchases}", purchases);
            let per_set = PURCHASES_PER_SET.replace("{purchases}", purchases);
            assert_eq!(rows(&written), rows(&per_set), "{form}");
            let plan = explain(&written);
            assert!(
                !grouped_apart(&plan, " on lego_purchases", " on lego_inventory_parts"),
                "{form}\n{plan}"
            );
        }
    }
}
