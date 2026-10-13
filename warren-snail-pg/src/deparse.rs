// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The conditions a scan hands the snail: a column, its instant on the UTC clock or that instant's
//! month, compared with a constant by `<`, `<=`, `=`, `>=` or `>`, written as the snail reads them.
//! Every other condition is PostgreSQL's to filter.

use pgrx::pg_sys;
use std::ffi::{CStr, CString};
use std::sync::OnceLock;
use warren_snail::catalog::{ColumnType, Table};

/// PostgreSQL's POSIX collation, which compares text by its bytes as C does.
const POSIX_COLLATION_OID: pg_sys::Oid = pg_sys::Oid::from_u32(951);

/// The microseconds from 1970-01-01 to 2000-01-01, UTC.
const FROM_1970_TO_2000: i64 = 946_684_800_000_000;

/// The functions the clock and its month are read through, once the clock is inlined:
/// `timezone(text, timestamptz)`, `extract(text, timestamp)` and `int2(numeric)`.
struct Functions {
    timezone: pg_sys::Oid,
    extract: pg_sys::Oid,
    int2: pg_sys::Oid,
}

unsafe fn regprocedure_in(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    pg_sys::regprocedurein(fcinfo)
}

fn functions() -> &'static Functions {
    static FUNCTIONS: OnceLock<Functions> = OnceLock::new();
    FUNCTIONS.get_or_init(|| {
        let oid = |signature: &str| {
            let signature = CString::new(signature).expect("a signature holds no NUL");
            unsafe {
                pgrx::direct_function_call::<pg_sys::Oid>(
                    regprocedure_in,
                    &[Some(pg_sys::Datum::from(signature.as_ptr()))],
                )
                .expect("a builtin function")
            }
        };
        Functions {
            timezone: oid("pg_catalog.timezone(text, timestamp with time zone)"),
            extract: oid("pg_catalog.extract(text, timestamp without time zone)"),
            int2: oid("pg_catalog.int2(numeric)"),
        }
    })
}

/// What a condition compares: a column, its instant on the UTC clock, or that instant's month.
#[derive(Clone, Copy)]
enum Level {
    Column(i16),
    Clock(i16),
    Month(i16),
}

unsafe fn text_of(c: *mut pg_sys::Const) -> String {
    CStr::from_ptr(pg_sys::text_to_cstring(
        (*c).constvalue.cast_mut_ptr::<pg_sys::text>(),
    ))
    .to_string_lossy()
    .into_owned()
}

/// The constant `e` is, unless it is NULL.
unsafe fn constant(e: *mut pg_sys::Node) -> Option<*mut pg_sys::Const> {
    let c = e as *mut pg_sys::Const;
    ((*e).type_ == pg_sys::NodeTag::T_Const && !(*c).constisnull).then_some(c)
}

/// The column of the relation `e` reads, through a relabelling.
unsafe fn column(e: *mut pg_sys::Node, relid: pg_sys::Index) -> Option<i16> {
    let mut e = e;
    while (*e).type_ == pg_sys::NodeTag::T_RelabelType {
        e = (*(e as *mut pg_sys::RelabelType)).arg as *mut pg_sys::Node;
    }
    if (*e).type_ != pg_sys::NodeTag::T_Var {
        return None;
    }
    let v = e as *mut pg_sys::Var;
    ((*v).varno as pg_sys::Index == relid && (*v).varlevelsup == 0 && (*v).varattno > 0)
        .then_some((*v).varattno)
}

/// The arguments of `e`, where it calls `funcid`.
unsafe fn call(e: *mut pg_sys::Node, funcid: pg_sys::Oid) -> Option<Vec<*mut pg_sys::Node>> {
    if (*e).type_ != pg_sys::NodeTag::T_FuncExpr {
        return None;
    }
    let f = e as *mut pg_sys::FuncExpr;
    ((*f).funcid == funcid).then(|| {
        pgrx::PgList::<pg_sys::Node>::from_pg((*f).args)
            .iter_ptr()
            .collect()
    })
}

/// The level `e` is: a column, `timezone('UTC', column)`, or `int2(extract('month', timezone('UTC',
/// column)))`.
unsafe fn level(e: *mut pg_sys::Node, relid: pg_sys::Index) -> Option<Level> {
    let f = functions();
    let clock = |e: *mut pg_sys::Node| -> Option<i16> {
        match call(e, f.timezone)?.as_slice() {
            [zone, at] => {
                let zone = constant(*zone)?;
                (text_of(zone) == "UTC").then_some(())?;
                column(*at, relid)
            }
            _ => None,
        }
    };
    if let Some(n) = column(e, relid) {
        return Some(Level::Column(n));
    }
    if let Some(n) = clock(e) {
        return Some(Level::Clock(n));
    }
    match call(e, f.int2)?.as_slice() {
        [inner] => match call(*inner, f.extract)?.as_slice() {
            [field, at] => {
                (text_of(constant(*field)?).eq_ignore_ascii_case("month")).then_some(())?;
                clock(*at).map(Level::Month)
            }
            _ => None,
        },
        _ => None,
    }
}

