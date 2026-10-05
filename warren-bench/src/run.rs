// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Running questions: resolving targets, one repetition at a time, the expected answers, and the
//! benchmark loop.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{Column, Connection, Executor, Row, Statement, ValueRef};
use tracing::{error, info, info_span, warn, Instrument};

use crate::canonical;
use crate::derived::{self, Family};
use crate::digest::{fingerprint, Digest, RowBuffer};
use crate::explain::{self, Facts, Leaves};
use crate::index_sets::{self, Resolved};
use crate::questions::{compose, differs_beyond_table, literal, Composed, Question, QuestionSet};
use crate::report::RESULTS;
use crate::tables::{self, Declared, Kind};
use crate::target::{self, Session, Target, Url};
use crate::watchdog::{self, watch, Guard, Watched, INTERRUPTED};
use crate::witness::{self, Reading, Split};

pub struct Ctx {
    pub guard: Guard,
    pub statement_timeout: String,
    pub work: PathBuf,
    /// `plan_cache_mode` for every session this program opens, or the server's own when `None`.
    pub plan_cache_mode: Option<String>,
}

/// The connection options for every session on `url`: the URL's own, and `plan_cache_mode` where
/// one is given.
fn session_options(url: &Url, ctx: &Ctx) -> Result<PgConnectOptions, String> {
    let o = url.session_options()?;
    Ok(match &ctx.plan_cache_mode {
        Some(m) => o.options([("plan_cache_mode", m.as_str())]),
        None => o,
    })
}

/// How a target was asked for on the command line.
pub enum Spec {
    Explicit {
        name: String,
        table: String,
        url: Url,
    },
}

/// A target with its control session (used for reading the catalog, polling memory and
/// cancelling) and its composed questions. Under an index set, the target is `<name>@<set>`, and
/// its resolved set says which indexes every repetition turns off.
pub struct Live {
    pub target: Target,
    pub control: Session,
    pub composed: Composed,
    /// The target's name as the command line gave it.
    pub base: String,
    pub resolved: Option<Resolved>,
    /// Every index of the database's own schemas, as `resolve` read it.
    pub roster: Option<Vec<index_sets::Index>>,
}

const CONTROL_TIMEOUT: &str = "30s";

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn control_session(url: &Url, ctx: &Ctx) -> Result<Session, String> {
    let opts = session_options(url, ctx)?;
    let mut s = target::open(&opts, CONTROL_TIMEOUT)
        .await
        .map_err(|e| format!("{}: {e}", url.redacted))?;
    let pid = s.pid;
    target::confirm_pid(&mut s.conn, pid).await?;
    if ctx.guard.poll_memory {
        watchdog::visible_backend(pid).map_err(|e| {
            format!(
                "{}: cannot watch this server's backend memory: {e}. \
                 Refusing to run without the memory watchdog unless --no-memory-watchdog is given",
                url.redacted
            )
        })?;
    }
    Ok(s)
}

/// Refuses while a loader is connected to the server.
pub async fn refuse_if_loading(control: &mut PgConnection, server: &str) -> Result<(), String> {
    let found = watchdog::loader_sessions(control).await?;
    if found.is_empty() {
        return Ok(());
    }
    for f in &found {
        error!(server, session = %f, "a loader session is connected");
    }
    Err(format!(
        "refusing to run: {} session(s) named {} are connected to {server}",
        found.len(),
        watchdog::loader_names()
    ))
}

/// The index sets a run asks for, and the file that declares them.
pub struct IndexSets {
    pub sets: index_sets::Sets,
    pub names: Vec<String>,
}

/// Connects to every target, reads its configuration and composes the questions for it. With index
/// sets, each target becomes one target per set, `<name>@<set>`, each certified by a trial of the
/// transaction its repetitions open.
pub async fn resolve(
    specs: &[Spec],
    ctx: &Ctx,
    set: &QuestionSet,
    index_sets: Option<&IndexSets>,
) -> Result<Vec<Live>, String> {
    let lives = resolve_targets(specs, ctx, set).await?;
    match index_sets {
        None => Ok(lives),
        Some(s) => with_index_sets(lives, ctx, s).await,
    }
}

/// One target per (target, index set): its name `<target>@<set>`, its configuration the target's
/// with the set and the indexes it turns off, each certified by `index_sets::certify`.
async fn with_index_sets(lives: Vec<Live>, ctx: &Ctx, s: &IndexSets) -> Result<Vec<Live>, String> {
    let mut seen = BTreeSet::new();
    if let Some(d) = s.names.iter().find(|n| !seen.insert(n.as_str())) {
        return Err(format!("the index set {d} is named twice"));
    }
    let mut out = Vec::new();
    for base in lives {
        let name = base.target.name.clone();
        if s.names.is_empty() {
            return Err(format!("{name}: no index set is named for this target"));
        }
        let roster = match &base.roster {
            Some(r) => r.clone(),
            None => return Err(format!("{name}: no indexes were read for this target")),
        };
        for set_name in &s.names {
            let span = info_span!("target", target = %name, index_set = %set_name);
            let live = async {
                let resolved = index_sets::resolve_set(&s.sets, set_name, &roster)?;
                index_sets::certify(&base.target.opts, &ctx.statement_timeout, &resolved).await?;
                let control = control_session(&base.target.url, ctx).await?;
                let mut t = base.target.clone();
                t.name = format!("{name}@{set_name}");
                t.config.push(("index_set".into(), set_name.clone()));
                t.config.push(("indexes_off".into(), resolved.off_names()));
                t.config_id = target::config_id(&t.config);
                info!(
                    indexes_off = resolved.off.len(),
                    roster = resolved.roster.len(),
                    config_id = %t.config_id,
                    "index set certified: its indexes are off inside every repetition's transaction, and only there"
                );
                Ok::<Live, String>(Live {
                    target: t,
                    control,
                    composed: base.composed.clone(),
                    base: base.base.clone(),
                    resolved: Some(resolved),
                    roster: Some(roster.clone()),
                })
            }
            .instrument(span)
            .await
            .map_err(|e| format!("{name}: {e}"))?;
            out.push(live);
        }
        base.control.close().await;
    }
    Ok(out)
}

async fn resolve_targets(
    specs: &[Spec],
    ctx: &Ctx,
    set: &QuestionSet,
) -> Result<Vec<Live>, String> {
    let mut wanted: Vec<(String, String, Url)> = Vec::new();
    for s in specs {
        match s {
            Spec::Explicit { name, table, url } => {
                wanted.push((name.clone(), table.clone(), url.clone()))
            }
        }
    }
    if wanted.is_empty() {
        return Err("no targets: give --target NAME TABLE URL".into());
    }
    let mut seen = BTreeSet::new();
    for (n, _, _) in &wanted {
        if !seen.insert(n.clone()) {
            return Err(format!(
                "two targets are named {n}; name them apart with --target"
            ));
        }
    }

    let mut out = Vec::new();
    for (name, table, url) in wanted {
        let span = info_span!("target", target = %name);
        let live = async {
            let mut control = control_session(&url, ctx).await?;
            refuse_if_loading(&mut control.conn, &url.server()).await?;
            let table = target::canonical_table(&mut control.conn, &table).await?;
            let d = target::describe(&mut control.conn, &table).await?;
            let (mut config, leaves, version, roster) = (d.config, d.leaves, d.version, d.roster);
            target::refuse_unlike_sessions(&config, roster.as_deref().unwrap_or(&[]))?;
            let local = target::local_host(&url.host);
            let hashes = target::library_hashes(&mut control.conn, &config, local).await;
            config.extend(hashes);
            // The benchmark's sessions set their own statement timeout; record theirs, not the
            // control session's.
            for (k, v) in config.iter_mut() {
                if k == "setting.statement_timeout" {
                    *v = ctx.statement_timeout.clone();
                }
            }
            let explain = explain::command(version);
            config.push(("explain".into(), explain.into()));
            let work = ctx.work.join(slug(&format!("{}__{}", url.database, name)));
            let composed = compose(set, &table, &work)?;
            refuse_unless_one_query(&mut control.conn, &composed.sql).await?;
            let config_id = target::config_id(&config);
            let t = Target {
                name: name.clone(),
                table,
                opts: session_options(&url, ctx)?,
                url: url.clone(),
                leaves,
                server_version_num: version,
                explain,
                config,
                config_id,
            };
            let settings: Vec<String> = t
                .config
                .iter()
                .filter_map(|(k, v)| k.strip_prefix("setting.").map(|k| format!("{k}={v}")))
                .collect();
            info!(
                url = %t.url.redacted,
                table = %t.table,
                version = %t.get("version"),
                server_version_num = t.server_version_num,
                explain = %t.explain,
                database = %t.get("database"),
                database_locale = %t
                    .config
                    .iter()
                    .filter_map(|(k, v)| k.strip_prefix("database.").map(|c| format!("{c}={v}")))
                    .collect::<Vec<_>>()
                    .join(" "),
                partitioning = %t.get("partitioning"),
                leaves = %t.get("leaves"),
                total_bytes = %t.get("total_bytes"),
                heap_bytes = %t.get("heap_bytes"),
                index_bytes = %t.get("index_bytes"),
                indexes = %t.get("indexes"),
                live_rows_estimate = %t.get("live_rows_estimate"),
                analyzed = %format!("{} .. {}", t.get("analyzed_earliest"), t.get("analyzed_latest")),
                leaves_never_analyzed = %t.get("leaves_never_analyzed"),
                config_id = %t.config_id,
                settings = %settings.join(" "),
                composed = %composed.dir.display(),
                "target"
            );
            Ok::<Live, String>(Live {
                target: t,
                control,
                composed,
                base: name.clone(),
                resolved: None,
                roster,
            })
        }
        .instrument(span)
        .await?;
        out.push(live);
    }

    if let Some((first, rest)) = out.split_first() {
        for other in rest {
            let d = differs_beyond_table(
                &first.composed,
                &first.target.table,
                &other.composed,
                &other.target.table,
            );
            if !d.is_empty() {
                return Err(format!(
                    "the composed questions for {} and {} differ beyond the table name: {:?}",
                    first.target.name, other.target.name, d
                ));
            }
        }
        info!(
            targets = out.len(),
            questions = set.questions.len(),
            "every target's composed questions are the same text but for the table name"
        );
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Timeout,
    Cancelled,
    Error,
    NotRun,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Timeout => "TIMEOUT",
            Status::Cancelled => "CANCELLED",
            Status::Error => "ERROR",
            Status::NotRun => "NOT_RUN",
        }
    }
}

