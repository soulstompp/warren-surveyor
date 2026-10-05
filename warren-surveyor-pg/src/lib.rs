// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! warren_surveyor_pg: the surveyor, an index that reads a table's other indexes while a statement
//! is planned.

use pgrx::pg_sys;
use pgrx::prelude::*;

mod budget;
mod carried;
mod closure;
#[allow(dead_code)]
mod conditions;
#[allow(dead_code)]
mod gin;
#[allow(dead_code)]
mod gist;
mod leaves;
#[allow(dead_code)]
mod measure;
mod options;
mod overlap;
#[cfg(any(test, feature = "pg_test"))]
mod planned;
mod price;
mod query;
#[allow(dead_code)]
mod reading;
mod region;
mod round;
mod size;
mod writes;

::pgrx::pg_module_magic!(name, version);

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    options::init();
    closure::init();
    size::init();
    leaves::init();
    round::init();
    price::init();
}

extension_sql!(
    r#"
CREATE FUNCTION surveyor_handler(internal) RETURNS index_am_handler
    LANGUAGE c STRICT AS 'MODULE_PATHNAME', 'surveyor_handler';

CREATE ACCESS METHOD surveyor TYPE INDEX HANDLER surveyor_handler;
COMMENT ON ACCESS METHOD surveyor IS 'an index that holds no entries and is never scanned: it reads the table''s other indexes while a statement is planned, so its idx_scan stays 0';

-- a class for each type a key column may hold, each ordered as the B-tree orders it
CREATE OPERATOR CLASS int2_ops DEFAULT FOR TYPE int2 USING surveyor AS
    FUNCTION 1 btint2cmp(int2, int2), FUNCTION 2 btint2sortsupport(internal);
CREATE OPERATOR CLASS int4_ops DEFAULT FOR TYPE int4 USING surveyor AS
    FUNCTION 1 btint4cmp(int4, int4), FUNCTION 2 btint4sortsupport(internal);
CREATE OPERATOR CLASS int8_ops DEFAULT FOR TYPE int8 USING surveyor AS
    FUNCTION 1 btint8cmp(int8, int8), FUNCTION 2 btint8sortsupport(internal);
CREATE OPERATOR CLASS text_ops DEFAULT FOR TYPE text USING surveyor AS
    FUNCTION 1 bttextcmp(text, text), FUNCTION 2 bttextsortsupport(internal);
CREATE OPERATOR CLASS numeric_ops DEFAULT FOR TYPE numeric USING surveyor AS
    FUNCTION 1 numeric_cmp(numeric, numeric), FUNCTION 2 numeric_sortsupport(internal);
CREATE OPERATOR CLASS timestamptz_ops DEFAULT FOR TYPE timestamptz USING surveyor AS
    FUNCTION 1 timestamptz_cmp(timestamptz, timestamptz), FUNCTION 2 timestamp_sortsupport(internal);
CREATE OPERATOR CLASS date_ops DEFAULT FOR TYPE date USING surveyor AS
    FUNCTION 1 date_cmp(date, date), FUNCTION 2 date_sortsupport(internal);
CREATE OPERATOR CLASS timestamp_ops DEFAULT FOR TYPE timestamp USING surveyor AS
    FUNCTION 1 timestamp_cmp(timestamp, timestamp), FUNCTION 2 timestamp_sortsupport(internal);
CREATE OPERATOR CLASS float4_ops DEFAULT FOR TYPE float4 USING surveyor AS
    FUNCTION 1 btfloat4cmp(float4, float4), FUNCTION 2 btfloat4sortsupport(internal);
CREATE OPERATOR CLASS float8_ops DEFAULT FOR TYPE float8 USING surveyor AS
    FUNCTION 1 btfloat8cmp(float8, float8), FUNCTION 2 btfloat8sortsupport(internal);
CREATE OPERATOR CLASS bool_ops DEFAULT FOR TYPE bool USING surveyor AS
    FUNCTION 1 btboolcmp(bool, bool);
CREATE OPERATOR CLASS uuid_ops DEFAULT FOR TYPE uuid USING surveyor AS
    FUNCTION 1 uuid_cmp(uuid, uuid), FUNCTION 2 uuid_sortsupport(internal);
CREATE OPERATOR CLASS bpchar_ops DEFAULT FOR TYPE bpchar USING surveyor AS
    FUNCTION 1 bpcharcmp(bpchar, bpchar), FUNCTION 2 bpchar_sortsupport(internal);
CREATE OPERATOR CLASS bytea_ops DEFAULT FOR TYPE bytea USING surveyor AS
    FUNCTION 1 byteacmp(bytea, bytea);
CREATE OPERATOR CLASS oid_ops DEFAULT FOR TYPE oid USING surveyor AS
    FUNCTION 1 btoidcmp(oid, oid), FUNCTION 2 btoidsortsupport(internal);
CREATE OPERATOR CLASS interval_ops DEFAULT FOR TYPE interval USING surveyor AS
    FUNCTION 1 interval_cmp(interval, interval);
CREATE OPERATOR CLASS time_ops DEFAULT FOR TYPE time USING surveyor AS
    FUNCTION 1 time_cmp(time, time);

-- the text type of a warren key: compared by code point, whatever the operating system's locale data;
-- where the schema already has one, it is kept only where another extension made it as the same
-- domain and a superuser owns it, since whoever owns the type may attach a check that runs as
-- whoever writes a key; any other type of that name stops the extension from being created
DO $warren_key$
DECLARE
    found oid;
BEGIN
    SELECT oid INTO found FROM pg_type
    WHERE typname = 'warren_key' AND typnamespace = '@extschema@'::regnamespace;
    IF found IS NULL THEN
        CREATE DOMAIN warren_key AS text COLLATE pg_c_utf8;
    ELSIF NOT EXISTS (
        SELECT 1 FROM pg_type t JOIN pg_roles r ON r.oid = t.typowner
        WHERE t.oid = found AND t.typtype = 'd' AND t.typbasetype = 'text'::regtype
          AND t.typcollation = 'pg_catalog.pg_c_utf8'::regcollation AND r.rolsuper
          AND EXISTS (SELECT 1 FROM pg_depend d
                      WHERE d.classid = 'pg_type'::regclass AND d.objid = found AND d.deptype = 'e')
    ) THEN
        RAISE EXCEPTION 'type %.warren_key exists, and is not a domain over text with collation pg_c_utf8 that an extension made and a superuser owns', '@extschema@'
            USING ERRCODE = 'duplicate_object',
                  HINT = 'Drop or rename that type, then create the extension again.';
    END IF;
END
$warren_key$;
"#,
    name = "surveyor",
);

