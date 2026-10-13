// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! warren_snail_pg: a warren-snail database file mounted in PostgreSQL as a schema of foreign
//! tables, read only. Every scan of one is a round of the snail's own, read from the file where it
//! lies.

use pgrx::pg_sys;
use pgrx::prelude::*;

mod compat;
mod datum;
mod import;
mod options;
mod scan;

::pgrx::pg_module_magic!(name, version);

extension_sql!(
    r#"
CREATE FUNCTION warren_snail_handler() RETURNS fdw_handler
    LANGUAGE c STRICT AS 'MODULE_PATHNAME', 'warren_snail_handler';
CREATE FUNCTION warren_snail_validator(text[], oid) RETURNS void
    LANGUAGE c STRICT AS 'MODULE_PATHNAME', 'warren_snail_validator';
CREATE FOREIGN DATA WRAPPER warren_snail
    HANDLER warren_snail_handler VALIDATOR warren_snail_validator;
COMMENT ON FOREIGN DATA WRAPPER warren_snail IS
    'A warren-snail file mounted where it lies: a server names the file, a foreign table its table';
"#,
    name = "warren_snail"
);

#[no_mangle]
pub extern "C" fn pg_finfo_warren_snail_handler() -> &'static pg_sys::Pg_finfo_record {
    static V1: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1
}

/// The wrapper's callbacks: a scan, read only; every write is left unset, so PostgreSQL refuses it.
#[no_mangle]
#[pg_guard]
pub unsafe extern "C-unwind" fn warren_snail_handler(
    _fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let mut routine = PgBox::<pg_sys::FdwRoutine>::alloc_node(pg_sys::NodeTag::T_FdwRoutine);
    routine.GetForeignRelSize = Some(scan::rel_size);
    routine.GetForeignPaths = Some(scan::paths);
    routine.GetForeignPlan = Some(scan::plan);
    routine.BeginForeignScan = Some(scan::begin);
    routine.IterateForeignScan = Some(scan::iterate);
    routine.ReScanForeignScan = Some(scan::rescan);
    routine.EndForeignScan = Some(scan::end);
    routine.ExplainForeignScan = Some(scan::explain);
    routine.IsForeignScanParallelSafe = Some(scan::parallel_safe);
    routine.ImportForeignSchema = Some(import::import);
    pg_sys::Datum::from(routine.into_pg())
}

#[no_mangle]
pub extern "C" fn pg_finfo_warren_snail_validator() -> &'static pg_sys::Pg_finfo_record {
    static V1: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
    &V1
}

