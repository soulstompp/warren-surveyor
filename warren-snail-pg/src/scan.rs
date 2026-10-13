// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! A foreign table's scan: priced from the snail's directory as it is planned, read a block at a
//! time as it runs, and shown by EXPLAIN as the snail's own round.

use crate::{compat, datum};
use arrow::array::{AsArray, RecordBatch};
use arrow::datatypes::Int64Type;
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::{PgList, PgMemoryContexts, PgRelation};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::Path;
use std::ptr::null_mut;
use warren_snail::cut::PAGE;
use warren_snail::read::{Round, Scan, Snail};

pub(crate) fn refuse(message: String) -> ! {
    ereport!(ERROR, PgSqlErrorCode::ERRCODE_FDW_ERROR, message);
}

/// The value of option `name` among `options`, a list of DefElem.
pub(crate) unsafe fn option(options: *mut pg_sys::List, name: &str) -> Option<String> {
    PgList::<pg_sys::DefElem>::from_pg(options)
        .iter_ptr()
        .find(|&d| CStr::from_ptr((*d).defname).to_bytes() == name.as_bytes())
        .map(|d| {
            CStr::from_ptr(pg_sys::defGetString(d))
                .to_string_lossy()
                .into_owned()
        })
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub(crate) fn open(file: &str) -> Snail {
    Snail::open(Path::new(file)).unwrap_or_else(|e| refuse(format!("the snail {file}: {e}")))
}

/// The file a server names, as it is read: a name that is not absolute lies under the data
/// directory.
pub(crate) unsafe fn server_file(server: *mut pg_sys::ForeignServer) -> String {
    let named = option((*server).options, "file")
        .unwrap_or_else(|| refuse("a warren_snail server names its file".into()));
    if Path::new(&named).is_absolute() {
        named
    } else {
        let data = CStr::from_ptr(pg_sys::DataDir).to_string_lossy();
        Path::new(data.as_ref())
            .join(named)
            .to_string_lossy()
            .into_owned()
    }
}

/// A foreign table as the snail reads it: the file its server names, the snail's table its option
/// names, and its own columns.
struct Target {
    /// The file as it is read: a name that is not absolute lies under the data directory.
    file: String,
    table: String,
    name: String,
    /// Each column not dropped: its number, name, type and type modifier.
    columns: Vec<(i16, String, pg_sys::Oid, i32)>,
}

impl Target {
    unsafe fn of(relid: pg_sys::Oid) -> Target {
        let ft = pg_sys::GetForeignTable(relid);
        let file = server_file(pg_sys::GetForeignServer((*ft).serverid));
        let table = option((*ft).options, "table")
            .unwrap_or_else(|| refuse("a warren_snail foreign table names its table".into()));
        let rel = PgRelation::open(relid);
        let columns = rel
            .tuple_desc()
            .iter()
            .filter(|a| !a.is_dropped())
            .map(|a| {
                (
                    a.num(),
                    a.name().to_string(),
                    a.type_oid().value(),
                    a.type_mod(),
                )
            })
            .collect();
        Target {
            file,
            table,
            name: rel.name().to_string(),
            columns,
        }
    }

    /// Refuses a table the snail does not hold, and a column it does not hold or the foreign table's
    /// type cannot hold.
    fn check(&self, snail: &Snail) {
        let t = snail
            .catalog
            .tables
            .iter()
            .find(|t| t.name == self.table)
            .unwrap_or_else(|| refuse(format!("{} is not in the snail {}", self.table, self.file)));
        for (_, name, oid, typmod) in &self.columns {
            let c = t
                .columns
                .iter()
                .find(|c| &c.name == name)
                .unwrap_or_else(|| {
                    refuse(format!(
                        "column {name} of {} is not in the snail's {}",
                        self.name, self.table
                    ))
                });
            if !datum::takes(c.ty, *oid, *typmod) {
                let pg = unsafe {
                    CStr::from_ptr(pg_sys::format_type_with_typemod(*oid, *typmod))
                        .to_string_lossy()
                        .into_owned()
                };
                refuse(format!(
                    "column {name} of {} is {} in the snail, which {pg} does not hold",
                    self.name,
                    c.ty.name()
                ));
            }
        }
    }

    /// The question a scan asks the snail: the columns numbered, or where it needs none, the count.
    fn question(&self, attnos: &[i16]) -> String {
        let table = self
            .table
            .split('.')
            .map(quoted)
            .collect::<Vec<_>>()
            .join(".");
        if attnos.is_empty() {
            return format!("SELECT count(*) FROM {table}");
        }
        let columns = attnos
            .iter()
            .map(|n| {
                let (_, name, _, _) = self.columns.iter().find(|c| c.0 == *n).unwrap();
                quoted(name)
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT {columns} FROM {table}")
    }
}

/// The columns a scan of `rel` returns or its conditions read, by number; every one where the row is
/// read whole.
unsafe fn needed(rel: *mut pg_sys::RelOptInfo, target: &Target) -> Vec<i16> {
    let mut attrs: *mut pg_sys::Bitmapset = null_mut();
    pg_sys::pull_varattnos(
        (*(*rel).reltarget).exprs as *mut pg_sys::Node,
        (*rel).relid,
        &mut attrs,
    );
    for ri in PgList::<pg_sys::RestrictInfo>::from_pg((*rel).baserestrictinfo).iter_ptr() {
        pg_sys::pull_varattnos((*ri).clause as *mut pg_sys::Node, (*rel).relid, &mut attrs);
    }
    let mut out = Vec::new();
    let mut at = -1;
    loop {
        at = pg_sys::bms_next_member(attrs, at);
        if at < 0 {
            break;
        }
        let attno = at + pg_sys::FirstLowInvalidHeapAttributeNumber;
        if attno == 0 {
            return target.columns.iter().map(|c| c.0).collect();
        }
        if attno > 0 {
            out.push(attno as i16);
        }
    }
    out
}

/// What planning a scan found: the file and the question, the columns it fills, the rows the
/// snail hands out and the pages it reads before its first row and after.
struct Planned {
    file: String,
    question: String,
    attnos: Vec<i16>,
    rows: f64,
    pages_first: u64,
    pages_after: u64,
}

#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn rel_size(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    relid: pg_sys::Oid,
) {
    let target = Target::of(relid);
    let snail = open(&target.file);
    target.check(&snail);
    let attnos = needed(rel, &target);
    let question = target.question(&attnos);
    let price = snail
        .price(&question, None)
        .unwrap_or_else(|e| refuse(format!("the snail {}: {question}: {e}", target.file)));
    // Every condition is PostgreSQL's to filter: the snail hands out each of the table's rows.
    let rows = price.rows_high as f64;
    let kept = pg_sys::clauselist_selectivity(
        root,
        (*rel).baserestrictinfo,
        0,
        pg_sys::JoinType::JOIN_INNER,
        null_mut(),
    );
    (*rel).rows = pg_sys::clamp_row_est(rows * kept);
    (*rel).tuples = price.table_rows as f64;
    let pages_first = snail.pages_at_open + price.directory_pages;
    let pages = (pages_first + price.column_pages) * PAGE as u64;
    (*rel).pages = pages.div_ceil(pg_sys::BLCKSZ as u64) as pg_sys::BlockNumber;
    let planned = Planned {
        file: target.file,
        question,
        attnos,
        rows,
        pages_first,
        pages_after: price.column_pages,
    };
    (*rel).fdw_private =
        PgMemoryContexts::CurrentMemoryContext.leak_and_drop_on_delete(planned) as *mut c_void;
}

/// One path: every page the snail reads priced as a page read in turn, at PostgreSQL's page size.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn paths(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    _relid: pg_sys::Oid,
) {
    let planned = &*((*rel).fdw_private as *const Planned);
    let page = pg_sys::seq_page_cost * PAGE as f64 / pg_sys::BLCKSZ as f64;
    let restrict = (*rel).baserestrictcost;
    let startup = restrict.startup + planned.pages_first as f64 * page;
    let total = startup
        + planned.pages_after as f64 * page
        + (pg_sys::cpu_tuple_cost + restrict.per_tuple) * planned.rows;
    let path = compat::foreign_scan_path(root, rel, (*rel).rows, startup, total);
    pg_sys::add_path(rel, path as *mut pg_sys::Path);
}

/// A text constant, as the plan keeps what the scan asks.
unsafe fn text_const(s: &str) -> *mut c_void {
    let text = pg_sys::cstring_to_text_with_len(s.as_ptr() as *const c_char, s.len() as c_int);
    pg_sys::makeConst(
        pg_sys::TEXTOID,
        -1,
        pg_sys::InvalidOid,
        -1,
        pg_sys::Datum::from(text),
        false,
        false,
    ) as *mut c_void
}

unsafe fn text_of(node: *mut c_void) -> String {
    let c = node as *mut pg_sys::Const;
    CStr::from_ptr(pg_sys::text_to_cstring(
        (*c).constvalue.cast_mut_ptr::<pg_sys::text>(),
    ))
    .to_string_lossy()
    .into_owned()
}

/// The plan: every condition kept for PostgreSQL to filter, and the file, the question and the
/// columns it fills kept as text.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn plan(
    _root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    _relid: pg_sys::Oid,
    _best: *mut pg_sys::ForeignPath,
    tlist: *mut pg_sys::List,
    scan_clauses: *mut pg_sys::List,
    outer_plan: *mut pg_sys::Plan,
) -> *mut pg_sys::ForeignScan {
    let planned = &*((*rel).fdw_private as *const Planned);
    let clauses = pg_sys::extract_actual_clauses(scan_clauses, false);
    let attnos = planned
        .attnos
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut private: *mut pg_sys::List = null_mut();
    for s in [
        planned.file.as_str(),
        planned.question.as_str(),
        attnos.as_str(),
    ] {
        private = pg_sys::lappend(private, text_const(s));
    }
    pg_sys::make_foreignscan(
        tlist,
        clauses,
        (*rel).relid,
        null_mut(),
        private,
        null_mut(),
        null_mut(),
        outer_plan,
    )
}

