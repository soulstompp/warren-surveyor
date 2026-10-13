// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The options the wrapper's objects take: a server the file it reads, a foreign table the snail's
//! table it stands for; nothing else takes one.

use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::PgList;
use std::ffi::CStr;

/// What an object of the catalog is called in a refusal, and the options it takes, every one of them
/// required.
fn takes(catalog: pg_sys::Oid) -> (&'static str, &'static [&'static str]) {
    if catalog == pg_sys::ForeignServerRelationId {
        ("a warren_snail server", &["file"])
    } else if catalog == pg_sys::ForeignTableRelationId {
        ("a warren_snail foreign table", &["table"])
    } else if catalog == pg_sys::UserMappingRelationId {
        ("a warren_snail user mapping", &[])
    } else if catalog == pg_sys::ForeignDataWrapperRelationId {
        ("the warren_snail wrapper", &[])
    } else {
        ("a warren_snail foreign table's column", &[])
    }
}

/// Refuses an option the object does not take, one given twice, one left out or empty, and a file
/// named by a role without the privileges of pg_read_server_files, as PostgreSQL's own file reader
/// refuses it. The whole list is given on every CREATE and ALTER, so a server's owner whose grant was
/// taken back cannot change its file either.
pub(crate) unsafe fn validate(options: pg_sys::Datum, catalog: pg_sys::Oid) {
    let (object, names) = takes(catalog);
    let given = PgList::<pg_sys::DefElem>::from_pg(pg_sys::untransformRelOptions(options));
    let mut seen: Vec<String> = Vec::new();
    for def in given.iter_ptr() {
        let name = CStr::from_ptr((*def).defname)
            .to_string_lossy()
            .into_owned();
        if !names.contains(&name.as_str()) {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
                format!("\"{name}\" is not an option of {object}")
            );
        }
        if seen.contains(&name) {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_SYNTAX_ERROR,
                format!("\"{name}\" is given twice")
            );
        }
        let value = CStr::from_ptr(pg_sys::defGetString(def)).to_string_lossy();
        if value.is_empty() {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_FDW_OPTION_NAME_NOT_FOUND,
                format!("{object} names its {name}")
            );
        }
        if name == "file"
            && !pg_sys::has_privs_of_role(
                pg_sys::GetUserId(),
                pg_sys::Oid::from(pg_sys::ROLE_PG_READ_SERVER_FILES),
            )
        {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
                "only a superuser or a role with the privileges of pg_read_server_files may name the file a warren_snail server reads"
            );
        }
        seen.push(name);
    }
    for name in names {
        if !seen.iter().any(|s| s == name) {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_FDW_OPTION_NAME_NOT_FOUND,
                format!("{object} names its {name}")
            );
        }
    }
}