fn classify(e: &sqlx::Error, w: &Watched) -> (Status, String) {
    let text = e.to_string();
    let status = match e {
        sqlx::Error::Database(d) => match d.code().as_deref() {
            Some("57014") if w.cancelled => Status::Cancelled,
            Some("57014") => Status::Timeout,
            Some("57P01") if w.terminated => Status::Cancelled,
            _ => Status::Error,
        },
        _ if w.terminated => Status::Cancelled,
        _ => Status::Error,
    };
    let why = if w.interrupted {
        format!("{text} (interrupted)")
    } else if w.cancelled {
        format!("{text} (backend memory {} kB above the cap)", w.peak_rss_kb)
    } else {
        text
    };
    (status, why)
}

/// The statements one repetition sends. A question with bound values is prepared first (not
/// timed), executed with its values, and deallocated.
pub struct Statements {
    /// Sent first, untimed: `BEGIN`, under an index set the `DROP INDEX` of every index the set
    /// turns off, then `READ_ONLY`. A failure here fails the repetition as an ERROR before the
    /// question.
    pub open: Vec<String>,
    /// Sent first on every path out once `open` has been sent (`ROLLBACK`), before any
    /// deallocation. A session where it fails is dropped.
    pub close: Vec<String>,
    /// Sent before the timed statement, untimed.
    pub prepare: Vec<String>,
    pub run: String,
    /// Sent after the timed statement.
    pub deallocate: Vec<String>,
    /// Sent before the EXPLAIN and before the plan-only EXPLAIN, untimed.
    pub explain_prepare: Vec<String>,
    pub explain: String,
    /// An EXPLAIN that plans and does not execute: what planning alone reads.
    pub plan_only: String,
    /// Sent after the EXPLAIN and after the plan-only EXPLAIN.
    pub explain_deallocate: Vec<String>,
}

const PREPARED: &str = "warren_bench_question";
const PLAN_ONLY: &str = "EXPLAIN (FORMAT JSON)";

/// The statements for question `q` composed as `sql`, on a target whose EXPLAIN command is
/// `explain` (see `explain::command`).
pub fn statements(q: &Question, sql: &str, explain: &str) -> Statements {
    if q.binds.is_empty() {
        Statements {
            open: vec![],
            close: vec![],
            prepare: vec![],
            run: sql.to_string(),
            deallocate: vec![],
            explain_prepare: vec![],
            explain: format!("{explain}\n{sql}"),
            plan_only: format!("{PLAN_ONLY}\n{sql}"),
            explain_deallocate: vec![],
        }
    } else {
        let args: Vec<String> = q.binds.iter().map(|v| literal(v)).collect();
        let exec = format!("EXECUTE {PREPARED}({})", args.join(", "));
        let prepare = vec![format!("PREPARE {PREPARED} AS\n{sql}")];
        let deallocate = vec![format!("DEALLOCATE {PREPARED}")];
        Statements {
            open: vec![],
            close: vec![],
            explain_prepare: prepare.clone(),
            explain: format!("{explain} {exec}"),
            plan_only: format!("{PLAN_ONLY} {exec}"),
            explain_deallocate: deallocate.clone(),
            prepare,
            run: exec,
            deallocate,
        }
    }
}

/// Sent last before the question: from here the repetition's transaction can write nothing.
pub const READ_ONLY: &str = "SET LOCAL transaction_read_only = on";

/// The statements for question `q` on a target. Every repetition runs inside a transaction it
/// rolls back, under an index set after the set's `DROP INDEX` statements, and read-only from the
/// question on.
pub fn statements_for(live: &Live, q: &Question) -> Statements {
    let sql = &live.composed.sql[&q.name];
    let st = statements(q, sql, live.target.explain);
    let (mut open, close) = match &live.resolved {
        Some(l) => (l.open(), l.close()),
        None => (vec!["BEGIN".to_string()], vec!["ROLLBACK".to_string()]),
    };
    open.push(READ_ONLY.to_string());
    Statements { open, close, ..st }
}

/// Refuses, by its question's name, any statement PostgreSQL does not parse as exactly one
/// statement that returns rows. Each is parsed on `conn`, outside any transaction, by the
/// protocol's own Parse, which takes one statement and runs nothing.
pub async fn refuse_unless_one_query(
    conn: &mut PgConnection,
    sql: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut refused = None;
    for (name, text) in sql {
        let why = match conn.prepare(text.as_str()).await {
            Err(e) => format!("PostgreSQL does not parse it as one statement: {e}"),
            Ok(st) if st.columns().is_empty() => "it returns no rows".to_string(),
            Ok(_) => continue,
        };
        refused = Some(format!(
            "{name}: refused, since a question is one statement that returns rows, and {why}"
        ));
        break;
    }
    conn.clear_cached_statements()
        .await
        .map_err(|e| format!("closing the parsed statements: {e}"))?;
    refused.map_or(Ok(()), Err)
}

/// Closes what `open` began: sends `close` (ROLLBACK). When it fails, the session is marked broken,
/// to be dropped, and the reason is returned.
async fn close_open(
    control: &mut PgConnection,
    session: &mut Session,
    guard: &Guard,
    st: &Statements,
) -> Result<(), String> {
    if st.close.is_empty() {
        return Ok(());
    }
    watched_each(control, session, guard, &st.close)
        .await
        .map_err(|(_, e)| {
            session.broken = true;
            error!(pid = session.pid, error = %e, "the repetition's ROLLBACK failed: the session is dropped");
            format!("the ROLLBACK failed, and the session was dropped: {e}")
        })
}

/// `error`, with what closing said added when it failed.
fn and_closed(error: String, closed: Result<(), String>) -> String {
    match closed {
        Ok(()) => error,
        Err(c) if error.is_empty() => c,
        Err(c) => format!("{error} | {c}"),
    }
}

/// Sends each of `sql` in turn on `session` under the watchdog, stopping at the first failure.
async fn watched_each(
    control: &mut PgConnection,
    session: &mut Session,
    guard: &Guard,
    sql: &[String],
) -> Result<(), (Status, String)> {
    for s in sql {
        watched_simple(control, session, guard, s).await?;
    }
    Ok(())
}

/// What planning alone reads: the counters around a plan-only EXPLAIN on `session`, which is left
/// with nothing pending. The planner can read rows itself (a column's actual extremes, through an
/// index), and those reads are in the timed statement's counters and in no scan node.
pub async fn planning_reads(
    control: &mut PgConnection,
    session: &mut Session,
    guard: &Guard,
    st: &Statements,
) -> Result<witness::Reading, String> {
    let planned = async {
        watched_each(control, session, guard, &st.open)
            .await
            .map_err(|(_, e)| format!("before the question: {e}"))?;
        watched_each(control, session, guard, &st.explain_prepare)
            .await
            .map_err(|(_, e)| e)?;
        // settled, so the session's earlier statements have all been counted before this reading
        let (before, _) = witness::settled(control).await?;
        fetch_explain(&mut session.conn, &st.plan_only)
            .await
            .map_err(|e| format!("plan-only EXPLAIN: {e}"))?;
        Ok::<witness::Counters, String>(before)
    }
    .await;
    let closed = close_open(control, session, guard, st).await;
    if !session.broken {
        if let Err((_, e)) = watched_each(control, session, guard, &st.explain_deallocate).await {
            warn!(error = %e, "deallocate failed");
        }
    }
    let before = planned.map_err(|e| and_closed(e, closed.clone()))?;
    closed?;
    witness::flush(&mut session.conn).await?;
    let (after, settled) = witness::settled(control).await?;
    Ok(witness::difference(&before, &after, settled))
}