#[no_mangle]
pub extern "C" fn pg_finfo_surveyor_handler() -> &'static pg_sys::Pg_finfo_record {
    static V1: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1
}

/// The access method's properties and callbacks.
#[no_mangle]
#[pg_guard]
pub unsafe extern "C-unwind" fn surveyor_handler(
    _fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let mut am = PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine);
    // no class carries an operator, and a count of 0 takes any operator number a class was given
    am.amstrategies = 0;
    // an index whose class carries a support function past this count cannot be opened
    am.amsupport = 2;
    am.amoptsprocnum = 0;
    am.amcanorder = false;
    am.amcanorderbyop = false;
    am.amcanhash = false;
    am.amconsistentequality = false;
    am.amconsistentordering = false;
    am.amcanbackward = false;
    am.amcanunique = false;
    am.amcanmulticol = true;
    am.amoptionalkey = false;
    am.amsearcharray = false;
    am.amsearchnulls = false;
    am.amstorage = false;
    am.amclusterable = false;
    am.ampredlocks = false;
    am.amcanparallel = false;
    am.amcanbuildparallel = false;
    am.amcaninclude = false;
    am.amusemaintenanceworkmem = false;
    // an update that changes only the columns a surveyor names stays HOT
    am.amsummarizing = true;
    am.amparallelvacuumoptions = pg_sys::VACUUM_OPTION_NO_PARALLEL as u8;
    am.amkeytype = pg_sys::InvalidOid;

    am.ambuild = Some(writes::build);
    am.ambuildempty = Some(writes::build_empty);
    am.aminsert = Some(writes::insert);
    am.ambulkdelete = Some(writes::bulk_delete);
    am.amvacuumcleanup = Some(writes::cleanup);
    am.amcostestimate = Some(cost);
    am.amoptions = Some(options::parse);
    am.amvalidate = Some(validate);
    am.ambeginscan = Some(writes::begin_scan);
    am.amrescan = Some(writes::rescan);
    am.amendscan = Some(writes::end_scan);

    pg_sys::Datum::from(am.into_pg())
}

/// Whether an operator class has a comparison procedure for its own type.
#[pg_guard]
unsafe extern "C-unwind" fn validate(opclass: pg_sys::Oid) -> bool {
    let family = pg_sys::get_opclass_family(opclass);
    let input = pg_sys::get_opclass_input_type(opclass);
    if pg_sys::get_opfamily_proc(family, input, input, 1) == pg_sys::InvalidOid {
        let name = std::ffi::CStr::from_ptr(pg_sys::format_type_be(input)).to_string_lossy();
        info!("surveyor operator class for {name} has no comparison function");
        return false;
    }
    true
}