/// A scan under way: the snail open for it, its round, the block it hands rows out of, and the
/// rounds it ended before, each rescan one.
struct Scanning {
    file: String,
    question: String,
    /// The column each of the snail's columns fills, counted from 1; none where the snail counts.
    attnos: Vec<i16>,
    snail: Option<Snail>,
    scan: Option<Scan>,
    batch: Option<RecordBatch>,
    at: usize,
    /// Where the snail counts: the rows still to hand out, once it has.
    counted: Option<u64>,
    ended: Vec<Round>,
    /// Whether it holds one of the files PostgreSQL lets a backend open beside its own.
    file_held: bool,
}

impl Scanning {
    fn begin_round(&mut self) {
        let snail = self
            .snail
            .as_ref()
            .expect("the snail is open while it scans");
        self.scan = Some(snail.scan(&self.question, None).unwrap_or_else(|e| {
            refuse(format!("the snail {}: {}: {e}", self.file, self.question))
        }));
        self.batch = None;
        self.at = 0;
        self.counted = None;
    }

    /// The scan's rounds, those ended and the one under way.
    fn rounds(&self) -> Vec<Round> {
        let mut rounds = self.ended.clone();
        rounds.extend(self.scan.as_ref().map(|s| s.round()));
        rounds
    }

    /// Closes the snail, its locks going with its file.
    fn close(&mut self) {
        self.batch = None;
        self.scan = None;
        self.snail = None;
        if self.file_held {
            self.file_held = false;
            unsafe { pg_sys::ReleaseExternalFD() };
        }
    }
}