/// Sends a statement and receives every row it returns; the time is from sending to the last row.
/// The rows are kept as sent, with each column's kind for the digest (see `canonical`).
async fn fetch_answer(
    conn: &mut PgConnection,
    sql: &str,
) -> Result<(RowBuffer, Duration), sqlx::Error> {
    let mut buf = RowBuffer::new();
    let start = Instant::now();
    let mut stream = sqlx::raw_sql(sql).fetch(&mut *conn);
    let mut first = true;
    while let Some(row) = stream.try_next().await? {
        if first {
            buf.set_kinds(
                row.columns()
                    .iter()
                    .map(|col| canonical::postgres(col.type_info().oid().map_or(0, |o| o.0)))
                    .collect(),
            );
            first = false;
        }
        for i in 0..row.len() {
            let v = row.try_get_raw(i)?;
            if v.is_null() {
                buf.push_value(None);
            } else {
                buf.push_value(Some(v.as_bytes().map_err(sqlx::Error::Decode)?));
            }
        }
        buf.end_row();
    }
    Ok((buf, start.elapsed()))
}

async fn fetch_explain(conn: &mut PgConnection, sql: &str) -> Result<String, sqlx::Error> {
    let rows = sqlx::raw_sql(sql).fetch_all(&mut *conn).await?;
    let row = rows.first().ok_or(sqlx::Error::RowNotFound)?;
    let v = row.try_get_raw(0)?;
    Ok(v.as_str().map_err(sqlx::Error::Decode)?.to_string())
}

pub struct Timed {
    pub status: Status,
    pub wall_ms: f64,
    pub answer: Option<Digest>,
    pub watched: Watched,
    pub error: String,
}

pub struct Explained {
    pub status: Status,
    pub facts: Option<Facts>,
    pub leaves: Option<Leaves>,
    pub watched: Watched,
    pub error: String,
}

/// Runs `sql` on `session` under the watchdog; `Err` holds the status and reason.
async fn watched_simple(
    control: &mut PgConnection,
    session: &mut Session,
    guard: &Guard,
    sql: &str,
) -> Result<(), (Status, String)> {
    let pid = session.pid;
    let (res, w) = watch(control, pid, guard, target::simple(&mut session.conn, sql)).await;
    res.map_err(|e| classify(&e, &w))
}

pub async fn run_timed(
    control: &mut PgConnection,
    session: &mut Session,
    guard: &Guard,
    st: &Statements,
) -> Timed {
    let failed = |status, error| Timed {
        status,
        wall_ms: 0.0,
        answer: None,
        watched: Watched::default(),
        error,
    };
    if let Err((_, error)) = watched_each(control, session, guard, &st.open).await {
        let closed = close_open(control, session, guard, st).await;
        return failed(
            Status::Error,
            and_closed(format!("before the question: {error}"), closed),
        );
    }
    if let Err((status, error)) = watched_each(control, session, guard, &st.prepare).await {
        let closed = close_open(control, session, guard, st).await;
        return failed(status, and_closed(error, closed));
    }
    let pid = session.pid;
    let (res, watched) = watch(
        control,
        pid,
        guard,
        fetch_answer(&mut session.conn, &st.run),
    )
    .await;
    let out = match res {
        Ok((buf, took)) => Timed {
            status: Status::Ok,
            wall_ms: took.as_secs_f64() * 1000.0,
            answer: Some(buf.digest()),
            watched,
            error: String::new(),
        },
        Err(e) => {
            let (status, error) = classify(&e, &watched);
            Timed {
                status,
                wall_ms: 0.0,
                answer: None,
                watched,
                error,
            }
        }
    };
    let closed = close_open(control, session, guard, st).await;
    if !session.broken {
        if let Err((_, e)) = watched_each(control, session, guard, &st.deallocate).await {
            warn!(error = %e, "deallocate failed");
        }
    }
    Timed {
        error: and_closed(out.error, closed),
        ..out
    }
}

pub async fn run_explain(
    control: &mut PgConnection,
    session: &mut Session,
    guard: &Guard,
    st: &Statements,
    leaves: &BTreeSet<String>,
    server_version_num: u32,
) -> Explained {
    let fail = |status, error| Explained {
        status,
        facts: None,
        leaves: None,
        watched: Watched::default(),
        error,
    };
    if let Err((_, error)) = watched_each(control, session, guard, &st.open).await {
        let closed = close_open(control, session, guard, st).await;
        return fail(
            Status::Error,
            and_closed(format!("before the question: {error}"), closed),
        );
    }
    if let Err((status, error)) = watched_each(control, session, guard, &st.explain_prepare).await {
        let closed = close_open(control, session, guard, st).await;
        return fail(status, and_closed(error, closed));
    }
    let pid = session.pid;
    let (res, watched) = watch(
        control,
        pid,
        guard,
        fetch_explain(&mut session.conn, &st.explain),
    )
    .await;
    let out = match res {
        Ok(text) => match explain::parse(&text, server_version_num) {
            Ok(f) => {
                let l = explain::leaves(&f, leaves);
                Explained {
                    status: Status::Ok,
                    facts: Some(f),
                    leaves: Some(l),
                    watched,
                    error: String::new(),
                }
            }
            Err(e) => Explained {
                status: Status::Error,
                facts: None,
                leaves: None,
                watched,
                error: e,
            },
        },
        Err(e) => {
            let (status, error) = classify(&e, &watched);
            Explained {
                status,
                facts: None,
                leaves: None,
                watched,
                error,
            }
        }
    };
    let closed = close_open(control, session, guard, st).await;
    if !session.broken {
        if let Err((_, e)) = watched_each(control, session, guard, &st.explain_deallocate).await {
            warn!(error = %e, "deallocate failed");
        }
    }
    Explained {
        error: and_closed(out.error, closed),
        ..out
    }
}

/// The expected answer of one question.
#[derive(Clone, Debug)]
pub struct Expected {
    pub rows: u64,
    pub digest: String,
}

/// One line per question: its expected answer, the question set it was computed for, and the
/// target it was computed on.
pub const EXPECTED: Declared = Declared {
    name: "expected answers",
    columns: &[
        ("question", Kind::Text),
        ("rows", Kind::Int),
        ("digest", Kind::Text),
        ("question_set", Kind::Text),
        ("reference", Kind::Text),
    ],
    key: &["question"],
};

/// One line per scan node of the first EXPLAIN of each (target, question): its place among the
/// plan's scans, its type, schema and relation, the index it read (a bitmap heap scan's bitmap index
/// scans' joined by `+`), and its figures: `rows` over every loop, and `plan_rows`, the planner's
/// estimate for one loop.
pub const LEAVES: Declared = Declared {
    name: "leaves",
    columns: &[
        ("target", Kind::Text),
        ("question", Kind::Text),
        ("node", Kind::Int),
        ("node_type", Kind::Text),
        ("schema", Kind::Text),
        ("relation", Kind::Text),
        ("index", Kind::Text),
        ("is_leaf", Kind::Bool),
        ("loops", Kind::Int),
        ("rows", Kind::Float),
        ("plan_rows", Kind::Float),
        ("index_searches", Kind::Int),
        ("index_cond", Kind::Text),
        ("heap_fetches", Kind::Int),
    ],
    key: &["target", "question", "node"],
};

/// The leaves as written before each scan recorded its estimate: read, never written.
pub const LEAVES_V2: Declared = Declared {
    name: "leaves (before scans recorded their estimate)",
    columns: &[
        ("target", Kind::Text),
        ("question", Kind::Text),
        ("node", Kind::Int),
        ("node_type", Kind::Text),
        ("schema", Kind::Text),
        ("relation", Kind::Text),
        ("index", Kind::Text),
        ("is_leaf", Kind::Bool),
        ("loops", Kind::Int),
        ("rows", Kind::Float),
        ("index_searches", Kind::Int),
        ("index_cond", Kind::Text),
        ("heap_fetches", Kind::Int),
    ],
    key: &["target", "question", "node"],
};

/// The leaves as written before each scan named its index: read, never written.
pub const LEAVES_V1: Declared = Declared {
    name: "leaves (before scans named their index)",
    columns: &[
        ("target", Kind::Text),
        ("question", Kind::Text),
        ("relation", Kind::Text),
        ("is_leaf", Kind::Bool),
        ("loops", Kind::Int),
        ("rows", Kind::Float),
    ],
    key: &["target", "question", "relation"],
};

