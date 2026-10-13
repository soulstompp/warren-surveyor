// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! A snail's column as a foreign table's column: which PostgreSQL type takes each of the snail's,
//! and each value the snail hands out as PostgreSQL's datum.

use arrow::array::{Array, ArrayRef, AsArray};
use arrow::datatypes::{
    DataType, Decimal128Type, Int16Type, Int32Type, Int64Type, TimeUnit, TimestampMicrosecondType,
};
use pgrx::pg_sys;
use pgrx::prelude::*;
use std::ffi::{c_char, c_int, CString};
use warren_snail::catalog::ColumnType;

/// The microseconds from 1970-01-01 UTC, where the snail counts an instant from, to 2000-01-01 UTC,
/// where PostgreSQL does.
const FROM_1970_TO_2000: i64 = 946_684_800_000_000;

/// The first instant PostgreSQL's timestamptz holds, 4714-11-24 00:00:00 UTC BC, and the first past
/// its last, 294277-01-01 00:00:00 UTC, each counted from 2000.
const FIRST_INSTANT: i64 = -211_813_488_000_000_000;
const PAST_LAST_INSTANT: i64 = 9_223_371_331_200_000_000;

/// The type modifier of a varchar of `n` characters, and of a numeric of `precision` and `scale`.
fn varchar_typmod(n: u32) -> i32 {
    n as i32 + pg_sys::VARHDRSZ as i32
}

fn numeric_typmod(precision: u8, scale: u8) -> i32 {
    ((i32::from(precision) << 16) | i32::from(scale)) + pg_sys::VARHDRSZ as i32
}

/// The PostgreSQL type a foreign table's column of the snail's type is declared as, with its type
/// modifier, as IMPORT FOREIGN SCHEMA declares it.
pub(crate) fn declared(ty: ColumnType) -> (pg_sys::Oid, i32) {
    match ty {
        ColumnType::Int2 => (pg_sys::INT2OID, -1),
        ColumnType::Int4 => (pg_sys::INT4OID, -1),
        ColumnType::Int8 => (pg_sys::INT8OID, -1),
        ColumnType::Timestamptz => (pg_sys::TIMESTAMPTZOID, -1),
        ColumnType::Text(None) => (pg_sys::TEXTOID, -1),
        ColumnType::Text(Some(n)) => (pg_sys::VARCHAROID, varchar_typmod(n)),
        ColumnType::Bytes => (pg_sys::BYTEAOID, -1),
        ColumnType::Numeric { precision, scale } => {
            (pg_sys::NUMERICOID, numeric_typmod(precision, scale))
        }
    }
}

/// Whether a foreign table's column of type `oid` and modifier `typmod` takes every value of the
/// snail's column: its own type, text for any text, a varchar or a numeric of no limit or a wider
/// one at the same scale.
pub(crate) fn takes(ty: ColumnType, oid: pg_sys::Oid, typmod: i32) -> bool {
    let (want, want_mod) = declared(ty);
    match ty {
        ColumnType::Text(n) => {
            oid == pg_sys::TEXTOID
                || (oid == pg_sys::VARCHAROID
                    && (typmod == -1 || n.is_some_and(|n| typmod >= varchar_typmod(n))))
        }
        ColumnType::Numeric { precision, scale } => {
            let held = typmod - pg_sys::VARHDRSZ as i32;
            oid == pg_sys::NUMERICOID
                && (typmod == -1
                    || ((held & 0xffff) == i32::from(scale)
                        && (held >> 16) >= i32::from(precision)))
        }
        _ => oid == want && typmod == want_mod,
    }
}

/// Row `i` of the snail's column `a` as PostgreSQL's datum, made in the current memory context;
/// none where it is NULL.
pub(crate) unsafe fn datum(a: &ArrayRef, i: usize) -> Option<pg_sys::Datum> {
    if a.is_null(i) {
        return None;
    }
    Some(match a.data_type() {
        DataType::Int16 => pg_sys::Datum::from(a.as_primitive::<Int16Type>().value(i)),
        DataType::Int32 => pg_sys::Datum::from(a.as_primitive::<Int32Type>().value(i)),
        DataType::Int64 => pg_sys::Datum::from(a.as_primitive::<Int64Type>().value(i)),
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            instant(a.as_primitive::<TimestampMicrosecondType>().value(i))
        }
        DataType::Utf8 => varlena(a.as_string::<i32>().value(i).as_bytes()),
        DataType::Binary => varlena(a.as_binary::<i32>().value(i)),
        DataType::Decimal128(_, scale) => numeric(
            a.as_primitive::<Decimal128Type>().value(i),
            (*scale).max(0) as u32,
        ),
        other => error!("the snail handed out a column of {other}, which no foreign table takes"),
    })
}

/// An instant counted from 1970 as a timestamptz, counted from 2000; the snail's largest and
/// smallest instants are PostgreSQL's infinity and its negative, which timestamptz holds as its own
/// largest and smallest.
fn instant(from_1970: i64) -> pg_sys::Datum {
    if from_1970 == i64::MAX || from_1970 == i64::MIN {
        return pg_sys::Datum::from(from_1970);
    }
    match from_1970.checked_sub(FROM_1970_TO_2000) {
        Some(t) if (FIRST_INSTANT..PAST_LAST_INSTANT).contains(&t) => pg_sys::Datum::from(t),
        _ => {
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_DATETIME_FIELD_OVERFLOW,
                format!(
                    "the snail holds an instant past timestamptz's range: {from_1970} µs from 1970"
                )
            );
        }
    }
}

/// Bytes as a varlena: text, varchar and bytea alike.
unsafe fn varlena(bytes: &[u8]) -> pg_sys::Datum {
    pg_sys::Datum::from(pg_sys::cstring_to_text_with_len(
        bytes.as_ptr() as *const c_char,
        bytes.len() as c_int,
    ))
}

unsafe fn numeric_in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    pg_sys::numeric_in(fcinfo)
}

/// A numeric of `scale` digits after the point, held as the integer of its smallest unit, read in
/// as PostgreSQL reads its text, so it keeps every digit of its scale.
unsafe fn numeric(units: i128, scale: u32) -> pg_sys::Datum {
    let text = CString::new(warren_snail::read::numeric_text(units, scale))
        .expect("a numeric's text holds no NUL");
    pgrx::direct_function_call_as_datum(
        numeric_in,
        &[
            Some(pg_sys::Datum::from(text.as_ptr())),
            Some(pg_sys::Datum::from(pg_sys::InvalidOid)),
            Some(pg_sys::Datum::from(-1i32)),
        ],
    )
    .expect("numeric_in returns a numeric")
}