/// The price of a path through a surveyor. No path reads one, since its classes match no
/// condition; a class that carries operators is asked, and answered with a price no plan takes.
#[pg_guard]
#[allow(clippy::too_many_arguments)]
unsafe extern "C-unwind" fn cost(
    _root: *mut pg_sys::PlannerInfo,
    _path: *mut pg_sys::IndexPath,
    _loop_count: f64,
    startup: *mut pg_sys::Cost,
    total: *mut pg_sys::Cost,
    selectivity: *mut pg_sys::Selectivity,
    correlation: *mut f64,
    pages: *mut f64,
) {
    *startup = pg_sys::disable_cost;
    *total = pg_sys::disable_cost;
    *selectivity = 0.0;
    *correlation = 0.0;
    *pages = 0.0;
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    pub(crate) fn texts(sql: &str) -> Vec<String> {
        Spi::connect(|client| {
            let mut out = Vec::new();
            for row in client.select(sql, None, &[])? {
                out.push(row.get::<String>(1)?.unwrap_or_default());
            }
            Ok::<_, pgrx::spi::SpiError>(out)
        })
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// Fixes the choices a GiST build makes between pages that take an entry equally well, which
    /// PostgreSQL draws from the session's own random state and `setseed` leaves alone, so that the
    /// same rows build the same index on every run. It is called in the statement before the build:
    /// a server built with assertions draws from that state on every miss of its catalog caches.
    #[pg_extern]
    fn same_gist_every_run() {
        unsafe { pg_sys::pg_prng_seed(std::ptr::addr_of_mut!(pg_sys::pg_global_prng_state), 1) };
    }

    fn plan(sql: &str) -> String {
        texts(&format!("EXPLAIN (COSTS OFF) {sql}")).join("\n")
    }

    /// Planner settings that leave any index as the way to read a table.
    const THROUGH_INDEXES: &str = "SET LOCAL enable_seqscan = off; SET LOCAL enable_sort = off";

    #[pg_test]
    fn a_surveyor_is_an_index_type_that_is_never_scanned() {
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT NOT pg_indexam_has_property(oid, 'can_order') \
                 AND NOT pg_indexam_has_property(oid, 'can_include') \
                 AND pg_indexam_has_property(oid, 'can_multi_col') \
                 AND NOT pg_indexam_has_property(oid, 'can_unique') \
                 AND NOT pg_indexam_has_property(oid, 'can_exclude') \
                 FROM pg_am WHERE amname = 'surveyor'"
            ),
            Ok(Some(true))
        );
        // what `\dA+` shows a DBA
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT obj_description(oid, 'pg_am') FROM pg_am WHERE amname = 'surveyor'"
            ),
            Ok(Some(
                "an index that holds no entries and is never scanned: it reads the table's other \
                 indexes while a statement is planned, so its idx_scan stays 0"
                    .to_string()
            ))
        );
        Spi::run(
            "CREATE TABLE shape (a int, b text); \
             CREATE INDEX shape_ab ON shape USING surveyor (a, b)",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT NOT pg_index_has_property('shape_ab'::regclass, 'index_scan') \
                 AND NOT pg_index_has_property('shape_ab'::regclass, 'bitmap_scan') \
                 AND NOT pg_index_has_property('shape_ab'::regclass, 'backward_scan') \
                 AND NOT pg_index_has_property('shape_ab'::regclass, 'clusterable')"
            ),
            Ok(Some(true))
        );
    }

    /// 30,000 rows with NULLs and repeated values in every column, and a surveyor on (a, b, c).
    fn probe() {
        Spi::run(
            "CREATE TABLE probe AS \
             SELECT g AS id, \
                    CASE WHEN g % 97 = 0 THEN NULL ELSE (g * 37) % 50 END AS a, \
                    CASE WHEN g % 89 = 0 THEN NULL ELSE 's' || lpad(((g * 13) % 30)::text, 2, '0') END AS b, \
                    CASE WHEN g % 83 = 0 THEN NULL \
                         ELSE timestamptz '2020-01-01 00:00:00+00' + ((g * 31) % 1000) * interval '1 hour' END AS c \
             FROM generate_series(1, 30000) g; \
             CREATE INDEX probe_key ON probe USING surveyor (a, b, c); \
             ANALYZE probe",
        )
        .expect("the probe table could not be made");
    }

    #[pg_test]
    fn no_plan_reads_a_surveyor() {
        probe();
        Spi::run(THROUGH_INDEXES).unwrap();
        for query in [
            "SELECT count(*)::text FROM probe WHERE a = 7",
            "SELECT count(*)::text FROM probe WHERE a = 7 AND b < 's10'",
            "SELECT count(*)::text FROM probe WHERE a IN (3, 7, 11)",
            "SELECT count(*)::text FROM probe WHERE a IS NULL",
            "SELECT a::text FROM probe ORDER BY a, b, c LIMIT 5",
        ] {
            let read = plan(query);
            assert!(!read.contains("probe_key"), "{query}\n{read}");
        }
        assert_eq!(
            texts("SELECT count(*)::text FROM probe WHERE a = 7"),
            texts("SELECT count(*) FILTER (WHERE a = 7)::text FROM probe")
        );
    }

    #[pg_test]
    fn a_surveyor_holds_no_page_after_its_build_and_new_rows() {
        probe();
        let size = || {
            Spi::get_one::<i64>("SELECT pg_relation_size('probe_key')")
                .unwrap()
                .unwrap()
        };
        assert_eq!(size(), 0);
        Spi::run(
            "INSERT INTO probe SELECT g, g % 50, 's01', now() FROM generate_series(30001, 40000) g; \
             UPDATE probe SET a = a + 1 WHERE id % 10 = 0; \
             DELETE FROM probe WHERE id % 7 = 0",
        )
        .unwrap();
        assert_eq!(size(), 0);
        Spi::run("REINDEX INDEX probe_key; ANALYZE probe").unwrap();
        assert_eq!(size(), 0);
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT indisvalid AND indisready FROM pg_index WHERE indexrelid = 'probe_key'::regclass"
            ),
            Ok(Some(true))
        );
    }

    #[pg_test]
    fn a_class_that_carries_operators_and_a_second_function_takes_no_path() {
        // a class written with the B-tree's five operators and a sort support function
        Spi::run(
            "CREATE OPERATOR CLASS carried_int4_ops FOR TYPE int4 USING surveyor AS \
                 OPERATOR 1 <, OPERATOR 2 <=, OPERATOR 3 =, OPERATOR 4 >=, OPERATOR 5 >, \
                 FUNCTION 1 btint4cmp(int4, int4), FUNCTION 2 btint4sortsupport(internal); \
             CREATE TABLE carried AS SELECT g AS id, g % 100 AS a FROM generate_series(1, 5000) g; \
             CREATE INDEX carried_a ON carried USING surveyor (a carried_int4_ops); \
             ANALYZE carried",
        )
        .unwrap();
        Spi::run(THROUGH_INDEXES).unwrap();
        for query in [
            "SELECT count(*)::text FROM carried WHERE a = 7",
            "SELECT count(*)::text FROM carried WHERE a < 3",
        ] {
            let read = plan(query);
            assert!(!read.contains("carried_a"), "{query}\n{read}");
        }
        assert_eq!(
            texts("SELECT count(*)::text FROM carried WHERE a = 7"),
            vec!["50".to_string()]
        );
    }

    #[pg_test]
    fn a_build_leaves_the_tables_count_of_rows_as_it_was() {
        // with autovacuum off, no build writes the table's count, whatever it reports
        assert_eq!(
            Spi::get_one::<String>("SELECT current_setting('autovacuum')"),
            Ok(Some("on".to_string()))
        );
        Spi::run(
            "CREATE TABLE counted AS SELECT g AS id, g % 100 AS a FROM generate_series(1, 10000) g; \
             ANALYZE counted",
        )
        .unwrap();
        let rows = || {
            Spi::get_one::<f32>("SELECT reltuples FROM pg_class WHERE oid = 'counted'::regclass")
                .unwrap()
                .unwrap()
        };
        let analyzed = rows();
        assert_eq!(analyzed, 10000.0);
        for step in [
            "CREATE INDEX counted_a ON counted USING surveyor (a)",
            "DELETE FROM counted WHERE id % 2 = 0",
            "REINDEX INDEX counted_a",
            "REINDEX TABLE counted",
        ] {
            Spi::run(step).unwrap();
            assert_eq!(rows(), analyzed, "{step}");
        }
    }

    #[pg_test]
    fn an_update_of_the_columns_a_surveyor_names_stays_hot() {
        Spi::run(
            "CREATE TABLE kept (id int PRIMARY KEY, a int, b text) WITH (fillfactor = 50); \
             INSERT INTO kept SELECT g, g % 100, 'b' || g FROM generate_series(1, 2000) g; \
             CREATE INDEX kept_ab ON kept USING surveyor (a, b)",
        )
        .unwrap();
        let updated = Spi::get_one::<i64>(
            "WITH u AS (UPDATE kept SET a = a + 1, b = b || '.' WHERE id % 10 = 0 RETURNING 1) \
             SELECT count(*) FROM u",
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated, 200);
        assert_eq!(
            Spi::get_one::<i64>("SELECT pg_stat_get_xact_tuples_hot_updated('kept'::regclass)"),
            Ok(Some(updated))
        );
    }

    #[pg_test(error = "access method \"surveyor\" does not support ASC/DESC options")]
    fn a_descending_key_is_refused() {
        Spi::run(
            "CREATE TABLE desc_probe (a int); \
             CREATE INDEX desc_probe_a ON desc_probe USING surveyor (a DESC)",
        )
        .unwrap();
    }

    // the size of a WITH RECURSIVE query, read while planning

    /// A tree of 200 themes, 20 roots each the parent of 9 others, beside 19,800 roots with nothing
    /// under them; a B-tree on the parent and a surveyor. `tree_plain` holds the same rows and
    /// B-tree with no surveyor.
    fn tree() {
        Spi::run(
            "CREATE TABLE tree (id int NOT NULL, parent_id int); \
             INSERT INTO tree SELECT g, CASE WHEN g > 20 AND g <= 200 THEN 1 + (g - 21) / 9 END \
             FROM generate_series(1, 20000) g; \
             CREATE TABLE tree_plain AS SELECT * FROM tree; \
             CREATE INDEX ON tree (parent_id); CREATE INDEX ON tree_plain (parent_id); \
             CREATE INDEX tree_key ON tree USING surveyor (parent_id, id); \
             ANALYZE tree; ANALYZE tree_plain",
        )
        .expect("the tree could not be made");
    }

    /// Theme 3 and the themes under it, counted, read from `table`.
    fn subtree(table: &str) -> String {
        format!(
            "WITH RECURSIVE sub(id) AS (SELECT 3 UNION ALL \
             SELECT t.id FROM {table} t JOIN sub ON t.parent_id = sub.id) \
             SELECT count(*)::text FROM sub"
        )
    }

    /// The rows the plan of `query` estimates on its first line naming `node`.
    fn estimated(query: &str, node: &str) -> f64 {
        let plan = texts(&format!("EXPLAIN {query}"));
        let line = plan
            .iter()
            .find(|l| l.contains(node))
            .unwrap_or_else(|| panic!("no {node}: {plan:?}"));
        let at = line.find("rows=").expect("no rows") + 5;
        line[at..].split(' ').next().unwrap().parse().unwrap()
    }

    #[pg_test]
    fn a_recursive_query_over_a_surveyor_table_is_planned_at_the_rows_it_returns() {
        tree();
        assert_eq!(texts(&subtree("tree")), vec!["10"]);
        assert_eq!(estimated(&subtree("tree"), "CTE Scan on sub"), 10.0);
        // the recursive query's own plan keeps the planner's estimate
        assert_ne!(estimated(&subtree("tree"), "Recursive Union"), 10.0);
        // over a table with no surveyor, the planner's estimate stands
        assert_eq!(texts(&subtree("tree_plain")), vec!["10"]);
        assert_ne!(estimated(&subtree("tree_plain"), "CTE Scan on sub"), 10.0);
    }

    #[pg_test]
    fn a_kept_plan_of_a_recursive_query_returns_the_rows_the_table_holds_when_it_runs() {
        tree();
        Spi::run(&format!("PREPARE kept AS {}", subtree("tree"))).unwrap();
        assert_eq!(texts("EXECUTE kept"), vec!["10"]);
        Spi::run("DELETE FROM tree WHERE id = 39").unwrap();
        // the plan made before the delete is run again
        assert_eq!(estimated("EXECUTE kept", "CTE Scan on sub"), 10.0);
        assert_eq!(texts("EXECUTE kept"), vec!["9"]);
        Spi::run("DEALLOCATE kept").unwrap();
    }

    #[pg_test]
    fn a_recursive_query_that_takes_a_value_from_outside_it_keeps_the_planners_estimate() {
        tree();
        let walk = "SELECT t.id FROM tree t JOIN sub ON t.parent_id = sub.id";
        for query in [
            // a volatile function
            format!(
                "WITH RECURSIVE sub(id) AS (SELECT (random() * 0)::int + 3 UNION ALL {walk}) \
                 SELECT count(*)::text FROM sub"
            ),
            // another WITH query
            format!(
                "WITH RECURSIVE r AS MATERIALIZED (SELECT 3 AS id), \
                 sub(id) AS (SELECT id FROM r UNION ALL {walk}) SELECT count(*)::text FROM sub"
            ),
            // a column of the query around it
            format!(
                "SELECT (SELECT count(*) FROM (WITH RECURSIVE sub(id) AS (SELECT o.id UNION ALL {walk}) \
                 SELECT id FROM sub) s)::text FROM tree o WHERE o.id = 3"
            ),
        ] {
            assert_eq!(texts(&query), vec!["10"], "{query}");
            assert_ne!(estimated(&query, "CTE Scan on sub"), 10.0, "{query}");
        }
        // a parameter
        Spi::run(&format!(
            "PREPARE from_root(int) AS WITH RECURSIVE sub(id) AS (SELECT $1 UNION ALL {walk}) \
             SELECT count(*)::text FROM sub"
        ))
        .unwrap();
        assert_eq!(texts("EXECUTE from_root(3)"), vec!["10"]);
        assert_ne!(estimated("EXECUTE from_root(3)", "CTE Scan on sub"), 10.0);
        Spi::run("DEALLOCATE from_root").unwrap();
    }

    #[pg_test]
    fn a_recursive_query_of_many_rows_whose_read_takes_few_pages_is_planned_at_its_rows() {
        tree();
        // the tree is read once, before the first step
        let many = "WITH RECURSIVE sub(id) AS (SELECT 1 UNION ALL SELECT sub.id + 1 FROM sub \
                    WHERE sub.id < 10000 AND EXISTS (SELECT FROM tree WHERE tree.id = 1)) \
                    SELECT count(*)::text FROM sub";
        assert_eq!(texts(many), vec!["10000"]);
        assert_eq!(estimated(many, "CTE Scan on sub"), 10000.0);
    }

    #[pg_test]
    fn a_recursive_query_whose_read_passes_its_tables_pages_or_fails_keeps_the_planners_estimate() {
        tree();
        // each step reads the tree again
        let past = "WITH RECURSIVE sub(id) AS (SELECT 1 UNION ALL SELECT sub.id + 1 FROM sub \
                    WHERE sub.id < 10000 AND EXISTS (SELECT FROM tree WHERE tree.id = sub.id % 7 + 1)) \
                    SELECT count(*)::text FROM sub";
        assert_eq!(texts(past), vec!["10000"]);
        assert_ne!(estimated(past, "CTE Scan on sub"), 10000.0);
        // the read raises an error, and the statement is planned all the same
        let failing = "WITH RECURSIVE sub(id) AS (SELECT 3 UNION ALL \
                       SELECT t.id / (t.id - t.id) FROM tree t JOIN sub ON t.parent_id = sub.id) \
                       SELECT count(*) FROM sub";
        assert_ne!(estimated(failing, "CTE Scan on sub"), 10.0);
        assert_eq!(texts(&subtree("tree")), vec!["10"]);
        assert_eq!(estimated(&subtree("tree"), "CTE Scan on sub"), 10.0);
    }

    #[pg_test]
    fn a_recursive_query_that_returns_more_rows_than_its_tables_hold_keeps_the_planners_estimate() {
        tree();
        // the tree is read once, before the first step, and no step reads a page
        let past = "WITH RECURSIVE sub(id) AS (SELECT 1 UNION ALL SELECT sub.id + 1 FROM sub \
                    WHERE sub.id < 30000 AND EXISTS (SELECT FROM tree WHERE tree.id = 1)) \
                    SELECT count(*)::text FROM sub";
        assert_eq!(texts(past), vec!["30000"]);
        assert_ne!(estimated(past, "CTE Scan on sub"), 30000.0);
        // fewer rows than the tree holds
        let within = past.replace("30000", "19000");
        assert_eq!(estimated(&within, "CTE Scan on sub"), 19000.0);
    }

    /// A tree of 60 themes on one page, theme `n` the parent of themes `2n` and `2n + 1`, with a
    /// B-tree on the parent and a surveyor; and 20,000 sets of those themes on many pages, kept as
    /// one table, as a partitioned table of two partitions, and as an inheritance parent with no rows
    /// of its own over two children.
    fn twig() {
        Spi::run(
            "CREATE TABLE twig (id int NOT NULL, parent_id int); \
             INSERT INTO twig SELECT g, CASE WHEN g > 1 THEN g / 2 END FROM generate_series(1, 60) g; \
             CREATE INDEX ON twig (parent_id); \
             CREATE INDEX twig_key ON twig USING surveyor (parent_id, id); \
             CREATE TABLE twig_sets AS SELECT g AS id, 1 + g % 60 AS theme_id, repeat('x', 100) AS name \
             FROM generate_series(1, 20000) g; \
             CREATE UNIQUE INDEX ON twig_sets (id); \
             CREATE TABLE twig_parts (id int, theme_id int, name text) PARTITION BY RANGE (id); \
             CREATE TABLE twig_parts_low PARTITION OF twig_parts FOR VALUES FROM (1) TO (10001); \
             CREATE TABLE twig_parts_high PARTITION OF twig_parts FOR VALUES FROM (10001) TO (20001); \
             INSERT INTO twig_parts SELECT * FROM twig_sets; \
             CREATE TABLE twig_kinds (id int, theme_id int, name text); \
             CREATE TABLE twig_kinds_low () INHERITS (twig_kinds); \
             CREATE TABLE twig_kinds_high () INHERITS (twig_kinds); \
             INSERT INTO twig_kinds_low SELECT * FROM twig_sets WHERE id <= 10000; \
             INSERT INTO twig_kinds_high SELECT * FROM twig_sets WHERE id > 10000; \
             ANALYZE twig; ANALYZE twig_sets; ANALYZE twig_parts; ANALYZE twig_kinds",
        )
        .expect("the twig could not be made");
    }

    /// Theme 2 and the themes under it, 31 of them, read from the twig.
    const UNDER_TWO: &str = "WITH RECURSIVE sub(id) AS (SELECT 2 UNION ALL \
                             SELECT t.id FROM twig t JOIN sub ON t.parent_id = sub.id)";

    #[pg_test]
    fn a_recursive_query_read_within_the_pages_of_the_tables_its_statement_reads_is_planned_at_its_rows(
    ) {
        twig();
        assert_eq!(
            texts(&format!("{UNDER_TWO} SELECT count(*)::text FROM sub")),
            vec!["31"]
        );
        let wrong = [
            // joined
            format!(
                "{UNDER_TWO} SELECT count(*)::text FROM sub JOIN twig_sets s ON s.theme_id = sub.id"
            ),
            // in a sublink
            format!(
                "{UNDER_TWO} SELECT (count(*) + 0 * (SELECT max(theme_id) FROM twig_sets))::text \
                 FROM sub"
            ),
            // in another WITH query
            format!(
                "{UNDER_TWO}, s AS MATERIALIZED (SELECT theme_id FROM twig_sets) \
                 SELECT count(*)::text FROM sub JOIN s ON s.theme_id = sub.id"
            ),
            // a partitioned table, joined and in a sublink of its own
            format!(
                "{UNDER_TWO} SELECT count(*)::text FROM sub JOIN twig_parts s ON s.theme_id = sub.id"
            ),
            format!(
                "{UNDER_TWO} SELECT count(*)::text FROM sub \
                 WHERE sub.id IN (SELECT theme_id FROM twig_parts GROUP BY theme_id)"
            ),
            // an inheritance parent, in a subquery of its own
            format!(
                "{UNDER_TWO} SELECT count(*)::text FROM sub \
                 JOIN (SELECT theme_id FROM twig_kinds OFFSET 0) s ON s.theme_id = sub.id"
            ),
        ]
        .into_iter()
        .map(|query| (estimated(&query, "CTE Scan on sub"), query))
        .filter(|&(rows, _)| rows != 31.0)
        .collect::<Vec<_>>();
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert_eq!(
            texts(&format!(
                "{UNDER_TWO} SELECT count(*)::text FROM sub JOIN twig_sets s ON s.theme_id = sub.id"
            )),
            texts(
                "SELECT count(*)::text FROM twig_sets WHERE theme_id IN \
                 (2, 4, 5, 8, 9, 10, 11, 16, 17, 18, 19, 20, 21, 22, 23, \
                  32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47)"
            )
        );
    }

    #[pg_test]
    fn a_recursive_query_whose_read_passes_the_pages_of_the_tables_its_statement_reads_keeps_the_planners_estimate(
    ) {
        twig();
        let read = [
            // the statement reads the tree alone
            format!("{UNDER_TWO} SELECT count(*)::text FROM sub"),
            // the sets are written, never read
            format!("{UNDER_TWO} INSERT INTO twig_sets SELECT 20000 + sub.id, sub.id, '' FROM sub"),
            format!(
                "{UNDER_TWO} INSERT INTO twig_sets SELECT 20000 + sub.id, sub.id, '' FROM sub \
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name"
            ),
        ]
        .into_iter()
        .filter(|query| estimated(query, "CTE Scan on sub") == 31.0)
        .collect::<Vec<_>>();
        assert!(read.is_empty(), "{read:#?}");
        // each step reads the tree again, past the pages of the tree and the sets
        let past = "WITH RECURSIVE sub(id) AS (SELECT 1 UNION ALL SELECT sub.id + 1 FROM sub \
                    WHERE sub.id < 10000 AND EXISTS (SELECT FROM twig WHERE twig.id = sub.id % 7 + 1)) \
                    SELECT count(*)::text FROM sub JOIN twig_sets s ON s.id = sub.id";
        assert_eq!(texts(past), vec!["10000"]);
        assert_ne!(estimated(past, "CTE Scan on sub"), 10000.0);
    }

    #[pg_test]
    fn every_operator_class_validates() {
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT bool_and(amvalidate(c.oid)) FROM pg_opclass c JOIN pg_am m ON m.oid = c.opcmethod \
                 WHERE m.amname = 'surveyor'"
            ),
            Ok(Some(true))
        );
        Spi::run(
            "CREATE OPERATOR CLASS lacking_ops FOR TYPE money USING surveyor AS OPERATOR 1 <, OPERATOR 3 =",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT amvalidate(c.oid) FROM pg_opclass c JOIN pg_am m ON m.oid = c.opcmethod \
                 WHERE m.amname = 'surveyor' AND c.opcname = 'lacking_ops'"
            ),
            Ok(Some(false))
        );
    }

    #[pg_test(error = "unrecognized parameter \"fillfactor\"")]
    fn an_unknown_storage_parameter_is_refused() {
        Spi::run(
            "CREATE TABLE untuned (a int); \
             CREATE INDEX untuned_a ON untuned USING surveyor (a) WITH (fillfactor = 50)",
        )
        .unwrap();
    }

    #[pg_test]
    fn a_warren_key_compares_by_code_point() {
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT '9'::warren_key < 'B'::warren_key AND 'B'::warren_key < 'a'::warren_key \
                 AND (SELECT collname = 'pg_c_utf8' FROM pg_collation c JOIN pg_type t ON t.typcollation = c.oid \
                      WHERE t.typname = 'warren_key')"
            ),
            Ok(Some(true))
        );
    }
}

