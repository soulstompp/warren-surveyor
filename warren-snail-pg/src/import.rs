// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! A snail's schema imported as foreign tables, each declared with the snail's own types; and the
//! functions its DDL declares, which an import cannot make.

use crate::{compat, datum, scan};
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::PgList;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::null_mut;
use warren_snail::catalog::Table;

unsafe fn text(s: *const c_char) -> String {
    CStr::from_ptr(s).to_string_lossy().into_owned()
}

fn identifier(name: &str) -> String {
    let name = CString::new(name).expect("a name holds no NUL");
    unsafe { text(pg_sys::quote_identifier(name.as_ptr())) }
}

fn literal(value: &str) -> String {
    let value = CString::new(value).expect("a name holds no NUL");
    unsafe { text(pg_sys::quote_literal_cstr(value.as_ptr())) }
}

/// A snail table's schema and its own name, either side of the last dot; a name with none lies in
/// `public`, as PostgreSQL places a table named alone.
fn parts(name: &str) -> (&str, &str) {
    name.rsplit_once('.').unwrap_or(("public", name))
}

/// The foreign table a snail table imports as: its columns in the snail's order, each of the type
/// the snail holds, text compared by its bytes, NOT NULL where the snail's is.
fn create(table: &Table, local: &str, server: &str) -> String {
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let (oid, typmod) = datum::declared(c.ty);
            let ty = unsafe { text(pg_sys::format_type_with_typemod(oid, typmod)) };
            let collate = if c.ty.is_text() { " COLLATE \"C\"" } else { "" };
            let not_null = if c.not_null { " NOT NULL" } else { "" };
            format!("{} {ty}{collate}{not_null}", identifier(&c.name))
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE FOREIGN TABLE {}.{} ({columns}) SERVER {} OPTIONS (\"table\" {})",
        identifier(local),
        identifier(parts(&table.name).1),
        identifier(server),
        literal(&table.name)
    )
}

/// One foreign table for each table the snail holds in the schema imported; PostgreSQL keeps those
/// LIMIT TO and EXCEPT name.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn import(
    stmt: *mut pg_sys::ImportForeignSchemaStmt,
    server: pg_sys::Oid,
) -> *mut pg_sys::List {
    if let Some(d) = PgList::<pg_sys::DefElem>::from_pg((*stmt).options)
        .iter_ptr()
        .next()
    {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
            format!(
                "\"{}\" is not an option of IMPORT FOREIGN SCHEMA from a warren_snail server",
                text((*d).defname)
            )
        );
    }
    let file = scan::server_file(pg_sys::GetForeignServer(server));
    let snail = scan::open(&file);
    let remote = text((*stmt).remote_schema);
    let tables: Vec<&Table> = snail
        .catalog
        .tables
        .iter()
        .filter(|t| parts(&t.name).0 == remote)
        .collect();
    if tables.is_empty() {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_FDW_SCHEMA_NOT_FOUND,
            format!("the snail {file} holds no table in schema {remote}")
        );
    }
    let (local, server_name) = (text((*stmt).local_schema), text((*stmt).server_name));
    let mut out: *mut pg_sys::List = null_mut();
    for t in tables {
        let sql = CString::new(create(t, &local, &server_name)).expect("a name holds no NUL");
        out = pg_sys::lappend(out, pg_sys::pstrdup(sql.as_ptr()) as *mut c_void);
    }
    out
}