/// An integer constant of any width; none for int64's least, which the snail does not read as a
/// literal.
unsafe fn integer(c: *mut pg_sys::Const) -> Option<i64> {
    let d = (*c).constvalue;
    let v = match (*c).consttype {
        pg_sys::INT2OID => i64::from(d.value() as i16),
        pg_sys::INT4OID => i64::from(d.value() as i32),
        pg_sys::INT8OID => d.value() as i64,
        _ => return None,
    };
    (v != i64::MIN).then_some(v)
}

/// Microseconds from 1970 as the civil date and time of day, UTC; none outside the years 1 to 9999.
fn civil(from_1970: i64) -> Option<String> {
    let days = from_1970.div_euclid(86_400_000_000);
    let of_day = from_1970.rem_euclid(86_400_000_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let (s, us) = (of_day / 1_000_000, of_day % 1_000_000);
    Some(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{us:06}",
        s / 3600,
        s / 60 % 60,
        s % 60
    ))
}

unsafe fn numeric_out(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    pg_sys::numeric_out(fcinfo)
}

/// A numeric constant as PostgreSQL writes it, where the snail reads it against a column of
/// `scale`: no more digits after the point than the scale, and no more than 18 in all.
unsafe fn numeric(c: *mut pg_sys::Const, scale: u8) -> Option<String> {
    if (*c).consttype != pg_sys::NUMERICOID {
        return None;
    }
    let text = pgrx::direct_function_call::<&CStr>(numeric_out, &[Some((*c).constvalue)])?
        .to_string_lossy()
        .into_owned();
    let digits = text.trim_start_matches('-');
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let fits = whole.chars().all(|ch| ch.is_ascii_digit())
        && frac.len() <= scale as usize
        && whole.trim_start_matches('0').len() + scale as usize <= 18;
    fits.then_some(text)
}

