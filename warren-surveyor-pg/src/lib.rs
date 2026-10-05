// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! warren_surveyor_pg: the surveyor, an index that reads a table's other indexes while a statement
//! is planned.
//!
//! A surveyor holds no entries and is never scanned. A table that carries one is planned with the
//! reads it turns on: the table's rows under its constant conditions, measured by its indexes while
//! planning; the size of a WITH RECURSIVE query over the table, read while planning; a grouping at
//! one value of a leading column, made by hashing; and the price of the table's B-trees.

use pgrx::pg_sys;
use pgrx::prelude::*;

mod budget;
mod carried;
mod closure;
#[allow(dead_code)]
mod conditions;
mod drive;
#[allow(dead_code)]
mod gin;
#[allow(dead_code)]
mod gist;
mod grouping;
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
    grouping::init();
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

    /// A catalogue each of whose facts lies in the table its key decides: 7 root themes and 40
    /// themes under them; 400 sets, each in a theme under a root; 600 builders, each in a zone; each
    /// builder's 10 holdings, each of a set; 20,000 purchases, each of a holding. Every row is written
    /// by joining what it relates to. Each table carries its primary key, a B-tree on what joins into
    /// it (the themes on their parent, the sets on their theme, the holdings on their set, the
    /// purchases on their holding) and one surveyor. The `*_plain` copies hold the same rows, primary
    /// keys and B-trees with no surveyor.
    fn sets_and_buys() {
        Spi::run(
            "CREATE TABLE themes (id int PRIMARY KEY, parent_id int); \
             INSERT INTO themes SELECT g, NULL FROM generate_series(1, 7) g; \
             INSERT INTO themes SELECT 100 + k, r.id FROM generate_series(0, 39) k \
                 JOIN themes r ON r.id = 1 + k % 7; \
             CREATE TABLE sets (set_num text PRIMARY KEY, theme_id int NOT NULL); \
             INSERT INTO sets SELECT 's' || g, t.id FROM generate_series(1, 400) g \
                 JOIN themes t ON t.id = 100 + g % 40; \
             CREATE TABLE builders (builder_id int PRIMARY KEY, zone int NOT NULL); \
             INSERT INTO builders SELECT g, g % 9 FROM generate_series(1, 600) g; \
             CREATE TABLE holdings (builder_id int NOT NULL, row_no int NOT NULL, set_num text NOT NULL, \
                                    PRIMARY KEY (builder_id, row_no)); \
             INSERT INTO holdings SELECT b.builder_id, r, s.set_num \
             FROM builders b CROSS JOIN generate_series(1, 10) r \
                 JOIN sets s ON s.set_num = 's' || (1 + (b.builder_id * 17 + r * 31) % 400); \
             CREATE TABLE buys (id int PRIMARY KEY, builder_id int NOT NULL, row_no int NOT NULL, \
                                qty int NOT NULL); \
             INSERT INTO buys SELECT g, h.builder_id, h.row_no, g % 7 FROM generate_series(1, 20000) g \
                 JOIN holdings h ON h.builder_id = 1 + (g * 7919) % 6000 % 600 \
                                AND h.row_no = 1 + (g * 7919) % 6000 / 600; \
             CREATE TABLE themes_plain AS SELECT * FROM themes; \
             CREATE TABLE sets_plain AS SELECT * FROM sets; \
             CREATE TABLE builders_plain AS SELECT * FROM builders; \
             CREATE TABLE holdings_plain AS SELECT * FROM holdings; \
             CREATE TABLE buys_plain AS SELECT * FROM buys; \
             ALTER TABLE themes_plain ADD PRIMARY KEY (id); \
             ALTER TABLE sets_plain ADD PRIMARY KEY (set_num); \
             ALTER TABLE builders_plain ADD PRIMARY KEY (builder_id); \
             ALTER TABLE holdings_plain ADD PRIMARY KEY (builder_id, row_no); \
             ALTER TABLE buys_plain ADD PRIMARY KEY (id); \
             CREATE INDEX ON themes (parent_id); CREATE INDEX ON themes_plain (parent_id); \
             CREATE INDEX ON sets (theme_id); CREATE INDEX ON sets_plain (theme_id); \
             CREATE INDEX ON holdings (set_num); CREATE INDEX ON holdings_plain (set_num); \
             CREATE INDEX ON buys (builder_id, row_no); CREATE INDEX ON buys_plain (builder_id, row_no); \
             CREATE INDEX themes_key ON themes USING surveyor (id); \
             CREATE INDEX sets_key ON sets USING surveyor (set_num); \
             CREATE INDEX builders_key ON builders USING surveyor (builder_id); \
             CREATE INDEX holdings_key ON holdings USING surveyor (builder_id, row_no); \
             CREATE INDEX buys_key ON buys USING surveyor (id); \
             ANALYZE themes; ANALYZE sets; ANALYZE builders; ANALYZE holdings; ANALYZE buys; \
             ANALYZE themes_plain; ANALYZE sets_plain; ANALYZE builders_plain; ANALYZE holdings_plain; \
             ANALYZE buys_plain",
        )
        .expect("the sets and buys could not be made");
    }

    /// `query` read from the plain copies of the tables `sets_and_buys` makes.
    fn on_plain(query: &str) -> String {
        let mut plain = query.to_string();
        for table in ["themes", "sets", "builders", "holdings", "buys"] {
            plain = plain.replace(&format!(" {table} "), &format!(" {table}_plain "));
        }
        plain
    }

    /// The purchases of the sets under one root, each against every holding of its set: `t` the
    /// themes under the root, `bb` the buyer, `ob` the holder.
    const EACH_PAIR: &str = "FROM themes t JOIN sets s ON s.theme_id = t.id \
        JOIN holdings h ON h.set_num = s.set_num \
        JOIN buys b ON b.builder_id = h.builder_id AND b.row_no = h.row_no \
        JOIN builders bb ON bb.builder_id = h.builder_id \
        JOIN holdings o ON o.set_num = s.set_num \
        JOIN builders ob ON ob.builder_id = o.builder_id";

    /// The answer of `query` on the tables with surveyors and, as written, on their plain copies.
    fn respelled_and_as_written(query: &str) -> (Vec<String>, Vec<String>, String, String) {
        let plain = on_plain(query);
        (
            texts(query),
            texts(&plain),
            texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {query}")).join("\n"),
            texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {plain}")).join("\n"),
        )
    }

    /// The columns of the first Group Key line of `plan`.
    fn group_key(plan: &str) -> String {
        plan.lines()
            .find(|l| l.contains("Group Key"))
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    #[pg_test]
    fn a_group_by_a_tables_unique_key_is_planned_on_the_key_alone() {
        Spi::run(
            "CREATE TABLE kits (set_num text PRIMARY KEY, theme int NOT NULL, qty int); \
             INSERT INTO kits SELECT 's' || g, g % 40, g % 7 FROM generate_series(1, 4000) g; \
             CREATE TABLE kits_plain AS SELECT * FROM kits; \
             ALTER TABLE kits_plain ADD PRIMARY KEY (set_num); \
             CREATE INDEX kits_theme ON kits USING surveyor (theme, set_num); \
             ANALYZE kits; ANALYZE kits_plain",
        )
        .unwrap();
        let keys = |query: &str| {
            group_key(&texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {query}")).join("\n"))
        };
        // the table with a surveyor groups as the one without
        let query = "SELECT set_num || ' ' || sum(qty) FROM kits GROUP BY set_num";
        let mut answer = texts(query);
        let mut written = texts(&query.replace("kits", "kits_plain"));
        answer.sort();
        written.sort();
        assert_eq!(answer, written);
        assert_eq!(keys(query), "Group Key: kits.set_num");
        assert_eq!(
            keys(&query.replace("kits", "kits_plain")),
            "Group Key: kits_plain.set_num"
        );
        // naming the surveyor's leading column ahead of the key does not keep it: the planner
        // groups by the key alone
        let named = "SELECT set_num || ' ' || sum(qty) FROM kits GROUP BY theme, set_num";
        assert_eq!(keys(named), "Group Key: kits.set_num");
    }

    /// Whether a plan groups rows it sorts first: a Sort or an Incremental Sort right beneath a
    /// GroupAggregate or a Group.
    fn sorts_to_group(plan: &str) -> bool {
        let nodes: Vec<&str> = plan
            .lines()
            .filter(|l| l.contains("->") || !l.starts_with(' '))
            .map(|l| l.trim_start().trim_start_matches("->").trim_start())
            .collect();
        nodes.windows(2).any(|w| {
            (w[0].starts_with("GroupAggregate") || w[0].starts_with("Group "))
                && (w[1].starts_with("Sort") || w[1].starts_with("Incremental Sort"))
        })
    }

    /// Each root's purchases against every holding of their sets, read one root at a time and
    /// grouped by buyer and holder: a line per root of its pairs, and of their purchases weighed by
    /// the two zones.
    fn each_root() -> String {
        format!(
            "SELECT r.id || ' ' || x.pairs || ' ' || x.weight AS line \
             FROM themes r CROSS JOIN LATERAL ( \
                 SELECT count(*) AS pairs, sum(y.n * (y.zone + 1) * (y.owned_zone + 1)) AS weight \
                 FROM (SELECT bb.zone, ob.zone AS owned_zone, count(*) AS n {EACH_PAIR} \
                       WHERE t.parent_id = r.id GROUP BY bb.builder_id, ob.builder_id) y) x \
             WHERE r.parent_id IS NULL ORDER BY 1"
        )
    }

    /// The count each of `lines` carries in its second field.
    fn counts_of(lines: &[String]) -> Vec<i64> {
        lines
            .iter()
            .map(|l| l.split(' ').nth(1).unwrap_or_default().parse().unwrap_or(0))
            .collect()
    }

    #[pg_test]
    fn a_grouping_read_one_leading_value_at_a_time_hashes_where_the_planner_sorts() {
        sets_and_buys();
        Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
        let (hashed, written, plan, plain_plan) = respelled_and_as_written(&each_root());
        assert_eq!(hashed, written);
        // every root pairs more builders than the 81 pairs of zones
        let pairs = counts_of(&hashed);
        assert_eq!(pairs.len(), 7, "{hashed:?}");
        assert!(pairs.iter().all(|&p| p > 81), "{hashed:?}");
        assert!(
            plan.contains("Group Key: bb.builder_id, ob.builder_id"),
            "{plan}"
        );
        assert!(plan.contains("HashAggregate"), "{plan}");
        assert!(!sorts_to_group(&plan), "{plan}");
        assert!(sorts_to_group(&plain_plan), "{plain_plan}");
    }

    #[pg_test]
    fn a_grouping_read_at_one_parameter_value_hashes_where_the_planner_sorts() {
        sets_and_buys();
        Spi::run("SET LOCAL work_mem = '64kB'; SET LOCAL plan_cache_mode = force_generic_plan")
            .unwrap();
        let query = format!(
            "SELECT bb.builder_id || ' ' || ob.builder_id || ' ' || count(*) AS line {EACH_PAIR} \
             WHERE t.parent_id = $1 GROUP BY bb.builder_id, ob.builder_id ORDER BY 1"
        );
        let plain = on_plain(&query);
        Spi::run(&format!(
            "PREPARE at_root(int) AS {query}; PREPARE plain_at_root(int) AS {plain}"
        ))
        .unwrap();
        let plan = texts("EXPLAIN (COSTS OFF, VERBOSE) EXECUTE at_root(3)").join("\n");
        let plain_plan = texts("EXPLAIN (COSTS OFF, VERBOSE) EXECUTE plain_at_root(3)").join("\n");
        let hashed = texts("EXECUTE at_root(3)");
        assert_eq!(hashed, texts("EXECUTE plain_at_root(3)"));
        assert!(hashed.len() > 81, "{} pairs", hashed.len());
        assert!(plan.contains("$1"), "{plan}");
        assert!(plan.contains("HashAggregate"), "{plan}");
        assert!(!sorts_to_group(&plan), "{plan}");
        assert!(sorts_to_group(&plain_plan), "{plain_plan}");
        Spi::run("DEALLOCATE at_root; DEALLOCATE plain_at_root").unwrap();
    }

    #[pg_test]
    fn a_grouping_that_reads_a_surveyor_table_at_every_leading_value_is_planned_as_chosen() {
        sets_and_buys();
        Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
        // each root's purchases, against every builder in their buyer's zone whatever its root,
        // grouped finely enough that the planner sorts to group them; no B-tree of the builders
        // leads with the zone
        let (grouped, written, plan, plain_plan) = respelled_and_as_written(
            "SELECT r.id || ' ' || x.groups || ' ' || x.weight AS line \
             FROM themes r CROSS JOIN LATERAL ( \
                 SELECT count(*) AS groups, sum(y.n * (y.qty + 1) * (y.zone + 1) * y.row_no) AS weight \
                 FROM (SELECT b.qty, b.row_no, z.builder_id, z.zone, count(*) AS n \
                       FROM themes t JOIN sets s ON s.theme_id = t.id \
                           JOIN holdings h ON h.set_num = s.set_num \
                           JOIN buys b ON b.builder_id = h.builder_id AND b.row_no = h.row_no \
                           JOIN builders bb ON bb.builder_id = h.builder_id \
                           JOIN builders z ON z.zone = bb.zone \
                       WHERE t.parent_id = r.id \
                       GROUP BY b.qty, b.row_no, z.builder_id, z.zone) y) x \
             WHERE r.parent_id IS NULL ORDER BY 1",
        );
        assert_eq!(grouped, written);
        let groups = counts_of(&grouped);
        assert_eq!(groups.len(), 7, "{grouped:?}");
        assert!(groups.iter().all(|&g| g > 0), "{grouped:?}");
        assert!(sorts_to_group(&plain_plan), "{plain_plan}");
        assert!(sorts_to_group(&plan), "{plan}");
    }

    #[pg_test]
    fn a_grouping_over_every_value_of_a_loop_is_planned_as_chosen() {
        sets_and_buys();
        Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
        // the loop's own grouping reads one root at a time; the grouping over it reads every root
        let (grouped, written, plan, _) = respelled_and_as_written(&format!(
            "SELECT count(*) || ' ' || sum(z.n * z.root * (z.zone + 1) * (z.owned_zone + 1)) AS line \
             FROM (SELECT r.id AS root, x.buyer, x.holder, x.zone, x.owned_zone, sum(x.n) AS n \
                   FROM themes r CROSS JOIN LATERAL ( \
                       SELECT bb.builder_id AS buyer, ob.builder_id AS holder, bb.zone, \
                              ob.zone AS owned_zone, count(*) AS n {EACH_PAIR} \
                       WHERE t.parent_id = r.id GROUP BY bb.builder_id, ob.builder_id) x \
                   WHERE r.parent_id IS NULL \
                   GROUP BY r.id, x.buyer, x.holder, x.zone, x.owned_zone) z"
        ));
        assert_eq!(grouped, written);
        let pairs: i64 = grouped[0].split(' ').next().unwrap().parse().unwrap();
        assert!(pairs > 7 * 81, "{grouped:?}");
        let hashed_in_loop = plan.lines().collect::<Vec<_>>().windows(3).any(|w| {
            w[0].contains("HashAggregate")
                && w[2].contains("Group Key: bb.builder_id, ob.builder_id")
        });
        assert!(hashed_in_loop, "{plan}");
        assert!(sorts_to_group(&plan), "{plan}");
    }

    #[pg_test]
    fn with_hashing_turned_off_a_grouping_at_one_leading_value_is_planned_as_chosen() {
        sets_and_buys();
        Spi::run("SET LOCAL work_mem = '64kB'; SET LOCAL enable_hashagg = off").unwrap();
        let (grouped, written, plan, _) = respelled_and_as_written(&each_root());
        assert_eq!(grouped, written);
        assert!(sorts_to_group(&plan), "{plan}");
    }

    #[pg_test]
    fn a_grouping_at_one_parameter_value_is_not_sorted_beneath_a_gather() {
        sets_and_buys();
        Spi::run(
            "SET LOCAL work_mem = '64kB'; SET LOCAL plan_cache_mode = force_generic_plan; \
             SET LOCAL max_parallel_workers_per_gather = 4; SET LOCAL parallel_setup_cost = 0; \
             SET LOCAL parallel_tuple_cost = 0; SET LOCAL min_parallel_table_scan_size = 0; \
             SET LOCAL enable_indexscan = off; SET LOCAL enable_indexonlyscan = off; \
             SET LOCAL enable_bitmapscan = off",
        )
        .unwrap();
        // grouped by a key with a group per row, in the order the answer is sorted by
        let query = "SELECT b.id || ' ' || count(*) AS line \
             FROM themes t JOIN sets s ON s.theme_id = t.id \
                 JOIN holdings h ON h.set_num = s.set_num \
                 JOIN buys b ON b.builder_id = h.builder_id AND b.row_no = h.row_no \
             WHERE t.parent_id = $1 GROUP BY b.id ORDER BY b.id";
        Spi::run(&format!(
            "PREPARE parallel_at_root(int) AS {query}; \
             PREPARE plain_parallel_at_root(int) AS {}",
            on_plain(query)
        ))
        .unwrap();
        let plan = texts("EXPLAIN (COSTS OFF) EXECUTE parallel_at_root(3)").join("\n");
        let plain_plan = texts("EXPLAIN (COSTS OFF) EXECUTE plain_parallel_at_root(3)").join("\n");
        assert_eq!(
            texts("EXECUTE parallel_at_root(3)"),
            texts("EXECUTE plain_parallel_at_root(3)")
        );
        assert!(
            plain_plan.contains("GroupAggregate") && plain_plan.contains("Gather Merge"),
            "{plain_plan}"
        );
        assert!(!plan.contains("GroupAggregate"), "{plan}");
        Spi::run("DEALLOCATE parallel_at_root; DEALLOCATE plain_parallel_at_root").unwrap();
    }

    /// A catalogue each of whose facts lies in the table its key decides: 4 themes; 1,200 sets,
    /// each in a theme; 3,000 builders, each in a zone; each builder's 4 holdings, each of a set;
    /// 200,000 purchases, each of a holding. Every row is written by joining what it relates to.
    /// Each table carries its primary key and a B-tree on each column that joins into it: the sets on
    /// their theme, the holdings on their set, the purchases on their holding. `crew` holds the
    /// builders again with no unique key, and a B-tree led by the zone. Each carries one surveyor, on
    /// its primary key or, without one, on the columns its B-tree names. The `*_plain` copies hold
    /// the same rows, primary keys and B-trees, with no surveyor; the themes, read by both, carry none.
    fn catalogue() {
        Spi::run(
            "CREATE TABLE themes (id int PRIMARY KEY); \
             INSERT INTO themes SELECT g FROM generate_series(1, 4) g; \
             CREATE TABLE kit_sets (set_num text PRIMARY KEY, theme_id int NOT NULL); \
             INSERT INTO kit_sets SELECT 's' || g, t.id FROM generate_series(1, 1200) g \
                 JOIN themes t ON t.id = 1 + g % 4; \
             CREATE TABLE kit_builders (id int PRIMARY KEY, zone int NOT NULL); \
             INSERT INTO kit_builders SELECT g, g % 9 FROM generate_series(1, 3000) g; \
             CREATE TABLE kit_holdings (builder_id int NOT NULL, row_no int NOT NULL, \
                                        set_num text NOT NULL, PRIMARY KEY (builder_id, row_no)); \
             INSERT INTO kit_holdings SELECT b.id, r, s.set_num \
             FROM kit_builders b CROSS JOIN generate_series(1, 4) r \
                 JOIN kit_sets s ON s.set_num = 's' || (1 + (b.id * 17 + r * 31) % 1200); \
             CREATE TABLE kit_buys (builder_id int NOT NULL, row_no int NOT NULL, qty int NOT NULL); \
             INSERT INTO kit_buys SELECT h.builder_id, h.row_no, g % 7 FROM generate_series(1, 200000) g \
                 JOIN kit_holdings h ON h.builder_id = 1 + (g * 7919) % 12000 % 3000 \
                                    AND h.row_no = 1 + (g * 7919) % 12000 / 3000; \
             CREATE TABLE crew (id int NOT NULL, zone int NOT NULL); \
             INSERT INTO crew SELECT id, zone FROM kit_builders; \
             CREATE TABLE kit_sets_plain AS SELECT * FROM kit_sets; \
             CREATE TABLE kit_holdings_plain AS SELECT * FROM kit_holdings; \
             CREATE TABLE kit_buys_plain AS SELECT * FROM kit_buys; \
             CREATE TABLE kit_builders_plain AS SELECT * FROM kit_builders; \
             CREATE TABLE crew_plain AS SELECT * FROM crew; \
             ALTER TABLE kit_sets_plain ADD PRIMARY KEY (set_num); \
             ALTER TABLE kit_holdings_plain ADD PRIMARY KEY (builder_id, row_no); \
             ALTER TABLE kit_builders_plain ADD PRIMARY KEY (id); \
             CREATE INDEX ON kit_sets (theme_id); CREATE INDEX ON kit_sets_plain (theme_id); \
             CREATE INDEX ON kit_holdings (set_num); CREATE INDEX ON kit_holdings_plain (set_num); \
             CREATE INDEX ON kit_buys (builder_id, row_no); CREATE INDEX ON kit_buys_plain (builder_id, row_no); \
             CREATE INDEX ON crew (zone, id); CREATE INDEX ON crew_plain (zone, id); \
             CREATE INDEX kit_sets_key ON kit_sets USING surveyor (set_num); \
             CREATE INDEX kit_holdings_key ON kit_holdings USING surveyor (builder_id, row_no); \
             CREATE INDEX kit_buys_key ON kit_buys USING surveyor (builder_id, row_no); \
             CREATE INDEX kit_builders_key ON kit_builders USING surveyor (id); \
             CREATE INDEX crew_key ON crew USING surveyor (zone, id); \
             ANALYZE themes; ANALYZE kit_sets; ANALYZE kit_holdings; ANALYZE kit_buys; \
             ANALYZE kit_builders; ANALYZE crew; ANALYZE kit_sets_plain; ANALYZE kit_holdings_plain; \
             ANALYZE kit_buys_plain; ANALYZE kit_builders_plain; ANALYZE crew_plain",
        )
        .expect("the catalogue could not be made");
    }

    /// `query` read from the plain copies of the tables `catalogue` makes.
    fn on_plain_catalogue(query: &str) -> String {
        let mut plain = query.to_string();
        for table in [
            "kit_sets",
            "kit_holdings",
            "kit_buys",
            "kit_builders",
            "crew",
        ] {
            plain = plain.replace(&format!(" {table} "), &format!(" {table}_plain "));
        }
        plain
    }

    /// The answer and plan of `query`, and of the same over the plain copies.
    fn catalogue_and_plain(query: &str) -> (Vec<String>, Vec<String>, String, String) {
        let plain = on_plain_catalogue(query);
        (
            texts(query),
            texts(&plain),
            texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {query}")).join("\n"),
            texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {plain}")).join("\n"),
        )
    }

    /// Each theme's purchases, grouped by builder, quantity and set, the builders joined by `joined`.
    fn each_theme(joined: &str) -> String {
        format!(
            "SELECT count(*)::text || ' ' || sum(x.n * x.builder * (x.qty + 1))::text AS line \
             FROM themes t CROSS JOIN LATERAL ( \
                 SELECT b.id AS builder, p.qty, count(*) AS n \
                 FROM kit_sets s JOIN kit_holdings c ON c.set_num = s.set_num \
                     JOIN kit_buys p ON p.builder_id = c.builder_id AND p.row_no = c.row_no {joined} \
                 WHERE s.theme_id = t.id GROUP BY b.id, p.qty, c.set_num) x"
        )
    }

    #[pg_test]
    fn a_grouping_whose_joins_carry_one_value_through_each_table_hashes_where_the_planner_sorts() {
        catalogue();
        Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
        // the sets are entered at the theme, the holdings at the block's sets, the purchases at the
        // block's holdings, and the builders by their own key from the block's holdings
        let (hashed, written, plan, plain_plan) =
            catalogue_and_plain(&each_theme("JOIN kit_builders b ON b.id = c.builder_id"));
        assert_eq!(hashed, written);
        assert!(!hashed[0].starts_with("0 "), "{hashed:?}");
        assert!(plan.contains("HashAggregate"), "{plan}");
        assert!(!sorts_to_group(&plan), "{plan}");
        assert!(sorts_to_group(&plain_plan), "{plain_plan}");
    }

    #[pg_test]
    fn a_grouping_at_one_parameter_whose_joins_carry_it_through_each_table_hashes() {
        catalogue();
        Spi::run("SET LOCAL work_mem = '64kB'; SET LOCAL plan_cache_mode = force_generic_plan")
            .unwrap();
        let query = "SELECT b.id || ' ' || p.qty || ' ' || c.set_num || ' ' || count(*) AS line \
             FROM kit_sets s JOIN kit_holdings c ON c.set_num = s.set_num \
                 JOIN kit_buys p ON p.builder_id = c.builder_id AND p.row_no = c.row_no \
                 JOIN kit_builders b ON b.id = c.builder_id \
             WHERE s.theme_id = $1 GROUP BY b.id, p.qty, c.set_num ORDER BY 1";
        let plain = on_plain_catalogue(query);
        Spi::run(&format!(
            "PREPARE at_theme(int) AS {query}; PREPARE plain_at_theme(int) AS {plain}"
        ))
        .unwrap();
        let plan = texts("EXPLAIN (COSTS OFF, VERBOSE) EXECUTE at_theme(3)").join("\n");
        let plain_plan = texts("EXPLAIN (COSTS OFF, VERBOSE) EXECUTE plain_at_theme(3)").join("\n");
        let hashed = texts("EXECUTE at_theme(3)");
        assert_eq!(hashed, texts("EXECUTE plain_at_theme(3)"));
        assert!(hashed.len() > 100, "{}", hashed.len());
        assert!(plan.contains("HashAggregate"), "{plan}");
        assert!(!sorts_to_group(&plan), "{plan}");
        assert!(sorts_to_group(&plain_plan), "{plain_plan}");
        Spi::run("DEALLOCATE at_theme; DEALLOCATE plain_at_theme").unwrap();
    }

    #[pg_test]
    fn a_grouping_whose_joins_carry_one_value_through_a_class_hierarchy_hashes_where_every_class_is_surveyed(
    ) {
        catalogue();
        // the purchases kept again as classes of one parent, by their quantity: the parent holding
        // none of them, and each table with the B-tree on the holding and a surveyor
        Spi::run(
            "CREATE TABLE kit_buys_kinds (builder_id int NOT NULL, row_no int NOT NULL, qty int NOT NULL); \
             CREATE TABLE kit_buys_few (CHECK (qty < 3)) INHERITS (kit_buys_kinds); \
             CREATE TABLE kit_buys_many (CHECK (qty >= 3)) INHERITS (kit_buys_kinds); \
             INSERT INTO kit_buys_few SELECT * FROM kit_buys WHERE qty < 3; \
             INSERT INTO kit_buys_many SELECT * FROM kit_buys WHERE qty >= 3; \
             CREATE INDEX ON kit_buys_kinds (builder_id, row_no); \
             CREATE INDEX ON kit_buys_few (builder_id, row_no); \
             CREATE INDEX ON kit_buys_many (builder_id, row_no); \
             CREATE INDEX kit_buys_kinds_key ON kit_buys_kinds USING surveyor (builder_id, row_no); \
             CREATE INDEX kit_buys_few_key ON kit_buys_few USING surveyor (builder_id, row_no); \
             CREATE INDEX kit_buys_many_key ON kit_buys_many USING surveyor (builder_id, row_no); \
             ANALYZE kit_buys_kinds; ANALYZE kit_buys_few; ANALYZE kit_buys_many; \
             SET LOCAL work_mem = '64kB'",
        )
        .unwrap();
        let flat = each_theme("JOIN kit_builders b ON b.id = c.builder_id");
        let query = flat.replace(" kit_buys ", " kit_buys_kinds ");
        let plan = |query: &str| texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {query}")).join("\n");
        let plain = on_plain_catalogue(&flat);
        assert_eq!(texts(&query), texts(&plain));
        let (hashed, plain_plan) = (plan(&query), plan(&plain));
        assert!(hashed.contains("HashAggregate"), "{hashed}");
        assert!(!sorts_to_group(&hashed), "{hashed}");
        assert!(sorts_to_group(&plain_plan), "{plain_plan}");
        // a class carrying no surveyor leaves the hierarchy outside the block: planned as chosen
        Spi::run("DROP INDEX kit_buys_many_key").unwrap();
        let chosen = plan(&query);
        assert!(sorts_to_group(&chosen), "{chosen}");
    }

    #[pg_test]
    fn a_grouping_through_a_class_hierarchy_hashes_past_an_empty_parent_that_carries_no_surveyor() {
        catalogue();
        // the purchases kept again as classes of one parent, by their quantity: the parent holding
        // none of them and carrying no surveyor, each table the B-tree on the holding, and each class
        // a surveyor
        Spi::run(
            "CREATE TABLE kit_buys_bare (builder_id int NOT NULL, row_no int NOT NULL, qty int NOT NULL); \
             CREATE TABLE kit_buys_low (CHECK (qty < 3)) INHERITS (kit_buys_bare); \
             CREATE TABLE kit_buys_high (CHECK (qty >= 3)) INHERITS (kit_buys_bare); \
             INSERT INTO kit_buys_low SELECT * FROM kit_buys WHERE qty < 3; \
             INSERT INTO kit_buys_high SELECT * FROM kit_buys WHERE qty >= 3; \
             CREATE INDEX ON kit_buys_bare (builder_id, row_no); \
             CREATE INDEX ON kit_buys_low (builder_id, row_no); \
             CREATE INDEX ON kit_buys_high (builder_id, row_no); \
             CREATE INDEX kit_buys_low_key ON kit_buys_low USING surveyor (builder_id, row_no); \
             CREATE INDEX kit_buys_high_key ON kit_buys_high USING surveyor (builder_id, row_no); \
             ANALYZE kit_buys_bare; ANALYZE kit_buys_low; ANALYZE kit_buys_high; \
             SET LOCAL work_mem = '64kB'",
        )
        .unwrap();
        let flat = each_theme("JOIN kit_builders b ON b.id = c.builder_id");
        let query = flat.replace(" kit_buys ", " kit_buys_bare ");
        let plan = |query: &str| texts(&format!("EXPLAIN (COSTS OFF, VERBOSE) {query}")).join("\n");
        assert_eq!(texts(&query), texts(&on_plain_catalogue(&flat)));
        let hashed = plan(&query);
        assert!(hashed.contains("HashAggregate"), "{hashed}");
        assert!(!sorts_to_group(&hashed), "{hashed}");
        // once the parent holds rows of its own, it carries no surveyor for them: planned as chosen
        Spi::run(
            "INSERT INTO kit_buys_bare SELECT * FROM kit_buys WHERE qty = 0 LIMIT 500; \
             ANALYZE kit_buys_bare",
        )
        .unwrap();
        let chosen = plan(&query);
        assert!(sorts_to_group(&chosen), "{chosen}");
    }

    #[pg_test]
    fn a_grouping_that_joins_a_table_on_a_column_it_is_not_entered_by_is_planned_as_chosen() {
        catalogue();
        Spi::run("SET LOCAL work_mem = '64kB'").unwrap();
        // the crew's B-tree leads with its zone and it has no unique key, so the builder joins no
        // block of it
        let (grouped, written, plan, plain_plan) =
            catalogue_and_plain(&each_theme("JOIN crew b ON b.id = c.builder_id"));
        assert_eq!(grouped, written);
        assert_eq!(
            sorts_to_group(&plan),
            sorts_to_group(&plain_plan),
            "{plan}\n{plain_plan}"
        );
        assert!(sorts_to_group(&plan), "{plan}");
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