/// The CREATE FUNCTION statements of the functions the snail a server names declares in `schema`, as
/// its DDL writes them, since IMPORT FOREIGN SCHEMA makes foreign tables alone. It takes USAGE on
/// the server, as IMPORT does.
#[pg_extern]
fn warren_snail_functions(server: &str, schema: &str) -> SetOfIterator<'static, String> {
    let statements = unsafe {
        let name = CString::new(server).expect("a name holds no NUL");
        let s = pg_sys::GetForeignServerByName(name.as_ptr(), false);
        let usage = compat::server_usage((*s).serverid, pg_sys::GetUserId());
        if usage != pg_sys::AclResult::ACLCHECK_OK {
            pg_sys::aclcheck_error(
                usage,
                pg_sys::ObjectType::OBJECT_FOREIGN_SERVER,
                name.as_ptr(),
            );
        }
        let wrapper = pg_sys::get_foreign_data_wrapper_oid(c"warren_snail".as_ptr(), false);
        if (*s).fdwid != wrapper {
            scan::refuse(format!("{server} is not a warren_snail server"));
        }
        let file = scan::server_file(s);
        let snail = scan::open(&file);
        let written = snail
            .catalog
            .function_statements()
            .unwrap_or_else(|e| scan::refuse(format!("the snail {file}: {e}")));
        snail
            .catalog
            .functions
            .iter()
            .zip(written)
            .filter(|(f, _)| parts(&f.name).0 == schema)
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
    };
    SetOfIterator::new(statements)
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use crate::scan::tests::{heap, kinds_batch, one, texts, written, DDL};
    use crate::tests::refusal;
    use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use pgrx::prelude::*;
    use std::sync::Arc;

    const CLOCK: &str = "CREATE FUNCTION shop.clock(timestamptz) RETURNS timestamp\n    \
                         LANGUAGE sql IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;";
    const OTHER_CLOCK: &str = "CREATE FUNCTION other.clock(timestamptz) RETURNS timestamp \
                               LANGUAGE sql IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;";

    /// A snail of two schemas, `shop` holding the kinds and the quoted `Notes`, `other` one table, each
    /// schema its own clock; and a server `s` naming it.
    fn two_schemas(name: &str) -> std::path::PathBuf {
        heap(300);
        let ddl = format!(
            "{CLOCK}\n{OTHER_CLOCK}\n{DDL}\n\
             CREATE TABLE shop.\"Notes\" (id bigint NOT NULL, body text NOT NULL, PRIMARY KEY (id));\n\
             CREATE TABLE other.elsewhere (id bigint NOT NULL, PRIMARY KEY (id));"
        );
        let notes = RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef),
            ("body", Arc::new(StringArray::from(vec!["a", "b", "c"]))),
        ])
        .unwrap();
        let elsewhere =
            RecordBatch::try_from_iter([("id", Arc::new(Int64Array::from(vec![1])) as ArrayRef)])
                .unwrap();
        let path = written(
            name,
            &ddl,
            &[
                ("shop.kinds", kinds_batch()),
                ("shop.Notes", notes),
                ("other.elsewhere", elsewhere),
            ],
        );
        Spi::run(&format!(
            "CREATE SERVER s FOREIGN DATA WRAPPER warren_snail OPTIONS (file '{}')",
            path.display()
        ))
        .unwrap();
        path
    }

    /// Each imported table's columns as PostgreSQL declares them, and its option.
    fn declared(table: &str) -> (String, String) {
        let columns = one(&format!(
            "SELECT string_agg(a.attname || ' ' || format_type(a.atttypid, a.atttypmod) \
               || coalesce(' COLLATE ' || co.collname, '') \
               || CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END, ', ' ORDER BY a.attnum) \
             FROM pg_attribute a LEFT JOIN pg_collation co ON co.oid = a.attcollation \
             WHERE a.attrelid = '{table}'::regclass AND a.attnum > 0"
        ));
        let options = one(&format!(
            "SELECT array_to_string(ftoptions, ',') FROM pg_foreign_table \
             WHERE ftrelid = '{table}'::regclass"
        ));
        (columns, options)
    }

    #[pg_test]
    fn a_schema_imports_as_foreign_tables_of_the_snails_own_types() {
        two_schemas("imported");
        Spi::run("CREATE SCHEMA imported").unwrap();
        Spi::run("IMPORT FOREIGN SCHEMA shop FROM SERVER s INTO imported").unwrap();
        assert_eq!(
            texts(
                "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n \
                 ON n.oid = c.relnamespace WHERE n.nspname = 'imported' ORDER BY 1"
            ),
            ["Notes", "kinds"]
        );
        assert_eq!(
            declared("imported.kinds"),
            (
                "id bigint NOT NULL, small smallint, mid integer, at timestamp with time zone, \
                 name text COLLATE C, code character varying(8) COLLATE C, raw bytea, \
                 price numeric(12,3)"
                    .to_string(),
                "table=shop.kinds".to_string()
            )
        );
        assert_eq!(
            declared("imported.\"Notes\""),
            (
                "id bigint NOT NULL, body text COLLATE C NOT NULL".to_string(),
                "table=shop.Notes".to_string()
            )
        );
        assert_eq!(
            texts("SELECT k::text FROM imported.kinds k ORDER BY id"),
            texts("SELECT h::text FROM kinds_heap h ORDER BY id")
        );
        assert_eq!(
            one("SELECT string_agg(body, ',' ORDER BY id) FROM imported.\"Notes\""),
            "a,b,c"
        );
    }

    #[pg_test]
    fn limit_to_and_except_name_the_tables_imported() {
        two_schemas("limited");
        for (schema, list) in [("a", "LIMIT TO (\"Notes\")"), ("b", "EXCEPT (\"Notes\")")] {
            Spi::run(&format!("CREATE SCHEMA {schema}")).unwrap();
            Spi::run(&format!(
                "IMPORT FOREIGN SCHEMA shop {list} FROM SERVER s INTO {schema}"
            ))
            .unwrap();
        }
        let imported = |schema: &str| {
            texts(&format!(
                "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n \
                 ON n.oid = c.relnamespace WHERE n.nspname = '{schema}' ORDER BY 1"
            ))
        };
        assert_eq!(imported("a"), ["Notes"]);
        assert_eq!(imported("b"), ["kinds"]);
    }

    #[pg_test]
    fn a_schema_the_snail_lacks_and_an_option_are_refused() {
        let path = two_schemas("refused");
        Spi::run("CREATE SCHEMA c").unwrap();
        for (sql, why) in [
            (
                "IMPORT FOREIGN SCHEMA nowhere FROM SERVER s INTO c".to_string(),
                format!(
                    "the snail {} holds no table in schema nowhere",
                    path.display()
                ),
            ),
            (
                "IMPORT FOREIGN SCHEMA shop FROM SERVER s INTO c OPTIONS (x 'y')".to_string(),
                "\"x\" is not an option of IMPORT FOREIGN SCHEMA from a warren_snail server"
                    .to_string(),
            ),
        ] {
            let e = refusal(&sql, None);
            assert!(
                e.as_deref().is_some_and(|e| e.contains(&why)),
                "{sql}: {e:?}"
            );
        }
    }

    /// The functions come back as the snail's DDL writes them, each schema its own, and run as
    /// PostgreSQL's; naming the file's functions takes USAGE on its server.
    #[pg_test]
    fn the_functions_come_back_as_the_snail_writes_them() {
        two_schemas("functions");
        assert_eq!(texts("SELECT warren_snail_functions('s', 'shop')"), [CLOCK]);
        assert_eq!(
            texts("SELECT warren_snail_functions('s', 'other')"),
            [OTHER_CLOCK]
        );
        assert_eq!(
            texts("SELECT warren_snail_functions('s', 'nowhere')"),
            Vec::<String>::new()
        );
        Spi::run("CREATE SCHEMA shop").unwrap();
        Spi::run(CLOCK).unwrap();
        assert_eq!(
            one("SELECT shop.clock('2026-10-13 12:00:00+01')::text"),
            "2026-10-13 11:00:00"
        );
        Spi::run("CREATE ROLE stranger NOLOGIN").unwrap();
        let ask = "SELECT warren_snail_functions('s', 'shop')";
        let e = refusal(ask, Some("stranger"));
        assert!(
            e.as_deref()
                .is_some_and(|e| e.contains("permission denied for foreign server s")),
            "{e:?}"
        );
        Spi::run("GRANT USAGE ON FOREIGN SERVER s TO stranger").unwrap();
        assert_eq!(refusal(ask, Some("stranger")), None);
        // A server of another wrapper, even one naming a file, names no snail.
        Spi::run("CREATE FOREIGN DATA WRAPPER other").unwrap();
        Spi::run("CREATE SERVER o FOREIGN DATA WRAPPER other OPTIONS (file 'functions.snail')")
            .unwrap();
        let e = refusal("SELECT warren_snail_functions('o', 'shop')", None);
        assert!(
            e.as_deref()
                .is_some_and(|e| e.contains("o is not a warren_snail server")),
            "{e:?}"
        );
    }
}
