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

use crate::digest::Digest;
use crate::questions::{Question, QuestionSet};
use crate::report::RESULTS;
use crate::run::{self, Ctx, Live, Plan, Spec, Statements, Status};
use crate::tables::{self, Value};
use crate::target::{self, Session, Url};
use crate::watchdog::Guard;

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
