// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tests against a Postgres server with the surveyor installed. They run only when asked (`cargo
//! test -- --ignored --test-threads=1`), against the server `BLOCKER_DB_URL` names; the
//! database that URL names is the one connected to while the scratch databases are made and
//! dropped.
//! Every test makes its database afresh: a small copy of the flat `lego` tables and a `lego_oo`
//! class hierarchy of their sets, with their keys as B-trees and one surveyor on each table, and
//! drops it when it ends.

// each test holds the lock for its whole length, awaits included, on a runtime of its own
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::postgres::PgConnection;

use crate::index_sets::{self, Index, Resolved};
use crate::questions::QuestionSet;
use crate::run::{self, Ctx, IndexSets, Plan, Spec, Statements, Status};
use crate::tables::{self, Value};
use crate::target::{self, Session, Url};
use crate::watchdog::Guard;

const DB: &str = "warren_bench_sets_check";

static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner())
}

fn server() -> Url {
    let pg = std::env::var("BLOCKER_DB_URL")
        .expect("BLOCKER_DB_URL must name the Postgres server to test against, as postgres://host:port/database");
    Url::parse(&pg).unwrap()
}

/// The same server and credentials, on `db`, with `query` (`?…`) as the URL's query.
fn on(url: &Url, db: &str, query: &str) -> Url {
    let scheme_end = url.text.find("://").unwrap() + 3;
    let slash = scheme_end + url.text[scheme_end..].find('/').unwrap();
    Url::parse(&format!("{}/{db}{query}", &url.text[..slash])).unwrap()
}

/// The URL of a target: `db`, whose sessions look for tables in `search_path`.
fn target_url(url: &Url, db: &str, search_path: &str, more: &str) -> Url {
    on(
        url,
        db,
        &format!("?options=-c%20search_path%3D{search_path}{more}"),
    )
}

async fn session(url: &Url) -> Session {
    target::open(&url.session_options().unwrap(), "60s")
        .await
        .unwrap()
}

