// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Keeping the server safe: the backend's resident memory is polled while a statement
//! runs, and the backend is cancelled above a cap. A run does not start while a loader is
//! connected.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use sqlx::postgres::PgConnection;
use tracing::{error, warn};

/// Set when the user interrupts the program; the running statement's backend is then cancelled.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// What a loading session's name starts with: the generator's sessions are `sqlc-brickgen/<n>`.
pub const LOADER_PREFIXES: [&str; 1] = ["sqlc-brickgen"];

/// The loaders' names, as a message gives them.
pub fn loader_names() -> String {
    LOADER_PREFIXES.map(|p| format!("{p}*")).join(" or ")
}

#[derive(Clone, Debug)]
pub struct Guard {
    pub cap_kb: u64,
    pub interval: Duration,
    pub grace: Duration,
    /// When false, memory is not polled (the server's processes are not visible here).
    pub poll_memory: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Watched {
    /// Largest resident memory seen, the backend and its parallel workers together, in kB.
    pub peak_rss_kb: u64,
    /// Largest anonymous (private) resident memory seen, in kB.
    pub peak_anon_kb: u64,
    pub samples: u32,
    pub cancelled: bool,
    pub terminated: bool,
    pub interrupted: bool,
}

/// `VmRSS` and `RssAnon` of a process, in kB.
pub fn proc_memory(pid: i32) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some((field("VmRSS:")?, field("RssAnon:").unwrap_or(0)))
}

/// Checks that `pid` is a server process this program can see, so that its memory can be read.
pub fn visible_backend(pid: i32) -> Result<(), String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map_err(|e| format!("/proc/{pid}/comm: {e}; the server is not on this machine"))?;
    if comm.trim() != "postgres" {
        return Err(format!(
            "process {pid} here is `{}`, not a postgres backend; the server is not on this machine",
            comm.trim()
        ));
    }
    proc_memory(pid)
        .map(|_| ())
        .ok_or_else(|| format!("/proc/{pid}/status: VmRSS not readable"))
}

/// Sessions whose `application_name` starts with one of the loaders' prefixes, one line each.
pub async fn loader_sessions(control: &mut PgConnection) -> Result<Vec<String>, String> {
    let rows: Vec<(i32, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT pid, datname::text, application_name, state
         FROM pg_stat_activity
         WHERE application_name LIKE ANY ($1) AND pid <> pg_backend_pid()
         ORDER BY pid",
    )
    .bind(LOADER_PREFIXES.map(|p| format!("{p}%")).to_vec())
    .fetch_all(control)
    .await
    .map_err(|e| format!("reading pg_stat_activity: {e}"))?;
    Ok(rows
        .into_iter()
        .map(|(pid, db, app, state)| {
            format!(
                "pid {pid} database {} application_name {app} state {}",
                db.unwrap_or_default(),
                state.unwrap_or_default()
            )
        })
        .collect())
}

async fn workers(control: &mut PgConnection, pid: i32) -> Vec<i32> {
    match sqlx::query_scalar::<_, i32>("SELECT pid FROM pg_stat_activity WHERE leader_pid = $1")
        .bind(pid)
        .fetch_all(control)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "could not list parallel workers");
            vec![]
        }
    }
}

async fn signal(control: &mut PgConnection, f: &str, pid: i32) {
    let q = format!("SELECT {f}($1)");
    if let Err(e) = sqlx::query(&q).bind(pid).execute(control).await {
        error!(error = %e, pid, function = f, "could not signal the backend");
    }
}

/// Cancels the statement running on `pid`.
async fn cancel(control: &mut PgConnection, pid: i32) {
    signal(control, "pg_cancel_backend", pid).await
}

/// Ends the session `pid`.
async fn terminate(control: &mut PgConnection, pid: i32) {
    signal(control, "pg_terminate_backend", pid).await
}

/// The memory of session `pid` in kB, resident and anonymous, and how many processes it is.
async fn memory(control: &mut PgConnection, pid: i32) -> (u64, u64, usize) {
    let mut pids = vec![pid];
    pids.extend(workers(control, pid).await);
    let (mut rss, mut anon) = (0u64, 0u64);
    for p in &pids {
        if let Some((r, a)) = proc_memory(*p) {
            rss += r;
            anon += a;
        }
    }
    (rss, anon, pids.len())
}

/// Runs `fut` (a statement on the session `pid`) while polling that session's memory through
/// `control`, another session on the same server. Above the cap, or when the user interrupts, the
/// statement is cancelled (`pg_cancel_backend`); if it is still running after the grace period,
/// the session is terminated (`pg_terminate_backend`).
pub async fn watch<F: Future>(
    control: &mut PgConnection,
    pid: i32,
    guard: &Guard,
    fut: F,
) -> (F::Output, Watched) {
    let mut w = Watched::default();
    let mut ticker = tokio::time::interval(guard.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cancelled_at: Option<Instant> = None;
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            out = &mut fut => return (out, w),
            _ = ticker.tick() => {
                if guard.poll_memory {
                    let (rss, anon, processes) = memory(control, pid).await;
                    w.samples += 1;
                    w.peak_rss_kb = w.peak_rss_kb.max(rss);
                    w.peak_anon_kb = w.peak_anon_kb.max(anon);
                    if rss > guard.cap_kb && cancelled_at.is_none() {
                        error!(pid, rss_kb = rss, cap_kb = guard.cap_kb, processes,
                               "backend memory above the cap: cancelling the backend");
                        cancel(control, pid).await;
                        w.cancelled = true;
                        cancelled_at = Some(Instant::now());
                    }
                }
                if INTERRUPTED.load(Ordering::SeqCst) && cancelled_at.is_none() {
                    warn!(pid, "interrupted: cancelling the backend");
                    cancel(control, pid).await;
                    w.cancelled = true;
                    w.interrupted = true;
                    cancelled_at = Some(Instant::now());
                }
                if let Some(t) = cancelled_at {
                    if !w.terminated && t.elapsed() > guard.grace {
                        error!(pid, grace_s = guard.grace.as_secs(), "still running after cancel: terminating the backend");
                        terminate(control, pid).await;
                        w.terminated = true;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process_memory() {
        let (rss, _anon) = proc_memory(std::process::id() as i32).expect("own /proc status");
        assert!(rss > 0);
    }

    #[test]
    fn a_process_that_is_not_postgres_is_refused() {
        assert!(visible_backend(std::process::id() as i32).is_err());
    }
}
