// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests against a Postgres server. They run only when asked (`cargo test -- --ignored
//! --test-threads=1`), against the server `BLOCKER_DB_URL` names; the database that URL names is
//! the one connected to while the database `warren_bench_check` is made and dropped. Every test
//! makes that database afresh, with one small table holding a column of each type an answer's
//! digest reads, and drops it when it ends.

// each test holds the lock for its whole length, awaits included, on a runtime of its own
#![allow(clippy::await_holding_lock)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sqlx::postgres::PgConnection;
use tracing_subscriber::prelude::*;

use crate::digest::Digest;
use crate::explain;
use crate::notice;
use crate::questions::{Question, QuestionSet};
use crate::report::RESULTS;
use crate::run::{self, Ctx, Live, Plan, Spec, Statements, Status};
use crate::tables::{self, Value};
use crate::target::{self, PlanSent, Session, Url};
use crate::watchdog::Guard;
use crate::witness;

/// The program's layer that catches the server's notices, for this thread while the guard lives.
fn catching_notices() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(tracing_subscriber::registry().with(notice::layer()))
}

const DB: &str = "warren_bench_check";

/// The tests share one database, so they run one at a time.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn server() -> Url {
    let pg = std::env::var("BLOCKER_DB_URL")
        .expect("BLOCKER_DB_URL must name the Postgres server to test against, as postgres://host:port/database");
    Url::parse(&pg).unwrap()
}

/// The same server and credentials, on the database `DB`.
fn on_check(url: &Url) -> Url {
    let scheme_end = url.text.find("://").unwrap() + 3;
    let slash = scheme_end + url.text[scheme_end..].find('/').unwrap();
    let query = url.text[slash..]
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    Url::parse(&format!("{}/{DB}{query}", &url.text[..slash])).unwrap()
}

async fn session(url: &Url, timeout: &str) -> Session {
    target::open(&url.session_options().unwrap(), timeout)
        .await
        .unwrap()
}

