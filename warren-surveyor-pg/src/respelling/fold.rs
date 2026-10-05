// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! Group before the join, at the cut: a grouping over a join is taken on each side of the join
//! first, and every condition that grouping rests on, broken alone, leaves the statement as written.

#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use crate::tests::texts;
    use pgrx::prelude::*;

    #[pg_test]
    fn a_grouping_over_a_join_is_taken_on_each_side_of_the_set_first() {
        lego(Indexes::Surveyors);
        assert!(respelled(BY_ZONE_AND_CATEGORY));
        assert!(!respelled(BY_ZONE_AND_CATEGORY_PER_SET));
        let answer = rows(BY_ZONE_AND_CATEGORY);
        assert!(answer.len() > 20, "{answer:?}");
        assert_eq!(answer, rows(BY_ZONE_AND_CATEGORY_PER_SET));
        let plan = explain(BY_ZONE_AND_CATEGORY);
        assert!(
            grouped_apart(&plan, " on lego_purchases", " on lego_inventory_parts"),
            "{plan}"
        );
        assert!(
            grouped_apart(&plan, " on lego_inventory_parts", " on lego_purchases"),
            "{plan}"
        );
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

    #[pg_test]
    fn every_aggregate_that_splits_returns_what_it_returns_as_written() {
        lego(Indexes::KeysAlone);
        let query = "SELECT b.home_zone, pc.name AS category, count(DISTINCT p.purchase_id), count(*), \
                count(ip.color_id), sum(ip.quantity), sum(ip.quantity::numeric / 3), sum(p.purchase_id), \
                sum(p.row_no::smallint), min(ip.part_num), max(p.ordered_at), min(b.name), max(ip.quantity) \
            FROM lego_purchases p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
            JOIN lego_builders b ON b.builder_id = p.builder_id \
            JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
            JOIN lego_inventory_parts ip ON ip.inventory_id = i.id \
            JOIN lego_parts pt ON pt.part_num = ip.part_num \
            JOIN lego_part_categories pc ON pc.id = pt.part_cat_id \
            GROUP BY 1, 2 HAVING count(*) > 20 AND max(ip.quantity) >= 5 ORDER BY 1, 2";
        // the purchases read through a subquery that is read whole, so nothing is respelled
        let written = query.replace(
            "FROM lego_purchases p",
            "FROM (SELECT * FROM lego_purchases OFFSET 0) p",
        );
        let answer = rows(query);
        assert!(answer.len() > 20, "{answer:?}");
        assert!(respelled(query));
        assert!(!respelled(&written));
        assert_eq!(answer, rows(&written));
    }

    #[pg_test]
    fn a_group_key_that_reads_both_sides_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        let query = by_zone_and_category_with(
            "pc.name AS category",
            "b.home_zone || ' ' || pc.name AS category",
        );
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn an_aggregate_that_reads_both_sides_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        let query =
            by_zone_and_category_with("sum(ip.quantity)", "sum(ip.quantity * b.builder_id)");
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn a_path_to_the_key_through_a_column_that_is_not_unique_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        Spi::run("ALTER TABLE lego.lego_collection DROP CONSTRAINT lego_collection_pkey").unwrap();
        assert!(!respelled(BY_ZONE_AND_CATEGORY));
    }

    #[pg_test]
    fn a_unique_key_whose_column_admits_null_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        Spi::run(
            "ALTER TABLE lego.lego_collection DROP CONSTRAINT lego_collection_pkey; \
             ALTER TABLE lego.lego_collection ALTER COLUMN row_no DROP NOT NULL; \
             CREATE UNIQUE INDEX lego_collection_row ON lego.lego_collection (builder_id, row_no)",
        )
        .unwrap();
        assert!(!respelled(BY_ZONE_AND_CATEGORY));
        Spi::run("ALTER TABLE lego.lego_collection ALTER COLUMN row_no SET NOT NULL").unwrap();
        assert!(respelled(BY_ZONE_AND_CATEGORY));
    }

    /// For each `g` of `beside`, the distinct values of `counted.u` its rows join and the sum of
    /// its own `w`.
    const COUNTED_BESIDE: &str = "SELECT b.g, count(DISTINCT t.u), sum(b.w) \
        FROM counted t JOIN beside b ON t.k = b.k GROUP BY b.g";

    #[pg_test]
    fn a_unique_column_not_null_by_a_constraint_not_validated_leaves_the_statement_as_written() {
        Spi::run(
            "CREATE TABLE counted (u int UNIQUE, k int); \
             INSERT INTO counted VALUES (NULL, 1), (NULL, 1), (1, 1); \
             ALTER TABLE counted ADD CONSTRAINT counted_u_not_null NOT NULL u NOT VALID; \
             CREATE TABLE beside (k int, g int, w int); \
             INSERT INTO beside VALUES (1, 1, 10)",
        )
        .unwrap();
        assert_eq!(rows(COUNTED_BESIDE), ["1 | 1 | 30"]);
        assert!(!respelled(COUNTED_BESIDE));
        // validated, the constraint holds of every row
        Spi::run(
            "DELETE FROM counted WHERE u IS NULL; \
             ALTER TABLE counted VALIDATE CONSTRAINT counted_u_not_null",
        )
        .unwrap();
        assert!(respelled(COUNTED_BESIDE));
        assert_eq!(rows(COUNTED_BESIDE), ["1 | 1 | 10"]);
    }

    #[pg_test]
    fn a_unique_index_under_another_collation_than_its_column_leaves_the_statement_as_written() {
        Spi::run(
            "CREATE COLLATION ignore_case \
                 (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
             CREATE TABLE counted (u text COLLATE ignore_case NOT NULL, k int); \
             CREATE UNIQUE INDEX counted_u ON counted (u COLLATE \"C\"); \
             INSERT INTO counted VALUES ('a', 1), ('A', 1); \
             CREATE TABLE beside (k int, g int, w int); \
             INSERT INTO beside VALUES (1, 1, 10)",
        )
        .unwrap();
        assert_eq!(rows(COUNTED_BESIDE), ["1 | 1 | 20"]);
        assert!(!respelled(COUNTED_BESIDE));
        // a unique index under the column's own collation
        Spi::run(
            "DELETE FROM counted WHERE u = 'A' COLLATE \"C\"; \
             DROP INDEX counted_u; \
             CREATE UNIQUE INDEX ON counted (u)",
        )
        .unwrap();
        assert!(respelled(COUNTED_BESIDE));
        assert_eq!(rows(COUNTED_BESIDE), ["1 | 1 | 10"]);
    }

    #[pg_test]
    fn a_join_by_a_type_without_equality_leaves_the_statement_as_written() {
        Spi::run(
            "CREATE TABLE regions (id int PRIMARY KEY, area box); \
             CREATE TABLE places (id int PRIMARY KEY, at point); \
             INSERT INTO regions VALUES (1, box '((0,0),(2,2))'), (2, box '((1,1),(3,3))'); \
             INSERT INTO places VALUES (1, point '(1,1)'), (2, point '(2.5,2.5)'), (3, point '(9,9)')",
        )
        .unwrap();
        let query = "SELECT r.id, count(*) FROM regions r JOIN places p ON p.at <@ r.area \
            GROUP BY r.id ORDER BY 1";
        assert_eq!(rows(query), ["1 | 1", "2 | 2"]);
        assert!(!respelled(query));
    }

    #[pg_test]
    fn a_key_of_a_type_without_ordering_leaves_the_statement_as_written() {
        Spi::run(
            "CREATE TABLE counted (u int PRIMARY KEY, x xid NOT NULL); \
             INSERT INTO counted VALUES (1, '5'), (2, '5'), (3, '6'); \
             CREATE TABLE beside (x xid, g int, w int); \
             INSERT INTO beside VALUES ('5', 1, 10), ('6', 1, 100), ('7', 2, 1)",
        )
        .unwrap();
        let query = "SELECT b.g, count(DISTINCT t.u), sum(b.w) \
            FROM counted t JOIN beside b ON t.x = b.x GROUP BY b.g";
        assert_eq!(rows(query), ["1 | 3 | 120"]);
        assert!(!respelled(query));
    }

    #[pg_test]
    fn an_outer_join_across_the_cut_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        let query = by_zone_and_category_with(
            "JOIN lego_inventories i ON",
            "LEFT JOIN lego_inventories i ON",
        );
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn a_count_of_distinct_values_that_are_not_a_unique_key_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        let query = by_zone_and_category_with(
            "count(DISTINCT p.purchase_id)",
            "count(DISTINCT p.builder_id)",
        );
        assert!(!respelled(&query));
    }

    #[pg_test]
    fn an_aggregate_that_does_not_split_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        for aggregate in [
            "avg(ip.quantity)",
            "sum(ip.quantity::float8)",
            "string_agg(ip.part_num, ',')",
            "sum(ip.quantity) FILTER (WHERE ip.is_spare)",
        ] {
            let query = by_zone_and_category_with("sum(ip.quantity)", aggregate);
            assert!(!respelled(&query), "{aggregate}");
        }
    }

    #[pg_test]
    fn a_correlated_reference_into_a_side_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        let inner = by_zone_and_category_with(
            "GROUP BY 1, 2 ORDER BY bricks DESC, 1, 2",
            "WHERE extract(month FROM p.ordered_at) = m.month GROUP BY 1, 2",
        );
        let query = format!(
            "SELECT m.month || ' ' || x.home_zone || ' ' || x.bricks \
             FROM (VALUES (6), (12)) m(month) CROSS JOIN LATERAL ({inner}) x"
        );
        assert!(!respelled(&query));
        assert!(!texts(&query).is_empty());
    }

    #[pg_test]
    fn a_side_that_carries_no_aggregate_of_its_own_leaves_the_statement_as_written() {
        lego(Indexes::Surveyors);
        for query in [
            by_zone_and_category_with(
                "count(DISTINCT p.purchase_id) AS purchases, sum(ip.quantity) AS bricks",
                "count(*) AS bricks",
            ),
            by_zone_and_category_with("count(DISTINCT p.purchase_id)", "max(ip.quantity)"),
        ] {
            assert!(!respelled(&query), "{query}");
        }
    }
}