/// One line per (target, configuration key).
pub const TARGETS: Declared = Declared {
    name: "targets",
    columns: &[
        ("target", Kind::Text),
        ("key", Kind::Text),
        ("value", Kind::Text),
    ],
    key: &["target", "key"],
};

/// Reads the expected answers, refusing a file made for another question set or missing a question.
pub fn read_expected(path: &Path, set: &QuestionSet) -> Result<BTreeMap<String, Expected>, String> {
    let t = tables::read(path, &EXPECTED)?;
    let mut out = BTreeMap::new();
    for r in &t.rows {
        let g = |k: &str| r.get(k).cloned().unwrap_or_default();
        if g("question_set") != set.hash {
            return Err(format!(
                "{}: made for question set {}, and the questions are now {}; the expected answers must be computed again before any run",
                path.display(),
                g("question_set"),
                set.hash
            ));
        }
        let rows = g("rows").parse().map_err(|_| {
            format!(
                "{}: {}: rows `{}`",
                path.display(),
                g("question"),
                g("rows")
            )
        })?;
        out.insert(
            g("question"),
            Expected {
                rows,
                digest: g("digest"),
            },
        );
    }
    let names: BTreeSet<String> = set
        .questions
        .iter()
        .filter(|q| q.derived_from.is_none())
        .map(|q| q.name.clone())
        .collect();
    let have: BTreeSet<String> = out.keys().cloned().collect();
    if names != have {
        return Err(format!(
            "{}: expected answers and questions differ: no answer for {:?}; answer for no question {:?}",
            path.display(),
            names.difference(&have).collect::<Vec<_>>(),
            have.difference(&names).collect::<Vec<_>>()
        ));
    }
    Ok(out)
}

/// Runs every question twice on the reference target, requires the same answer both times, and
/// writes the answers. No time is recorded.
pub async fn truth(
    live: &mut Live,
    set: &QuestionSet,
    ctx: &Ctx,
    out: &Path,
    force: bool,
) -> Result<(), String> {
    if out.exists() && !force {
        return Err(format!(
            "{} exists; the expected answers are fixed once. Give --force to compute them again",
            out.display()
        ));
    }
    let mut lines: Vec<Vec<String>> = Vec::new();
    for q in &set.questions {
        let span = info_span!("question", question = %q.name);
        let line = async {
            refuse_if_loading(&mut live.control.conn, &live.target.url.server()).await?;
            let st = statements_for(live, q);
            let mut digests = Vec::new();
            for _ in 0..2 {
                let mut s = target::open(&live.target.opts, &ctx.statement_timeout).await?;
                target::confirm_pid(&mut live.control.conn, s.pid).await?;
                let t = run_timed(&mut live.control.conn, &mut s, &ctx.guard, &st).await;
                s.close().await;
                if t.status != Status::Ok {
                    return Err(format!("{}: {} {}", q.name, t.status.word(), t.error));
                }
                digests.push(t.answer.unwrap());
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return Err("interrupted".into());
                }
            }
            if digests[0] != digests[1] {
                return Err(format!(
                    "{}: two runs gave different answers ({} and {})",
                    q.name, digests[0], digests[1]
                ));
            }
            info!(rows = digests[0].rows, digest = %digests[0].hex(), "expected answer");
            Ok::<Vec<String>, String>(vec![
                q.name.clone(),
                digests[0].rows.to_string(),
                digests[0].hex(),
                set.hash.clone(),
                format!(
                    "{} {}{}",
                    live.target.url.redacted,
                    live.target.table,
                    live.resolved
                        .as_ref()
                        .map(|l| format!(" index_set={}", l.set))
                        .unwrap_or_default()
                ),
            ])
        }
        .instrument(span)
        .await?;
        lines.push(line);
    }
    if let Some(p) = out.parent() {
        fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    tables::write(out, &EXPECTED, &lines)?;
    info!(file = %out.display(), questions = set.questions.len(), "expected answers written");
    Ok(())
}

pub struct Plan {
    pub cold: u32,
    pub warm: u32,
    /// A file whose existence stops the run before its next question.
    pub lock: Option<PathBuf>,
}

/// How the benchmark ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ending {
    Complete,
    StoppedForLoader,
    /// The committed indexes changed while a target ran under an index set.
    RosterChanged,
    /// The lock file appeared.
    StoppedForLock,
    Interrupted,
}

/// How the benchmark ended, and every scan that used an index its target's set had turned off.
#[derive(Debug)]
pub struct Finished {
    pub ending: Ending,
    pub unlawful: Vec<String>,
}

/// Whether the committed roster is still the one a set was resolved against; the reason when not.
async fn roster_unchanged(live: &mut Live) -> Result<(), String> {
    let Some(l) = &live.resolved else {
        return Ok(());
    };
    let now = index_sets::read_roster(&mut live.control.conn).await?;
    let fp = index_sets::roster_fingerprint(&now);
    if fp == l.fingerprint {
        Ok(())
    } else {
        Err(format!(
            "the committed indexes changed while {} ran under the index set {}: roster {} became {fp}",
            live.target.name, l.set, l.fingerprint
        ))
    }
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

/// RIGHT when the statement finished and its rows and digest are the expected ones (and the
/// EXPLAIN, where there was one, counted the same rows); WRONG when it finished otherwise; `-`
/// when it did not finish.
fn verdict(timed: Option<&Timed>, exp: &Expected, explain_rows_ok: Option<bool>) -> &'static str {
    let status = timed.map_or(Status::NotRun, |t| t.status);
    match timed.and_then(|t| t.answer) {
        Some(d) if status == Status::Ok => {
            if d.rows == exp.rows && d.hex() == exp.digest && explain_rows_ok != Some(false) {
                "RIGHT"
            } else {
                "WRONG"
            }
        }
        _ => "-",
    }
}

/// What the statistics counters said about one timed execution (see `witness`): its own reading,
/// and the reading of planning alone, which is the part of it no scan node reports.
pub enum Seen {
    Read {
        timed: Reading,
        planning: Option<Result<Reading, String>>,
    },
    /// The server predates `pg_stat_force_next_flush()`, or does not count (`track_counts` off).
    NotReported,
    Failed(String),
}

impl Seen {
    /// The reading's status: the timed one's, or planning's where planning's was not OK, since the
    /// law needs both.
    fn word(&self) -> &'static str {
        match self {
            Seen::Read {
                planning: Some(Err(_)),
                ..
            } => "ERROR",
            Seen::Read {
                planning: Some(Ok(p)),
                ..
            } if p.status != "OK" => p.status,
            Seen::Read { timed, .. } => timed.status,
            Seen::NotReported => "NOT_REPORTED",
            Seen::Failed(_) => "ERROR",
        }
    }
}