/// Tests that need a session of their own, outside a test's transaction.
#[cfg(test)]
mod sessions {
    /// A session on the test server, started with the extension installed.
    pub(crate) fn session() -> postgres::Client {
        pgrx_tests::run_test(
            "a_surveyor_is_an_index_type_that_is_never_scanned",
            None,
            crate::pg_test::postgresql_conf_options(),
        )
        .expect("the test server did not start");
        pgrx_tests::client()
            .expect("no session on the test server")
            .0
    }

    #[test]
    fn new_rows_write_nothing_to_the_wal_for_a_surveyor() {
        let mut db = session();
        db.batch_execute(
            "CREATE EXTENSION IF NOT EXISTS pg_walinspect; \
             DROP TABLE IF EXISTS logged_rows; \
             CREATE TABLE logged_rows (id int, a int); \
             CREATE INDEX logged_rows_a ON logged_rows USING surveyor (a)",
        )
        .unwrap();
        let start: String = db
            .query_one("SELECT pg_current_wal_insert_lsn()::text", &[])
            .unwrap()
            .get(0);
        db.batch_execute(
            "INSERT INTO logged_rows SELECT g, g % 100 FROM generate_series(1, 5000) g",
        )
        .unwrap();
        let row = db
            .query_one(
                "SELECT count(*) FILTER (WHERE relfilenode = pg_relation_filenode('logged_rows_a')), \
                        count(*) FILTER (WHERE relfilenode = pg_relation_filenode('logged_rows')) \
                 FROM pg_get_wal_block_info($1::text::pg_lsn, pg_current_wal_flush_lsn(), false)",
                &[&start],
            )
            .unwrap();
        // the table's own blocks are there to be seen
        assert!(row.get::<_, i64>(1) > 0, "no block of the table");
        assert_eq!(row.get::<_, i64>(0), 0, "blocks of the surveyor");
    }

