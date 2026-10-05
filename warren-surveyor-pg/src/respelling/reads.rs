// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a respelled statement reads, as whom, what its plan rests on, and how it is planned when
//! its spelling cannot be.

#[pgrx::pg_schema]
mod tests {
    use crate::respelling::catalogue::*;
    use pgrx::prelude::*;
    use std::cell::Cell;
    use std::ffi::CStr;

    static mut NEXT_PATHLIST: pg_sys::set_rel_pathlist_hook_type = None;

    thread_local! {
        /// The error a scan of a WITH query named `side_a` raises while it is planned, if any.
        static FAILURE: Cell<Option<PgSqlErrorCode>> = const { Cell::new(None) };
    }

    #[pg_guard]
    unsafe extern "C-unwind" fn failing_at_side_a(
        root: *mut pg_sys::PlannerInfo,
        rel: *mut pg_sys::RelOptInfo,
        rti: pg_sys::Index,
        rte: *mut pg_sys::RangeTblEntry,
    ) {
        if let Some(next) = NEXT_PATHLIST {
            next(root, rel, rti, rte);
        }
        if let Some(code) = FAILURE.get() {
            if (*rte).rtekind == pg_sys::RTEKind::RTE_CTE
                && CStr::from_ptr((*rte).ctename).to_bytes() == b"side_a"
            {
                ereport!(PgLogLevel::ERROR, code, "a seeded failure");
            }
        }
    }