/// The counters' figures for one repetition, split by the target's leaves: everything the timed
/// statement read, what planning alone read, and the difference, which is what execution read.
struct Figures {
    status: &'static str,
    timed: Split,
    planning: Option<(&'static str, Split)>,
    execution: Option<Split>,
}

fn figures(seen: Option<&Seen>, t: &Target) -> Option<Figures> {
    let Some(Seen::Read { timed, planning }) = seen else {
        return None;
    };
    let schema = t.table.split_once('.').map_or("public", |(s, _)| s);
    let planning = planning.as_ref().and_then(|p| p.as_ref().ok());
    Some(Figures {
        status: timed.status,
        timed: witness::split(&timed.deltas, schema, &t.leaves),
        planning: planning.map(|p| (p.status, witness::split(&p.deltas, schema, &t.leaves))),
        execution: planning
            .map(|p| witness::split(&witness::minus(&timed.deltas, &p.deltas), schema, &t.leaves)),
    })
}

/// Whether execution and its EXPLAIN agree. Asked only when both readings were taken alone and
/// settled.
struct Agreement {
    /// The rows read, within the rounding of EXPLAIN's per-loop averages.
    rows: bool,
    /// The leaves: every leaf EXPLAIN certainly read rows from gave up rows by the counters, and
    /// every leaf that gave up rows by the counters was executed by EXPLAIN. Empty leaves, and
    /// leaves a parallel plan only initialised, drop out of both sides.
    leaves: bool,
    /// The leaves each side read and the other did not, when `leaves` is false.
    leaves_detail: String,
}

fn agreement(w: Option<&Figures>, l: Option<&Leaves>) -> Option<Agreement> {
    let (w, l) = (w?, l?);
    let (planning_status, _) = w.planning.as_ref()?;
    let e = w.execution.as_ref()?;
    if w.status != "OK" || *planning_status != "OK" {
        return None;
    }
    let near =
        |counted: i64, reported: f64, slack: f64| (counted as f64 - reported).abs() <= slack.ceil();
    let rows = near(e.leaf_rows, l.leaf_rows_read, l.leaf_slack)
        && near(e.other_rows, l.other_rows_read, l.other_slack);
    let unseen: Vec<&str> = l
        .yielded
        .difference(&e.yielded)
        .map(String::as_str)
        .collect();
    let unplanned: Vec<&str> = e
        .yielded
        .difference(&l.executed)
        .map(String::as_str)
        .collect();
    let mut leaves_detail = String::new();
    if !unplanned.is_empty() {
        leaves_detail.push_str(&format!(
            "rows on leaves EXPLAIN did not run: {}",
            unplanned.join(", ")
        ));
    }
    if !unseen.is_empty() {
        if !leaves_detail.is_empty() {
            leaves_detail.push_str("; ");
        }
        leaves_detail.push_str(&format!(
            "EXPLAIN rows on leaves the counters did not see: {}",
            unseen.join(", ")
        ));
    }
    Some(Agreement {
        rows,
        leaves: unseen.is_empty() && unplanned.is_empty(),
        leaves_detail,
    })
}

#[allow(clippy::too_many_arguments)]
fn result_line(
    t: &Target,
    q: &Question,
    phase: &str,
    rep: u32,
    session: u32,
    position: usize,
    exp: &Expected,
    timed: Option<&Timed>,
    ex: Option<&Explained>,
    seen: Option<&Seen>,
    note: &str,
) -> (Vec<String>, &'static str) {
    let status = timed.map_or(Status::NotRun, |t| t.status);
    let explain_rows_ok = ex
        .and_then(|e| e.facts.as_ref())
        .map(|f| f.rows.round() as u64 == exp.rows);
    let verdict = verdict(timed, exp, explain_rows_ok);
    let f = ex.and_then(|e| e.facts.as_ref());
    let l = ex.and_then(|e| e.leaves.as_ref());
    let w = figures(seen, t);
    let agree = agreement(w.as_ref(), l);
    let mut error = String::new();
    if let Some(t) = timed {
        error.push_str(&t.error);
    }
    if let Some(e) = ex {
        if !e.error.is_empty() {
            if !error.is_empty() {
                error.push_str(" | ");
            }
            error.push_str("explain: ");
            error.push_str(&e.error);
        }
    }
    let witness_error = match seen {
        Some(Seen::Failed(e)) => Some(e.as_str()),
        Some(Seen::Read {
            planning: Some(Err(e)),
            ..
        }) => Some(e.as_str()),
        _ => None,
    };
    if let Some(e) = witness_error {
        if !error.is_empty() {
            error.push_str(" | ");
        }
        error.push_str("witness: ");
        error.push_str(e);
    }
    if !note.is_empty() {
        if !error.is_empty() {
            error.push_str(" | ");
        }
        error.push_str(note);
    }
    // which leaves a plan read
    let leaf_set = l.map(|l| {
        let names: Vec<&[u8]> = l.planned.iter().map(|s| s.as_bytes()).collect();
        crate::digest::fingerprint(&names)
    });
    let fields = vec![
        t.name.clone(),
        t.config_id.clone(),
        q.name.clone(),
        q.spelling().to_string(),
        q.selection().to_string(),
        phase.to_string(),
        rep.to_string(),
        session.to_string(),
        position.to_string(),
        status.word().to_string(),
        verdict.to_string(),
        timed
            .filter(|t| t.status == Status::Ok)
            .map(|t| format!("{:.3}", t.wall_ms))
            .unwrap_or_default(),
        opt(timed.and_then(|t| t.answer).map(|d| d.rows)),
        opt(timed.and_then(|t| t.answer).map(|d| d.hex())),
        exp.rows.to_string(),
        exp.digest.clone(),
        opt(timed.map(|t| t.watched.peak_rss_kb)),
        opt(timed.map(|t| t.watched.peak_anon_kb)),
        ex.map_or("NOT_RUN", |e| e.status.word()).to_string(),
        opt(f.and_then(|f| f.planning_ms).map(|x| format!("{x:.3}"))),
        opt(f.and_then(|f| f.execution_ms).map(|x| format!("{x:.3}"))),
        opt(f.map(|f| format!("{}", f.rows))),
        opt(explain_rows_ok),
        opt(f.and_then(|f| f.shared_hit)),
        opt(f.and_then(|f| f.shared_read)),
        opt(f.and_then(|f| f.shared_dirtied)),
        opt(f.and_then(|f| f.shared_written)),
        opt(f.and_then(|f| f.temp_read)),
        opt(f.and_then(|f| f.temp_written)),
        opt(f.and_then(|f| f.planning_shared_hit)),
        opt(f.and_then(|f| f.planning_shared_read)),
        opt(f.and_then(|f| f.planner_memory_used_kb)),
        opt(f.and_then(|f| f.planner_memory_allocated_kb)),
        opt(f.and_then(|f| f.jit_ms).map(|x| format!("{x:.3}"))),
        opt(f.and_then(|f| f.workers_launched)),
        opt(l.map(|l| l.planned.len())),
        opt(l.map(|l| l.executed.len())),
        opt(l.map(|l| l.leaf_scans)),
        opt(l.map(|l| l.other_scans)),
        opt(f.and_then(|f| f.subplans_removed)),
        opt(leaf_set),
        opt(ex.map(|e| e.watched.peak_rss_kb)),
        opt(f.and_then(|f| f.plan_rows).map(|x| format!("{x}"))),
        opt(l.map(|l| format!("{}", l.leaf_rows_read))),
        opt(l.map(|l| format!("{}", l.other_rows_read))),
        seen.map_or("", Seen::word).to_string(),
        opt(w.as_ref().map(|w| {
            w.execution
                .as_ref()
                .map_or(w.timed.touched.len(), |e| e.touched.len())
        })),
        opt(w.as_ref().map(|w| w.timed.leaf_rows)),
        opt(w.as_ref().map(|w| w.timed.leaf_blocks)),
        opt(w.as_ref().map(|w| w.timed.other_rows)),
        opt(w.as_ref().map(|w| w.timed.other_blocks)),
        opt(w
            .as_ref()
            .and_then(|w| w.planning.as_ref())
            .map(|p| p.1.leaf_rows)),
        opt(w
            .as_ref()
            .and_then(|w| w.planning.as_ref())
            .map(|p| p.1.other_rows)),
        opt(agree.as_ref().map(|a| a.rows)),
        opt(agree.as_ref().map(|a| a.leaves)),
        agree
            .as_ref()
            .map(|a| a.leaves_detail.clone())
            .unwrap_or_default(),
        w.as_ref()
            .and_then(|w| w.execution.as_ref())
            .map(|e| witness::others_text(&e.others))
            .unwrap_or_default(),
        w.as_ref()
            .and_then(|w| w.planning.as_ref())
            .map(|p| witness::others_text(&p.1.others))
            .unwrap_or_default(),
        l.map(|l| explain::others_text(&l.others))
            .unwrap_or_default(),
        error,
    ];
    debug_assert_eq!(fields.len(), RESULTS.columns.len());
    (fields, verdict)
}

/// Refuses a directory that already holds a run's `results/` or `leaves/`, before anything is written
/// into it, so that no run overwrites another's files.
pub fn refuse_used(out_dir: &Path) -> Result<(), String> {
    for d in [out_dir.join("results"), out_dir.join("leaves")] {
        if d.exists() {
            return Err(format!(
                "{} exists; a run writes into a directory of its own",
                d.display()
            ));
        }
    }
    Ok(())
}

/// Runs the benchmark: every question on every target, one statement at a time. Questions are
/// taken in order; for each, the targets are taken in an order rotated by one per question.
/// Each target gets `cold` repetitions, each in new sessions, then `warm` repetitions in the last
/// of those sessions. A repetition is the timed statement, then its EXPLAIN in a second session.
/// After a repetition that does not finish, that target's remaining repetitions of the question
/// are recorded as not run. Each question's lines are written as one file in `results/` and
/// `leaves/` once every target has answered it, or the run has stopped.
pub async fn bench(
    lives: &mut [Live],
    set: &QuestionSet,
    expected: &BTreeMap<String, Expected>,
    ctx: &Ctx,
    plan: &Plan,
    out_dir: &Path,
) -> Result<Finished, String> {
    refuse_used(out_dir)?;
    let results = out_dir.join("results");
    let leaves = out_dir.join("leaves");
    for d in [&results, &leaves] {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }

    let mut ending = Ending::Complete;
    let mut unlawful: Vec<String> = Vec::new();
    let n = lives.len();
    'questions: for (qi, q) in set.questions.iter().enumerate() {
        let exp = &expected[q.answers_as()];
        let mut lines: Vec<Vec<String>> = Vec::new();
        let mut leaf_lines: Vec<Vec<String>> = Vec::new();
        let mut outcome: Result<Option<Ending>, String> = Ok(None);
        for pos in 0..n {
            let ti = (qi + pos) % n;
            let live = &mut lives[ti];
            let span = info_span!("question", question = %q.name, target = %live.target.name);
            let stop = async {
                let reps: Vec<(&str, u32, u32)> = (0..plan.cold)
                    .map(|c| ("cold", c, c))
                    .chain((0..plan.warm).map(|wi| ("warm", wi, plan.cold - 1)))
                    .collect();
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return Ok::<Option<Ending>, String>(Some(Ending::Interrupted));
                }
                if let Err(e) = refuse_if_loading(&mut live.control.conn, &live.target.url.server()).await {
                    error!(error = %e, "stopping the run");
                    return Ok(Some(Ending::StoppedForLoader));
                }
                if let Some(lock) = plan.lock.as_ref().filter(|l| l.exists()) {
                    error!(lock = %lock.display(), "the lock appeared: stopping the run");
                    return Ok(Some(Ending::StoppedForLock));
                }
                let st = statements_for(live, q);
                let witnessed = witness::reports(live.target.server_version_num, live.target.get("setting.track_counts"));
                let mut sessions: Option<(Session, Session)> = None;
                let mut failed: Option<String> = None;
                let mut leaves_written = false;
                for (phase, rep, session_no) in reps {
                    if let Some(why) = &failed {
                        let (fields, _) = result_line(&live.target, q, phase, rep, session_no, pos + 1, exp, None, None, None, why);
                        lines.push(fields);
                        continue;
                    }
                    if phase == "cold" {
                        if let Some((a, b)) = sessions.take() {
                            a.close().await;
                            b.close().await;
                        }
                        let a = target::open(&live.target.opts, &ctx.statement_timeout).await?;
                        let b = target::open(&live.target.opts, &ctx.statement_timeout).await?;
                        target::confirm_pid(&mut live.control.conn, a.pid).await?;
                        target::confirm_pid(&mut live.control.conn, b.pid).await?;
                        sessions = Some((a, b));
                    }
                    let (ts, es) = sessions.as_mut().unwrap();
                    // The counters are read on the control session, outside the timed window: once
                    // before, and once the timed session has sent its statistics.
                    let before = if witnessed {
                        Some(witness::read(&mut live.control.conn).await)
                    } else {
                        None
                    };
                    let timed = run_timed(&mut live.control.conn, ts, &ctx.guard, &st).await;
                    let mut seen = match before {
                        None => Some(Seen::NotReported),
                        Some(Err(e)) => Some(Seen::Failed(e)),
                        Some(Ok(b)) => match witness::flush(&mut ts.conn).await {
                            Err(e) => Some(Seen::Failed(e)),
                            Ok(()) if timed.status != Status::Ok => None,
                            Ok(()) => match witness::settled(&mut live.control.conn).await {
                                Ok((a, settled)) => Some(Seen::Read {
                                    timed: witness::difference(&b, &a, settled),
                                    planning: None,
                                }),
                                Err(e) => Some(Seen::Failed(e)),
                            },
                        },
                    };
                    let ex = if timed.status == Status::Ok {
                        // the EXPLAIN first, so a cold EXPLAIN plans in a session nothing has
                        // warmed; planning's own reads are measured after it
                        let ex = run_explain(
                            &mut live.control.conn,
                            es,
                            &ctx.guard,
                            &st,
                            &live.target.leaves,
                            live.target.server_version_num,
                        ).await;
                        if witnessed {
                            if let Err(e) = witness::flush(&mut es.conn).await {
                                warn!(error = %e, "the EXPLAIN session's statistics were not sent");
                            }
                        }
                        if let Some(Seen::Read { planning, .. }) = &mut seen {
                            *planning = Some(planning_reads(&mut live.control.conn, es, &ctx.guard, &st).await);
                        }
                        Some(ex)
                    } else {
                        None
                    };
                    let (fields, verdict) = result_line(&live.target, q, phase, rep, session_no, pos + 1, exp, Some(&timed), ex.as_ref(), seen.as_ref(), "");
                    lines.push(fields);
                    if verdict == "WRONG" {
                        error!(phase, rep, rows = ?timed.answer.map(|d| d.rows), expected_rows = exp.rows, "WRONG answer");
                    }
                    info!(
                        phase,
                        rep,
                        status = timed.status.word(),
                        verdict,
                        wall_ms = format!("{:.3}", timed.wall_ms),
                        planning_ms = ?ex.as_ref().and_then(|e| e.facts.as_ref()).and_then(|f| f.planning_ms),
                        execution_ms = ?ex.as_ref().and_then(|e| e.facts.as_ref()).and_then(|f| f.execution_ms),
                        leaves = ?ex.as_ref().and_then(|e| e.leaves.as_ref()).map(|l| l.planned.len()),
                        peak_rss_kb = timed.watched.peak_rss_kb,
                        witness = seen.as_ref().map_or("", Seen::word),
                        rows_read = ?figures(seen.as_ref(), &live.target).map(|w| w.timed.leaf_rows),
                        "repetition"
                    );
                    if let (Some(l), Some(f)) = (&live.resolved, ex.as_ref().and_then(|e| e.facts.as_ref())) {
                        for u in l.unlawful(f) {
                            error!(phase, rep, law = %u, "a scan used an index its set turned off");
                            unlawful.push(format!("{} {} {phase} {rep}: {u}", live.target.name, q.name));
                        }
                    }
                    if let (false, Some(f)) = (leaves_written, ex.as_ref().and_then(|e| e.facts.as_ref())) {
                        for s in &f.scans {
                            leaf_lines.push(vec![
                                live.target.name.clone(),
                                q.name.clone(),
                                s.node.to_string(),
                                s.node_type.clone(),
                                s.schema.clone().unwrap_or_default(),
                                s.relation.clone(),
                                s.index().unwrap_or_default(),
                                live.target.leaves.contains(&s.relation).to_string(),
                                s.loops.to_string(),
                                format!("{}", s.rows),
                                opt(s.plan_rows),
                                opt(s.index_searches),
                                s.index_cond.clone().unwrap_or_default(),
                                opt(s.heap_fetches),
                            ]);
                        }
                        leaves_written = true;
                    }
                    if timed.status != Status::Ok {
                        failed = Some(format!(
                            "not run: an earlier repetition ended {}",
                            timed.status.word()
                        ));
                        sessions = None;
                    } else if ts.broken || es.broken {
                        failed = Some("not run: an earlier repetition's ROLLBACK failed, and its session was dropped".into());
                        sessions = None;
                    }
                    if timed.watched.interrupted || INTERRUPTED.load(Ordering::SeqCst) {
                        return Ok(Some(Ending::Interrupted));
                    }
                }
                if let Some((a, b)) = sessions.take() {
                    a.close().await;
                    b.close().await;
                }
                if let Err(e) = roster_unchanged(live).await {
                    error!(error = %e, "stopping the run");
                    return Ok(Some(Ending::RosterChanged));
                }
                Ok(None)
            }
            .instrument(span)
            .await;
            if !matches!(stop, Ok(None)) {
                outcome = stop;
                break;
            }
        }
        let part = format!("part-{:04}.parquet", qi + 1);
        tables::write(&results.join(&part), &RESULTS, &lines)?;
        tables::write(&leaves.join(&part), &LEAVES, &leaf_lines)?;
        if let Some(e) = outcome? {
            ending = e;
            break 'questions;
        }
    }
    info!(dir = %results.display(), ending = ?ending, unlawful = unlawful.len(), "results written");
    Ok(Finished { ending, unlawful })
}

