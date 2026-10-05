// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! A join distributes over UNION ALL: a relation joined alike in every arm is read once, and every
//! condition that rests on, broken alone, leaves the statement as written.

#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use pgrx::prelude::*;

    /// Each purchase with the sets its version 1 inventory holds: the set itself, once, and each set
    /// nested in it, as many times as it is nested; the purchases read as a WITH query in each arm.
    const NESTED_ARMS: &str = "WITH bought AS ( \
            SELECT p.purchase_id, p.builder_id, c.set_num FROM lego_purchases p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no) \
        SELECT bt.purchase_id, bt.set_num, i.id AS inventory_id, 1 AS copies \
        FROM bought bt JOIN lego_inventories i ON i.set_num = bt.set_num AND i.version = 1 \
        UNION ALL \
        SELECT bt.purchase_id, bt.set_num, ci.id, sub.quantity \
        FROM bought bt JOIN lego_inventories i ON i.set_num = bt.set_num AND i.version = 1 \
        JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
        JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1";

    /// `NESTED_ARMS` with `from` replaced by `to`.
    fn nested_arms_with(from: &str, to: &str) -> String {
        assert!(NESTED_ARMS.contains(from), "{from}");
        NESTED_ARMS.replacen(from, to, 1)
    }

    fn sorted(mut rows: Vec<String>) -> Vec<String> {
        rows.sort();
        rows
    }

    #[pg_test]
    fn a_relation_joined_alike_in_every_arm_of_a_union_all_is_read_once() {
        lego(Indexes::Surveyors);
        // the first arm read whole, so nothing is respelled
        let written = nested_arms_with(
            "SELECT bt.purchase_id, bt.set_num, i.id AS inventory_id, 1 AS copies \
        FROM bought bt JOIN lego_inventories i ON i.set_num = bt.set_num AND i.version = 1",
            "(SELECT bt.purchase_id, bt.set_num, i.id AS inventory_id, 1 AS copies \
        FROM bought bt JOIN lego_inventories i ON i.set_num = bt.set_num AND i.version = 1 OFFSET 0)",
        );
        // the WITH query is read in place, where as written each arm scans it
        let plan = explain(NESTED_ARMS);
        assert!(!plan.contains("CTE Scan on bought"), "{plan}");
        let written_plan = explain(&written);
        assert_eq!(
            written_plan.matches("CTE Scan on bought").count(),
            2,
            "{written_plan}"
        );
        assert!(respelled(NESTED_ARMS));
        assert!(!respelled(&written));
        let answer = sorted(rows(NESTED_ARMS));
        assert!(answer.len() > 100, "{}", answer.len());
        assert_eq!(answer, sorted(rows(&written)));
    }

    #[pg_test]
    fn arms_that_join_the_relation_on_different_columns_are_left_as_written() {
        lego(Indexes::Surveyors);
        let query = nested_arms_with(
            "JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
        JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1",
            "JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
        JOIN lego_inventories ci ON ci.id = bt.builder_id AND ci.version = 1",
        );
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn arms_with_different_conditions_of_their_own_on_the_relation_are_left_as_written() {
        lego(Indexes::Surveyors);
        let query = nested_arms_with(
            "i.set_num = bt.set_num AND i.version = 1",
            "i.set_num = bt.set_num AND i.version = 1 AND bt.purchase_id % 2 = 0",
        );
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn an_arm_returning_the_rest_where_another_returns_the_relation_is_left_as_written() {
        lego(Indexes::Surveyors);
        let query = nested_arms_with(
            "SELECT bt.purchase_id, bt.set_num, ci.id, sub.quantity",
            "SELECT bt.purchase_id, ci.set_num, ci.id, sub.quantity",
        );
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn arms_that_join_the_relation_by_a_type_without_equality_are_left_as_written() {
        Spi::run(
            "CREATE TABLE regions (id int PRIMARY KEY, area box); \
             CREATE TABLE places (id int PRIMARY KEY, at point); \
             INSERT INTO regions VALUES (1, box '((0,0),(2,2))'), (2, box '((1,1),(3,3))'); \
             INSERT INTO places VALUES (1, point '(1,1)'), (2, point '(2.5,2.5)'), (3, point '(9,9)')",
        )
        .unwrap();
        let query = "SELECT r.id, p.id FROM regions r JOIN places p ON p.at <@ r.area \
            UNION ALL \
            SELECT r.id, q.id FROM regions r JOIN places q ON q.at <@ r.area AND q.id > 1 \
            ORDER BY 1, 2";
        assert_eq!(rows(query), ["1 | 1", "2 | 1", "2 | 2", "2 | 2"]);
        assert!(!respelled(query));
    }
}