    #[test]
    fn a_surveyor_stays_empty_and_valid_through_vacuum_and_every_rebuild() {
        let mut db = session();
        db.batch_execute(
            "DROP TABLE IF EXISTS swept; \
             CREATE TABLE swept AS SELECT g AS id, g % 100 AS a FROM generate_series(1, 20000) g; \
             CREATE INDEX swept_a ON swept USING surveyor (a); \
             DELETE FROM swept WHERE id % 3 = 0; \
             UPDATE swept SET a = a + 1 WHERE id % 5 = 0",
        )
        .unwrap();
        for step in [
            "VACUUM swept",
            "ANALYZE swept",
            "VACUUM FULL swept",
            "REINDEX TABLE swept",
            "REINDEX INDEX CONCURRENTLY swept_a",
            "TRUNCATE swept",
        ] {
            db.batch_execute(step).unwrap();
            let row = db
                .query_one(
                    "SELECT pg_relation_size('swept_a'), indisvalid AND indisready \
                     FROM pg_index WHERE indexrelid = 'swept_a'::regclass",
                    &[],
                )
                .unwrap_or_else(|e| panic!("{step}: {e}"));
            assert_eq!(row.get::<_, i64>(0), 0, "{step}");
            assert!(row.get::<_, bool>(1), "{step}");
        }
    }