/// Adds a family's statements to every target's questions, and its fingerprint to every target's
/// configuration. The family must hold a directory for every target, and none for any other; a
/// target under an index set takes the directory of the target it was named as. Each statement is
/// refused unless PostgreSQL parses it as one statement that returns rows.
pub async fn attach(lives: &mut [Live], family: &Family) -> Result<(), String> {
    let names: BTreeSet<String> = lives.iter().map(|l| l.base.clone()).collect();
    derived::same_targets(family, &names)?;
    for l in lives.iter_mut() {
        refuse_unless_one_query(&mut l.control.conn, &family.sql[&l.base]).await?;
        for (q, text) in &family.sql[&l.base] {
            l.composed.sql.insert(q.clone(), text.clone());
        }
        let t = &mut l.target;
        t.config.push((
            format!("derived.{}", family.name),
            family.fingerprints[&l.base].clone(),
        ));
        t.config_id = target::config_id(&t.config);
    }
    info!(family = %family.name, dir = %family.dir.display(), statements = family.questions.len(), "derived statements attached to every target");
    Ok(())
}

/// One line per (target, question) that `check` ran.
pub const CHECK: Declared = Declared {
    name: "answers checked",
    columns: &[
        ("target", Kind::Text),
        ("question", Kind::Text),
        ("answers_as", Kind::Text),
        ("statement", Kind::Text),
        ("status", Kind::Text),
        ("verdict", Kind::Text),
        ("rows", Kind::Int),
        ("digest", Kind::Text),
        ("expected_rows", Kind::Int),
        ("expected_digest", Kind::Text),
        ("error", Kind::Text),
    ],
    key: &["target", "question"],
};