/// Refuses an option the object it is given on does not take, one given twice, and a file named by
/// a role that may not read the server's files.
#[no_mangle]
#[pg_guard]
pub unsafe extern "C-unwind" fn warren_snail_validator(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let given = (*fcinfo).args.as_slice(2);
    options::validate(
        given[0].value,
        pg_sys::Oid::from_datum(given[1].value, false).unwrap(),
    );
    pg_sys::Datum::from(0)
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// The message a statement is refused with, run as `who` where one is named; none where it runs.
    pub(crate) fn refusal(sql: &str, who: Option<&str>) -> Option<String> {
        Spi::run(
            "CREATE OR REPLACE FUNCTION pg_temp.refusal(sql text, who text) RETURNS text \
             LANGUAGE plpgsql AS $f$ \
             BEGIN \
               IF who IS NOT NULL THEN EXECUTE format('SET LOCAL ROLE %I', who); END IF; \
               EXECUTE sql; \
               IF who IS NOT NULL THEN RESET ROLE; END IF; \
               RETURN NULL; \
             EXCEPTION WHEN OTHERS THEN RETURN SQLERRM; \
             END $f$",
        )
        .unwrap();
        Spi::get_one_with_args::<String>(
            "SELECT pg_temp.refusal($1, $2)",
            &[sql.into(), who.into()],
        )
        .unwrap()
    }

    #[pg_test]
    fn the_wrapper_is_there_with_its_handler_and_validator() {
        let (handler, validator) = Spi::get_two::<String, String>(
            "SELECT w.fdwhandler::regproc::text, w.fdwvalidator::regproc::text \
             FROM pg_foreign_data_wrapper w WHERE w.fdwname = 'warren_snail'",
        )
        .unwrap();
        assert_eq!(
            (handler.as_deref(), validator.as_deref()),
            (Some("warren_snail_handler"), Some("warren_snail_validator"))
        );
    }

    #[pg_test]
    fn a_server_takes_a_file_and_a_foreign_table_its_table() {
        Spi::run("CREATE SERVER s FOREIGN DATA WRAPPER warren_snail OPTIONS (file 'shop.snail')")
            .unwrap();
        Spi::run("CREATE FOREIGN TABLE t (a integer) SERVER s OPTIONS (\"table\" 'shop.orders')")
            .unwrap();
        Spi::run("ALTER SERVER s OPTIONS (SET file '/var/lib/elsewhere.snail')").unwrap();
    }

    /// Each option where it does not belong, given twice, or missing: refused, each with its reason.
    #[pg_test]
    fn an_option_where_it_does_not_belong_is_refused() {
        let refused = |sql: &str, why: &str| {
            let e = refusal(sql, None);
            assert!(
                e.as_deref().is_some_and(|e| e.contains(why)),
                "{sql}: {e:?}"
            );
        };
        refused(
            "CREATE SERVER x FOREIGN DATA WRAPPER warren_snail OPTIONS (path 'a.snail')",
            "\"path\" is not an option of a warren_snail server",
        );
        refused(
            "CREATE SERVER x FOREIGN DATA WRAPPER warren_snail",
            "a warren_snail server names its file",
        );
        refused(
            "CREATE SERVER x FOREIGN DATA WRAPPER warren_snail OPTIONS (file '')",
            "a warren_snail server names its file",
        );
        Spi::run("CREATE SERVER s FOREIGN DATA WRAPPER warren_snail OPTIONS (file 'a.snail')")
            .unwrap();
        refused(
            "CREATE FOREIGN TABLE t (a integer) SERVER s OPTIONS (file 'b.snail')",
            "\"file\" is not an option of a warren_snail foreign table",
        );
        refused(
            "CREATE FOREIGN TABLE t (a integer) SERVER s",
            "a warren_snail foreign table names its table",
        );
        refused(
            "CREATE USER MAPPING FOR CURRENT_USER SERVER s OPTIONS (file 'b.snail')",
            "\"file\" is not an option of a warren_snail user mapping",
        );
        refused(
            "ALTER FOREIGN DATA WRAPPER warren_snail OPTIONS (file 'b.snail')",
            "\"file\" is not an option of the warren_snail wrapper",
        );
        refused(
            "CREATE FOREIGN TABLE t (a integer OPTIONS (file 'c.snail')) SERVER s OPTIONS (\"table\" 's.t')",
            "\"file\" is not an option of a warren_snail foreign table's column",
        );
    }

    /// Naming the file a server reads takes a superuser, or a role with the privileges of
    /// pg_read_server_files, as PostgreSQL's own file reader does.
    #[pg_test]
    fn only_a_role_that_may_read_the_servers_files_names_one() {
        Spi::run("CREATE ROLE reader NOLOGIN").unwrap();
        Spi::run("GRANT USAGE ON FOREIGN DATA WRAPPER warren_snail TO reader").unwrap();
        let create = "CREATE SERVER r FOREIGN DATA WRAPPER warren_snail OPTIONS (file 'a.snail')";
        let e = refusal(create, Some("reader"));
        assert!(
            e.as_deref()
                .is_some_and(|e| e.contains("pg_read_server_files")),
            "{e:?}"
        );
        Spi::run("GRANT pg_read_server_files TO reader").unwrap();
        assert_eq!(refusal(create, Some("reader")), None);
        // The grant taken back, the server's owner may no longer change the file it names.
        Spi::run("REVOKE pg_read_server_files FROM reader").unwrap();
        let e = refusal(
            "ALTER SERVER r OPTIONS (SET file 'b.snail')",
            Some("reader"),
        );
        assert!(
            e.as_deref()
                .is_some_and(|e| e.contains("pg_read_server_files")),
            "{e:?}"
        );
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
