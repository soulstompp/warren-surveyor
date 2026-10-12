// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! Each side keeps only the keys the other holds: where a grouping is taken on each side first, the
//! side the counted table is not on is grouped only over the keys the counted side's grouping holds.

#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use pgrx::prelude::*;

    /// The December purchases, by home zone and part category.
    const IN_DECEMBER: &str = "WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12";

    #[pg_test]
    fn the_side_without_the_counted_table_is_grouped_only_at_the_keys_the_counted_side_holds() {
        lego(Indexes::Surveyors);
        let query = by_zone_and_category_with(
            "GROUP BY 1, 2 ORDER BY",
            &format!("{IN_DECEMBER} GROUP BY 1, 2 ORDER BY"),
        );
        let per_set = BY_ZONE_AND_CATEGORY_PER_SET.replacen(
            "GROUP BY 1, 2), k AS",
            &format!("{IN_DECEMBER} GROUP BY 1, 2), k AS"),
            1,
        );
        assert_ne!(per_set, BY_ZONE_AND_CATEGORY_PER_SET);
        assert!(respelled(&query));
        let answer = rows(&query);
        assert!(answer.len() > 7, "{answer:?}");
        assert_eq!(answer, rows(&per_set));
        // the purchases' grouping is a WITH query, and the lines are grouped under a read of it
        let plan = explain(&query);
        assert!(plan.contains("CTE side_a"), "{plan}");
        assert!(
            grouped_with(
                &plan,
                " on lego_inventory_parts",
                " on lego_purchases",
                "CTE Scan on side_a"
            ),
            "{plan}"
        );
    }

    /// The December purchases' bricks by home zone, as written: a purchase brings its set's
    /// version 1 lines, and each nested set's version 1 lines times the nesting quantity.
    const BRICKS_BY_ZONE: &str = "WITH bought AS ( \
            SELECT p.purchase_id, b.home_zone, c.set_num \
            FROM lego_purchases p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
            JOIN lego_builders b   ON b.builder_id = p.builder_id \
            WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12 \
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
        SELECT inv.home_zone, count(DISTINCT inv.purchase_id) AS purchases, \
               sum(ip.quantity * inv.copies) AS bricks \
        FROM inventory inv \
        JOIN lego_inventory_parts ip ON ip.inventory_id = inv.inventory_id \
        GROUP BY 1 ORDER BY 1";

    /// The same, with the purchases and the bricks each grouped per set before the join.
    const BRICKS_BY_ZONE_PER_SET: &str = "WITH per_set_zone AS ( \
            SELECT c.set_num, b.home_zone, count(*) AS purchases \
            FROM lego_purchases p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
            JOIN lego_builders b   ON b.builder_id = p.builder_id \
            WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12 \
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
        set_bricks AS ( \
            SELECT si.set_num, sum(ip.quantity * si.copies) AS bricks \
            FROM set_inv si \
            JOIN lego_inventory_parts ip ON ip.inventory_id = si.inventory_id \
            GROUP BY 1 \
        ) \
        SELECT z.home_zone, sum(z.purchases)::bigint AS purchases, \
               sum(z.purchases * k.bricks) AS bricks \
        FROM per_set_zone z \
        JOIN set_bricks k ON k.set_num = z.set_num \
        GROUP BY 1 ORDER BY 1";

    /// For the node that passes the inventories' union on once it is kept to the sets bought (the
    /// union itself, or the first node over it that reads `side_a`), the rows the planner counts
    /// on it and the rows it passed, from a plan with its costs and the rows each node ran.
    fn kept(plan: &str) -> (f64, f64) {
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
        let reads_side_a = |j: usize| {
            nodes[j].1.contains("CTE Scan on side_a")
                || nodes[j + 1..]
                    .iter()
                    .take_while(|n| n.0 > nodes[j].0)
                    .any(|n| n.1.contains("CTE Scan on side_a"))
        };
        let unions: Vec<usize> = (0..nodes.len())
            .filter(|&i| nodes[i].1.starts_with("Append"))
            .collect();
        assert_eq!(unions.len(), 1, "{plan}");
        let mut at = unions[0];
        while !reads_side_a(at) {
            at = (0..at)
                .rev()
                .find(|&j| nodes[j].0 < nodes[at].0)
                .unwrap_or_else(|| panic!("nothing over the union reads side_a\n{plan}"));
        }
        let number = |after: &str| -> f64 {
            let line = nodes[at].1;
            let from = line.find(after).unwrap_or_else(|| panic!("{line}")) + after.len();
            let rows = &line[from..][line[from..].find("rows=").unwrap() + 5..];
            rows[..rows.find(' ').unwrap()].parse().unwrap()
        };
        (number("(cost="), number("(actual"))
    }

    #[pg_test]
    fn a_union_kept_to_the_keys_is_kept_in_each_arm_and_counted_near_what_it_keeps() {
        lego_of(Indexes::BTrees, 4000);
        let answer = rows(BRICKS_BY_ZONE);
        assert!(answer.len() > 3, "{answer:?}");
        assert_eq!(answer, rows(BRICKS_BY_ZONE_PER_SET));
        assert!(respelled(BRICKS_BY_ZONE));
        let plan = crate::tests::texts(&format!(
            "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {BRICKS_BY_ZONE}"
        ))
        .join("\n");
        let (counted, passed) = kept(&plan);
        assert!(
            passed > 0.0 && counted <= 2.0 * passed && passed <= 2.0 * counted,
            "counted {counted}, passed {passed}\n{plan}"
        );
        // side_a read by the join of the sides and in each arm of the inventories
        assert_eq!(plan.matches("CTE Scan on side_a").count(), 3, "{plan}");
    }

    /// The December purchases' bricks by home zone, over the lines of the first 50 version 1
    /// inventories by number and of every nested set; `{also}` is a further condition.
    const BRICKS_OF_THE_FIRST_INVENTORIES: &str = "SELECT b.home_zone, \
               count(DISTINCT p.purchase_id) AS purchases, sum(ip.quantity * u.copies) AS bricks \
        FROM lego_purchases p \
        JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
        JOIN lego_builders b   ON b.builder_id = p.builder_id \
        JOIN ((SELECT i.set_num, i.id AS inventory_id, 1 AS copies FROM lego_inventories i \
               WHERE i.version = 1 ORDER BY i.id LIMIT 50) \
              UNION ALL \
              (SELECT i.set_num, ci.id, sub.quantity FROM lego_inventories i \
               JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
               JOIN lego_inventories ci     ON ci.set_num = sub.set_num AND ci.version = 1 \
               WHERE i.version = 1)) u ON u.set_num = c.set_num \
        JOIN lego_inventory_parts ip ON ip.inventory_id = u.inventory_id \
        WHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12 {also} \
        GROUP BY 1 ORDER BY 1";

    #[pg_test]
    fn a_union_with_an_arm_that_takes_only_its_first_rows_is_kept_as_a_whole() {
        lego_of(Indexes::BTrees, 4000);
        let query = BRICKS_OF_THE_FIRST_INVENTORIES.replace("{also}", "");
        // a volatile condition that holds for every row: planned as written
        let as_written = BRICKS_OF_THE_FIRST_INVENTORIES.replace("{also}", "AND random() >= 0");
        assert!(respelled(&query));
        assert!(!respelled(&as_written));
        let answer = rows(&query);
        assert!(answer.len() > 3, "{answer:?}");
        assert_eq!(answer, rows(&as_written));
        // side_a read by the join of the sides and once over the union
        let plan = explain(&query);
        assert_eq!(plan.matches("CTE Scan on side_a").count(), 2, "{plan}");
    }
}