/// Runs each of `questions` once on every target, in a new session, and checks the answer. No
/// time and no plan is recorded. Writes one line per (target, question) to `out`.
pub async fn check(
    lives: &mut [Live],
    questions: &[&Question],
    expected: &BTreeMap<String, Expected>,
    ctx: &Ctx,
    out: &Path,
    force: bool,
) -> Result<Ending, String> {
    if out.exists() && !force {
        return Err(format!(
            "{} exists; give --force to replace it",
            out.display()
        ));
    }
    let mut lines: Vec<Vec<String>> = Vec::new();
    let mut ending = Ending::Complete;
    'questions: for q in questions {
        let exp = &expected[q.answers_as()];
        for live in lives.iter_mut() {
            let span = info_span!("check", question = %q.name, target = %live.target.name);
            let line = async {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    return Ok::<Option<Vec<String>>, String>(None);
                }
                refuse_if_loading(&mut live.control.conn, &live.target.url.server()).await?;
                let st = statements_for(live, q);
                let sql = &live.composed.sql[&q.name];
                let mut s = target::open(&live.target.opts, &ctx.statement_timeout).await?;
                target::confirm_pid(&mut live.control.conn, s.pid).await?;
                let t = run_timed(&mut live.control.conn, &mut s, &ctx.guard, &st).await;
                s.close().await;
                let v = verdict(Some(&t), exp, None);
                if v == "WRONG" {
                    error!(rows = ?t.answer.map(|d| d.rows), expected_rows = exp.rows, "WRONG answer");
                } else {
                    info!(status = t.status.word(), verdict = v, "checked");
                }
                Ok(Some(vec![
                    live.target.name.clone(),
                    q.name.clone(),
                    q.answers_as().to_string(),
                    fingerprint(&[sql.as_bytes()]),
                    t.status.word().to_string(),
                    v.to_string(),
                    opt(t.answer.map(|d| d.rows)),
                    opt(t.answer.map(|d| d.hex())),
                    exp.rows.to_string(),
                    exp.digest.clone(),
                    t.error,
                ]))
            }
            .instrument(span)
            .await?;
            match line {
                Some(l) => lines.push(l),
                None => {
                    ending = Ending::Interrupted;
                    break 'questions;
                }
            }
            if let Err(e) = roster_unchanged(live).await {
                error!(error = %e, "stopping the check");
                ending = Ending::RosterChanged;
                break 'questions;
            }
        }
    }
    if let Some(p) = out.parent() {
        fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    tables::write(out, &CHECK, &lines)?;
    info!(file = %out.display(), lines = lines.len(), "answers checked");
    Ok(ending)
}

/// Writes each target's configuration, one line per (target, key).
pub fn write_targets(lives: &[Live], path: &Path) -> Result<(), String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for l in lives {
        let t = &l.target;
        let mut kv: Vec<(String, String)> = vec![
            ("url".into(), t.url.redacted.clone()),
            ("config_id".into(), t.config_id.clone()),
            ("composed".into(), l.composed.dir.display().to_string()),
        ];
        kv.extend(t.config.iter().cloned());
        rows.extend(kv.into_iter().map(|(k, v)| vec![t.name.clone(), k, v]));
    }
    tables::write(path, &TARGETS, &rows)
}