    #[test]
    fn a_cancel_inside_the_read_of_a_recursive_query_is_raised_as_a_cancel_and_the_session_goes_on()
    {
        let mut db = session();
        db.batch_execute(
            "DROP TABLE IF EXISTS halted; DROP FUNCTION IF EXISTS halted_at(int); \
             CREATE TABLE halted AS SELECT g AS id FROM generate_series(1, 20000) g; \
             CREATE INDEX ON halted (id); \
             CREATE INDEX halted_order ON halted USING surveyor (id); \
             ANALYZE halted; \
             CREATE FUNCTION halted_at(int) RETURNS int IMMUTABLE LANGUAGE plpgsql AS $$ \
             BEGIN \
                 IF $1 > 5 THEN RAISE EXCEPTION 'halted at %', $1 USING ERRCODE = 'query_canceled'; END IF; \
                 RETURN $1; \
             END $$; \
             LOAD 'warren_surveyor_pg'",
        )
        .unwrap();
        // the read raises a cancel at its sixth row, as a statement timeout or a cancel request
        // raises one while it runs
        let planned = db
            .query(
                "EXPLAIN WITH RECURSIVE n(i) AS (SELECT min(id) FROM halted \
                 UNION ALL SELECT halted_at(i + 1) FROM n WHERE i < 10) SELECT count(*) FROM n",
                &[],
            )
            .map(|_| ())
            .map_err(|e| {
                e.as_db_error().map_or_else(
                    || e.to_string(),
                    |d| format!("{}: {}", d.code().code(), d.message()),
                )
            });
        let after = db
            .query_one("SELECT count(*) FROM halted", &[])
            .map(|row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string());
        db.batch_execute("DROP TABLE halted; DROP FUNCTION halted_at(int)")
            .ok();
        assert_eq!(planned, Err("57014: halted at 6".to_string()));
        assert_eq!(after, Ok(20000));
    }