    /// Runs `f` while a scan of a WITH query named `side_a` raises an ERROR of `code` as it is
    /// planned.
    fn failing_at_side_a_while<T>(code: PgSqlErrorCode, f: impl FnOnce() -> T) -> T {
        FAILURE.set(Some(code));
        unsafe {
            NEXT_PATHLIST = pg_sys::set_rel_pathlist_hook;
            pg_sys::set_rel_pathlist_hook = Some(failing_at_side_a);
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe { pg_sys::set_rel_pathlist_hook = NEXT_PATHLIST };
        FAILURE.set(None);
        result.unwrap_or_else(|e| std::panic::resume_unwind(e))
    }

    #[pg_test]
    fn a_respelled_statement_that_fails_to_plan_is_planned_as_written() {
        lego(Indexes::Surveyors);
        let expected = rows(BY_ZONE_AND_CATEGORY_PER_SET);
        let (respelled_now, answer, plan) =
            failing_at_side_a_while(PgSqlErrorCode::ERRCODE_INTERNAL_ERROR, || {
                (
                    respelled(BY_ZONE_AND_CATEGORY),
                    rows(BY_ZONE_AND_CATEGORY),
                    explain(BY_ZONE_AND_CATEGORY),
                )
            });
        assert!(!respelled_now);
        assert_eq!(answer, expected);
        assert!(!plan.contains("side_a"), "{plan}");
        assert!(respelled(BY_ZONE_AND_CATEGORY));
    }

    #[pg_test(error = "a seeded failure")]
    fn a_respelled_statement_canceled_while_it_is_planned_is_canceled() {
        lego(Indexes::Surveyors);
        failing_at_side_a_while(PgSqlErrorCode::ERRCODE_QUERY_CANCELED, || {
            rows(BY_ZONE_AND_CATEGORY)
        });
    }

    /// The subtransactions begun while `f` runs.
    fn subtransactions_while(f: impl FnOnce()) -> u32 {
        unsafe fn next_id() -> pg_sys::SubTransactionId {
            let (context, owner) = (pg_sys::CurrentMemoryContext, pg_sys::CurrentResourceOwner);
            pg_sys::BeginInternalSubTransaction(std::ptr::null());
            let id = pg_sys::GetCurrentSubTransactionId();
            pg_sys::ReleaseCurrentSubTransaction();
            pg_sys::CurrentMemoryContext = context;
            pg_sys::CurrentResourceOwner = owner;
            id
        }
        unsafe {
            let before = next_id();
            f();
            next_id() - before - 1
        }
    }

    #[pg_test]
    fn only_a_statement_the_respelling_changes_is_planned_inside_a_subtransaction() {
        lego(Indexes::KeysAlone);
        let as_written = [
            "SELECT b.home_zone, count(*) FROM lego_builders b GROUP BY 1",
            &by_zone_and_category_with(
                "count(DISTINCT p.purchase_id)",
                "count(DISTINCT p.builder_id)",
            ),
        ];
        for query in as_written {
            assert_eq!(
                subtransactions_while(|| assert!(!respelled(query))),
                0,
                "{query}"
            );
        }
        assert_eq!(
            subtransactions_while(|| assert!(respelled(BY_ZONE_AND_CATEGORY))),
            1
        );
    }

    #[pg_test]
    fn nothing_is_respelled_in_a_parallel_operation() {
        lego(Indexes::KeysAlone);
        unsafe { pg_sys::EnterParallelMode() };
        let in_parallel = std::panic::catch_unwind(|| respelled(BY_ZONE_AND_CATEGORY));
        unsafe { pg_sys::ExitParallelMode() };
        assert!(!in_parallel.unwrap_or_else(|e| std::panic::resume_unwind(e)));
        assert!(respelled(BY_ZONE_AND_CATEGORY));
    }

    #[pg_test]
    fn a_prepared_statement_follows_the_unique_key_its_spelling_rests_on() {
        lego(Indexes::Surveyors);
        Spi::run(&format!("PREPARE by_zone AS {BY_ZONE_AND_CATEGORY}")).unwrap();
        let grouped_per_set = || {
            let plan = explain("EXECUTE by_zone");
            grouped_apart(&plan, " on lego_purchases", " on lego_inventory_parts")
        };
        assert!(grouped_per_set());
        Spi::run("ALTER TABLE lego.lego_collection DROP CONSTRAINT lego_collection_pkey").unwrap();
        assert!(!grouped_per_set());
        Spi::run("ALTER TABLE lego.lego_collection ADD PRIMARY KEY (builder_id, row_no)").unwrap();
        assert!(grouped_per_set());
        Spi::run("DEALLOCATE by_zone").unwrap();
    }

    /// A role that may read every table of the catalogue but its part categories, and a view of
    /// the categories behind a security barrier that it may read.
    fn reader_of_open_categories() {
        Spi::run(
            "CREATE VIEW lego.open_categories WITH (security_barrier) AS \
                 SELECT id, name FROM lego.lego_part_categories; \
             CREATE ROLE surveyor_reader; \
             GRANT USAGE ON SCHEMA lego TO surveyor_reader; \
             GRANT SELECT ON lego.lego_purchases, lego.lego_collection, lego.lego_builders, \
                 lego.lego_inventories, lego.lego_inventory_parts, lego.lego_parts, \
                 lego.open_categories TO surveyor_reader",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_view_read_by_a_respelled_statement_is_read_as_its_owner() {
        lego(Indexes::Surveyors);
        reader_of_open_categories();
        let expected = rows(BY_ZONE_AND_CATEGORY_PER_SET);
        let through_view =
            by_zone_and_category_with("JOIN lego_part_categories pc", "JOIN open_categories pc");
        Spi::run("SET LOCAL ROLE surveyor_reader").unwrap();
        assert!(respelled(&through_view));
        assert_eq!(rows(&through_view), expected);
        Spi::run("RESET ROLE").unwrap();
    }

    #[pg_test(error = "permission denied for table lego_part_categories")]
    fn a_respelled_statement_reads_no_table_its_reader_may_not() {
        lego(Indexes::Surveyors);
        reader_of_open_categories();
        Spi::run("SET LOCAL ROLE surveyor_reader").unwrap();
        assert!(respelled(BY_ZONE_AND_CATEGORY));
        rows(BY_ZONE_AND_CATEGORY);
    }

    #[pg_test]
    fn a_respelled_statement_returns_the_rows_row_security_admits() {
        lego(Indexes::Surveyors);
        let expected = rows(&BY_ZONE_AND_CATEGORY_PER_SET.replace(
            "JOIN lego_builders b ON b.builder_id = p.builder_id",
            "JOIN lego_builders b ON b.builder_id = p.builder_id WHERE b.home_zone <> 'zone 3'",
        ));
        assert_ne!(expected, rows(BY_ZONE_AND_CATEGORY_PER_SET));
        Spi::run(
            "ALTER TABLE lego.lego_builders ENABLE ROW LEVEL SECURITY; \
             CREATE POLICY outside_zone_3 ON lego.lego_builders USING (home_zone <> 'zone 3'); \
             CREATE ROLE surveyor_reader; \
             GRANT USAGE ON SCHEMA lego TO surveyor_reader; \
             GRANT SELECT ON ALL TABLES IN SCHEMA lego TO surveyor_reader; \
             SET LOCAL ROLE surveyor_reader",
        )
        .unwrap();
        assert!(respelled(BY_ZONE_AND_CATEGORY));
        assert_eq!(rows(BY_ZONE_AND_CATEGORY), expected);
        Spi::run("RESET ROLE").unwrap();
    }

    /// Row security on the purchases whose condition holds a subquery: a reader sees the purchases
    /// of builders outside zone 3. Then the reader, subject to it, is the current role.
    fn purchases_admitted_by_a_subquery() {
        Spi::run(
            "ALTER TABLE lego.lego_purchases ENABLE ROW LEVEL SECURITY; \
             CREATE POLICY outside_zone_3 ON lego.lego_purchases USING (builder_id IN \
                 (SELECT builder_id FROM lego.lego_builders WHERE home_zone <> 'zone 3')); \
             CREATE ROLE purchase_reader; \
             GRANT USAGE ON SCHEMA lego TO purchase_reader; \
             GRANT SELECT ON ALL TABLES IN SCHEMA lego TO purchase_reader; \
             SET LOCAL ROLE purchase_reader",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_grouping_respelled_returns_the_rows_row_security_with_a_subquery_admits() {
        lego(Indexes::Surveyors);
        let every_row = rows(BY_ZONE_AND_CATEGORY);
        // the purchases read through a subquery that is read whole, so nothing is respelled
        let written = BY_ZONE_AND_CATEGORY.replace(
            "FROM lego_purchases p",
            "FROM (SELECT * FROM lego_purchases OFFSET 0) p",
        );
        purchases_admitted_by_a_subquery();
        assert!(!respelled(&written));
        let expected = rows(&written);
        assert_ne!(expected, every_row);
        assert_eq!(rows(BY_ZONE_AND_CATEGORY), expected);
        assert!(respelled(BY_ZONE_AND_CATEGORY));
        Spi::run("RESET ROLE").unwrap();
    }

    #[pg_test]
    fn a_union_all_respelled_returns_the_rows_row_security_with_a_subquery_admits() {
        lego(Indexes::Surveyors);
        let arms = "SELECT p.purchase_id, i.id AS inventory_id FROM lego_purchases p \
                JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
                JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
            UNION ALL \
            SELECT p.purchase_id, ci.id FROM lego_purchases p \
                JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
                JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
                JOIN lego_inventory_sets sub ON sub.inventory_id = i.id \
                JOIN lego_inventories ci ON ci.set_num = sub.set_num AND ci.version = 1 \
            ORDER BY 1, 2";
        let every_row = rows(arms);
        // the first arm read whole, so nothing is respelled
        let written =
            arms.replacen("SELECT", "(SELECT", 1)
                .replacen(" UNION ALL", " OFFSET 0) UNION ALL", 1);
        purchases_admitted_by_a_subquery();
        assert!(!respelled(&written));
        let expected = rows(&written);
        assert_ne!(expected, every_row);
        assert_eq!(rows(arms), expected);
        assert!(respelled(arms));
        Spi::run("RESET ROLE").unwrap();
    }

    #[pg_test]
    fn a_respelled_statement_keeps_its_text_and_its_names() {
        lego(Indexes::Surveyors);
        // a WITH query and relations named like the sides the respelling makes, and a dollar sign
        // in a group key
        let query = "WITH side_a AS MATERIALIZED (SELECT id, name FROM lego_part_categories) \
            SELECT side_a.home_zone || ' [$1$2]' AS zone, side_b.name AS category, \
                count(DISTINCT p.purchase_id) AS purchases, sum(ip.quantity) AS bricks \
            FROM lego_purchases p \
            JOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no \
            JOIN lego_builders side_a ON side_a.builder_id = p.builder_id \
            JOIN lego_inventories i ON i.set_num = c.set_num AND i.version = 1 \
            JOIN lego_inventory_parts ip ON ip.inventory_id = i.id \
            JOIN lego_parts pt ON pt.part_num = ip.part_num \
            JOIN side_a side_b ON side_b.id = pt.part_cat_id \
            GROUP BY 1, 2 ORDER BY bricks DESC, 1, 2";
        assert!(respelled(query));
        let expected: Vec<String> = rows(BY_ZONE_AND_CATEGORY_PER_SET)
            .into_iter()
            .map(|r| r.replacen(" | ", " [$1$2] | ", 1))
            .collect();
        assert_eq!(rows(query), expected);
    }
}
