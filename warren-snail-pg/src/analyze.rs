// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! ANALYZE of a foreign table: every row the snail holds read, a sample of them kept as PostgreSQL's
//! own sampler keeps it, and the table's rows and pages as the snail counts them.

use crate::{compat, datum, scan};
use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::PgMemoryContexts;
use std::ffi::c_int;
use warren_snail::cut::PAGE;

/// The question that reads every column of the foreign table, and the snail open to answer it.
unsafe fn whole(relation: pg_sys::Relation) -> (scan::Target, warren_snail::read::Snail, String) {
    let target = scan::Target::of((*relation).rd_id);
    let snail = scan::open(&target.file);
    target.check(&snail);
    let attnos: Vec<i16> = target.columns.iter().map(|c| c.0).collect();
    let question = target.question(&attnos, &[]);
    (target, snail, question)
}

/// The sampler, and the pages the table's whole read takes, at PostgreSQL's page size.
#[pg_guard]
pub(crate) unsafe extern "C-unwind" fn analyze(
    relation: pg_sys::Relation,
    func: *mut pg_sys::AcquireSampleRowsFunc,
    totalpages: *mut pg_sys::BlockNumber,
) -> bool {
    let (target, snail, question) = whole(relation);
    let price = snail
        .price(&question, None)
        .unwrap_or_else(|e| scan::refuse(format!("the snail {}: {question}: {e}", target.file)));
    let pages = (snail.pages_at_open + price.directory_pages + price.column_pages) * PAGE as u64;
    *totalpages = pages.div_ceil(pg_sys::BLCKSZ as u64) as pg_sys::BlockNumber;
    *func = Some(sample);
    true
}