    #[test]
    fn a_recursive_query_that_never_ends_is_planned_without_being_read_to_its_end() {
        let mut db = session();
        db.batch_execute(
            "DROP TABLE IF EXISTS endless; \
             CREATE TABLE endless AS SELECT g AS id FROM generate_series(1, 20000) g; \
             CREATE INDEX ON endless (id); \
             CREATE INDEX endless_order ON endless USING surveyor (id); \
             ANALYZE endless; \
             LOAD 'warren_surveyor_pg'; SET statement_timeout = '10s'",
        )
        .unwrap();
        let message = |e: postgres::Error| {
            e.as_db_error()
                .map_or_else(|| e.to_string(), |d| d.message().to_string())
        };
        // its first step reads a few pages of the table, and no step after it reads one
        let endless = "WITH RECURSIVE n(i) AS (SELECT min(id) FROM endless \
                       UNION ALL SELECT i + 1 FROM n)";
        let failed = [
            // ended by the LIMIT
            format!("{endless} SELECT * FROM n LIMIT 10"),
            // ended by the first row past a value
            format!("{endless} SELECT (SELECT i FROM n WHERE i > 100 LIMIT 1)"),
            // never ended, and only planned
            format!("{endless} SELECT count(*) FROM n"),
        ]
        .into_iter()
        .filter_map(|query| {
            db.query(&format!("EXPLAIN {query}"), &[])
                .err()
                .map(|e| format!("{query}: {}", message(e)))
        })
        .collect::<Vec<_>>();
        let rows = db
            .query_one(
                &format!("{endless} SELECT count(*) FROM (SELECT * FROM n LIMIT 10) s"),
                &[],
            )
            .map(|row| row.get::<_, i64>(0))
            .map_err(message);
        db.batch_execute("DROP TABLE endless").ok();
        assert!(failed.is_empty(), "{failed:#?}");
        assert_eq!(rows, Ok(10));
    }