async fn exec(conn: &mut PgConnection, sql: &str) {
    target::simple(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

const TABLE: &str = "CREATE TABLE law (
    id integer NOT NULL, name varchar(40), flag boolean, amount numeric(10,2),
    ratio double precision, day date, at timestamp(6), instant timestamptz, big bigint)";

/// The rows, each instant written with an offset. The last row is there twice.
const ROWS: &str = "INSERT INTO law VALUES
    (1, 'naïve ☂', true, 12.50, 0.30000000000000004, '2020-02-29', '2020-02-29 23:59:59.5',
     '2021-03-28 01:30:00+01', 9223372036854775807),
    (2, '75053-1', false, -0.10, 1e100, '1999-12-31', '1999-12-31 00:00:00',
     '1999-12-31 23:00:00-01', -5),
    (3, '', true, 0, -2.5e-7, NULL, NULL, NULL, NULL),
    (4, NULL, NULL, NULL, NULL, '2000-01-01', '2000-01-01 12:00:00.000001',
     '2000-01-01 00:00:00+00', 0),
    (4, NULL, NULL, NULL, NULL, '2000-01-01', '2000-01-01 12:00:00.000001',
     '2000-01-01 00:00:00+00', 0)";

/// Makes `DB` afresh, with the table `law` and its rows. Returns the URL of `DB`.
async fn fixture() -> Url {
    let pg = server();
    drop_check(&pg).await;
    let mut a = session(&pg, "60s").await;
    exec(&mut a.conn, &format!("CREATE DATABASE {DB}")).await;
    a.close().await;
    let db = on_check(&pg);
    let mut a = session(&db, "60s").await;
    exec(&mut a.conn, TABLE).await;
    exec(&mut a.conn, ROWS).await;
    exec(&mut a.conn, "ANALYZE law").await;
    a.close().await;
    db
}

/// Drops `DB`.
async fn drop_check(pg: &Url) {
    let mut a = session(pg, "60s").await;
    let started = Instant::now();
    // a session to it that has just been closed may still be ending
    while let Err(e) = target::simple(&mut a.conn, &format!("DROP DATABASE IF EXISTS {DB}")).await {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "DROP DATABASE {DB}: {e}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    a.close().await;
}

async fn cleanup() {
    drop_check(&server()).await;
}

fn guard(cap_kb: u64) -> Guard {
    Guard {
        cap_kb,
        interval: Duration::from_millis(50),
        grace: Duration::from_secs(10),
        poll_memory: true,
    }
}

fn plain(name: &str) -> Question {
    Question {
        name: name.into(),
        binds: vec![],
        derived_from: None,
    }
}

/// Runs the statements once on `url`'s database under `guard`, from a control session and a
/// session of their own.
async fn timed(url: &Url, st: &Statements, g: &Guard, timeout: &str) -> run::Timed {
    let mut control = session(url, "30s").await;
    let mut s = session(url, timeout).await;
    let t = run::run_timed(&mut control.conn, &mut s, g, st).await;
    s.close().await;
    control.close().await;
    t
}

async fn digest_of_law(url: &Url) -> Digest {
    let st = run::statements(&plain("q/all"), "SELECT * FROM law", "EXPLAIN");
    let t = timed(url, &st, &guard(u64::MAX), "60s").await;
    assert_eq!(t.status, Status::Ok, "{}", t.error);
    t.answer.unwrap()
}

#[tokio::test]
#[ignore]
async fn the_same_rows_give_the_same_digest_and_one_changed_value_changes_it() {
    let _one = one_at_a_time();
    let db = fixture().await;
    let a = digest_of_law(&db).await;
    assert_eq!(a.rows, 5);
    assert_eq!(digest_of_law(&db).await, a);
    let mut c = session(&db, "60s").await;
    exec(&mut c.conn, "UPDATE law SET amount = 12.51 WHERE id = 1").await;
    c.close().await;
    let changed = digest_of_law(&db).await;
    assert_eq!(changed.rows, a.rows);
    assert_ne!(changed, a);
    cleanup().await;
}

fn put(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("warren-bench-server-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// A small question set over the target's table: every row, the rows chosen by two bound values
/// (named so the composer numbers them against the order they are used in), the rest by EXCEPT,
/// and an aggregate.
fn questions(dir: &Path) -> QuestionSet {
    let q = dir.join("sqlc");
    put(
        &q,
        "q/all.sqlc",
        "# every row\nSELECT w.*\nFROM (\n    :compose(target/table.sqlc)\n) w\n",
    );
    put(
        &q,
        "q/named.sqlc",
        "# the rows chosen by name or by id\nSELECT w.*\nFROM (\n    :compose(target/table.sqlc)\n) w\nWHERE w.name = :bind(name) OR w.id = :bind(id)\n",
    );
    put(&q, "q/rest.sqlc", ":except(q/all.sqlc, q/named.sqlc)\n");
    put(
        &q,
        "q/totals.sqlc",
        "SELECT count(*) AS n, sum(w.amount) AS amount, max(w.at) AS at, min(w.instant) AS instant\nFROM (\n    :compose(target/table.sqlc)\n) w\n",
    );
    let binds = dir.join("binds.tsv");
    fs::write(
        &binds,
        "question\tname\tvalue\nq/named\tname\t75053-1\nq/named\tid\t4\nq/rest\tname\t75053-1\nq/rest\tid\t4\n",
    )
    .unwrap();
    QuestionSet::load(&q, &binds).unwrap()
}

fn ctx(work: &Path) -> Ctx {
    Ctx {
        guard: guard(2048 * 1024),
        statement_timeout: "60s".into(),
        work: work.to_path_buf(),
        plan_cache_mode: None,
    }
}

async fn resolve(url: &Url, set: &QuestionSet, c: &Ctx) -> Vec<Live> {
    let specs = vec![Spec::Explicit {
        name: "pg".into(),
        table: "law".into(),
        url: url.clone(),
    }];
    run::resolve(&specs, c, set, None).await.unwrap()
}

fn verdicts(path: &Path) -> BTreeMap<(String, String), String> {
    tables::read(path, &run::CHECK)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| {
            (
                (r["target"].clone(), r["question"].clone()),
                r["verdict"].clone(),
            )
        })
        .collect()
}

#[tokio::test]
#[ignore]
async fn a_question_set_answers_right_and_a_changed_value_is_wrong() {
    let _one = one_at_a_time();
    let db = fixture().await;
    let dir = scratch("questions");
    let set = questions(&dir);
    let c = ctx(&dir.join("work"));
    let mut lives = resolve(&db, &set, &c).await;
    let expected = dir.join("expected.parquet");
    run::truth(&mut lives[0], &set, &c, &expected, false)
        .await
        .unwrap();
    let exp = run::read_expected(&expected, &set).unwrap();
    let chosen: Vec<&Question> = set.questions.iter().collect();
    let out = dir.join("check.parquet");
    run::check(&mut lives, &chosen, &exp, &c, &out, false)
        .await
        .unwrap();
    let v = verdicts(&out);
    assert_eq!(v.len(), 4);
    assert!(v.values().all(|x| x == "RIGHT"), "{v:?}");

    let mut s = session(&db, "60s").await;
    exec(&mut s.conn, "UPDATE law SET flag = false WHERE id = 1").await;
    s.close().await;
    run::check(&mut lives, &chosen, &exp, &c, &out, true)
        .await
        .unwrap();
    let v = verdicts(&out);
    let wrong: BTreeSet<(String, String)> = v
        .iter()
        .filter(|(_, x)| *x == "WRONG")
        .map(|(k, _)| k.clone())
        .collect();
    let want: BTreeSet<(String, String)> = ["q/all", "q/rest"]
        .iter()
        .map(|q| ("pg".to_string(), q.to_string()))
        .collect();
    assert_eq!(wrong, want, "{v:?}");
    for l in lives {
        l.control.close().await;
    }
    fs::remove_dir_all(&dir).unwrap();
    cleanup().await;
}

#[tokio::test]
#[ignore]
async fn a_run_writes_its_declared_columns() {
    let _one = one_at_a_time();
    let _log = catching_notices();
    let db = fixture().await;
    let dir = scratch("run");
    let set = questions(&dir);
    let c = ctx(&dir.join("work"));
    let mut lives = resolve(&db, &set, &c).await;
    let expected = dir.join("expected.parquet");
    run::truth(&mut lives[0], &set, &c, &expected, false)
        .await
        .unwrap();
    let exp = run::read_expected(&expected, &set).unwrap();
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    run::write_targets(&lives, &out.join("targets.parquet")).unwrap();
    let plan = Plan {
        cold: 1,
        warm: 1,
        lock: None,
    };
    let finished = run::bench(&mut lives, &set, &exp, &c, &plan, &out)
        .await
        .unwrap();
    assert_eq!(finished.ending, run::Ending::Complete);
    assert!(finished.unlawful.is_empty());

    // every file holds exactly its declared columns, or it is not read at all
    let results = tables::read_values(&out.join("results"), &RESULTS).unwrap();
    tables::read_values(&out.join("leaves"), &run::LEAVES).unwrap();
    let targets = tables::read_values(&out.join("targets.parquet"), &run::TARGETS).unwrap();
    let col = |n: &str| RESULTS.names().iter().position(|c| *c == n).unwrap();
    assert_eq!(results.len(), 2 * 4);
    for r in &results {
        assert_eq!(r[col("verdict")], Value::Text("RIGHT".into()), "{r:?}");
        assert_eq!(r[col("explain_status")], Value::Text("OK".into()), "{r:?}");
        assert_eq!(r[col("explain_rows_ok")], Value::Bool(true), "{r:?}");
        assert!(matches!(r[col("wall_ms")], Value::Float(_)));
        assert!(matches!(r[col("execution_ms")], Value::Float(_)));
        assert!(matches!(r[col("planning_ms")], Value::Float(_)));
        assert!(matches!(r[col("rows")], Value::Int(_)));
        // each repetition's timed plan came, and the EXPLAIN planned the same tree
        assert_eq!(r[col("plan_moved")], Value::Bool(false), "{r:?}");
        assert_eq!(r[col("timed_plan")], Value::Null, "{r:?}");
        assert_eq!(r[col("explain_plan")], Value::Null, "{r:?}");
    }
    let value = |t: &str, k: &str| {
        targets
            .iter()
            .find(|r| r[0] == Value::Text(t.into()) && r[1] == Value::Text(k.into()))
            .map(|r| r[2].clone())
            .unwrap_or_else(|| panic!("{t} has no {k}"))
    };
    assert!(matches!(
        value("pg", "setting.server_version_num"),
        Value::Text(_)
    ));
    match value("pg", "timed_plan") {
        Value::Text(t) => assert!(t.starts_with("auto_explain"), "{t}"),
        v => panic!("{v:?}"),
    }

    // the text export of the results reads back to the same values
    let text = tables::to_tsv(&RESULTS, results.clone()).unwrap();
    let back = tables::from_tsv(&RESULTS, &text).unwrap();
    assert_eq!(tables::to_tsv(&RESULTS, back).unwrap(), text);

    for l in lives {
        l.control.close().await;
    }
    fs::remove_dir_all(&dir).unwrap();
    cleanup().await;
}

/// The statements of `sql` on the timed session, inside the transaction a repetition opens.
fn timed_statements(sql: &str, version: u32) -> Statements {
    Statements {
        open: vec!["BEGIN".into(), run::READ_ONLY.into()],
        close: vec!["ROLLBACK".into()],
        show_plan: vec![run::SHOW_PLAN.into()],
        ..run::statements(&plain("q/timed"), sql, explain::command(version))
    }
}

async fn version(url: &Url) -> u32 {
    let mut s = session(url, "60s").await;
    let v = target::scalar(&mut s.conn, "SHOW server_version_num")
        .await
        .unwrap()
        .unwrap();
    s.close().await;
    v.parse().unwrap()
}

/// A timed session on `url` as the bench opens one for a target the server sends its plan.
async fn timed_session(url: &Url, sent: &PlanSent) -> Session {
    let mut s = session(url, "60s").await;
    if *sent == PlanSent::Loaded {
        exec(&mut s.conn, run::LOAD_PLAN).await;
    }
    s
}

#[tokio::test]
#[ignore]
async fn the_timed_run_keeps_its_own_plan_among_the_plans_its_session_is_sent() {
    let _one = one_at_a_time();
    let _log = catching_notices();
    let db = fixture().await;
    let v = version(&db).await;
    // a function the planner evaluates while the question is planned is sent its own plan first
    let mut c = session(&db, "60s").await;
    exec(
        &mut c.conn,
        "CREATE FUNCTION law_first(integer) RETURNS integer LANGUAGE sql IMMUTABLE
         AS $$ SELECT min(x) FROM (VALUES ($1)) v(x) $$",
    )
    .await;
    c.close().await;
    let sent = run::plan_sent(&db.session_options().unwrap(), "60s").await;
    assert!(
        matches!(sent, PlanSent::Loaded | PlanSent::Preloaded),
        "{sent:?}"
    );
    let sql = "SELECT id FROM law WHERE id = law_first(2)";
    let mut s = timed_session(&db, &sent).await;
    exec(&mut s.conn, "BEGIN").await;
    exec(&mut s.conn, run::SHOW_PLAN).await;
    let (ran, notices) = notice::catch(target::simple(&mut s.conn, sql)).await;
    ran.unwrap();
    exec(&mut s.conn, "ROLLBACK").await;
    let plans = notices.iter().filter(|n| n.contains("\"Plan\"")).count();
    assert!(plans >= 2, "{notices:#?}");

    // the timed run keeps the question's own
    let mut control = session(&db, "30s").await;
    let st = timed_statements(sql, v);
    let t = run::run_timed(&mut control.conn, &mut s, &guard(u64::MAX), &st).await;
    assert_eq!(t.status, Status::Ok, "{}", t.error);
    let p = t.plan.expect("asked for").expect("sent");
    assert_eq!(p["Node Type"], "Seq Scan", "{p}");
    assert_eq!(p["Relation Name"], "law", "{p}");
    // and nothing the session runs after its transaction is explained
    let (flushed, after) = notice::catch(witness::flush(&mut s.conn)).await;
    flushed.unwrap();
    assert!(after.is_empty(), "{after:#?}");
    s.close().await;
    control.close().await;
    cleanup().await;
}

/// The timed run of `st` in a session that first ran `timed_set`, and its EXPLAIN in another that
/// first ran `explain_set`.
async fn both_ways(
    url: &Url,
    sent: &PlanSent,
    st: &Statements,
    version: u32,
    timed_set: &str,
    explain_set: &str,
) -> (run::Timed, run::Explained) {
    let g = guard(u64::MAX);
    let mut control = session(url, "30s").await;
    let mut a = timed_session(url, sent).await;
    exec(&mut a.conn, timed_set).await;
    let t = run::run_timed(&mut control.conn, &mut a, &g, st).await;
    let mut b = session(url, "60s").await;
    exec(&mut b.conn, explain_set).await;
    let e = run::run_explain(&mut control.conn, &mut b, &g, st, &BTreeSet::new(), version).await;
    a.close().await;
    b.close().await;
    control.close().await;
    assert_eq!(t.status, Status::Ok, "{}", t.error);
    assert_eq!(e.status, Status::Ok, "{}", e.error);
    (t, e)
}

#[tokio::test]
#[ignore]
async fn one_plan_compares_alike_from_both_sessions_and_a_plan_made_otherwise_has_moved() {
    let _one = one_at_a_time();
    let _log = catching_notices();
    let db = fixture().await;
    let v = version(&db).await;
    let sent = run::plan_sent(&db.session_options().unwrap(), "60s").await;
    let sql = "SELECT a.name, count(*) AS n FROM law a JOIN law b ON b.id = a.id
               WHERE a.flag IS NOT NULL GROUP BY a.name ORDER BY a.name";
    let st = timed_statements(sql, v);
    let plans = |t: &run::Timed, e: &run::Explained| {
        let p = t.plan.clone().unwrap().unwrap();
        let f = e.facts.as_ref().unwrap();
        (explain::parting(&p, &f.plan), p, f.plan.clone())
    };

    // planned alike: the plan sent as the statement ends and the EXPLAIN ANALYZE are one tree
    let (t, e) = both_ways(&db, &sent, &st, v, "RESET ALL", "RESET ALL").await;
    let (part, p, f) = plans(&t, &e);
    assert_eq!(
        run::moved(Some(&t), Some(&e)),
        Some(false),
        "{part:?}\n{p}\n{f}"
    );
    // also with a WITH query, an InitPlan and parallel workers, which the EXPLAIN ANALYZE reports
    // per worker
    let parallel = "SET parallel_setup_cost = 0; SET parallel_tuple_cost = 0;
                    SET min_parallel_table_scan_size = 0; SET max_parallel_workers_per_gather = 2";
    let sql = "WITH m AS MATERIALIZED (SELECT id, name FROM law)
               SELECT m.name, count(*) AS n FROM m JOIN law b ON b.id = m.id
               WHERE b.id >= (SELECT min(id) FROM law) GROUP BY m.name";
    let wide = timed_statements(sql, v);
    let (t, e) = both_ways(&db, &sent, &wide, v, parallel, parallel).await;
    let (part, p, f) = plans(&t, &e);
    assert!(f.to_string().contains("Workers Planned"), "{f}");
    assert_eq!(
        run::moved(Some(&t), Some(&e)),
        Some(false),
        "{part:?}\n{p}\n{f}"
    );

    // planned otherwise: the timed session may not sort, the EXPLAIN's may not hash
    let (t, e) = both_ways(
        &db,
        &sent,
        &st,
        v,
        "SET enable_sort = off",
        "SET enable_hashagg = off",
    )
    .await;
    let (part, p, f) = plans(&t, &e);
    assert_eq!(run::moved(Some(&t), Some(&e)), Some(true), "{p}\n{f}");
    assert!(part.is_some());
    cleanup().await;
}
