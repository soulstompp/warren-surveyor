// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! What a statement read, from the server's per-relation counters rather than from its EXPLAIN.
//! The counters are read on the control session before and after the statement, and the
//! statement's session forces its statistics out before the second read. Parallel workers send
//! theirs when they exit, so the difference includes every process that scanned. It includes the
//! planner's own reads too (the planner can fetch a column's actual extremes through an index to
//! estimate a range), which no EXPLAIN scan node reports. It is the statement's own figure only
//! while nothing else in the database reads the same relations, which is checked at both reads.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use sqlx::postgres::PgConnection;

/// The first server version (`server_version_num`) with `pg_stat_force_next_flush()`.
pub const FROM: u32 = 150_000;

/// Whether a server can report what a statement read: it has `pg_stat_force_next_flush()` and
/// counts at all (`track_counts`, as the target recorded it).
pub fn reports(server_version_num: u32, track_counts: &str) -> bool {
    server_version_num >= FROM && track_counts == "on"
}

const APPLICATION_NAME: &str = crate::target::APPLICATION_NAME;
const SETTLE_TRIES: u32 = 20;
const SETTLE_PAUSE: Duration = Duration::from_millis(25);

/// Per relation (`schema.relation`): scans started, rows read, and blocks read or hit.
pub type Deltas = BTreeMap<String, (i64, i64, i64)>;

/// One reading of the counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Counters {
    pub rels: Deltas,
    /// Other backends connected to the database: anything but this program's own sessions.
    pub others: i64,
}

/// The difference of two readings. `status` is OK, NOT_ALONE when another backend was connected to
/// the database at either reading, or UNSETTLED when the counters kept changing after the
/// statement; the figures are kept in every case.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub status: &'static str,
    pub deltas: Deltas,
}

/// A reading's relations split between a target's leaves and everything else.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Split {
    /// The leaves with a scan started on them. A parallel scan is started on every leaf the plan
    /// initialises, including one that execution then skips.
    pub touched: BTreeSet<String>,
    /// The leaves that gave up rows. An empty leaf, or one only initialised, is not among them.
    pub yielded: BTreeSet<String>,
    pub leaf_rows: i64,
    pub leaf_blocks: i64,
    pub other_rows: i64,
    pub other_blocks: i64,
    /// Every other relation read, with its rows and blocks.
    pub others: BTreeMap<String, (i64, i64)>,
}

/// The other relations as one line of text: `relation rows/blocks`, comma-separated, in name order.
pub fn others_text(others: &BTreeMap<String, (i64, i64)>) -> String {
    others
        .iter()
        .map(|(r, (rows, blocks))| format!("{r} {rows}/{blocks}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One reading of the counters, taken from a fresh statistics snapshot.
pub async fn read(control: &mut PgConnection) -> Result<Counters, String> {
    let e = |e: sqlx::Error| format!("reading the statistics counters: {e}");
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut *control)
        .await
        .map_err(e)?;
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT s.schemaname || '.' || s.relname,
                coalesce(s.seq_scan, 0) + coalesce(s.idx_scan, 0),
                coalesce(s.seq_tup_read, 0) + coalesce(s.idx_tup_fetch, 0),
                coalesce(io.heap_blks_read, 0) + coalesce(io.heap_blks_hit, 0)
                  + coalesce(io.idx_blks_read, 0) + coalesce(io.idx_blks_hit, 0)
         FROM pg_stat_all_tables s JOIN pg_statio_all_tables io USING (relid)
         WHERE s.schemaname <> 'information_schema' AND s.schemaname NOT LIKE 'pg\\_%'",
    )
    .fetch_all(&mut *control)
    .await
    .map_err(e)?;
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND pid <> pg_backend_pid()
           AND coalesce(application_name, '') <> $1",
    )
    .bind(APPLICATION_NAME)
    .fetch_one(&mut *control)
    .await
    .map_err(e)?;
    Ok(Counters {
        rels: rows
            .into_iter()
            .map(|(r, a, b, c)| (r, (a, b, c)))
            .collect(),
        others,
    })
}

/// Sends this session's pending statistics now; they go when the statement finishes.
pub async fn flush(session: &mut PgConnection) -> Result<(), String> {
    sqlx::query("SELECT pg_stat_force_next_flush()")
        .execute(session)
        .await
        .map(|_| ())
        .map_err(|e| format!("pg_stat_force_next_flush(): {e}"))
}

/// Reads until two readings in a row agree, and says whether they did.
pub async fn settled(control: &mut PgConnection) -> Result<(Counters, bool), String> {
    let mut last = read(control).await?;
    for _ in 0..SETTLE_TRIES {
        tokio::time::sleep(SETTLE_PAUSE).await;
        let next = read(control).await?;
        if next.rels == last.rels {
            return Ok((next, true));
        }
        last = next;
    }
    Ok((last, false))
}