fn quoted_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The condition `clause` as the snail reads it, of the foreign table whose columns are `columns`
/// over the snail's `table`; none where PostgreSQL keeps it.
pub(crate) unsafe fn condition(
    clause: *mut pg_sys::Expr,
    relid: pg_sys::Index,
    columns: &[(i16, String, pg_sys::Oid, i32)],
    table: &Table,
) -> Option<String> {
    let clause = clause as *mut pg_sys::Node;
    if (*clause).type_ != pg_sys::NodeTag::T_OpExpr {
        return None;
    }
    let op = clause as *mut pg_sys::OpExpr;
    let args: Vec<*mut pg_sys::Node> = pgrx::PgList::<pg_sys::Node>::from_pg((*op).args)
        .iter_ptr()
        .collect();
    let [left, right] = args.as_slice() else {
        return None;
    };
    let (side, c, flipped) = match (level(*left, relid), constant(*right)) {
        (Some(l), Some(c)) => (*left, (l, c), false),
        _ => match (level(*right, relid), constant(*left)) {
            (Some(l), Some(c)) => (*right, (l, c), true),
            _ => return None,
        },
    };
    let (level, c) = c;
    let family = (*pg_sys::lookup_type_cache(
        pg_sys::exprType(side),
        pg_sys::TYPECACHE_BTREE_OPFAMILY as i32,
    ))
    .btree_opf;
    let strategy = match pg_sys::get_op_opfamily_strategy((*op).opno, family) {
        s @ 1..=5 if flipped => 6 - s,
        s @ 1..=5 => s,
        _ => return None,
    };
    let operator = ["<", "<=", "=", ">=", ">"][strategy as usize - 1];
    let attno = match level {
        Level::Column(n) | Level::Clock(n) | Level::Month(n) => n,
    };
    let (_, name, _, _) = columns.iter().find(|col| col.0 == attno)?;
    let ty = table.columns.iter().find(|col| &col.name == name)?.ty;
    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let (lhs, literal) = match level {
        Level::Month(_) => (
            format!("extract(month FROM {quoted} AT TIME ZONE 'UTC')::smallint"),
            integer(c)?.to_string(),
        ),
        Level::Clock(_) => {
            ((*c).consttype == pg_sys::TIMESTAMPOID).then_some(())?;
            let at = (*c).constvalue.value() as i64;
            (
                format!("{quoted} AT TIME ZONE 'UTC'"),
                quoted_text(&civil(at.checked_add(FROM_1970_TO_2000)?)?),
            )
        }
        Level::Column(_) => (
            quoted,
            match ty {
                ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => integer(c)?.to_string(),
                ColumnType::Timestamptz => {
                    ((*c).consttype == pg_sys::TIMESTAMPTZOID).then_some(())?;
                    let at = (*c).constvalue.value() as i64;
                    quoted_text(&format!(
                        "{}+00",
                        civil(at.checked_add(FROM_1970_TO_2000)?)?
                    ))
                }
                ColumnType::Text(_) => {
                    let ordered =
                        [pg_sys::C_COLLATION_OID, POSIX_COLLATION_OID].contains(&(*op).inputcollid);
                    let equal = strategy == 3
                        && ((*op).inputcollid == pg_sys::InvalidOid
                            || pg_sys::get_collation_isdeterministic((*op).inputcollid));
                    (ordered || equal).then_some(())?;
                    [pg_sys::TEXTOID, pg_sys::VARCHAROID]
                        .contains(&(*c).consttype)
                        .then_some(())?;
                    quoted_text(&text_of(c))
                }
                ColumnType::Numeric { scale, .. } => numeric(c, scale)?,
                ColumnType::Bytes => return None,
            },
        ),
    };
    Some(format!("{lhs} {operator} {literal}"))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
pub(crate) mod tests {
    use super::civil;
    use crate::scan::tests::{one, texts, written};
    use arrow::array::{
        ArrayRef, BinaryArray, Decimal128Array, Int16Array, Int64Array, RecordBatch, StringArray,
        TimestampMicrosecondArray,
    };
    use pgrx::prelude::*;
    use std::sync::Arc;
    use warren_snail::read::Snail;

    #[pg_test]
    fn an_instant_is_written_as_its_civil_date_and_time_within_years_1_to_9999() {
        assert_eq!(civil(0).as_deref(), Some("1970-01-01 00:00:00.000000"));
        assert_eq!(civil(-1).as_deref(), Some("1969-12-31 23:59:59.999999"));
        assert_eq!(
            civil(951_782_400_000_001).as_deref(),
            Some("2000-02-29 00:00:00.000001")
        );
        assert_eq!(
            civil(-62_135_596_800_000_000).as_deref(),
            Some("0001-01-01 00:00:00.000000")
        );
        assert_eq!(civil(-62_135_596_800_000_001), None);
        assert_eq!(
            civil(253_402_300_799_999_999).as_deref(),
            Some("9999-12-31 23:59:59.999999")
        );
        assert_eq!(civil(253_402_300_800_000_000), None);
    }

    const DDL: &str = "CREATE FUNCTION shop.clock(timestamptz) RETURNS timestamp LANGUAGE sql \
                       IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;\n\
                       CREATE TABLE shop.events (id bigint NOT NULL, at timestamptz NOT NULL, \
                       kind text NOT NULL, tag varchar(8) NOT NULL, amount numeric(10,2) NOT NULL, \
                       small smallint NOT NULL, raw bytea NOT NULL, note text, PRIMARY KEY (id));\n\
                       CREATE INDEX events_month ON shop.events \
                       ((extract(month FROM shop.clock(at))::smallint), at);";

    /// 20,000 events over three years, in a heap of PostgreSQL's and in a snail, the snail's table
    /// imported into `mounted` and its clock made in `shop`.
    pub(crate) fn events() -> std::path::PathBuf {
        Spi::run("SET LOCAL TimeZone = 'UTC'").unwrap();
        Spi::run(
            "CREATE TABLE events_heap (id bigint NOT NULL PRIMARY KEY, at timestamptz NOT NULL, \
             kind text COLLATE \"C\" NOT NULL, tag varchar(8) COLLATE \"C\" NOT NULL, \
             amount numeric(10,2) NOT NULL, small smallint NOT NULL, raw bytea NOT NULL, \
             note text COLLATE \"C\")",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO events_heap SELECT i, \
               timestamptz '2021-01-01 00:00:00+00' + (i * 4733 % 94608000) * interval '1 second' \
                 + (i % 1000) * interval '1 microsecond', \
               (ARRAY['a\\b', 'it''s', 'Zürich', 'Évora', 'AWS hourly', 'Z'])[1 + i % 6], \
               'x' || (i % 97)::text, \
               (i * 7919 % 200000 - 100000)::numeric / 100, \
               (i % 200 - 100)::smallint, \
               decode(lpad(to_hex(i % 256), 2, '0'), 'hex'), \
               CASE WHEN i % 10 = 0 THEN NULL ELSE 'n' || i END \
             FROM generate_series(1, 20000) i",
        )
        .unwrap();
        let (mut id, mut at, mut kind, mut tag) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut amount, mut small, mut raw, mut note) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        Spi::connect(|client| {
            let sql = "SELECT id, extract(epoch FROM date_trunc('second', at))::bigint * 1000000 \
                       + extract(microseconds FROM at)::bigint % 1000000, kind, tag, \
                       replace(amount::text, '.', '')::bigint, small, raw, note FROM events_heap";
            for row in client.select(sql, None, &[])? {
                id.push(row.get::<i64>(1)?.unwrap());
                at.push(row.get::<i64>(2)?.unwrap());
                kind.push(row.get::<String>(3)?.unwrap());
                tag.push(row.get::<String>(4)?.unwrap());
                amount.push(i128::from(row.get::<i64>(5)?.unwrap()));
                small.push(row.get::<i16>(6)?.unwrap());
                raw.push(row.get::<Vec<u8>>(7)?.unwrap());
                note.push(row.get::<String>(8)?);
            }
            Ok::<_, pgrx::spi::SpiError>(())
        })
        .unwrap();
        let batch = RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(id)) as ArrayRef),
            (
                "at",
                Arc::new(TimestampMicrosecondArray::from(at).with_timezone("UTC")),
            ),
            ("kind", Arc::new(StringArray::from(kind))),
            ("tag", Arc::new(StringArray::from(tag))),
            (
                "amount",
                Arc::new(
                    Decimal128Array::from(amount)
                        .with_precision_and_scale(10, 2)
                        .unwrap(),
                ),
            ),
            ("small", Arc::new(Int16Array::from(small))),
            (
                "raw",
                Arc::new(BinaryArray::from_iter_values(
                    raw.iter().map(|r| r.as_slice()),
                )),
            ),
            ("note", Arc::new(StringArray::from(note))),
        ])
        .unwrap();
        let path = written("events", DDL, &[("shop.events", batch)]);
        Spi::run(&format!(
            "CREATE SERVER s FOREIGN DATA WRAPPER warren_snail OPTIONS (file '{}')",
            path.display()
        ))
        .unwrap();
        Spi::run("CREATE SCHEMA mounted").unwrap();
        Spi::run("IMPORT FOREIGN SCHEMA shop FROM SERVER s INTO mounted").unwrap();
        Spi::run("CREATE SCHEMA shop").unwrap();
        for f in texts("SELECT warren_snail_functions('s', 'shop')") {
            Spi::run(&f).unwrap();
        }
        path
    }

    /// The snail's question for `condition`, and whether PostgreSQL filters any of it.
    fn planned(condition: &str) -> (String, bool) {
        let plan = texts(&format!(
            "EXPLAIN (COSTS OFF) SELECT id FROM mounted.events WHERE {condition}"
        ));
        let question = plan
            .iter()
            .find_map(|l| l.trim_start().strip_prefix("Snail Query: "))
            .unwrap_or_else(|| panic!("{plan:#?}"))
            .to_string();
        let filtered = plan.iter().any(|l| l.trim_start().starts_with("Filter: "));
        (question, filtered)
    }

    fn same_rows(condition: &str) {
        let ids = |table: &str| {
            one(&format!(
                "SELECT coalesce(string_agg(id::text, ',' ORDER BY id), '') FROM {table} \
                 WHERE {condition}"
            ))
        };
        assert_eq!(ids("mounted.events"), ids("events_heap"), "{condition}");
    }

    /// Each condition the snail reads goes into its question, and none of it is filtered above;
    /// each it does not stays PostgreSQL's; either way the rows are the heap's.
    #[pg_test]
    fn the_conditions_the_snail_reads_are_handed_to_it_and_the_rest_filtered() {
        events();
        let pushed = [
            ("id = 700", "\"id\" = 700"),
            ("id < 50", "\"id\" < 50"),
            ("50 > id", "\"id\" < 50"),
            ("small >= -3", "\"small\" >= -3"),
            (
                "at >= '2022-06-01 00:00:00+00' AND at < '2022-07-01 00:00:00+00'",
                "\"at\" >= '2022-06-01 00:00:00.000000+00' AND \"at\" < '2022-07-01 00:00:00.000000+00'",
            ),
            ("kind = 'a\\b'", "\"kind\" = 'a\\b'"),
            ("kind = 'it''s'", "\"kind\" = 'it''s'"),
            ("kind >= 'Z'", "\"kind\" >= 'Z'"),
            ("tag = 'x1'", "\"tag\" = 'x1'"),
            ("kind = 'Z' COLLATE \"default\"", "\"kind\" = 'Z'"),
            ("amount < 12.34", "\"amount\" < 12.34"),
            (
                "shop.clock(at) >= '2023-12-01'",
                "\"at\" AT TIME ZONE 'UTC' >= '2023-12-01 00:00:00.000000'",
            ),
            (
                "extract(month FROM shop.clock(at))::smallint = 12",
                "extract(month FROM \"at\" AT TIME ZONE 'UTC')::smallint = 12",
            ),
        ];
        for (condition, read) in pushed {
            let (question, filtered) = planned(condition);
            assert!(
                question.ends_with(&format!(" WHERE {read}")) && !filtered,
                "{condition}: {question}, filtered {filtered}"
            );
            same_rows(condition);
        }
        let kept = [
            "id <> 5",
            "id = 1 OR id = 2",
            "id IN (1, 2, 3)",
            "note IS NULL",
            "kind LIKE 'Z%'",
            "raw = '\\x01'",
            "kind >= 'Z' COLLATE \"default\"",
            "amount < 12.345",
            "id > -9223372036854775808",
            "at < now()",
            "at < '10000-01-01 00:00:00+00'",
            "extract(month FROM at)::smallint = 12",
            "extract(day FROM shop.clock(at))::smallint = 12",
            "at AT TIME ZONE 'Europe/Lisbon' >= '2023-12-01'",
            "at < 'infinity'",
            "amount < 12345678901234567.0",
            "at < date '2022-01-01'",
            "shop.clock(at) >= date '2023-12-01'",
        ];
        for condition in kept {
            let (question, filtered) = planned(condition);
            assert!(
                !question.contains(" WHERE ") && filtered,
                "{condition}: {question}, filtered {filtered}"
            );
            same_rows(condition);
        }
        // A condition on no column is decided once, above the scan, and filters no row of it.
        let (question, filtered) = planned("now() > '2000-01-01 00:00:00+00'");
        assert!(
            !question.contains(" WHERE ") && !filtered,
            "{question}, filtered {filtered}"
        );
        // A parameter stays PostgreSQL's.
        Spi::run("SET LOCAL plan_cache_mode = force_generic_plan").unwrap();
        Spi::run("PREPARE p(bigint) AS SELECT id FROM mounted.events WHERE id = $1").unwrap();
        let plan = texts("EXPLAIN (COSTS OFF) EXECUTE p(5)");
        assert!(
            plan.iter().any(|l| l.trim_start() == "Filter: (id = $1)"),
            "{plan:#?}"
        );
    }

    /// December's count is answered from the counts: the snail reads no column page.
    #[pg_test]
    fn december_s_count_reads_no_column_page() {
        events();
        let december = "extract(month FROM shop.clock(at))::smallint = 12";
        assert_eq!(
            one(&format!(
                "SELECT count(*)::text FROM mounted.events WHERE {december}"
            )),
            one(&format!(
                "SELECT count(*)::text FROM events_heap WHERE {december}"
            ))
        );
        let plan = texts(&format!(
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) \
             SELECT count(*) FROM mounted.events WHERE {december}"
        ));
        let shown = |label: &str| {
            plan.iter()
                .find_map(|l| l.trim_start().strip_prefix(label))
                .unwrap_or_else(|| panic!("{label}: {plan:#?}"))
                .to_string()
        };
        assert_eq!(
            shown("Snail Query: "),
            "SELECT count(*) FROM \"shop\".\"events\" \
             WHERE extract(month FROM \"at\" AT TIME ZONE 'UTC')::smallint = 12"
        );
        assert_eq!(shown("Snail Column Pages Read: "), "0");
        assert!(
            plan.iter().all(|l| !l.trim_start().starts_with("Filter: ")),
            "{plan:#?}"
        );
    }

    /// The planner is told the most rows the snail's counts allow for the conditions it reads.
    #[pg_test]
    fn the_planner_is_told_the_rows_the_counts_allow() {
        let path = events();
        let question = "SELECT \"id\" FROM \"shop\".\"events\" WHERE \"id\" < 50";
        let price = Snail::open(&path).unwrap().price(question, None).unwrap();
        let plan = texts("EXPLAIN SELECT id FROM mounted.events WHERE id < 50");
        assert!(
            plan[0].contains(&format!(" rows={} ", price.rows_high)),
            "{price:?}: {plan:#?}"
        );
    }
}