async fn exec(conn: &mut PgConnection, sql: &str) {
    target::simple(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn scalar(conn: &mut PgConnection, sql: &str) -> String {
    target::scalar(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .unwrap_or_default()
}

/// The flat tables as a 3NF design leaves them, a class hierarchy of the sets, and their keys as
/// B-trees: the ones the samples' questions walk.
pub(crate) const FIXTURE: &str = r#"
CREATE SCHEMA lego;
CREATE FUNCTION lego.clock(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE
    AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;
CREATE TABLE lego.lego_themes (id integer PRIMARY KEY, name varchar(255) NOT NULL, parent_id integer);
INSERT INTO lego.lego_themes VALUES
    (158, 'Star Wars', NULL), (182, 'Rebels', 158), (174, 'Ultimate Collector Series', 158),
    (900, 'Deep', 182), (50, 'Town', NULL), (52, 'City', 50), (1, 'Technic', NULL),
    (186, 'Castle', NULL);
CREATE TABLE lego.lego_sets (set_num varchar(255) PRIMARY KEY, name varchar(255) NOT NULL,
    year integer, theme_id integer, num_parts integer);
INSERT INTO lego.lego_sets SELECT '75053-1', 'The Ghost', 2014, 182, 929
    UNION ALL SELECT '10179-1', 'Millennium Falcon', 2007, 174, 5195
    UNION ALL SELECT '7904-1', 'Advent Calendar 2006 City', 2006, 52, 0;
INSERT INTO lego.lego_sets SELECT 'g' || i || '-1', 'generated ' || i, 1990 + i % 35,
    (ARRAY[158, 182, 174, 900, 50, 52, 1, 186])[i % 8 + 1], i % 500 FROM generate_series(1, 800) i;
INSERT INTO lego.lego_sets SELECT 'n' || i || '-1', 'nested ' || i, 2006, 52, 10
    FROM generate_series(1, 24) i;
CREATE TABLE lego.lego_inventories (id integer PRIMARY KEY, version integer NOT NULL,
    set_num varchar(255) NOT NULL);
INSERT INTO lego.lego_inventories SELECT row_number() OVER (ORDER BY set_num), 1, set_num
    FROM lego.lego_sets;
INSERT INTO lego.lego_inventories SELECT 100000, 2, '75053-1';
CREATE TABLE lego.lego_part_categories (id integer PRIMARY KEY, name varchar(255) NOT NULL);
INSERT INTO lego.lego_part_categories SELECT i, 'category ' || i FROM generate_series(1, 40) i;
CREATE TABLE lego.lego_parts (part_num varchar(255) PRIMARY KEY, name text NOT NULL,
    part_cat_id integer NOT NULL);
INSERT INTO lego.lego_parts SELECT 'p' || i, 'part ' || i, i % 40 + 1 FROM generate_series(0, 399) i;
CREATE TABLE lego.lego_inventory_parts (inventory_id integer NOT NULL, part_num varchar(255) NOT NULL,
    color_id integer NOT NULL, quantity integer NOT NULL, is_spare boolean NOT NULL);
INSERT INTO lego.lego_inventory_parts SELECT inv.id, 'p' || ((inv.id * 7 + k) % 400), k % 6,
    1 + k % 3, k = 4 FROM lego.lego_inventories inv, generate_series(0, 4) k;
CREATE TABLE lego.lego_inventory_sets (inventory_id integer NOT NULL, set_num varchar(255) NOT NULL,
    quantity integer NOT NULL);
INSERT INTO lego.lego_inventory_sets SELECT inv.id, 'n' || i || '-1', 1
    FROM lego.lego_inventories inv, generate_series(1, 24) i WHERE inv.set_num = '7904-1';
CREATE TABLE lego.lego_collection (builder_id integer NOT NULL, row_no integer NOT NULL,
    set_num varchar(255) NOT NULL, typed_set_num varchar(255), typed_name varchar(255),
    PRIMARY KEY (builder_id, row_no));
INSERT INTO lego.lego_collection SELECT i / 10 + 1, i % 10 + 1,
    CASE WHEN i % 97 = 0 THEN '10179-1' ELSE 'g' || (i % 800 + 1) || '-1' END, NULL, NULL
    FROM generate_series(0, 2999) i;
CREATE TABLE lego.lego_purchases (purchase_id bigint PRIMARY KEY, builder_id integer NOT NULL,
    row_no integer NOT NULL, store varchar(64) NOT NULL, ordered_at timestamptz NOT NULL,
    ordered_local varchar(32) NOT NULL, delivered_at timestamptz);
INSERT INTO lego.lego_purchases SELECT row_number() OVER (ORDER BY c.builder_id, c.row_no, k),
    c.builder_id, c.row_no, 'store',
    timestamptz '2008-01-01 00:00:00+00' + ((c.builder_id * 10 + c.row_no) * 37 + k * 5) * interval '1 hour',
    'local', NULL
    FROM lego.lego_collection c, generate_series(0, 1) k;
CREATE INDEX lego_themes_parent_id_id_idx ON lego.lego_themes (parent_id, id);
CREATE INDEX lego_sets_theme_id_set_num_idx ON lego.lego_sets (theme_id, set_num);
CREATE INDEX lego_sets_theme_id_year_idx ON lego.lego_sets (theme_id, year) INCLUDE (set_num);
CREATE INDEX lego_inventories_set_num_version_id_idx ON lego.lego_inventories (set_num, version, id);
CREATE INDEX lego_inventory_sets_inventory_id_set_num_idx
    ON lego.lego_inventory_sets (inventory_id, set_num);
CREATE INDEX lego_inventory_parts_inventory_id_part_num_color_id_idx
    ON lego.lego_inventory_parts (inventory_id, part_num, color_id);
CREATE INDEX lego_parts_part_cat_id_part_num_idx ON lego.lego_parts (part_cat_id, part_num);
CREATE INDEX lego_collection_set_num_builder_id_row_no_idx
    ON lego.lego_collection (set_num, builder_id, row_no);
CREATE INDEX lego_purchases_builder_id_row_no_month_clock_idx ON lego.lego_purchases
    (builder_id, row_no, (EXTRACT(month FROM lego.clock(ordered_at))), (lego.clock(ordered_at)));
CREATE INDEX lego_purchases_month_clock_idx ON lego.lego_purchases
    ((EXTRACT(month FROM lego.clock(ordered_at))), (lego.clock(ordered_at))) INCLUDE (builder_id, row_no);
CREATE SCHEMA lego_oo;
CREATE TABLE lego_oo.lego_sets (LIKE lego.lego_sets);
ALTER TABLE lego_oo.lego_sets ADD PRIMARY KEY (set_num);
CREATE TABLE lego_oo.lego_sets_star_wars () INHERITS (lego_oo.lego_sets);
CREATE TABLE lego_oo.lego_sets_town () INHERITS (lego_oo.lego_sets);
CREATE TABLE lego_oo.lego_sets_other () INHERITS (lego_oo.lego_sets);
INSERT INTO lego_oo.lego_sets_star_wars SELECT * FROM lego.lego_sets WHERE theme_id IN (158, 182, 174, 900);
INSERT INTO lego_oo.lego_sets_town SELECT * FROM lego.lego_sets WHERE theme_id IN (50, 52);
INSERT INTO lego_oo.lego_sets_other SELECT * FROM lego.lego_sets
    WHERE theme_id NOT IN (158, 182, 174, 900, 50, 52);
ALTER TABLE lego_oo.lego_sets_star_wars ADD PRIMARY KEY (set_num);
ALTER TABLE lego_oo.lego_sets_town ADD PRIMARY KEY (set_num);
ALTER TABLE lego_oo.lego_sets_other ADD PRIMARY KEY (set_num);
CREATE INDEX lego_sets_star_wars_theme_id_set_num_idx ON lego_oo.lego_sets_star_wars (theme_id, set_num);
CREATE INDEX lego_sets_star_wars_theme_id_year_idx ON lego_oo.lego_sets_star_wars (theme_id, year) INCLUDE (set_num);
CREATE INDEX lego_sets_town_theme_id_set_num_idx ON lego_oo.lego_sets_town (theme_id, set_num);
CREATE INDEX lego_sets_town_theme_id_year_idx ON lego_oo.lego_sets_town (theme_id, year) INCLUDE (set_num);
CREATE INDEX lego_sets_other_theme_id_set_num_idx ON lego_oo.lego_sets_other (theme_id, set_num);
CREATE INDEX lego_sets_other_theme_id_year_idx ON lego_oo.lego_sets_other (theme_id, year) INCLUDE (set_num);
ANALYZE;
"#;

/// What makes the surveyor present and loaded in every session of a database.
const EXTENSION: &str = "CREATE EXTENSION IF NOT EXISTS warren_surveyor_pg";
fn preload(db: &str) -> String {
    format!("ALTER DATABASE {db} SET session_preload_libraries = 'warren_surveyor_pg'")
}

/// One surveyor on each table of the fixture, on the columns of its unique key, with every name
/// quoted.
const SURVEYORS: &str = r#"
CREATE INDEX "lego_themes_surveyor" ON "lego"."lego_themes" USING surveyor ("id");
CREATE INDEX "lego_sets_surveyor" ON "lego"."lego_sets" USING surveyor ("set_num");
CREATE INDEX "lego_inventories_surveyor" ON "lego"."lego_inventories" USING surveyor ("id");
CREATE INDEX "lego_part_categories_surveyor" ON "lego"."lego_part_categories" USING surveyor ("id");
CREATE INDEX "lego_parts_surveyor" ON "lego"."lego_parts" USING surveyor ("part_num");
CREATE INDEX "lego_inventory_parts_surveyor" ON "lego"."lego_inventory_parts"
    USING surveyor ("inventory_id", "part_num", "color_id", "is_spare");
CREATE INDEX "lego_inventory_sets_surveyor" ON "lego"."lego_inventory_sets"
    USING surveyor ("inventory_id", "set_num");
CREATE INDEX "lego_collection_surveyor" ON "lego"."lego_collection" USING surveyor ("builder_id", "row_no");
CREATE INDEX "lego_purchases_surveyor" ON "lego"."lego_purchases" USING surveyor ("purchase_id");
CREATE INDEX "lego_sets_surveyor" ON "lego_oo"."lego_sets" USING surveyor ("set_num");
CREATE INDEX "lego_sets_star_wars_surveyor" ON "lego_oo"."lego_sets_star_wars" USING surveyor ("set_num");
CREATE INDEX "lego_sets_town_surveyor" ON "lego_oo"."lego_sets_town" USING surveyor ("set_num");
CREATE INDEX "lego_sets_other_surveyor" ON "lego_oo"."lego_sets_other" USING surveyor ("set_num");
ANALYZE "lego"."lego_themes";
"#;

/// How many surveyors `SURVEYORS` makes.
const SURVEYOR_COUNT: usize = 13;

async fn drop_db(db: &str) {
    let mut c = session(&server()).await;
    exec(
        &mut c.conn,
        &format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE datname = '{db}' AND pid <> pg_backend_pid()"
        ),
    )
    .await;
    exec(&mut c.conn, &format!("DROP DATABASE IF EXISTS {db}")).await;
    c.close().await;
}

/// Makes `DB` afresh: the fixture, the surveyor's extension loaded in every session, and one
/// surveyor on each table. Returns the database's URL.
async fn fixture() -> Url {
    let pg = server();
    drop_db(DB).await;
    let mut c = session(&pg).await;
    exec(&mut c.conn, &format!("CREATE DATABASE {DB}")).await;
    exec(&mut c.conn, &preload(DB)).await;
    c.close().await;
    let url = on(&pg, DB, "");
    let mut s = session(&url).await;
    exec(&mut s.conn, FIXTURE).await;
    exec(&mut s.conn, EXTENSION).await;
    exec(&mut s.conn, SURVEYORS).await;
    s.close().await;
    url
}

fn guard() -> Guard {
    Guard {
        cap_kb: 4 * 1024 * 1024,
        interval: Duration::from_millis(50),
        grace: Duration::from_secs(5),
        poll_memory: true,
    }
}

fn ctx(work: &Path) -> Ctx {
    Ctx {
        guard: guard(),
        statement_timeout: "60s".into(),
        work: work.to_path_buf(),
        plan_cache_mode: None,
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("warren-bench-pg-{tag}-{}", std::process::id()));
    if d.exists() {
        fs::remove_dir_all(&d).unwrap();
    }
    fs::create_dir_all(&d).unwrap();
    d
}

fn put(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn standard_sets() -> index_sets::Sets {
    index_sets::read_sets(&Path::new(env!("CARGO_MANIFEST_DIR")).join("index_sets.json")).unwrap()
}

/// Two questions over the sets: one plain, one bound.
fn questions(dir: &Path) -> QuestionSet {
    put(
        dir,
        "sqlc/q/star_wars_sets.sqlc",
        "# The sets filed under the Star Wars theme itself.\n-- lego_sets on theme_id\n\
         SELECT s.set_num, s.year FROM lego_sets s WHERE s.theme_id = 158\n",
    );
    put(
        dir,
        "sqlc/q/set_lines.sqlc",
        "# The lines of one set.\n-- lego_inventories and lego_inventory_parts\n\
         SELECT i.set_num, ip.part_num, ip.color_id, ip.quantity\n\
         FROM lego_inventories i JOIN lego_inventory_parts ip ON ip.inventory_id = i.id\n\
         WHERE i.set_num = :bind(set_num)\n",
    );
    put(
        dir,
        "binds.tsv",
        "question\tname\tvalue\nq/set_lines\tset_num\t75053-1\n",
    );
    QuestionSet::load(&dir.join("sqlc"), &dir.join("binds.tsv")).unwrap()
}

fn spec(name: &str, url: &Url) -> Spec {
    Spec::Explicit {
        name: name.into(),
        table: "lego_sets".into(),
        url: url.clone(),
    }
}

fn index_sets(names: &[&str]) -> IndexSets {
    IndexSets {
        sets: standard_sets(),
        names: names.iter().map(|s| s.to_string()).collect(),
    }
}

/// The index each scan of a leaves directory read, by target.
fn scanned_indexes(leaves: &Path) -> Vec<(String, String)> {
    let names = run::LEAVES.names();
    let col = |c: &str| names.iter().position(|n| *n == c).unwrap();
    tables::read_values(leaves, &run::LEAVES)
        .unwrap()
        .into_iter()
        .filter(|r| r[col("index")] != Value::Null)
        .map(|r| (r[col("target")].text(), r[col("index")].text()))
        .collect()
}

#[tokio::test]
#[ignore]
async fn index_sets_turn_indexes_off_only_inside_each_repetition() {
    let _one = one_at_a_time();
    let url = fixture().await;
    let pg = server();
    let dir = scratch("sets");
    let set = questions(&dir);
    let c = ctx(&dir.join("work"));
    let flat = target_url(&pg, DB, "lego", "%20-c%20enable_seqscan%3Doff");
    let oo = target_url(&pg, DB, "lego_oo,lego", "%20-c%20enable_seqscan%3Doff");

    // the driver passes the URL's options to every session
    let mut s = session(&oo).await;
    assert_eq!(
        scalar(&mut s.conn, "SHOW search_path").await,
        "lego_oo,lego"
    );
    assert_eq!(
        scalar(&mut s.conn, "SHOW session_preload_libraries").await,
        "warren_surveyor_pg"
    );
    s.close().await;

    let specs = vec![spec("flat", &flat), spec("oo", &oo)];
    let mut lives = run::resolve(&specs, &c, &set, Some(&index_sets(&["all", "dba", "keys"])))
        .await
        .unwrap();
    let names: Vec<&str> = lives.iter().map(|l| l.target.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "flat@all",
            "flat@dba",
            "flat@keys",
            "oo@all",
            "oo@dba",
            "oo@keys"
        ]
    );
    for l in &lives {
        let t = &l.target;
        assert_eq!(
            t.table,
            if l.base == "flat" {
                "lego.lego_sets"
            } else {
                "lego_oo.lego_sets"
            }
        );
        assert!(!t.get("extension.warren_surveyor_pg").is_empty());
        assert_eq!(
            t.get("setting.session_preload_libraries"),
            "warren_surveyor_pg"
        );
        // the build of the surveyor each run measured
        let sha = t.get("library.warren_surveyor_pg.sha256");
        assert!(
            sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()),
            "{sha}"
        );
        assert!(!t.get("roster").is_empty());
        assert!(t.get("setting.search_path").starts_with("lego"));
        let off = t.get("indexes_off");
        if t.name.ends_with("@all") {
            assert!(off.is_empty(), "{}: {off}", t.name);
        } else if t.name.ends_with("@dba") {
            let names: Vec<&str> = off.split(',').collect();
            assert_eq!(names.len(), SURVEYOR_COUNT, "{}: {off}", t.name);
            assert!(names.iter().all(|n| n.ends_with("_surveyor")), "{off}");
        } else {
            assert!(
                !off.is_empty() && !off.contains("_pkey"),
                "{}: {off}",
                t.name
            );
        }
    }
    let ids: BTreeSet<&str> = lives.iter().map(|l| l.target.config_id.as_str()).collect();
    assert_eq!(
        ids.len(),
        6,
        "every target and set has a configuration of its own"
    );

    let expected = dir.join("expected.parquet");
    run::truth(&mut lives[0], &set, &c, &expected, false)
        .await
        .unwrap();
    let exp = run::read_expected(&expected, &set).unwrap();
    assert!(
        tables::read(&expected, &run::EXPECTED).unwrap().rows[0]["reference"]
            .ends_with("index_set=all")
    );
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    run::write_indexes(&lives, &out.join("indexes.parquet")).unwrap();
    let plan = Plan {
        cold: 1,
        warm: 1,
        lock: None,
    };
    let finished = run::bench(&mut lives, &set, &exp, &c, &plan, &out)
        .await
        .unwrap();
    assert_eq!(finished.ending, run::Ending::Complete);
    assert!(finished.unlawful.is_empty(), "{:?}", finished.unlawful);
    let results = tables::read(&out.join("results"), &crate::report::RESULTS).unwrap();
    assert_eq!(results.rows.len(), 6 * 2 * 2);
    for r in &results.rows {
        assert_eq!(
            r["verdict"], "RIGHT",
            "{} {}: {}",
            r["target"], r["question"], r["error"]
        );
    }
    // each scan read an index its set held, and a key was read where the set held it; no scan
    // read a surveyor
    let scans = scanned_indexes(&out.join("leaves"));
    assert!(!scans.is_empty());
    assert!(
        scans.iter().all(|(_, ix)| !ix.contains("_surveyor")),
        "{scans:?}"
    );
    // every scan records the planner's estimate, under every set
    let leaves = tables::read(&out.join("leaves"), &run::LEAVES).unwrap();
    for r in &leaves.rows {
        assert!(r["plan_rows"].parse::<f64>().is_ok(), "{r:?}");
    }
    let sets: BTreeSet<&str> = leaves
        .rows
        .iter()
        .map(|r| r["target"].rsplit('@').next().unwrap())
        .collect();
    assert_eq!(sets, ["all", "dba", "keys"].into());
    for (t, ix) in &scans {
        if t.ends_with("@keys") {
            assert!(!ix.contains("_idx"), "{t} read {ix}");
        }
    }
    assert!(
        scans
            .iter()
            .any(|(t, ix)| t.ends_with("@all") && ix.contains("_idx")),
        "{scans:?}"
    );
    // outside the repetitions every index is where it was
    let mut s = session(&url).await;
    let after = index_sets::read_roster(&mut s.conn).await.unwrap();
    s.close().await;
    assert_eq!(
        index_sets::roster_fingerprint(&after),
        lives[0].resolved.as_ref().unwrap().fingerprint
    );
    // the recorded roster: under each set exactly the indexes it turned off are absent
    let ix = tables::read(&out.join("indexes.parquet"), &index_sets::INDEXES).unwrap();
    assert_eq!(ix.rows.len(), 6 * after.len());
    for r in &ix.rows {
        let turned_off = (r["index_set"] == "keys" && r["constraint"].is_empty())
            || (r["index_set"] == "dba" && r["method"] == "surveyor");
        assert_eq!(r["present"], (!turned_off).to_string(), "{r:?}");
    }
    for l in lives {
        l.control.close().await;
    }
    drop_db(DB).await;
    fs::remove_dir_all(&dir).unwrap();
}

/// The statements of one repetition sent with `open` and `close` as given.
fn enveloped(run: &str, open: &[&str], close: &[&str], binds: bool) -> Statements {
    let q = crate::questions::Question {
        name: "q/x".into(),
        binds: if binds {
            vec!["75053-1".into()]
        } else {
            vec![]
        },
        derived_from: None,
    };
    Statements {
        open: open.iter().map(|s| s.to_string()).collect(),
        close: close.iter().map(|s| s.to_string()).collect(),
        ..run::statements(&q, run, crate::explain::command(180_000))
    }
}

#[tokio::test]
#[ignore]
async fn a_repetition_rolls_back_first_and_a_session_that_cannot_is_dropped() {
    let _one = one_at_a_time();
    let url = fixture().await;
    let g = guard();
    let mut control = session(&url).await;
    let drop_one = "DROP INDEX lego.lego_sets_theme_id_set_num_idx";
    let present = "SELECT count(*) FROM pg_class WHERE relname = 'lego_sets_theme_id_set_num_idx'";
    let prepared = "SELECT count(*) FROM pg_prepared_statements";
    let lines = "SELECT i.set_num FROM lego.lego_inventories i WHERE i.set_num = $1";

    // the index is off inside the repetition, the ROLLBACK is sent before the DEALLOCATE, and
    // both leave the session with nothing open
    let mut s = session(&url).await;
    let st = enveloped(lines, &["BEGIN", drop_one], &["ROLLBACK"], true);
    let t = run::run_timed(&mut control.conn, &mut s, &g, &st).await;
    assert_eq!(t.status, Status::Ok, "{}", t.error);
    assert_eq!(t.answer.unwrap().rows, 2);
    assert!(!s.broken);
    assert_eq!(scalar(&mut s.conn, present).await, "1");
    assert_eq!(scalar(&mut s.conn, prepared).await, "0");
    // seed: with no ROLLBACK the session is left inside the repetition, the index off for it
    let st = enveloped(lines, &["BEGIN", drop_one], &[], true);
    let t = run::run_timed(&mut control.conn, &mut s, &g, &st).await;
    assert_eq!(t.status, Status::Ok, "{}", t.error);
    assert_eq!(scalar(&mut s.conn, present).await, "0");
    exec(&mut s.conn, "ROLLBACK").await;
    assert_eq!(scalar(&mut s.conn, present).await, "1");

    // a failure before the question is an ERROR saying so, and the session is rolled back
    let st = enveloped(
        lines,
        &["BEGIN", "DROP INDEX lego.no_such_idx"],
        &["ROLLBACK"],
        true,
    );
    let t = run::run_timed(&mut control.conn, &mut s, &g, &st).await;
    assert_eq!(t.status, Status::Error);
    assert!(t.error.starts_with("before the question: "), "{}", t.error);
    assert!(!s.broken);
    assert_eq!(scalar(&mut s.conn, "SELECT 1").await, "1");
    // the same failure in the EXPLAIN session
    let e = run::run_explain(
        &mut control.conn,
        &mut s,
        &g,
        &st,
        &BTreeSet::new(),
        180_000,
    )
    .await;
    assert_eq!(e.status, Status::Error);
    assert!(e.error.starts_with("before the question: "), "{}", e.error);
    let p = run::planning_reads(&mut control.conn, &mut s, &g, &st).await;
    assert!(p.unwrap_err().starts_with("before the question: "));
    assert_eq!(scalar(&mut s.conn, "SELECT 1").await, "1");
    s.close().await;

    // a session whose ROLLBACK fails is marked, to be dropped
    let mut s = session(&url).await;
    let st = enveloped(
        "SELECT pg_terminate_backend(pg_backend_pid())",
        &["BEGIN"],
        &["ROLLBACK"],
        false,
    );
    let t = run::run_timed(&mut control.conn, &mut s, &g, &st).await;
    assert!(s.broken, "{}", t.error);
    assert!(t.error.contains("the ROLLBACK failed"), "{}", t.error);
    s.close().await;
    control.close().await;
    drop_db(DB).await;
}

#[tokio::test]
#[ignore]
async fn an_index_set_breaks_its_certificate_its_law_or_its_roster_and_is_seen_to() {
    let _one = one_at_a_time();
    let url = fixture().await;
    let pg = server();
    let dir = scratch("seeds");
    let set = questions(&dir);
    let c = ctx(&dir.join("work"));
    let flat = target_url(&pg, DB, "lego", "%20-c%20enable_seqscan%3Doff");

    // the trial transaction must leave exactly the roster less what the set turns off
    let mut s = session(&url).await;
    let roster = index_sets::read_roster(&mut s.conn).await.unwrap();
    s.close().await;
    let good = index_sets::resolve_set(&standard_sets(), "keys", &roster).unwrap();
    let opts = flat.session_options().unwrap();
    index_sets::certify(&opts, "60s", &good).await.unwrap();
    let mut ghost = good.clone();
    let mut extra: Index = ghost.roster[0].clone();
    extra.name = "no_such_idx".into();
    extra.definition = extra
        .definition
        .replace(&ghost.roster[0].name, "no_such_idx");
    ghost.roster.push(extra);
    let e = index_sets::certify(&opts, "60s", &ghost).await.unwrap_err();
    assert!(
        e.contains("expected and not present [\"lego.no_such_idx\"]"),
        "{e}"
    );
    let mut missing: Resolved = good.clone();
    missing.off.clear();
    missing.off.insert(0);
    missing.roster[0].qualified = "lego.no_such_idx".into();
    let e = index_sets::certify(&opts, "60s", &missing)
        .await
        .unwrap_err();
    assert!(
        e.starts_with("index set keys: before the question: "),
        "{e}"
    );

    // a scan of an index the set does not hold present breaks the law, and the exit status
    let mut lives = run::resolve(
        &[spec("flat", &flat)],
        &c,
        &set,
        Some(&index_sets(&["all"])),
    )
    .await
    .unwrap();
    let expected = dir.join("expected.parquet");
    run::truth(&mut lives[0], &set, &c, &expected, false)
        .await
        .unwrap();
    let exp = run::read_expected(&expected, &set).unwrap();
    for i in &mut lives[0].resolved.as_mut().unwrap().roster {
        i.name.push_str("_renamed");
    }
    let out = dir.join("out");
    let plan = Plan {
        cold: 1,
        warm: 0,
        lock: None,
    };
    let finished = run::bench(&mut lives, &set, &exp, &c, &plan, &out)
        .await
        .unwrap();
    assert!(!finished.unlawful.is_empty());
    assert!(
        finished.unlawful[0].contains("does not hold present"),
        "{:?}",
        finished.unlawful
    );
    assert_eq!(crate::exit_status(&finished, 0), crate::EXIT_UNLAWFUL);

    // a committed index made while an index set runs stops the run, and the check
    let mut lives = run::resolve(
        &[spec("flat", &flat)],
        &c,
        &set,
        Some(&index_sets(&["keys"])),
    )
    .await
    .unwrap();
    let mut s = session(&url).await;
    exec(
        &mut s.conn,
        "CREATE INDEX lego_themes_name_idx ON lego.lego_themes (name)",
    )
    .await;
    let out = dir.join("out2");
    let finished = run::bench(&mut lives, &set, &exp, &c, &plan, &out)
        .await
        .unwrap();
    assert_eq!(finished.ending, run::Ending::RosterChanged);
    let ending = run::check(
        &mut lives,
        &set.questions.iter().collect::<Vec<_>>(),
        &exp,
        &c,
        &dir.join("check.parquet"),
        false,
    )
    .await
    .unwrap();
    assert_eq!(ending, run::Ending::RosterChanged);
    exec(&mut s.conn, "DROP INDEX lego.lego_themes_name_idx").await;
    s.close().await;
    for l in lives {
        l.control.close().await;
    }
    drop_db(DB).await;
    fs::remove_dir_all(&dir).unwrap();
}

/// The questions the test's samples ask, plain SQL over the fixture, and their bound values.
const SAMPLE_QUESTIONS: [(&str, &str); 9] = [
    (
        "lines/of_set",
        "SELECT s.set_num, i.version, ip.inventory_id, ip.part_num, ip.color_id, ip.quantity, ip.is_spare\nFROM lego_sets s\nJOIN lego_inventories i ON i.set_num = s.set_num\nJOIN lego_inventory_parts ip ON ip.inventory_id = i.id\nWHERE s.set_num = $1\n",
    ),
    (
        "parts/of_category",
        "SELECT p.part_num, p.name\nFROM lego_part_categories c\nJOIN lego_parts p ON p.part_cat_id = c.id\nWHERE c.id = $1\n",
    ),
    (
        "purchases/december_2010s",
        "SELECT p.purchase_id, c.set_num, p.ordered_at\nFROM lego_purchases p\nJOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no\nWHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12\n  AND lego.clock(p.ordered_at) >= '2010-01-01'\n  AND lego.clock(p.ordered_at) < '2020-01-01'\n",
    ),
    (
        "purchases/december",
        "SELECT p.purchase_id, c.set_num, p.ordered_at\nFROM lego_purchases p\nJOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no\nWHERE extract(month FROM lego.clock(p.ordered_at))::smallint = 12\n",
    ),
    (
        "purchases/in_2015",
        "SELECT p.purchase_id, c.set_num, p.ordered_at\nFROM lego_purchases p\nJOIN lego_collection c ON c.builder_id = p.builder_id AND c.row_no = p.row_no\nWHERE lego.clock(p.ordered_at) >= '2015-01-01'\n  AND lego.clock(p.ordered_at) < '2016-01-01'\n",
    ),
    (
        "purchases/of_set",
        "SELECT c.builder_id, c.row_no, p.purchase_id, p.ordered_at\nFROM lego_sets s\nJOIN lego_collection c ON c.set_num = s.set_num\nJOIN lego_purchases p ON p.builder_id = c.builder_id AND p.row_no = c.row_no\nWHERE s.set_num = $1\n",
    ),
    (
        "sets/nested",
        "SELECT i.set_num AS outer_set, i.version, n.set_num, n.name, x.quantity\nFROM lego_sets s\nJOIN lego_inventories i ON i.set_num = s.set_num\nJOIN lego_inventory_sets x ON x.inventory_id = i.id\nJOIN lego_sets n ON n.set_num = x.set_num\nWHERE s.set_num = $1\n",
    ),
    (
        "sets/theme_in_years",
        "SELECT s.set_num, s.name, s.year\nFROM lego_themes t\nJOIN lego_sets s ON s.theme_id = t.id\nWHERE t.id = $3\n  AND s.year BETWEEN $1 AND $2\n",
    ),
    (
        "sets/theme_subtree",
        "WITH RECURSIVE subtree (id) AS (\n    SELECT t.id FROM lego_themes t WHERE t.id = $1\n    UNION\n    SELECT c.id FROM lego_themes c JOIN subtree p ON c.parent_id = p.id\n)\nSELECT s.set_num, s.name, s.year, s.theme_id\nFROM subtree t\nJOIN lego_sets s ON s.theme_id = t.id\n",
    ),
];

const SAMPLE_BINDS: &str = "question\tname\tvalue\nlines/of_set\tset_num\t75053-1\nsets/theme_subtree\ttheme_id\t158\nsets/theme_in_years\ttheme_id\t52\nsets/theme_in_years\tfirst_year\t2010\nsets/theme_in_years\tlast_year\t2019\nsets/nested\tset_num\t7904-1\nparts/of_category\tpart_cat_id\t27\npurchases/of_set\tset_num\t10179-1\n";

/// The test's two samples under `dir/samples`: `smoke`, three questions under every index, and
/// `keys`, every question under the DBA's indexes alone and with the surveyors.
fn write_samples(dir: &Path) {
    let s = dir.join("samples");
    for (name, text) in SAMPLE_QUESTIONS {
        put(&s, &format!("questions/{name}.sql"), text);
    }
    put(&s, "binds.tsv", SAMPLE_BINDS);
    put(
        &s,
        "smoke/sample.json",
        r#"{"questions": "../questions", "binds": "../binds.tsv", "only": ["lines/of_set", "sets/theme_in_years", "parts/of_category"], "index_sets": ["all"], "cold": 1, "warm": 1, "statement_timeout": "60s"}"#,
    );
    put(
        &s,
        "keys/sample.json",
        r#"{"questions": "../questions", "binds": "../binds.tsv", "only": [], "index_sets": ["dba", "all"], "cold": 1, "warm": 3, "statement_timeout": "120s"}"#,
    );
}

fn sample_args(name: &str, targets: &[(&str, &Url)], dir: &Path) -> crate::SampleArgs {
    crate::SampleArgs {
        name: name.into(),
        target: targets
            .iter()
            .flat_map(|(n, u)| [n.to_string(), "lego_sets".to_string(), u.text.clone()])
            .collect(),
        samples: dir.join("samples"),
        out: dir.join("out"),
        lock: dir.join("TIMING.lock"),
        index_set_file: Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("index_sets.json")),
        work: Some(dir.join("work")),
        rss_cap_mb: 4096,
        watch_interval_ms: 100,
        terminate_after_s: 30,
        no_memory_watchdog: false,
        cold: None,
        warm: None,
    }
}

/// The one run directory of a sample, the one written last.
fn last_run(dir: &Path, name: &str) -> PathBuf {
    let mut runs: Vec<PathBuf> = fs::read_dir(dir.join("out").join(name))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    runs.sort();
    runs.pop().unwrap()
}

#[tokio::test]
#[ignore]
async fn the_samples_answer_alike_on_the_flat_tables_and_on_their_classes() {
    let _one = one_at_a_time();
    fixture().await;
    let pg = server();
    let dir = scratch("samples");
    write_samples(&dir);
    let flat = target_url(&pg, DB, "lego", "");
    let oo = target_url(&pg, DB, "lego_oo,lego", "");

    let code = crate::sample_cmd(sample_args("smoke", &[("flat", &flat)], &dir))
        .await
        .unwrap();
    let run = last_run(&dir, "smoke");
    let summary = fs::read_to_string(run.join("summary.txt")).unwrap();
    assert_eq!(code, 0, "{summary}");
    assert!(summary.starts_with(
        "SAMPLE smoke: a quick check, not a measurement; expected answers by one route\n"
    ));
    let targets = tables::read(&run.join("targets.parquet"), &run::TARGETS).unwrap();
    let kinds: Vec<(&str, &str)> = targets
        .rows
        .iter()
        .filter(|r| r["key"] == "run.kind")
        .map(|r| (r["target"].as_str(), r["value"].as_str()))
        .collect();
    assert_eq!(kinds, [("flat@all", "sample")]);
    assert_eq!(
        tables::read(&dir.join("out/smoke/expected.parquet"), &run::EXPECTED)
            .unwrap()
            .rows
            .len(),
        3
    );

    // every question of the keys sample, under the DBA's indexes alone and with the
    // surveyors, answers alike on the flat tables and on the class hierarchy, against one truth
    let code = crate::sample_cmd(sample_args("keys", &[("flat", &flat), ("oo", &oo)], &dir))
        .await
        .unwrap();
    let run = last_run(&dir, "keys");
    let results = tables::read(&run.join("results"), &crate::report::RESULTS).unwrap();
    for r in &results.rows {
        assert_eq!(
            r["verdict"], "RIGHT",
            "{} {}: {}",
            r["target"], r["question"], r["error"]
        );
    }
    assert_eq!(
        code,
        0,
        "{}",
        fs::read_to_string(run.join("summary.txt")).unwrap()
    );
    let per_target: BTreeSet<&str> = results.rows.iter().map(|r| r["target"].as_str()).collect();
    assert_eq!(
        per_target,
        ["flat@all", "flat@dba", "oo@all", "oo@dba"].into()
    );
    // beside the results: each relation's scans under both sets, the estimate against the actual
    let summary = fs::read_to_string(run.join("summary.txt")).unwrap();
    assert!(summary.contains("scans.txt"), "{summary}");
    let scans = fs::read_to_string(run.join("scans.txt")).unwrap();
    let mut lines = scans.lines();
    assert_eq!(
        lines.next().unwrap(),
        "question\trelation\ttarget\tnode\tscan\tindex\testimate\tactual\tloops\theap_fetches"
    );
    let body: Vec<Vec<&str>> = lines.map(|l| l.split('\t').collect()).collect();
    assert!(body.iter().all(|f| f.len() == 10), "{scans}");
    assert!(
        body.iter().all(|f| f[6].parse::<f64>().is_ok()),
        "every scan records its estimate: {scans}"
    );
    for set in ["@all", "@dba"] {
        assert!(
            body.iter()
                .any(|f| f[2].ends_with(set) && f[1] == "lego.lego_purchases"),
            "{set}: {scans}"
        );
    }
    let exp = tables::read(&dir.join("out/keys/expected.parquet"), &run::EXPECTED).unwrap();
    assert_eq!(exp.rows.len(), 9);
    assert!(exp.rows.iter().all(|r| r["rows"] != "0"), "{:?}", exp.rows);

    // seeds: the lock refuses a sample; a changed expected answer is WRONG; expected answers made
    // on another database are refused
    fs::write(dir.join("TIMING.lock"), "a heavy job\n").unwrap();
    let e = crate::sample_cmd(sample_args("smoke", &[("flat", &flat)], &dir))
        .await
        .unwrap_err();
    assert!(e.contains("TIMING.lock exists"), "{e}");
    fs::remove_file(dir.join("TIMING.lock")).unwrap();
    let path = dir.join("out/smoke/expected.parquet");
    let mut rows = tables::read_values(&path, &run::EXPECTED).unwrap();
    rows[0][2] = Value::Text("0000000000000000".repeat(3));
    tables::write_values(&path, &run::EXPECTED, &rows).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let code = crate::sample_cmd(sample_args("smoke", &[("flat", &flat)], &dir))
        .await
        .unwrap();
    assert_eq!(code, crate::EXIT_WRONG);
    for r in &mut rows {
        r[4] = Value::Text("postgres://elsewhere:5432/other lego.lego_sets index_set=all".into());
    }
    tables::write_values(&path, &run::EXPECTED, &rows).unwrap();
    let e = crate::sample_cmd(sample_args("smoke", &[("flat", &flat)], &dir))
        .await
        .unwrap_err();
    assert!(e.contains("was made on"), "{e}");
    drop_db(DB).await;
    fs::remove_dir_all(&dir).unwrap();
}

/// Plain questions in a directory of their own, `(name, text)` each, with no bound values.
fn plain_questions(dir: &Path, texts: &[(&str, &str)]) -> QuestionSet {
    for (name, text) in texts {
        put(dir, &format!("plain/{name}.sql"), text);
    }
    put(dir, "plain.tsv", "question\tname\tvalue\n");
    QuestionSet::load(&dir.join("plain"), &dir.join("plain.tsv")).unwrap()
}

#[tokio::test]
#[ignore]
async fn a_question_that_is_not_one_statement_returning_rows_is_refused_by_name() {
    let _one = one_at_a_time();
    fixture().await;
    let pg = server();
    let flat = target_url(&pg, DB, "lego", "");
    let fine = (
        "q/fine",
        "SELECT set_num FROM lego_sets WHERE theme_id = 158\n",
    );
    for (name, text) in [
        ("q/commits", "SELECT 1 AS n; COMMIT\n"),
        ("q/only_commit", "COMMIT\n"),
    ] {
        let dir = scratch("one-statement");
        let set = plain_questions(&dir, &[fine, (name, text)]);
        let c = ctx(&dir.join("work"));
        for sets in [None, Some(index_sets(&["dba"]))] {
            let e = run::resolve(&[spec("flat", &flat)], &c, &set, sets.as_ref())
                .await
                .err()
                .unwrap_or_default();
            assert!(e.contains(name), "{name}: {e}");
        }
        fs::remove_dir_all(&dir).unwrap();
    }
    let dir = scratch("one-statement");
    let set = plain_questions(&dir, &[fine]);
    let c = ctx(&dir.join("work"));
    run::resolve(&[spec("flat", &flat)], &c, &set, None)
        .await
        .unwrap();
    fs::remove_dir_all(&dir).unwrap();
    drop_db(DB).await;
}

#[tokio::test]
#[ignore]
async fn a_question_that_writes_writes_no_row_and_is_recorded_as_an_error() {
    let _one = one_at_a_time();
    let url = fixture().await;
    let pg = server();
    let dir = scratch("writes");
    let set = plain_questions(
        &dir,
        &[(
            "q/writes",
            "INSERT INTO lego_themes (id, name) VALUES (99999, 'written') RETURNING id\n",
        )],
    );
    let c = ctx(&dir.join("work"));
    let flat = target_url(&pg, DB, "lego", "");
    let written = "SELECT count(*) FROM lego.lego_themes WHERE id = 99999";
    let mut probe = session(&url).await;
    let mut lives = run::resolve(&[spec("flat", &flat)], &c, &set, None)
        .await
        .unwrap();
    lives.extend(
        run::resolve(
            &[spec("flat", &flat)],
            &c,
            &set,
            Some(&index_sets(&["all", "dba"])),
        )
        .await
        .unwrap(),
    );
    let q = &set.questions[0];
    let exp = std::collections::BTreeMap::from([(
        q.name.clone(),
        run::Expected {
            rows: 1,
            digest: String::new(),
        },
    )]);
    // as `truth`, `check` and `run` send it, with no index set and under each
    let out = dir.join("check.parquet");
    run::check(&mut lives, &[q], &exp, &c, &out, false)
        .await
        .unwrap();
    for line in tables::read(&out, &run::CHECK).unwrap().rows {
        assert_eq!(
            line["status"], "ERROR",
            "{}: {}",
            line["target"], line["error"]
        );
        assert!(line["error"].contains("read-only"), "{}", line["error"]);
    }
    assert_eq!(scalar(&mut probe.conn, written).await, "0");
    // and as its EXPLAIN's session sends it
    let g = guard();
    for live in lives.iter_mut() {
        let st = run::statements_for(live, q);
        let mut s = session(&flat).await;
        let e = run::run_explain(
            &mut live.control.conn,
            &mut s,
            &g,
            &st,
            &live.target.leaves,
            live.target.server_version_num,
        )
        .await;
        assert_eq!(e.status, Status::Error, "{}: {}", live.target.name, e.error);
        s.close().await;
    }
    assert_eq!(scalar(&mut probe.conn, written).await, "0");
    probe.close().await;
    fs::remove_dir_all(&dir).unwrap();
    drop_db(DB).await;
}

#[tokio::test]
#[ignore]
async fn after_a_run_under_dba_every_index_and_surveyor_is_still_there() {
    let _one = one_at_a_time();
    let url = fixture().await;
    let pg = server();
    let flat = target_url(&pg, DB, "lego", "");
    let mut probe = session(&url).await;
    let before =
        index_sets::roster_fingerprint(&index_sets::read_roster(&mut probe.conn).await.unwrap());
    let reads = (
        "q/sets",
        "SELECT set_num FROM lego_sets WHERE theme_id = 158\n",
    );
    let commits = ("q/commits", "SELECT 1 AS n; COMMIT\n");
    let g = guard();
    for texts in [vec![reads], vec![reads, commits]] {
        let dir = scratch("kept");
        let set = plain_questions(&dir, &texts);
        let c = ctx(&dir.join("work"));
        // every question once under dba, as each repetition of a run sends it
        let refused = match run::resolve(
            &[spec("flat", &flat)],
            &c,
            &set,
            Some(&index_sets(&["dba"])),
        )
        .await
        {
            Ok(mut lives) => {
                for live in lives.iter_mut() {
                    for q in &set.questions {
                        let st = run::statements_for(live, q);
                        let mut s = session(&flat).await;
                        run::run_timed(&mut live.control.conn, &mut s, &g, &st).await;
                        s.close().await;
                    }
                }
                None
            }
            Err(e) => Some(e),
        };
        let after = index_sets::roster_fingerprint(
            &index_sets::read_roster(&mut probe.conn).await.unwrap(),
        );
        assert_eq!(after, before, "the indexes changed after {texts:?}");
        // a question that commits is refused by its name, before anything runs
        match refused {
            None => assert_eq!(texts.len(), 1, "a question that commits was run"),
            Some(e) => assert!(e.contains(commits.0), "{e}"),
        }
        fs::remove_dir_all(&dir).unwrap();
    }
    let surveyors = "SELECT count(*) FROM pg_class c JOIN pg_am a ON a.oid = c.relam \
                     WHERE a.amname = 'surveyor'";
    assert_eq!(
        scalar(&mut probe.conn, surveyors).await,
        SURVEYOR_COUNT.to_string()
    );
    probe.close().await;
    drop_db(DB).await;
}