    #[test]
    fn a_recursive_query_read_while_planning_waits_for_no_lock_on_a_partition_the_planner_prunes() {
        let mut holder = session();
        holder
            .batch_execute(
                "DROP TABLE IF EXISTS pruned_twig, pruned_parts; \
                 CREATE TABLE pruned_twig (id int NOT NULL, parent_id int); \
                 INSERT INTO pruned_twig SELECT g, CASE WHEN g > 1 THEN g / 2 END \
                 FROM generate_series(1, 60) g; \
                 CREATE INDEX ON pruned_twig (parent_id); \
                 CREATE INDEX pruned_twig_key ON pruned_twig USING surveyor (parent_id, id); \
                 CREATE TABLE pruned_parts (id int, theme_id int) PARTITION BY RANGE (id); \
                 CREATE TABLE pruned_parts_low PARTITION OF pruned_parts FOR VALUES FROM (1) TO (10001); \
                 CREATE TABLE pruned_parts_high PARTITION OF pruned_parts FOR VALUES FROM (10001) TO (20001); \
                 INSERT INTO pruned_parts SELECT g, 1 + g % 60 FROM generate_series(1, 20000) g; \
                 ANALYZE pruned_twig; ANALYZE pruned_parts",
            )
            .unwrap();
        holder
            .batch_execute("BEGIN; LOCK TABLE pruned_parts_high IN ACCESS EXCLUSIVE MODE")
            .unwrap();
        let mut db = session();
        db.batch_execute("LOAD 'warren_surveyor_pg'; SET statement_timeout = '3s'")
            .unwrap();
        let planned = db
            .query(
                "EXPLAIN WITH RECURSIVE sub(id) AS (SELECT 2 UNION ALL \
                 SELECT t.id FROM pruned_twig t JOIN sub ON t.parent_id = sub.id) \
                 SELECT count(*) FROM sub JOIN pruned_parts p ON p.theme_id = sub.id \
                 WHERE p.id < 100",
                &[],
            )
            .map(|_| ())
            .map_err(|e| {
                e.as_db_error()
                    .map_or_else(|| e.to_string(), |d| d.message().to_string())
            });
        holder
            .batch_execute("COMMIT; DROP TABLE pruned_twig, pruned_parts")
            .unwrap();
        assert_eq!(planned, Ok(()));
    }

    /// A session on the database `name` of the server `db` is a session on.
    fn session_on(db: &mut postgres::Client, name: &str) -> postgres::Client {
        let port: String = db
            .query_one("SELECT current_setting('port')", &[])
            .unwrap()
            .get(0);
        postgres::Config::new()
            .host("localhost")
            .port(port.parse().unwrap())
            .user(&pgrx_tests::get_pg_user())
            .dbname(name)
            .connect(postgres::NoTls)
            .unwrap_or_else(|e| panic!("no session on {name}: {e}"))
    }

    #[test]
    fn the_extension_keeps_a_warren_key_type_already_there_only_where_an_extension_made_it_as_its_own(
    ) {
        let mut db = session();
        for step in [
            "DROP DATABASE IF EXISTS warren_key_taken",
            "DROP ROLE IF EXISTS warren_key_maker",
            "CREATE DATABASE warren_key_taken",
            "CREATE ROLE warren_key_maker",
        ] {
            db.batch_execute(step).unwrap();
        }
        let mut taken = session_on(&mut db, "warren_key_taken");
        taken
            .batch_execute(
                "CREATE EXTENSION pg_prewarm; GRANT CREATE ON SCHEMA public TO warren_key_maker",
            )
            .unwrap();
        let mut installed = Vec::new();
        for (made, by) in [
            // by a role that may create in the schema, and may add a CHECK to it later
            (
                "CREATE DOMAIN warren_key AS text COLLATE pg_c_utf8",
                "a user",
            ),
            (
                "SET ROLE warren_key_maker; CREATE DOMAIN warren_key AS text COLLATE pg_c_utf8; \
                 RESET ROLE; ALTER EXTENSION pg_prewarm ADD DOMAIN warren_key",
                "an extension, owned by a user",
            ),
            (
                "CREATE DOMAIN warren_key AS text COLLATE \"C\"; \
                 ALTER EXTENSION pg_prewarm ADD DOMAIN warren_key",
                "an extension, of another collation",
            ),
            (
                "CREATE DOMAIN warren_key AS text COLLATE pg_c_utf8; \
                 ALTER EXTENSION pg_prewarm ADD DOMAIN warren_key",
                "an extension, as its own",
            ),
        ] {
            taken.batch_execute(made).unwrap();
            let install = taken.batch_execute("CREATE EXTENSION warren_surveyor_pg");
            installed.push((
                by,
                install.map_err(|e| {
                    e.as_db_error()
                        .map_or_else(|| e.to_string(), |d| d.message().to_string())
                }),
            ));
            for undo in [
                "DROP EXTENSION IF EXISTS warren_surveyor_pg",
                "ALTER EXTENSION pg_prewarm DROP DOMAIN warren_key",
                "DROP DOMAIN IF EXISTS warren_key",
            ] {
                taken.batch_execute(undo).ok();
            }
        }
        drop(taken);
        db.batch_execute("DROP DATABASE warren_key_taken").unwrap();
        db.batch_execute("DROP ROLE warren_key_maker").unwrap();
        let refused = "type public.warren_key exists, and is not a domain over text with \
                       collation pg_c_utf8 that an extension made and a superuser owns";
        assert_eq!(
            installed,
            [
                ("a user", Err(refused.to_string())),
                ("an extension, owned by a user", Err(refused.to_string())),
                (
                    "an extension, of another collation",
                    Err(refused.to_string())
                ),
                ("an extension, as its own", Ok(())),
            ]
        );
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {
        // perform one-off initialization when the pg_test framework starts
    }

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // return any postgresql.conf settings that are required for your tests
        vec![]
    }
}