/// Every row the snail holds read, and `targrows` of them kept, each as likely as any other: the
/// first ones, then each that follows in turn of the rows PostgreSQL's sampler picks.
#[pg_guard]
unsafe extern "C-unwind" fn sample(
    relation: pg_sys::Relation,
    elevel: c_int,
    rows: *mut pg_sys::HeapTuple,
    targrows: c_int,
    totalrows: *mut f64,
    totaldeadrows: *mut f64,
) -> c_int {
    let (target, snail, question) = whole(relation);
    let mut round = snail
        .scan(&question, None)
        .unwrap_or_else(|e| scan::refuse(format!("the snail {}: {question}: {e}", target.file)));
    let desc = (*relation).rd_att;
    let natts = (*desc).natts as usize;
    let mut values = vec![pg_sys::Datum::from(0); natts];
    let mut nulls = vec![true; natts];
    let mut state: pg_sys::ReservoirStateData = std::mem::zeroed();
    pg_sys::reservoir_init_selection_state(&mut state, targrows);
    let rows = std::slice::from_raw_parts_mut(rows, targrows as usize);
    let (mut kept, mut seen, mut skip) = (0usize, 0f64, -1f64);
    let mut row_memory = PgMemoryContexts::new("warren_snail sampled row");
    loop {
        let batch = match snail.next_block(&mut round) {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => scan::refuse(format!("the snail {}: {e}", target.file)),
        };
        pgrx::check_for_interrupts!();
        for i in 0..batch.num_rows() {
            let at = if kept < targrows as usize {
                Some(kept)
            } else {
                if skip < 0.0 {
                    skip = pg_sys::reservoir_get_next_S(&mut state, seen, targrows);
                }
                let at = (skip <= 0.0)
                    .then(|| (targrows as f64 * compat::random_fract(&mut state)) as usize);
                skip -= 1.0;
                at
            };
            seen += 1.0;
            let Some(at) = at else { continue };
            pg_sys::MemoryContextReset(row_memory.value());
            row_memory.switch_to(|_| {
                nulls.fill(true);
                for (j, (attno, ..)) in target.columns.iter().enumerate() {
                    if let Some(d) = datum::datum(batch.column(j), i) {
                        values[*attno as usize - 1] = d;
                        nulls[*attno as usize - 1] = false;
                    }
                }
            });
            let tuple = pg_sys::heap_form_tuple(desc, values.as_mut_ptr(), nulls.as_mut_ptr());
            if at < kept {
                pg_sys::heap_freetuple(rows[at]);
            } else {
                kept += 1;
            }
            rows[at] = tuple;
        }
    }
    *totalrows = seen;
    *totaldeadrows = 0.0;
    ereport!(
        PgLogLevel::from(elevel as isize),
        PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION,
        format!(
            "\"{}\": the snail holds {seen} rows of {}; {kept} kept as the sample",
            pgrx::PgRelation::from_pg(relation).name(),
            target.table
        )
    );
    kept as c_int
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use crate::deparse::tests::events;
    use crate::scan::tests::{generation_held, mounted, named_whole, one, texts};
    use pgrx::prelude::*;
    use warren_snail::read::Snail;

    /// Each column's statistics, as pg_stats keeps them, but for the width and the correlation,
    /// which follow where the heap lays its rows.
    fn statistics(table: &str) -> Vec<String> {
        texts(&format!(
            "SELECT format('%s %s %s %s %s %s', attname, null_frac, n_distinct, \
               most_common_vals, most_common_freqs, histogram_bounds) \
             FROM pg_stats WHERE tablename = '{table}' ORDER BY attname"
        ))
    }

    /// A table smaller than the sample is read whole on both sides, so its statistics are the
    /// heap's; the table's rows and pages are the snail's, and the snail is let go.
    #[pg_test]
    fn a_table_within_the_sample_gathers_the_statistics_the_heap_gathers() {
        let path = mounted("kinds_analyzed", 3000, named_whole);
        Spi::run("ANALYZE kinds").unwrap();
        Spi::run("ANALYZE kinds_heap").unwrap();
        let ours = statistics("kinds");
        assert_eq!(ours.len(), 8);
        assert_eq!(ours, statistics("kinds_heap"));
        let snail = Snail::open(&path).unwrap();
        let question = "SELECT \"id\", \"small\", \"mid\", \"at\", \"name\", \"code\", \"raw\", \
                        \"price\" FROM \"shop\".\"kinds\"";
        let price = snail.price(question, None).unwrap();
        let pages = (snail.pages_at_open + price.directory_pages + price.column_pages) * 4096;
        assert_eq!(
            one(
                "SELECT format('%s %s', reltuples, relpages) FROM pg_class WHERE relname = 'kinds'"
            ),
            format!("3010 {}", pages.div_ceil(pg_sys::BLCKSZ as u64))
        );
        drop(snail);
        assert!(!generation_held(&path));
    }

    /// A table past the sample is read whole and counted whole, a sample of it kept.
    #[pg_test]
    fn a_table_past_the_sample_is_counted_whole_and_sampled() {
        events();
        Spi::run("SET LOCAL default_statistics_target = 10").unwrap();
        Spi::run("ANALYZE mounted.events").unwrap();
        assert_eq!(
            one(
                "SELECT reltuples::text FROM pg_class WHERE relname = 'events' \
                 AND relnamespace = 'mounted'::regnamespace"
            ),
            "20000"
        );
        let stat = |column: &str, field: &str| -> f64 {
            one(&format!(
                "SELECT {field}::text FROM pg_stats WHERE schemaname = 'mounted' \
                 AND tablename = 'events' AND attname = '{column}'"
            ))
            .parse()
            .unwrap()
        };
        assert_eq!(stat("id", "n_distinct"), -1.0);
        let nulls = stat("note", "null_frac");
        assert!((0.07..0.13).contains(&nulls), "{nulls}");
        // The sample is drawn from every block, not the first ones the snail hands out: the
        // middle of the ids' histogram lies near the middle of 1 to 20,000.
        let middle: f64 = one(
            "SELECT h[array_length(h, 1) / 2 + 1]::text FROM (SELECT histogram_bounds::text::bigint[] \
             AS h FROM pg_stats WHERE schemaname = 'mounted' AND tablename = 'events' \
             AND attname = 'id') s",
        )
        .parse()
        .unwrap();
        assert!((7000.0..13000.0).contains(&middle), "{middle}");
    }

    /// Beside the conditions the snail reads, EXPLAIN shows the rows its counts bound and
    /// PostgreSQL's own estimate of them, against the rows the scan hands out.
    #[pg_test]
    fn explain_shows_the_snails_rows_beside_the_statistics() {
        events();
        Spi::run("ANALYZE mounted.events").unwrap();
        let plan = texts(
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) \
             SELECT id FROM mounted.events WHERE id < 50",
        );
        let shown = |label: &str| {
            plan.iter()
                .find_map(|l| l.trim_start().strip_prefix(label))
                .unwrap_or_else(|| panic!("{label}: {plan:#?}"))
                .to_string()
        };
        let bounds: Vec<u64> = shown("Snail Rows: ")
            .split(" to ")
            .map(|n| n.parse().unwrap())
            .collect();
        assert!(bounds[0] <= 49 && 49 <= bounds[1], "{plan:#?}");
        let estimate: u64 = shown("Statistics Rows: ").parse().unwrap();
        assert!((25..100).contains(&estimate), "{plan:#?}");
        // PostgreSQL 18 prints actual rows to two places.
        assert!(
            plan[0].contains("actual rows=49 ") || plan[0].contains("actual rows=49.00 "),
            "{plan:#?}"
        );
    }
}