impl Drop for Scanning {
    fn drop(&mut self) {
        self.close();
    }
}

/// Opens the snail. Its round begins with the first row asked for, or here under EXPLAIN alone, so
/// that EXPLAIN shows what it would read. The scan lives in the query's memory, so an ERROR that
/// ends the query closes the snail.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn begin(node: *mut pg_sys::ForeignScanState, eflags: c_int) {
    let plan = (*node).ss.ps.plan as *mut pg_sys::ForeignScan;
    let private = (*plan).fdw_private;
    let file = text_of(pg_sys::list_nth(private, 0));
    let question = text_of(pg_sys::list_nth(private, 1));
    let attnos = text_of(pg_sys::list_nth(private, 2))
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<i16>().expect("the plan keeps column numbers"))
        .collect();
    let scanning = Scanning {
        file,
        question,
        attnos,
        snail: None,
        scan: None,
        batch: None,
        at: 0,
        counted: None,
        ended: Vec::new(),
        file_held: false,
    };
    let state = PgMemoryContexts::For((*(*node).ss.ps.state).es_query_cxt)
        .leak_and_drop_on_delete(scanning);
    (*node).fdw_state = state as *mut c_void;
    let state = &mut *state;
    if !pg_sys::AcquireExternalFD() {
        ereport!(
            ERROR,
            PgSqlErrorCode::ERRCODE_INSUFFICIENT_RESOURCES,
            format!(
                "the snail {}: this backend has opened as many files as it may",
                state.file
            )
        );
    }
    state.file_held = true;
    state.snail = Some(open(&state.file));
    if eflags as u32 & pg_sys::EXEC_FLAG_EXPLAIN_ONLY != 0 {
        state.begin_round();
    }
}

#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn iterate(
    node: *mut pg_sys::ForeignScanState,
) -> *mut pg_sys::TupleTableSlot {
    let state = &mut *((*node).fdw_state as *mut Scanning);
    let slot = (*node).ss.ss_ScanTupleSlot;
    pg_sys::ExecClearTuple(slot);
    if state.scan.is_none() {
        state.begin_round();
    }
    let snail = state
        .snail
        .as_ref()
        .expect("the snail is open while it scans");
    let scan = state.scan.as_mut().expect("a round is under way");
    if state.attnos.is_empty() {
        let left = match state.counted {
            Some(n) => n,
            None => match snail.next_block(scan) {
                Ok(Some(b)) if b.num_rows() > 0 => {
                    b.column(0).as_primitive::<Int64Type>().value(0) as u64
                }
                Ok(_) => 0,
                Err(e) => refuse(format!("the snail {}: {e}", state.file)),
            },
        };
        if left == 0 {
            state.counted = Some(0);
            return slot;
        }
        state.counted = Some(left - 1);
        return pg_sys::ExecStoreAllNullTuple(slot);
    }
    loop {
        if let Some(b) = &state.batch {
            if state.at < b.num_rows() {
                let natts = (*(*slot).tts_tupleDescriptor).natts as usize;
                let values = std::slice::from_raw_parts_mut((*slot).tts_values, natts);
                let nulls = std::slice::from_raw_parts_mut((*slot).tts_isnull, natts);
                nulls.fill(true);
                for (j, &attno) in state.attnos.iter().enumerate() {
                    if let Some(d) = datum::datum(b.column(j), state.at) {
                        values[attno as usize - 1] = d;
                        nulls[attno as usize - 1] = false;
                    }
                }
                state.at += 1;
                return pg_sys::ExecStoreVirtualTuple(slot);
            }
        }
        match snail.next_block(scan) {
            Ok(Some(b)) => {
                state.batch = Some(b);
                state.at = 0;
            }
            Ok(None) => return slot,
            Err(e) => refuse(format!("the snail {}: {e}", state.file)),
        }
    }
}