pub fn difference(before: &Counters, after: &Counters, settled: bool) -> Reading {
    let mut deltas = Deltas::new();
    for (rel, &(s, r, b)) in &after.rels {
        let (s0, r0, b0) = before.rels.get(rel).copied().unwrap_or((0, 0, 0));
        if (s - s0, r - r0, b - b0) != (0, 0, 0) {
            deltas.insert(rel.clone(), (s - s0, r - r0, b - b0));
        }
    }
    Reading {
        status: if !settled {
            "UNSETTLED"
        } else if before.others > 0 || after.others > 0 {
            "NOT_ALONE"
        } else {
            "OK"
        },
        deltas,
    }
}

/// `a` less `b`, relation by relation.
pub fn minus(a: &Deltas, b: &Deltas) -> Deltas {
    let mut out = a.clone();
    for (rel, &(s, r, k)) in b {
        let e = out.entry(rel.clone()).or_insert((0, 0, 0));
        *e = (e.0 - s, e.1 - r, e.2 - k);
    }
    out
}

/// Splits relations between the target's leaves (those named in `leaves`, within `schema`) and
/// every other relation.
pub fn split(d: &Deltas, schema: &str, leaves: &BTreeSet<String>) -> Split {
    let mut out = Split::default();
    for (rel, &(scans, rows, blocks)) in d {
        let leaf = rel
            .split_once('.')
            .filter(|(s, name)| *s == schema && leaves.contains(*name))
            .map(|(_, name)| name);
        match leaf {
            Some(name) => {
                if scans > 0 {
                    out.touched.insert(name.to_string());
                }
                if rows > 0 {
                    out.yielded.insert(name.to_string());
                }
                out.leaf_rows += rows;
                out.leaf_blocks += blocks;
            }
            None => {
                out.other_rows += rows;
                out.other_blocks += blocks;
                if (rows, blocks) != (0, 0) {
                    out.others.insert(rel.clone(), (rows, blocks));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(rels: &[(&str, (i64, i64, i64))], others: i64) -> Counters {
        Counters {
            rels: rels.iter().map(|(r, c)| (r.to_string(), *c)).collect(),
            others,
        }
    }

    fn leaves() -> BTreeSet<String> {
        ["a", "b"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_server_reports_only_with_the_flush_and_with_track_counts_on() {
        assert!(reports(FROM, "on"));
        assert!(!reports(FROM - 1, "on"));
        assert!(!reports(FROM, "off"));
        assert!(!reports(FROM, ""));
    }

    #[test]
    fn a_difference_names_the_leaves_scanned_and_splits_leaves_from_the_rest() {
        let before = counters(
            &[
                ("s.a", (1, 100, 10)),
                ("s.b", (1, 50, 5)),
                ("public.lego_colors", (0, 0, 0)),
            ],
            0,
        );
        let after = counters(
            &[
                ("s.a", (2, 180, 18)),
                ("s.b", (1, 50, 5)),
                ("public.lego_colors", (1, 135, 1)),
                ("other.a", (1, 7, 1)),
            ],
            0,
        );
        let r = difference(&before, &after, true);
        assert_eq!(r.status, "OK");
        let s = split(&r.deltas, "s", &leaves());
        assert_eq!(s.touched.iter().collect::<Vec<_>>(), ["a"]);
        assert_eq!((s.leaf_rows, s.leaf_blocks), (80, 8));
        assert_eq!((s.other_rows, s.other_blocks), (142, 2));
        assert_eq!(
            others_text(&s.others),
            "other.a 7/1, public.lego_colors 135/1"
        );
    }

    #[test]
    fn a_leaf_started_and_left_empty_is_touched_but_yields_nothing() {
        let d: Deltas = [
            ("s.a".to_string(), (1, 0, 0)),
            ("s.b".to_string(), (1, 3, 1)),
        ]
        .into_iter()
        .collect();
        let s = split(&d, "s", &leaves());
        assert_eq!(s.touched.iter().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(s.yielded.iter().collect::<Vec<_>>(), ["b"]);
        assert!(s.others.is_empty());
    }

    #[test]
    fn the_planners_own_reads_come_off_relation_by_relation() {
        let timed: Deltas = [
            ("s.a".to_string(), (1, 80, 8)),
            ("s.b".to_string(), (1, 2, 1)),
            ("public.lego_colors".to_string(), (3, 137, 2)),
        ]
        .into_iter()
        .collect();
        let planning: Deltas = [
            ("s.b".to_string(), (1, 2, 1)),
            ("public.lego_colors".to_string(), (2, 2, 1)),
        ]
        .into_iter()
        .collect();
        let s = split(&minus(&timed, &planning), "s", &leaves());
        assert_eq!(s.touched.iter().collect::<Vec<_>>(), ["a"]);
        assert_eq!((s.leaf_rows, s.other_rows), (80, 135));
    }

    #[test]
    fn another_backend_or_moving_counters_mark_the_reading() {
        let c = counters(&[("s.a", (1, 1, 1))], 0);
        let busy = counters(&[("s.a", (1, 1, 1))], 1);
        assert_eq!(difference(&c, &busy, true).status, "NOT_ALONE");
        assert_eq!(difference(&busy, &c, true).status, "NOT_ALONE");
        assert_eq!(difference(&c, &c, false).status, "UNSETTLED");
    }
}