/// Writes every target's indexes, and which of them its index set held present: one line
/// per (index set, target, index). Writes nothing when no target has a roster.
pub fn write_indexes(lives: &[Live], path: &Path) -> Result<(), String> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for l in lives {
        match (&l.resolved, &l.roster) {
            (Some(resolved), _) => rows.extend(index_sets::index_rows(
                Some(&resolved.set),
                &l.base,
                &resolved.roster,
                &resolved.off,
            )),
            (None, Some(r)) => {
                rows.extend(index_sets::index_rows(None, &l.base, r, &BTreeSet::new()))
            }
            (None, None) => {}
        }
    }
    if lives.iter().all(|l| l.roster.is_none()) {
        return Ok(());
    }
    tables::write(path, &index_sets::INDEXES, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_is_as_good_as_its_worse_half() {
        let r = |status| Reading {
            status,
            deltas: witness::Deltas::new(),
        };
        let seen = |planning| Seen::Read {
            timed: r("OK"),
            planning,
        };
        assert_eq!(seen(None).word(), "OK");
        assert_eq!(seen(Some(Ok(r("OK")))).word(), "OK");
        assert_eq!(seen(Some(Ok(r("NOT_ALONE")))).word(), "NOT_ALONE");
        assert_eq!(seen(Some(Err("x".into()))).word(), "ERROR");
        let unsettled = Seen::Read {
            timed: r("UNSETTLED"),
            planning: Some(Ok(r("OK"))),
        };
        assert_eq!(unsettled.word(), "UNSETTLED");
    }

    fn target() -> Target {
        let url = Url::parse("postgres://localhost:5432/db").unwrap();
        Target {
            name: "t".into(),
            table: "s.t".into(),
            opts: url.session_options().unwrap(),
            url,
            leaves: BTreeSet::new(),
            server_version_num: 180_006,
            explain: explain::command(180_006),
            config: vec![],
            config_id: "c".into(),
        }
    }

    fn column(name: &str) -> usize {
        RESULTS.names().iter().position(|c| *c == name).unwrap()
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warren-bench-run-{tag}-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_directory_holding_a_run_is_refused_before_anything_is_written_into_it() {
        let d = scratch("used");
        assert!(refuse_used(&d).is_ok());
        for kind in ["results", "leaves"] {
            fs::create_dir_all(d.join(kind)).unwrap();
            let e = refuse_used(&d).unwrap_err();
            assert!(
                e.contains(kind) && e.contains("directory of its own"),
                "{e}"
            );
            fs::remove_dir_all(d.join(kind)).unwrap();
        }
        fs::remove_dir_all(&d).unwrap();
    }

    fn timed(rows: &[&str]) -> Timed {
        let mut b = RowBuffer::new();
        for r in rows {
            b.push_row([Some(r.as_bytes())]);
        }
        Timed {
            status: Status::Ok,
            wall_ms: 1.0,
            answer: Some(b.digest()),
            watched: Watched::default(),
            error: String::new(),
        }
    }

    fn expected(rows: &[&str]) -> Expected {
        let mut b = RowBuffer::new();
        for r in rows {
            b.push_row([Some(r.as_bytes())]);
        }
        let d = b.digest();
        Expected {
            rows: d.rows,
            digest: d.hex(),
        }
    }

    fn q() -> Question {
        Question {
            name: "predicate/scopes/x".into(),
            binds: vec![],
            derived_from: None,
        }
    }

    fn names(xs: &[&str]) -> BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// Counters for one repetition: planning read nothing, execution read `leaves` as
    /// `(leaf, scans, rows)`.
    fn counted(leaves: &[(&str, i64, i64)]) -> Figures {
        let deltas: witness::Deltas = leaves
            .iter()
            .map(|(l, s, r)| (format!("s.{l}"), (*s, *r, *r)))
            .collect();
        let all = names(&leaves.iter().map(|(l, _, _)| *l).collect::<Vec<_>>());
        let e = witness::split(&deltas, "s", &all);
        Figures {
            status: "OK",
            timed: e.clone(),
            planning: Some(("OK", Split::default())),
            execution: Some(e),
        }
    }

    /// An EXPLAIN that executed `executed`, reading `rows` in all from `yielded`.
    fn reported(executed: &[&str], yielded: &[&str], rows: f64) -> Leaves {
        Leaves {
            planned: names(executed),
            executed: names(executed),
            leaf_scans: executed.len(),
            other_scans: 0,
            leaf_rows_read: rows,
            other_rows_read: 0.0,
            leaf_slack: 0.0,
            other_slack: 0.0,
            yielded: names(yielded),
            others: BTreeMap::new(),
        }
    }

    #[test]
    fn a_leaf_a_parallel_plan_only_initialised_does_not_break_the_law() {
        // b was started by a parallel scan's initialisation and then pruned during execution: the
        // counters show a scan with no rows, and the EXPLAIN never ran it.
        let w = counted(&[("a", 1, 10), ("b", 1, 0)]);
        let l = reported(&["a"], &["a"], 10.0);
        let a = agreement(Some(&w), Some(&l)).unwrap();
        assert!(a.rows && a.leaves, "{}", a.leaves_detail);
    }

    #[test]
    fn an_empty_leaf_both_sides_scanned_does_not_break_the_law() {
        let w = counted(&[("a", 1, 10), ("e", 1, 0)]);
        let l = reported(&["a", "e"], &["a"], 10.0);
        assert!(agreement(Some(&w), Some(&l)).unwrap().leaves);
    }

    #[test]
    fn rows_from_a_leaf_the_explain_did_not_run_break_the_law_and_are_named() {
        let w = counted(&[("a", 1, 10), ("c", 1, 4)]);
        let l = reported(&["a"], &["a"], 10.0);
        let a = agreement(Some(&w), Some(&l)).unwrap();
        assert!(!a.leaves);
        assert!(!a.rows);
        assert_eq!(a.leaves_detail, "rows on leaves EXPLAIN did not run: c");
        let l = reported(&["a", "d"], &["a", "d"], 14.0);
        let w = counted(&[("a", 1, 14)]);
        let a = agreement(Some(&w), Some(&l)).unwrap();
        assert_eq!(
            a.leaves_detail,
            "EXPLAIN rows on leaves the counters did not see: d"
        );
    }

    #[test]
    fn the_right_answer_is_right() {
        let (_, v) = result_line(
            &target(),
            &q(),
            "warm",
            0,
            0,
            1,
            &expected(&["a", "b"]),
            Some(&timed(&["b", "a"])),
            None,
            None,
            "",
        );
        assert_eq!(v, "RIGHT");
    }

    #[test]
    fn a_seeded_wrong_answer_is_wrong() {
        // the same number of rows, one value changed
        let (fields, v) = result_line(
            &target(),
            &q(),
            "warm",
            0,
            0,
            1,
            &expected(&["a", "b"]),
            Some(&timed(&["a", "c"])),
            None,
            None,
            "",
        );
        assert_eq!(v, "WRONG");
        let i = RESULTS
            .names()
            .iter()
            .position(|c| *c == "verdict")
            .unwrap();
        assert_eq!(fields[i], "WRONG");
    }

    #[test]
    fn an_explain_that_counts_other_rows_makes_the_answer_wrong() {
        let f = Facts {
            rows: 3.0,
            ..Facts::default()
        };
        let ex = Explained {
            status: Status::Ok,
            facts: Some(f),
            leaves: None,
            watched: Watched::default(),
            error: String::new(),
        };
        let (_, v) = result_line(
            &target(),
            &q(),
            "warm",
            0,
            0,
            1,
            &expected(&["a", "b"]),
            Some(&timed(&["a", "b"])),
            Some(&ex),
            None,
            "",
        );
        assert_eq!(v, "WRONG");
    }

    #[test]
    fn a_derived_statement_is_checked_against_the_question_it_answers_as() {
        let d = Question {
            name: "derived/scopes/x".into(),
            binds: vec![],
            derived_from: Some("directive/scopes/x".into()),
        };
        let mut exp = BTreeMap::new();
        exp.insert("directive/scopes/x".to_string(), expected(&["a", "b"]));
        exp.insert("predicate/scopes/x".to_string(), expected(&["z"]));
        let e = &exp[d.answers_as()];
        assert_eq!(verdict(Some(&timed(&["b", "a"])), e, None), "RIGHT");
        assert_eq!(verdict(Some(&timed(&["a"])), e, None), "WRONG");
        assert_eq!(verdict(Some(&timed(&["z"])), e, None), "WRONG");
        assert_eq!(verdict(None, e, None), "-");
    }

    #[test]
    fn a_timeout_has_no_verdict() {
        let mut t = timed(&[]);
        t.status = Status::Timeout;
        t.answer = None;
        let (fields, v) = result_line(
            &target(),
            &q(),
            "cold",
            0,
            0,
            1,
            &expected(&["a"]),
            Some(&t),
            None,
            None,
            "",
        );
        assert_eq!(v, "-");
        let i = RESULTS.names().iter().position(|c| *c == "status").unwrap();
        assert_eq!(fields[i], "TIMEOUT");
        assert_eq!(fields.len(), RESULTS.columns.len());
    }

    #[test]
    fn bound_values_are_prepared_then_executed() {
        let q = Question {
            name: "directive/text/set_parts".into(),
            binds: vec!["75053-1".into()],
            derived_from: None,
        };
        let st = statements(&q, "SELECT 1 WHERE $1 = $1", explain::command(180_006));
        assert_eq!(st.prepare.len(), 1);
        assert!(st.prepare[0].starts_with("PREPARE warren_bench_question AS"));
        assert_eq!(st.explain_prepare, st.prepare);
        assert_eq!(st.run, "EXECUTE warren_bench_question('75053-1')");
        assert!(st
            .explain
            .ends_with("EXECUTE warren_bench_question('75053-1')"));
        assert_eq!(st.deallocate, vec!["DEALLOCATE warren_bench_question"]);
        assert_eq!(st.explain_deallocate, st.deallocate);
    }

    #[test]
    fn the_explain_sent_is_the_targets_own() {
        let bound = Question {
            name: "directive/text/set_parts".into(),
            binds: vec!["75053-1".into()],
            derived_from: None,
        };
        for q in [q(), bound] {
            let old = statements(&q, "SELECT 1", explain::command(130_023));
            assert!(
                old.explain
                    .starts_with("EXPLAIN (ANALYZE, BUFFERS, SUMMARY, VERBOSE, FORMAT JSON)"),
                "{}",
                old.explain
            );
            let new = statements(&q, "SELECT 1", explain::command(180_006));
            assert!(
                new.explain.starts_with(
                    "EXPLAIN (ANALYZE, BUFFERS, SUMMARY, MEMORY, VERBOSE, FORMAT JSON)"
                ),
                "{}",
                new.explain
            );
            assert_eq!(old.run, new.run);
        }
    }

    #[test]
    fn planner_memory_the_server_did_not_report_is_null_in_the_results() {
        let f = Facts {
            planning_ms: Some(2.4),
            execution_ms: Some(3.3),
            rows: 2.0,
            shared_hit: Some(0),
            shared_read: Some(503),
            planning_shared_hit: Some(437),
            planning_shared_read: Some(1),
            ..Facts::default()
        };
        let ex = Explained {
            status: Status::Ok,
            facts: Some(f),
            leaves: None,
            watched: Watched::default(),
            error: String::new(),
        };
        let (fields, v) = result_line(
            &target(),
            &q(),
            "warm",
            0,
            0,
            1,
            &expected(&["a", "b"]),
            Some(&timed(&["a", "b"])),
            Some(&ex),
            None,
            "",
        );
        assert_eq!(v, "RIGHT");
        assert_eq!(fields[column("planner_memory_used_kb")], "");
        assert_eq!(fields[column("planner_memory_allocated_kb")], "");
        assert_eq!(fields[column("planning_shared_hit")], "437");
        assert_eq!(fields[column("shared_hit")], "0");
        assert_eq!(fields[column("shared_dirtied")], "");
        let d = scratch("results");
        let p = d.join("part-0001.parquet");
        tables::write(&p, &RESULTS, &[fields]).unwrap();
        let back = tables::read_values(&p, &RESULTS).unwrap();
        fs::remove_dir_all(&d).unwrap();
        use crate::tables::Value;
        assert_eq!(back[0][column("planner_memory_used_kb")], Value::Null);
        assert_eq!(back[0][column("planner_memory_allocated_kb")], Value::Null);
        assert_eq!(back[0][column("planning_shared_hit")], Value::Int(437));
        assert_eq!(back[0][column("shared_hit")], Value::Int(0));
    }

    #[test]
    fn a_setting_or_column_the_server_lacks_is_recorded_as_null() {
        let mut config = target::named(
            "setting",
            vec![
                ("io_method".into(), None),
                ("jit".into(), Some("on".into())),
            ],
        );
        config.extend(target::named(
            "database",
            vec![
                ("datlocprovider".into(), None),
                ("datcollate".into(), Some("C.UTF-8".into())),
            ],
        ));
        let rows: Vec<Vec<String>> = config
            .iter()
            .map(|(k, v)| vec!["t".into(), k.clone(), v.clone()])
            .collect();
        let d = scratch("targets");
        let p = d.join("targets.parquet");
        tables::write(&p, &TARGETS, &rows).unwrap();
        let back = tables::read_values(&p, &TARGETS).unwrap();
        fs::remove_dir_all(&d).unwrap();
        use crate::tables::Value;
        let value = |key: &str| {
            back.iter()
                .find(|r| r[1] == Value::Text(key.into()))
                .map(|r| r[2].clone())
                .unwrap()
        };
        assert_eq!(value("setting.io_method"), Value::Null);
        assert_eq!(value("setting.jit"), Value::Text("on".into()));
        assert_eq!(value("database.datlocprovider"), Value::Null);
        assert_eq!(value("database.datcollate"), Value::Text("C.UTF-8".into()));
    }
}