/// Ends the round under way; the next row asked for begins another, on the root the snail read when
/// the scan began.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn rescan(node: *mut pg_sys::ForeignScanState) {
    let state = &mut *((*node).fdw_state as *mut Scanning);
    if let Some(scan) = state.scan.take() {
        state.ended.push(scan.round());
    }
}

#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn end(node: *mut pg_sys::ForeignScanState) {
    if let Some(state) = ((*node).fdw_state as *mut Scanning).as_mut() {
        state.close();
    }
}

unsafe fn explain_text(label: &str, value: &str, es: *mut pg_sys::ExplainState) {
    let (label, value) = (CString::new(label).unwrap(), CString::new(value).unwrap());
    pg_sys::ExplainPropertyText(label.as_ptr(), value.as_ptr(), es);
}

unsafe fn explain_integer(label: &str, value: u64, es: *mut pg_sys::ExplainState) {
    let label = CString::new(label).unwrap();
    pg_sys::ExplainPropertyInteger(label.as_ptr(), null_mut(), value as i64, es);
}

/// The snail's round: the file and the question, the order read and what each order consulted was
/// priced at, and the directory and column pages, predicted and, run, read.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn explain(
    node: *mut pg_sys::ForeignScanState,
    es: *mut pg_sys::ExplainState,
) {
    let Some(state) = ((*node).fdw_state as *const Scanning).as_ref() else {
        return;
    };
    explain_text("Snail File", &state.file, es);
    explain_text("Snail Query", &state.question, es);
    let rounds = state.rounds();
    if let Some(last) = rounds.last() {
        explain_text("Snail Order", &last.order, es);
        let priced = last
            .consulted
            .iter()
            .map(|c| match c.exact {
                Some(n) => format!("{} {n} pages", c.order),
                None => format!("{} {} to {} pages, left unread", c.order, c.low, c.high),
            })
            .collect::<Vec<_>>()
            .join("; ");
        if !priced.is_empty() {
            explain_text("Snail Orders Priced", &priced, es);
        }
    }
    let sum = |f: fn(&Round) -> u64| rounds.iter().map(f).sum::<u64>();
    explain_integer("Snail Directory Pages", sum(|r| r.directory_pages), es);
    explain_integer(
        "Snail Column Pages Predicted",
        sum(|r| r.column_pages_predicted),
        es,
    );
    if (*es).analyze {
        explain_integer("Snail Column Pages Read", sum(|r| r.column_pages_read), es);
        if let Some(snail) = &state.snail {
            explain_integer("Snail Pages Held", snail.pages_held(), es);
        }
        if rounds.len() > 1 {
            explain_integer("Snail Rounds", rounds.len() as u64, es);
        }
    }
}

/// A scan is read by the backend that planned it alone.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn parallel_safe(
    _root: *mut pg_sys::PlannerInfo,
    _rel: *mut pg_sys::RelOptInfo,
    _rte: *mut pg_sys::RangeTblEntry,
) -> bool {
    false
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
pub(crate) mod tests {
    use crate::tests::refusal;
    use arrow::array::{
        ArrayRef, BinaryArray, Decimal128Array, Int16Array, Int32Array, Int64Array, RecordBatch,
        StringArray, TimestampMicrosecondArray,
    };
    use pgrx::prelude::*;
    use std::ffi::CStr;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use warren_snail::catalog::Catalog;
    use warren_snail::read::Snail;
    use warren_snail::write::Rows;

    pub(crate) const DDL: &str =
        "CREATE TABLE shop.kinds (id bigint NOT NULL, small smallint, mid integer, \
                       at timestamptz, name text, code varchar(8), raw bytea, price numeric(12,3), \
                       PRIMARY KEY (id));";
    pub(crate) const COLUMNS: &str =
        "id bigint NOT NULL, small smallint, mid integer, at timestamptz, \
                           name text, code varchar(8), raw bytea, price numeric(12,3)";

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

    pub(crate) fn one(sql: &str) -> String {
        texts(sql).pop().unwrap_or_else(|| panic!("{sql}: no row"))
    }

    /// PostgreSQL's own copy of the rows, a heap table: `n` rows of every type, NULLs among them,
    /// then each type's edges.
    pub(crate) fn heap(n: i64) {
        Spi::run("SET LOCAL TimeZone = 'UTC'").unwrap();
        Spi::run(&format!(
            "CREATE TABLE kinds_heap ({COLUMNS}, PRIMARY KEY (id))"
        ))
        .unwrap();
        Spi::run(&format!(
            "INSERT INTO kinds_heap SELECT i, \
               CASE WHEN i % 17 = 0 THEN NULL ELSE (i * 7919 % 65536 - 32768)::smallint END, \
               CASE WHEN i % 13 = 0 THEN NULL ELSE (i * 104729 % 2147483647 - 1073741823)::integer END, \
               CASE WHEN i % 11 = 0 THEN NULL ELSE timestamptz '1950-01-01 00:00:00+00' \
                    + (i * 7777777777 % 3000000000000000) * interval '1 microsecond' END, \
               CASE WHEN i % 7 = 0 THEN NULL ELSE (ARRAY['', 'Évora ', 'Zürich', 'a\"b', '🐌', \
                    'Lisboa–Tejo'])[1 + i % 6] || (i % 50)::text END, \
               CASE WHEN i % 5 = 0 THEN NULL ELSE lpad((i % 99999)::text, (1 + i % 8)::integer, 'é') END, \
               CASE WHEN i % 9 = 0 THEN NULL ELSE decode(lpad(to_hex(i * 2654435761 % 4294967296), 8, '0') \
                    || CASE WHEN i % 4 = 0 THEN '00' ELSE '' END, 'hex') END, \
               CASE WHEN i % 3 = 0 THEN NULL ELSE (i * 48271 % 2000000 - 1000000)::numeric / 1000 END \
             FROM generate_series(1, {n}) i"
        ))
        .unwrap();
        Spi::run(
            "INSERT INTO kinds_heap VALUES \
             (-9223372036854775808, -32768, -2147483648, '0001-01-01 00:00:00+00', '', '', '\\x', \
              -999999999.999), \
             (9223372036854775807, 32767, 2147483647, '9999-12-31 23:59:59.999999+00', \
              'Ålesund 🐌', '12345678', '\\x00ff00', 999999999.999), \
             (0, 0, 0, '1969-12-31 23:59:59.999999+00', 'a\"b''c', 'é', '\\x00', 0.000), \
             (-1, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
             (-2, -1, -1, '2000-01-01 00:00:00+00', E'tab\\there\\nline', 'x', '\\xff', -0.001), \
             (-3, 1, 1, '1970-01-01 00:00:00+00', 'Lisboa–Tejo', 'Zürich', '\\x41', 0.500), \
             (-4, 2, 2, '4714-11-24 00:00:00+00 BC', 'first', 'a', '\\x01', 0.001), \
             (-5, 3, 3, '294247-01-10 04:00:54.775807+00', 'last', 'b', '\\x02', 0.002)",
        )
        .unwrap();
    }

    /// The data directory, where a server's relative file lies.
    pub(crate) fn data_dir() -> PathBuf {
        PathBuf::from(
            unsafe { CStr::from_ptr(pg_sys::DataDir) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// The heap's rows as the snail takes them.
    pub(crate) fn kinds_batch() -> RecordBatch {
        let (mut id, mut small, mut mid, mut at) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut text, mut code, mut raw, mut price) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        Spi::connect(|client| {
            // An instant's whole seconds and its microseconds apart: past int64, extract's epoch is
            // rounded.
            let sql = "SELECT id, small, mid, \
                       extract(epoch FROM date_trunc('second', at))::bigint * 1000000 \
                       + extract(microseconds FROM at)::bigint % 1000000, name, code, raw, \
                       replace(price::text, '.', '')::bigint FROM kinds_heap";
            for row in client.select(sql, None, &[])? {
                id.push(row.get::<i64>(1)?.unwrap());
                small.push(row.get::<i16>(2)?);
                mid.push(row.get::<i32>(3)?);
                at.push(row.get::<i64>(4)?);
                text.push(row.get::<String>(5)?);
                code.push(row.get::<String>(6)?);
                raw.push(row.get::<Vec<u8>>(7)?);
                price.push(row.get::<i64>(8)?.map(i128::from));
            }
            Ok::<_, pgrx::spi::SpiError>(())
        })
        .unwrap();
        RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(id)) as ArrayRef),
            ("small", Arc::new(Int16Array::from(small))),
            ("mid", Arc::new(Int32Array::from(mid))),
            (
                "at",
                Arc::new(TimestampMicrosecondArray::from(at).with_timezone("UTC")),
            ),
            ("name", Arc::new(StringArray::from(text))),
            ("code", Arc::new(StringArray::from(code))),
            (
                "raw",
                Arc::new(BinaryArray::from_iter(raw.iter().map(|r| r.as_deref()))),
            ),
            (
                "price",
                Arc::new(
                    Decimal128Array::from(price)
                        .with_precision_and_scale(12, 3)
                        .unwrap(),
                ),
            ),
        ])
        .unwrap()
    }

    /// A snail of `ddl` in the data directory, as `name`.snail, each table's rows from its batch.
    pub(crate) fn written(name: &str, ddl: &str, tables: &[(&str, RecordBatch)]) -> PathBuf {
        let path = data_dir().join(format!("{name}.snail"));
        let _ = std::fs::remove_file(&path);
        let catalog = Catalog::parse(ddl).unwrap();
        let rows: Vec<(String, Rows)> = tables
            .iter()
            .map(|(name, batch)| {
                let table = catalog.tables.iter().find(|t| t.name == *name).unwrap();
                let rows = Rows::from_batches(table, std::slice::from_ref(batch)).unwrap();
                (name.to_string(), rows)
            })
            .collect();
        warren_snail::write::write(&path, &catalog, &rows).unwrap();
        path
    }

    /// The heap's rows written as a snail in the data directory, as `name`.snail.
    fn snail(name: &str) -> PathBuf {
        written(name, DDL, &[("shop.kinds", kinds_batch())])
    }

    /// The heap, its snail, a server naming the file as `file` and the foreign table `kinds`.
    fn mounted(name: &str, n: i64, file: impl Fn(&Path) -> String) -> PathBuf {
        heap(n);
        let path = snail(name);
        Spi::run(&format!(
            "CREATE SERVER kinds_file FOREIGN DATA WRAPPER warren_snail OPTIONS (file '{}')",
            file(&path)
        ))
        .unwrap();
        Spi::run(&format!(
            "CREATE FOREIGN TABLE kinds ({COLUMNS}) SERVER kinds_file \
             OPTIONS (\"table\" 'shop.kinds')"
        ))
        .unwrap();
        path
    }

    fn named_in_data_dir(path: &Path) -> String {
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    fn named_whole(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    fn explained(sql: &str) -> Vec<String> {
        texts(&format!(
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) {sql}"
        ))
    }

    /// The value EXPLAIN prints for `label`.
    fn shown<'a>(plan: &'a [String], label: &str) -> Option<&'a str> {
        let label = format!("{label}: ");
        plan.iter()
            .find_map(|l| l.trim_start().strip_prefix(label.as_str()))
    }

    /// Whether a round anywhere holds the generation the snail's file is at, or an older one.
    fn generation_held(path: &Path) -> bool {
        let generation = Snail::open(path).unwrap().generation();
        let file = std::fs::File::open(path).unwrap();
        warren_snail::lock::older_held(&file, generation + 1).unwrap()
    }

    #[pg_test]
    fn every_value_comes_out_as_postgresql_holds_it() {
        mounted("kinds_values", 3000, named_in_data_dir);
        assert_eq!(one("SELECT count(*)::text FROM kinds"), "3008");
        for (a, b) in [("kinds", "kinds_heap"), ("kinds_heap", "kinds")] {
            let sql = format!(
                "SELECT d::text FROM (SELECT * FROM {a} EXCEPT ALL SELECT * FROM {b}) d ORDER BY 1"
            );
            assert_eq!(texts(&sql), Vec::<String>::new(), "{sql}");
        }
        // Each value's text, a numeric's scale and an instant's microseconds among it.
        assert_eq!(
            texts("SELECT k::text FROM kinds k ORDER BY id"),
            texts("SELECT h::text FROM kinds_heap h ORDER BY id")
        );
    }

    #[pg_test]
    fn conditions_are_filtered_above_the_snail_and_a_count_reads_no_column() {
        mounted("kinds_count", 3000, named_whole);
        for q in [
            "SELECT count(*)::text FROM {}",
            "SELECT count(*)::text FROM {} WHERE small > 0 AND name LIKE 'Z%'",
            "SELECT string_agg(id::text, ',' ORDER BY id) FROM {} WHERE price < 0 AND raw IS NOT NULL",
        ] {
            assert_eq!(
                one(&q.replace("{}", "kinds")),
                one(&q.replace("{}", "kinds_heap")),
                "{q}"
            );
        }
        let plan = explained("SELECT count(*) FROM kinds");
        assert_eq!(
            shown(&plan, "Snail Query"),
            Some("SELECT count(*) FROM \"shop\".\"kinds\""),
            "{plan:#?}"
        );
        let plan = explained("SELECT id FROM kinds WHERE small > 0");
        assert_eq!(
            shown(&plan, "Snail Query"),
            Some("SELECT \"id\", \"small\" FROM \"shop\".\"kinds\""),
            "{plan:#?}"
        );
        assert!(
            plan.iter().any(|l| l.trim_start().starts_with("Filter: ")),
            "{plan:#?}"
        );
    }

    /// EXPLAIN prints the snail's own round: the round read directly, its pages predicted and read
    /// alike, and none held, since inside PostgreSQL a snail holds none.
    #[pg_test]
    fn explain_shows_the_round_the_snail_reads_directly() {
        let path = mounted("kinds_explain", 20000, named_whole);
        let plan = explained("SELECT id, name FROM kinds");
        let question = "SELECT \"id\", \"name\" FROM \"shop\".\"kinds\"";
        let (_, round) = Snail::open(&path).unwrap().ask(question, None).unwrap();
        assert!(round.blocks_read > 1, "{round:?}");
        assert_eq!(shown(&plan, "Snail File"), Some(path.to_str().unwrap()));
        assert_eq!(shown(&plan, "Snail Query"), Some(question));
        assert_eq!(shown(&plan, "Snail Order"), Some(round.order.as_str()));
        let pages = |label: &str| -> u64 {
            shown(&plan, label)
                .unwrap_or_else(|| panic!("{label}: {plan:#?}"))
                .parse()
                .unwrap()
        };
        assert_eq!(pages("Snail Directory Pages"), round.directory_pages);
        assert_eq!(
            pages("Snail Column Pages Predicted"),
            round.column_pages_predicted
        );
        assert_eq!(pages("Snail Column Pages Read"), round.column_pages_read);
        assert_eq!(
            pages("Snail Column Pages Read"),
            pages("Snail Column Pages Predicted")
        );
        assert!(pages("Snail Column Pages Read") > 0, "{plan:#?}");
        assert_eq!(pages("Snail Pages Held"), 0);
        // Rows asked for a block at a time: a scan stopped at its first row has read its first
        // block alone.
        let plan = explained("SELECT id, name FROM kinds LIMIT 1");
        let read = shown(&plan, "Snail Column Pages Read").map(|n| n.parse::<u64>().unwrap());
        assert!(
            read.is_some_and(|n| n > 0 && n < round.column_pages_read),
            "{plan:#?}"
        );
    }

    /// The planner is told the snail's rows and pages, each page read in turn at PostgreSQL's page
    /// size: those it reads before its first row, from the root to the directory, and the column
    /// pages after.
    #[pg_test]
    fn the_planner_is_told_the_snails_rows_and_pages() {
        let path = mounted("kinds_priced", 20000, named_whole);
        let snail = Snail::open(&path).unwrap();
        let price = snail
            .price("SELECT \"id\" FROM \"shop\".\"kinds\"", None)
            .unwrap();
        assert_eq!(
            (price.rows_low, price.rows_high, price.table_rows),
            (20008, 20008, 20008)
        );
        assert!(price.column_pages > 0, "{price:?}");
        let page = unsafe { pg_sys::seq_page_cost } * 4096.0 / pg_sys::BLCKSZ as f64;
        let first = (snail.pages_at_open + price.directory_pages) as f64 * page;
        let total = first
            + price.column_pages as f64 * page
            + unsafe { pg_sys::cpu_tuple_cost } * price.rows_high as f64;
        let plan = texts("EXPLAIN SELECT id FROM kinds");
        let want = format!("(cost={first:.2}..{total:.2} rows=20008 ");
        assert!(plan[0].contains(&want), "{want}: {plan:#?}");
        // EXPLAIN alone shows the round priced, and nothing read.
        assert_eq!(shown(&plan, "Snail Order"), Some(price.order.as_str()));
        let predicted = price.column_pages.to_string();
        assert_eq!(
            shown(&plan, "Snail Column Pages Predicted"),
            Some(predicted.as_str())
        );
        assert_eq!(shown(&plan, "Snail Column Pages Read"), None);
    }

    /// A scan begun again, once for each row outside it, reads the snail afresh each time.
    #[pg_test]
    fn a_scan_begun_again_reads_its_round_again() {
        mounted("kinds_rescan", 3000, named_whole);
        let q = "SELECT string_agg((SELECT count(*) FROM {} k WHERE k.id % 3 = g)::text, ',' \
                 ORDER BY g) FROM generate_series(0, 2) g";
        assert_eq!(
            one(&q.replace("{}", "kinds")),
            one(&q.replace("{}", "kinds_heap"))
        );
        let plan = explained(&q.replace("{}", "kinds"));
        assert_eq!(shown(&plan, "Snail Rounds"), Some("3"), "{plan:#?}");
    }

    /// The snail's generation is let go when a query ends, when a cursor closes, when a statement
    /// fails, and after EXPLAIN.
    #[pg_test]
    fn the_snail_is_let_go_however_the_scan_ends() {
        let path = mounted("kinds_let_go", 3000, named_whole);
        assert!(!generation_held(&path));
        assert_eq!(one("SELECT count(*)::text FROM kinds"), "3008");
        assert!(!generation_held(&path));
        Spi::connect(|client| {
            let mut cursor = client.open_cursor("SELECT * FROM kinds", &[]);
            cursor.fetch(1).unwrap();
            assert!(generation_held(&path), "an open cursor holds its round");
            drop(cursor);
            assert!(!generation_held(&path), "a closed cursor");
        });
        let e = refusal("SELECT 1 / (small - small) FROM kinds", None);
        assert!(
            e.as_deref().is_some_and(|e| e.contains("division by zero")),
            "{e:?}"
        );
        assert!(!generation_held(&path), "after an ERROR");
        texts("EXPLAIN SELECT * FROM kinds");
        assert!(!generation_held(&path), "after EXPLAIN");
    }

    #[pg_test]
    fn writes_and_parallel_plans_are_refused() {
        mounted("kinds_refused", 300, named_whole);
        for (sql, why) in [
            (
                "INSERT INTO kinds (id) VALUES (1)",
                "cannot insert into foreign table",
            ),
            ("UPDATE kinds SET small = 1", "cannot update foreign table"),
            ("DELETE FROM kinds", "cannot delete from foreign table"),
        ] {
            let e = refusal(sql, None);
            assert!(
                e.as_deref().is_some_and(|e| e.contains(why)),
                "{sql}: {e:?}"
            );
        }
        Spi::run(
            "SELECT set_config(name, 'on', true) FROM pg_settings \
             WHERE name IN ('force_parallel_mode', 'debug_parallel_query')",
        )
        .unwrap();
        for setting in [
            "parallel_setup_cost",
            "parallel_tuple_cost",
            "min_parallel_table_scan_size",
        ] {
            Spi::run(&format!("SET LOCAL {setting} = 0")).unwrap();
        }
        let plan = texts("EXPLAIN (COSTS OFF) SELECT * FROM kinds");
        assert!(plan.iter().all(|l| !l.contains("Gather")), "{plan:#?}");
        let plan = texts("EXPLAIN (COSTS OFF) SELECT * FROM kinds_heap");
        assert!(
            plan.iter().any(|l| l.contains("Gather")),
            "the heap is read in parallel: {plan:#?}"
        );
    }

    /// An instant the snail holds before PostgreSQL's first is refused as it is read; the first
    /// itself comes out.
    #[pg_test]
    fn an_instant_before_postgresqls_first_is_refused() {
        let first = -211_813_488_000_000_000 + 946_684_800_000_000;
        let ddl =
            "CREATE TABLE shop.instants (id bigint NOT NULL, at timestamptz, PRIMARY KEY (id));";
        let catalog = Catalog::parse(ddl).unwrap();
        for (name, at) in [("first", first), ("before", first - 1)] {
            let path = data_dir().join(format!("instants_{name}.snail"));
            let _ = std::fs::remove_file(&path);
            let batch = RecordBatch::try_from_iter([
                ("id", Arc::new(Int64Array::from(vec![1])) as ArrayRef),
                (
                    "at",
                    Arc::new(TimestampMicrosecondArray::from(vec![at]).with_timezone("UTC")),
                ),
            ])
            .unwrap();
            let rows = Rows::from_batches(&catalog.tables[0], &[batch]).unwrap();
            warren_snail::write::write(&path, &catalog, &[("shop.instants".to_string(), rows)])
                .unwrap();
            Spi::run(&format!(
                "CREATE SERVER {name} FOREIGN DATA WRAPPER warren_snail OPTIONS (file '{}')",
                path.display()
            ))
            .unwrap();
            Spi::run(&format!(
                "CREATE FOREIGN TABLE {name} (id bigint, at timestamptz) SERVER {name} \
                 OPTIONS (\"table\" 'shop.instants')"
            ))
            .unwrap();
        }
        Spi::run("SET LOCAL TimeZone = 'UTC'").unwrap();
        assert_eq!(
            one("SELECT at::text FROM first"),
            "4714-11-24 00:00:00+00 BC"
        );
        assert_eq!(one("SELECT id::text FROM before"), "1");
        let e = refusal("SELECT at FROM before", None);
        assert!(
            e.as_deref()
                .is_some_and(|e| e.contains("the snail holds an instant past timestamptz's range")),
            "{e:?}"
        );
    }

    /// A foreign table whose columns the snail does not hold, or holds as values its type cannot
    /// hold, is refused as it is planned; a type that holds every value is taken.
    #[pg_test]
    fn a_column_the_snail_cannot_fill_is_refused() {
        let path = mounted("kinds_columns", 30, named_whole);
        let table = |name: &str, columns: &str, option: &str| {
            Spi::run(&format!(
                "CREATE FOREIGN TABLE {name} ({columns}) SERVER kinds_file \
                 OPTIONS (\"table\" '{option}')"
            ))
            .unwrap();
        };
        table("narrow", "id integer", "shop.kinds");
        table("unheld", "id bigint, colour text", "shop.kinds");
        table("scaled", "price numeric(12,2)", "shop.kinds");
        table("short", "code varchar(4)", "shop.kinds");
        table("tight", "price numeric(11,3)", "shop.kinds");
        table("coarse", "at timestamptz(3)", "shop.kinds");
        table("elsewhere", "id bigint", "shop.nowhere");
        table(
            "wide",
            "id bigint, name varchar, code text, price numeric(15,3), raw bytea",
            "shop.kinds",
        );
        for (sql, why) in [
            (
                "SELECT * FROM narrow",
                "column id of narrow is bigint in the snail, which integer does not hold",
            ),
            (
                "SELECT id FROM unheld",
                "column colour of unheld is not in the snail's shop.kinds",
            ),
            (
                "SELECT * FROM scaled",
                "column price of scaled is numeric(12,3) in the snail, which numeric(12,2) does not hold",
            ),
            (
                "SELECT * FROM short",
                "column code of short is varchar(8) in the snail, which character varying(4) does not hold",
            ),
            (
                "SELECT * FROM tight",
                "column price of tight is numeric(12,3) in the snail, which numeric(11,3) does not hold",
            ),
            (
                "SELECT * FROM coarse",
                "column at of coarse is timestamptz in the snail, which timestamp(3) with time zone does not hold",
            ),
            (
                "SELECT * FROM elsewhere",
                &format!("shop.nowhere is not in the snail {}", path.display()),
            ),
        ] {
            let e = refusal(sql, None);
            assert!(e.as_deref().is_some_and(|e| e.contains(why)), "{sql}: {e:?}");
        }
        assert_eq!(
            texts("SELECT w::text FROM wide w ORDER BY id"),
            texts("SELECT row(id, name, code, price, raw)::text FROM kinds_heap ORDER BY id")
        );
        Spi::run("ALTER SERVER kinds_file OPTIONS (SET file 'no such.snail')").unwrap();
        let e = refusal("SELECT * FROM kinds", None);
        assert!(
            e.as_deref().is_some_and(|e| e.contains(&format!(
                "the snail {}",
                data_dir().join("no such.snail").display()
            ))),
            "{e:?}"
        );
    }
}
