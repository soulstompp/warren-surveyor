// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runs a declared set of questions against PostgreSQL targets. A target is a connection
//! URL and a table; the questions are plain `.sql` files, sent as written, or `.sqlc` templates
//! composed once per target with `cargo sqlc`. Every timing comes with its answer, checked against
//! expected answers fixed beforehand.

mod canonical;
mod derived;
mod digest;
mod explain;
mod index_sets;
mod kit;
#[cfg(test)]
mod pg_tests;
mod questions;
mod report;
mod run;
mod samples;
#[cfg(test)]
mod server_tests;
mod stats;
mod tables;
mod target;
mod watchdog;
mod witness;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::questions::QuestionSet;
use crate::run::{Ctx, Ending, Finished, IndexSets, Plan, Spec};
use crate::target::Url;
use crate::watchdog::{Guard, INTERRUPTED};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "warren-bench",
    version,
    about = "Run a declared set of questions against Postgres targets, and check every answer"
)]
struct Cli {
    #[arg(long, value_enum, default_value = "pretty", global = true)]
    log_format: LogFormat,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Compose and verify the questions for every target (plain questions are written as they
    /// are), then stop.
    Compose(Common),
    /// Run every question on one reference target and write the expected answers.
    Truth {
        #[command(flatten)]
        common: Common,
        /// The expected answers file to write (Parquet).
        #[arg(long)]
        out: PathBuf,
        /// Replace an existing expected answers file.
        #[arg(long)]
        force: bool,
    },
    /// Run every question on every target, and check every answer.
    Run {
        #[command(flatten)]
        common: Common,
        /// The expected answers, written by `truth`.
        #[arg(long)]
        expected: PathBuf,
        /// Directory for results/, leaves/, targets.parquet, summary.parquet and summary.txt.
        #[arg(long)]
        out: PathBuf,
        /// Repetitions each in new sessions.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..))]
        cold: u32,
        /// Repetitions in the session of the last cold repetition.
        #[arg(long, default_value_t = 5)]
        warm: u32,
        /// A directory of derived statements, one directory per target (see the README). May be
        /// given more than once.
        #[arg(long)]
        derived: Vec<PathBuf>,
    },
    /// Run questions once on every target and check each answer. Records no time and no plan.
    Check {
        #[command(flatten)]
        common: Common,
        /// The expected answers, written by `truth`.
        #[arg(long)]
        expected: PathBuf,
        /// A directory of derived statements, one directory per target (see the README). May be
        /// given more than once.
        #[arg(long)]
        derived: Vec<PathBuf>,
        /// Check only the questions whose names start with this. May be given more than once.
        #[arg(long)]
        only: Vec<String>,
        /// The file to write (Parquet), one line per (target, question).
        #[arg(long)]
        out: PathBuf,
        /// Replace an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Run a sample: a few questions on a few index sets, certified against expected answers
    /// computed once by this program. A quick check, never a measurement; refused while the lock file exists.
    Sample(SampleArgs),
    /// Print the summary of a results file, or of a run's results directory.
    Summarize {
        #[arg(long)]
        results: PathBuf,
    },
    /// Write any file this program wrote (or a directory of them) as tab-separated text.
    Export {
        /// A Parquet file, or a directory of them.
        #[arg(long)]
        from: PathBuf,
        /// The text file to write.
        #[arg(long)]
        to: PathBuf,
    },
    /// Read text written by `export` back into the Parquet file it came from.
    Import {
        /// The text file.
        #[arg(long)]
        from: PathBuf,
        /// The Parquet file to write.
        #[arg(long)]
        to: PathBuf,
    },
    /// Print the kit's surveyor.sql, or write its questions, binds and surveyor.sql out to read.
    Kit {
        #[command(subcommand)]
        cmd: KitCmd,
    },
}

#[derive(Subcommand, Debug)]
enum KitCmd {
    /// Print surveyor.sql, which turns the surveyor on or off for a whole database:
    /// `warren-bench kit surveyor-sql | psql -X -d "$DB" -v mode=on -f -`.
    SurveyorSql,
    /// Write the questions, binds.tsv and surveyor.sql into DIR, to read or edit; nothing is
    /// written where any of them exists already.
    Write {
        /// The directory to write into, made where it does not exist.
        dir: PathBuf,
    },
}

#[derive(Args, Debug)]
struct Common {
    /// A target: its name, its table, and a connection URL naming host, port and database
    /// (postgres://…).
    #[arg(long = "target", num_args = 3, value_names = ["NAME", "TABLE", "URL"])]
    target: Vec<String>,
    /// The questions: a directory of plain `.sql` files, or of `.sqlc` templates. By default the
    /// kit's, built into the bench and written once into its cache directory.
    #[arg(long)]
    questions: Option<PathBuf>,
    /// Values for bound parameters: `question  name  value` lines, with a header. By default the
    /// kit's.
    #[arg(long)]
    binds: Option<PathBuf>,
    /// Where each target's questions are composed. By default `compose` in the bench's cache
    /// directory, `$XDG_CACHE_HOME/warren-bench`, else `~/.cache/warren-bench`.
    #[arg(long)]
    work: Option<PathBuf>,
    /// `statement_timeout` for every statement the questions send.
    #[arg(long, default_value = "120s")]
    statement_timeout: String,
    /// Cancel a backend whose resident memory, with its parallel workers', exceeds this.
    #[arg(long, default_value_t = 2048)]
    rss_cap_mb: u64,
    /// How often the backend's memory is read while a statement runs.
    #[arg(long, default_value_t = 100)]
    watch_interval_ms: u64,
    /// Terminate a cancelled backend that is still running after this long.
    #[arg(long, default_value_t = 30)]
    terminate_after_s: u64,
    /// Run without reading the backend's memory (for a server on another machine).
    #[arg(long)]
    no_memory_watchdog: bool,
    /// `plan_cache_mode` for every session: auto, force_custom_plan or force_generic_plan. A bound
    /// question is prepared afresh in each repetition, so under the server's default it always
    /// gets a custom plan; force_generic_plan measures the plan a long-lived statement would reuse.
    #[arg(long)]
    plan_cache_mode: Option<String>,
    /// Index sets, comma-separated: each target becomes one target per set, `<target>@<set>`,
    /// every repetition of which runs as
    /// `BEGIN; DROP INDEX …; SET LOCAL transaction_read_only = on; <question>; ROLLBACK`.
    #[arg(long = "index-sets", value_delimiter = ',')]
    index_sets: Vec<String>,
    /// The file that declares the index sets. By default the bench's own.
    #[arg(long)]
    index_set_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct SampleArgs {
    /// The sample: `<samples>/<name>/sample.json`.
    name: String,
    /// A target: its name, its table, and a connection URL naming host, port and database. The
    /// targets of a sample read one database.
    #[arg(long = "target", num_args = 3, value_names = ["NAME", "TABLE", "URL"], required = true)]
    target: Vec<String>,
    /// Repetitions each in new sessions, in place of the sample's.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    cold: Option<u32>,
    /// Repetitions in the session of the last cold repetition, in place of the sample's.
    #[arg(long)]
    warm: Option<u32>,
    /// The directory of samples, each `<name>/sample.json`.
    #[arg(long)]
    samples: PathBuf,
    /// Where samples write: `<out>/<name>/expected.parquet`, and each run in `<out>/<name>/<UTC>/`.
    #[arg(long)]
    out: PathBuf,
    /// The lock a heavy job holds: the sample refuses to start while it exists, and stops before
    /// its next question if it appears. The sample never takes it.
    #[arg(long)]
    lock: PathBuf,
    /// The file that declares the index sets. By default the bench's own.
    #[arg(long)]
    index_set_file: Option<PathBuf>,
    /// Where each target's questions are composed. By default `compose` in the bench's cache
    /// directory, `$XDG_CACHE_HOME/warren-bench`, else `~/.cache/warren-bench`.
    #[arg(long)]
    work: Option<PathBuf>,
    /// Cancel a backend whose resident memory, with its parallel workers', exceeds this.
    #[arg(long, default_value_t = 2048)]
    rss_cap_mb: u64,
    /// How often the backend's memory is read while a statement runs.
    #[arg(long, default_value_t = 100)]
    watch_interval_ms: u64,
    /// Terminate a cancelled backend that is still running after this long.
    #[arg(long, default_value_t = 30)]
    terminate_after_s: u64,
    /// Run without reading the backend's memory (for a server on another machine).
    #[arg(long)]
    no_memory_watchdog: bool,
}

fn init_logging(fmt: LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn"));
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match fmt {
        LogFormat::Pretty => b.init(),
        LogFormat::Json => b.json().init(),
    }
}

fn valid_timeout(s: &str) -> bool {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let unit = &s[digits..];
    digits > 0 && ["", "ms", "s", "min", "h", "d"].contains(&unit)
}

impl Common {
    fn specs(&self) -> Result<Vec<Spec>, String> {
        let mut out = Vec::new();
        for c in self.target.chunks(3) {
            let [name, table, url] = c else {
                return Err("--target takes NAME TABLE URL".into());
            };
            out.push(Spec::Explicit {
                name: name.clone(),
                table: table.clone(),
                url: Url::parse(url)?,
            });
        }
        Ok(out)
    }

    /// The index sets asked for, read from the declaring file, else the bench's own; `None` when
    /// none were.
    fn index_sets(&self) -> Result<Option<IndexSets>, String> {
        if self.index_sets.is_empty() {
            return Ok(None);
        }
        Ok(Some(IndexSets {
            sets: match &self.index_set_file {
                Some(f) => index_sets::read_sets(f)?,
                None => index_sets::built_in()?,
            },
            names: self.index_sets.clone(),
        }))
    }

    /// The questions: those given, else the kit's, read from the bench's cache directory.
    fn question_set(&self) -> Result<QuestionSet, String> {
        if let (Some(q), Some(b)) = (&self.questions, &self.binds) {
            return QuestionSet::load(q, b);
        }
        let kit = kit::cached(&kit::cache_root()?)?;
        QuestionSet::load(
            self.questions.as_ref().unwrap_or(&kit.join(kit::QUESTIONS)),
            self.binds.as_ref().unwrap_or(&kit.join(kit::BINDS)),
        )
    }

    fn ctx(&self) -> Result<Ctx, String> {
        if let Some(m) = &self.plan_cache_mode {
            if !["auto", "force_custom_plan", "force_generic_plan"].contains(&m.as_str()) {
                return Err(format!(
                    "--plan-cache-mode `{m}`: want auto, force_custom_plan or force_generic_plan"
                ));
            }
        }
        if target::timeout_ms(&self.statement_timeout).is_none()
            || !valid_timeout(&self.statement_timeout)
        {
            return Err(format!(
                "--statement-timeout `{}`: want digits and one of ms, s, min, h, d",
                self.statement_timeout
            ));
        }
        Ok(Ctx {
            guard: Guard {
                cap_kb: self.rss_cap_mb * 1024,
                interval: Duration::from_millis(self.watch_interval_ms.max(10)),
                grace: Duration::from_secs(self.terminate_after_s),
                poll_memory: !self.no_memory_watchdog,
            },
            statement_timeout: self.statement_timeout.clone(),
            work: match &self.work {
                Some(w) => w.clone(),
                None => kit::cache_root()?.join("compose"),
            },
            plan_cache_mode: self.plan_cache_mode.clone(),
        })
    }

    fn banner(&self, set: &QuestionSet, specs: &[Spec], ctx: &Ctx) {
        let targets: Vec<String> = specs
            .iter()
            .map(|s| match s {
                Spec::Explicit { name, table, url } => {
                    format!("{name} = {table} at {}", url.redacted)
                }
            })
            .collect();
        info!(
            questions_dir = %set.dir.display(),
            questions = set.questions.len(),
            question_set = %set.hash,
            binds = %set.binds.display(),
            work = %ctx.work.display(),
            statement_timeout = %self.statement_timeout,
            rss_cap_mb = self.rss_cap_mb,
            watch_interval_ms = self.watch_interval_ms,
            terminate_after_s = self.terminate_after_s,
            memory_watchdog = !self.no_memory_watchdog,
            application_name = target::APPLICATION_NAME,
            targets = %targets.join("; "),
            "configuration"
        );
    }
}

/// Exit status: 0 every answer RIGHT; 1 some answer WRONG; 2 refused or failed to start;
/// 3 no WRONG answer, but some repetition timed out, was cancelled, failed or did not run;
/// 4 every answer RIGHT and complete, but some execution and its EXPLAIN disagree on the rows read
/// or the leaves scanned; 5 no WRONG answer, but a scan used an index its target's index set
/// turned off; 130 interrupted.
const EXIT_WRONG: u8 = 1;
const EXIT_REFUSED: u8 = 2;
const EXIT_INCOMPLETE: u8 = 3;
const EXIT_DISAGREE: u8 = 4;
const EXIT_UNLAWFUL: u8 = 5;
const EXIT_INTERRUPTED: u8 = 130;

/// Every kind of file this program writes, and the older forms of them it still reads.
const KNOWN: [&tables::Declared; 11] = [
    &report::RESULTS,
    &report::RESULTS_V2,
    &report::RESULTS_V1,
    &report::SUMMARY,
    &run::EXPECTED,
    &run::LEAVES,
    &run::LEAVES_V2,
    &run::LEAVES_V1,
    &run::TARGETS,
    &run::CHECK,
    &index_sets::INDEXES,
];

/// Reads the derived families and adds their statements to the question set.
fn families(dirs: &[PathBuf], set: &mut QuestionSet) -> Result<Vec<derived::Family>, String> {
    let mut out = Vec::new();
    for d in dirs {
        let f = derived::load(d, set)?;
        derived::extend(set, &f)?;
        info!(family = %f.name, dir = %d.display(), statements = f.questions.len(), targets = f.sql.len(), "derived statements");
        out.push(f);
    }
    Ok(out)
}

/// Prints the summary of a results file or directory, and with `out_dir` writes summary.parquet,
/// summary.txt and rows.txt there. `heading`, when given, opens the printed and written summary.
fn summarize(
    results: &std::path::Path,
    out_dir: Option<&std::path::Path>,
    heading: Option<&str>,
) -> Result<u8, String> {
    let kind = tables::recognise(
        results,
        &[&report::RESULTS, &report::RESULTS_V2, &report::RESULTS_V1],
    )?;
    let t = tables::read(results, kind)?;
    let cells = report::summarise(&t);
    let mut targets: Vec<String> = Vec::new();
    for r in &t.rows {
        let n = r.get("target").cloned().unwrap_or_default();
        if !targets.contains(&n) {
            targets.push(n);
        }
    }
    let mut text = report::summary_text(&cells, &targets);
    // a run's leaves give, scan by scan, the tables each plan reads and the planner's estimate
    // against the actual rows
    let scans = match out_dir.map(|d| d.join("leaves")).filter(|l| l.exists()) {
        Some(l) => Some(report::scans_text(&tables::read(&l, &run::LEAVES)?)),
        None => None,
    };
    if scans.is_some() {
        text = format!(
            "The tables each plan reads, scan by scan, with the planner's estimate against the actual rows: scans.txt\n{text}"
        );
    }
    if let Some(h) = heading {
        text = format!("{h}\n\n{text}");
    }
    print!("{text}");
    if let Some(d) = out_dir {
        tables::write(
            &d.join("summary.parquet"),
            &report::SUMMARY,
            &report::summary_rows(&cells),
        )?;
        std::fs::write(d.join("summary.txt"), &text).map_err(|e| e.to_string())?;
        std::fs::write(d.join("rows.txt"), report::rows_text(&t)).map_err(|e| e.to_string())?;
        if let Some(s) = &scans {
            std::fs::write(d.join("scans.txt"), s).map_err(|e| e.to_string())?;
        }
    }
    let disagree = report::disagreements(&t);
    for line in disagree.iter().take(20) {
        error!(disagreement = %line, "the execution and its EXPLAIN disagree");
    }
    for (status, n) in report::unwitnessed(&t) {
        warn!(status = %status, repetitions = n, "the rows witness was not taken alone and settled, so the law was not asked of these");
    }
    // an older form's verdicts are read as that program recorded them, never re-judged
    if kind.name == report::RESULTS.name {
        println!(
            "execution against EXPLAIN, rows read and leaves scanned: {} disagreements",
            disagree.len()
        );
    } else if kind.name == report::RESULTS_V2.name {
        println!(
            "execution against EXPLAIN, as recorded under the older leaves rule: {} disagreements",
            disagree.len()
        );
    }
    let wrong = cells.iter().any(|c| c.wrong > 0);
    let incomplete = cells
        .iter()
        .any(|c| c.timeout + c.cancelled + c.error + c.not_run + c.explain_failed > 0);
    for c in cells.iter().filter(|c| !c.all_right()) {
        error!(question = %c.question, target = %c.target, phase = %c.phase, verdict = %c.verdict(), errors = ?c.errors, "not every repetition answered RIGHT");
    }
    Ok(if wrong {
        EXIT_WRONG
    } else if incomplete {
        EXIT_INCOMPLETE
    } else if !disagree.is_empty() {
        EXIT_DISAGREE
    } else {
        0
    })
}

/// The exit status of a run: the summary's, except that an interrupted run says so; a run where a
/// scan used an index its set turned off exits 5 unless some answer was WRONG; and a run a loader,
/// a changed roster or the lock stopped is incomplete even where what it did run was RIGHT (with or
/// without a disagreement).
fn exit_status(f: &Finished, summary: u8) -> u8 {
    match f.ending {
        Ending::Interrupted => EXIT_INTERRUPTED,
        _ if summary == EXIT_WRONG => EXIT_WRONG,
        _ if !f.unlawful.is_empty() => EXIT_UNLAWFUL,
        Ending::StoppedForLoader | Ending::RosterChanged | Ending::StoppedForLock
            if summary == 0 || summary == EXIT_DISAGREE =>
        {
            EXIT_INCOMPLETE
        }
        _ => summary,
    }
}

/// Prints and records every scan that used an index its target's set turned off.
fn report_unlawful(f: &Finished, out: &std::path::Path) -> Result<(), String> {
    if f.unlawful.is_empty() {
        println!("every scan used an index its target's set held present");
        return Ok(());
    }
    for u in f.unlawful.iter().take(20) {
        error!(law = %u, "a scan used an index its set turned off");
    }
    println!(
        "{} scans used an index their target's set turned off; see unlawful.txt",
        f.unlawful.len()
    );
    let p = out.join("unlawful.txt");
    std::fs::write(&p, f.unlawful.join("\n") + "\n").map_err(|e| format!("{}: {e}", p.display()))
}

/// `YYYYMMDDTHHMMSSZ` now.
fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    utc_stamp(secs)
}

/// `YYYYMMDDTHHMMSSZ` at `secs` seconds after 1970-01-01T00:00:00Z.
fn utc_stamp(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The database a reference names: an expected answers file's `reference` starts with its URL.
fn reference_database(reference: &str) -> Option<(String, String)> {
    let url = Url::parse(reference.split_whitespace().next()?).ok()?;
    Some((url.server(), url.database))
}

async fn sample_cmd(a: SampleArgs) -> Result<u8, String> {
    if a.lock.exists() {
        return Err(format!(
            "{} exists: a heavy job holds the machine, and a sample waits for it",
            a.lock.display()
        ));
    }
    let s = samples::read(&a.samples, &a.name)?;
    let mut set = QuestionSet::load(&s.questions, &s.binds)?;
    samples::restrict(&mut set, &s.only)?;
    let common = Common {
        target: a.target,
        questions: Some(s.questions.clone()),
        binds: Some(s.binds.clone()),
        work: a.work,
        statement_timeout: s.statement_timeout.clone(),
        rss_cap_mb: a.rss_cap_mb,
        watch_interval_ms: a.watch_interval_ms,
        terminate_after_s: a.terminate_after_s,
        no_memory_watchdog: a.no_memory_watchdog,
        plan_cache_mode: None,
        index_sets: s.index_sets.clone(),
        index_set_file: a.index_set_file,
    };
    let specs = common.specs()?;
    let dbs: std::collections::BTreeSet<(String, String)> = specs
        .iter()
        .map(|x| match x {
            Spec::Explicit { url, .. } => (url.server(), url.database.clone()),
        })
        .collect();
    if dbs.len() > 1 {
        return Err(format!(
            "the targets of a sample read one database, and these read {dbs:?}"
        ));
    }
    let ctx = common.ctx()?;
    common.banner(&set, &specs, &ctx);
    let sets = common.index_sets()?;
    let mut lives = run::resolve(&specs, &ctx, &set, sets.as_ref()).await?;
    let dir = a.out.join(&s.name);
    let expected = dir.join("expected.parquet");
    // the truth is made on the first target, under its first set
    if !expected.exists() {
        info!(file = %expected.display(), reference = %lives[0].target.name, "the sample's expected answers are computed once, by this program, run twice");
        run::truth(&mut lives[0], &set, &ctx, &expected, false).await?;
    }
    let made_on: std::collections::BTreeSet<Option<(String, String)>> =
        tables::read(&expected, &run::EXPECTED)?
            .rows
            .iter()
            .map(|r| reference_database(r.get("reference").map_or("", String::as_str)))
            .collect();
    let here = dbs.iter().next().cloned();
    if made_on.len() != 1 || made_on.iter().any(|m| *m != here) {
        return Err(format!(
            "{} was made on {made_on:?}, and the targets read {here:?}; move it aside to compute this database's",
            expected.display()
        ));
    }
    let exp = run::read_expected(&expected, &set)?;
    let out = dir.join(utc_now());
    if out.exists() {
        return Err(format!("{} exists", out.display()));
    }
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let (cold, warm) = (a.cold.unwrap_or(s.cold), a.warm.unwrap_or(s.warm));
    let truth_at = here
        .as_ref()
        .map(|(server, db)| format!("{server}/{db}"))
        .unwrap_or_default();
    for l in lives.iter_mut() {
        l.target.config.push(("run.kind".into(), "sample".into()));
        l.target.config.push(("run.sample".into(), s.name.clone()));
        l.target.config.push(("run.truth".into(), truth_at.clone()));
        l.target
            .config
            .push(("run.repetitions".into(), format!("cold {cold} warm {warm}")));
        l.target.config_id = target::config_id(&l.target.config);
    }
    info!(sample = %s.name, cold, warm, index_sets = ?s.index_sets, out = %out.display(), "sample");
    run::write_targets(&lives, &out.join("targets.parquet"))?;
    run::write_indexes(&lives, &out.join("indexes.parquet"))?;
    let finished = run::bench(
        &mut lives,
        &set,
        &exp,
        &ctx,
        &Plan {
            cold,
            warm,
            lock: Some(a.lock.clone()),
        },
        &out,
    )
    .await?;
    let heading = format!(
        "SAMPLE {}: a quick check, not a measurement; expected answers by one route",
        s.name
    );
    let code = summarize(&out.join("results"), Some(&out), Some(&heading))?;
    report_unlawful(&finished, &out)?;
    Ok(exit_status(&finished, code))
}

/// Prints surveyor.sql, or writes the kit into a directory.
fn kit_cmd(cmd: KitCmd) -> Result<u8, String> {
    match cmd {
        KitCmd::SurveyorSql => {
            let text = kit::file(kit::SURVEYOR_SQL)
                .ok_or_else(|| format!("the bench holds no {}", kit::SURVEYOR_SQL))?;
            let mut out = std::io::stdout().lock();
            match out.write_all(text).and_then(|()| out.flush()) {
                // a reader that stops early, as `head` does, closes the pipe: no error of ours
                Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
                    Err(format!("standard output: {e}"))
                }
                _ => Ok(0),
            }
        }
        KitCmd::Write { dir } => {
            let written = kit::write(&dir)?;
            info!(dir = %dir.display(), files = written.len(), "wrote the kit");
            Ok(0)
        }
    }
}

async fn main_inner(cli: Cli) -> Result<u8, String> {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            INTERRUPTED.store(true, Ordering::SeqCst);
        }
    });
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match cli.cmd {
        Cmd::Summarize { results } => summarize(&results, None, None),
        Cmd::Kit { cmd } => kit_cmd(cmd),
        Cmd::Sample(a) => sample_cmd(a).await,
        Cmd::Check {
            common,
            expected,
            derived,
            only,
            out,
            force,
        } => check_cmd(common, expected, derived, only, out, force).await,
        Cmd::Export { from, to } => {
            let d = tables::recognise(&from, &KNOWN)?;
            let text = tables::to_tsv(d, tables::read_values(&from, d)?)
                .map_err(|e| format!("{}: {e}", from.display()))?;
            std::fs::write(&to, text).map_err(|e| format!("{}: {e}", to.display()))?;
            info!(from = %from.display(), to = %to.display(), kind = d.name, "exported");
            Ok(0)
        }
        Cmd::Import { from, to } => {
            let text =
                std::fs::read_to_string(&from).map_err(|e| format!("{}: {e}", from.display()))?;
            let head = text.lines().next().unwrap_or_default();
            let d = KNOWN
                .iter()
                .find(|d| d.names().join("\t") == head)
                .ok_or_else(|| {
                    format!(
                        "{}: its header is not that of any file this program writes",
                        from.display()
                    )
                })?;
            let rows =
                tables::from_tsv(d, &text).map_err(|e| format!("{}: {e}", from.display()))?;
            tables::write_values(&to, d, &rows)?;
            info!(from = %from.display(), to = %to.display(), kind = d.name, rows = rows.len(), "imported");
            Ok(0)
        }
        Cmd::Compose(common) => {
            let set = common.question_set()?;
            let specs = common.specs()?;
            let ctx = common.ctx()?;
            common.banner(&set, &specs, &ctx);
            let sets = common.index_sets()?;
            let lives = run::resolve(&specs, &ctx, &set, sets.as_ref()).await?;
            for l in &lives {
                println!("{}\t{}", l.target.name, l.composed.dir.display());
            }
            Ok(0)
        }
        Cmd::Truth { common, out, force } => {
            let set = common.question_set()?;
            let specs = common.specs()?;
            let ctx = common.ctx()?;
            common.banner(&set, &specs, &ctx);
            let sets = common.index_sets()?;
            if sets.as_ref().is_some_and(|s| s.names.len() != 1) {
                return Err("truth takes exactly one index set, or none".into());
            }
            let mut lives = run::resolve(&specs, &ctx, &set, sets.as_ref()).await?;
            if lives.len() != 1 {
                return Err(format!(
                    "truth takes exactly one reference target, not {}",
                    lives.len()
                ));
            }
            run::truth(&mut lives[0], &set, &ctx, &out, force).await?;
            Ok(0)
        }
        Cmd::Run {
            common,
            expected,
            out,
            cold,
            warm,
            derived,
        } => {
            let mut set = common.question_set()?;
            let exp = run::read_expected(&expected, &set)?;
            let specs = common.specs()?;
            let fams = families(&derived, &mut set)?;
            let ctx = common.ctx()?;
            common.banner(&set, &specs, &ctx);
            info!(cold, warm, expected = %expected.display(), out = %out.display(), started_unix = started, "plan");
            let sets = common.index_sets()?;
            let mut lives = run::resolve(&specs, &ctx, &set, sets.as_ref()).await?;
            for f in &fams {
                run::attach(&mut lives, f).await?;
            }
            run::refuse_used(&out)?;
            std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            run::write_targets(&lives, &out.join("targets.parquet"))?;
            run::write_indexes(&lives, &out.join("indexes.parquet"))?;
            let finished = run::bench(
                &mut lives,
                &set,
                &exp,
                &ctx,
                &Plan {
                    cold,
                    warm,
                    lock: None,
                },
                &out,
            )
            .await?;
            let code = summarize(&out.join("results"), Some(&out), None)?;
            report_unlawful(&finished, &out)?;
            Ok(exit_status(&finished, code))
        }
    }
}

async fn check_cmd(
    common: Common,
    expected: PathBuf,
    derived: Vec<PathBuf>,
    only: Vec<String>,
    out: PathBuf,
    force: bool,
) -> Result<u8, String> {
    let mut set = common.question_set()?;
    let exp = run::read_expected(&expected, &set)?;
    let specs = common.specs()?;
    let fams = families(&derived, &mut set)?;
    let chosen: Vec<&questions::Question> = set
        .questions
        .iter()
        .filter(|q| only.is_empty() || only.iter().any(|p| q.name.starts_with(p.as_str())))
        .collect();
    if chosen.is_empty() {
        return Err(format!("no question's name starts with any of {only:?}"));
    }
    let ctx = common.ctx()?;
    common.banner(&set, &specs, &ctx);
    info!(questions = chosen.len(), expected = %expected.display(), out = %out.display(), "check");
    let sets = common.index_sets()?;
    let mut lives = run::resolve(&specs, &ctx, &set, sets.as_ref()).await?;
    for f in &fams {
        run::attach(&mut lives, f).await?;
    }
    let ending = run::check(&mut lives, &chosen, &exp, &ctx, &out, force).await?;
    let t = tables::read(&out, &run::CHECK)?;
    let count = |v: &str| t.rows.iter().filter(|r| r["verdict"] == v).count();
    let (right, wrong) = (count("RIGHT"), count("WRONG"));
    let unanswered = t.rows.len() - right - wrong;
    println!(
        "checked {} answers: {right} RIGHT, {wrong} WRONG, {unanswered} not answered",
        t.rows.len()
    );
    Ok(if ending == Ending::Interrupted {
        EXIT_INTERRUPTED
    } else if wrong > 0 {
        EXIT_WRONG
    } else if unanswered > 0 || ending != Ending::Complete {
        EXIT_INCOMPLETE
    } else {
        0
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log_format);
    match main_inner(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            error!(error = %e, "stopped");
            eprintln!("error: {e}");
            ExitCode::from(EXIT_REFUSED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_dates_and_times() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_790_507_627), "20260927T111347Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
    }

    #[test]
    fn timeouts_are_digits_and_a_unit() {
        assert!(valid_timeout("120s"));
        assert!(valid_timeout("500ms"));
        assert!(valid_timeout("5min"));
        assert!(valid_timeout("1000"));
        assert!(!valid_timeout("s"));
        assert!(!valid_timeout("10s; DROP"));
        assert!(!valid_timeout(""));
    }

    #[test]
    fn a_results_file_with_a_wrong_answer_fails_the_exit_status() {
        let dir = std::env::temp_dir().join(format!("warren-bench-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("results.parquet");
        let line = |verdict: &str| {
            report::RESULTS
                .names()
                .iter()
                .map(|c| match *c {
                    "target" => "t".to_string(),
                    "question" => "q".to_string(),
                    "phase" => "warm".to_string(),
                    "status" => "OK".to_string(),
                    "explain_status" => "OK".to_string(),
                    "verdict" => verdict.to_string(),
                    "wall_ms" => "1.0".to_string(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        };
        tables::write(&path, &report::RESULTS, &[line("RIGHT"), line("RIGHT")]).unwrap();
        assert_eq!(summarize(&path, None, None).unwrap(), 0);
        tables::write(&path, &report::RESULTS, &[line("RIGHT"), line("WRONG")]).unwrap();
        assert_eq!(summarize(&path, None, None).unwrap(), EXIT_WRONG);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn line_of(d: &tables::Declared, fields: &[(&str, &str)]) -> Vec<String> {
        d.names()
            .iter()
            .map(|c| match fields.iter().find(|(k, _)| k == c) {
                Some((_, v)) => v.to_string(),
                None => match *c {
                    "target" => "t".to_string(),
                    "question" => "q".to_string(),
                    "phase" => "warm".to_string(),
                    "status" | "explain_status" | "witness_status" => "OK".to_string(),
                    "verdict" => "RIGHT".to_string(),
                    "wall_ms" => "1.0".to_string(),
                    _ => String::new(),
                },
            })
            .collect()
    }

    #[test]
    fn a_run_a_loader_stopped_is_incomplete_whatever_its_summary_said_short_of_wrong() {
        let f = |ending| Finished {
            ending,
            unlawful: vec![],
        };
        for stopped in [
            Ending::StoppedForLoader,
            Ending::RosterChanged,
            Ending::StoppedForLock,
        ] {
            let stopped = f(stopped);
            assert_eq!(exit_status(&stopped, 0), EXIT_INCOMPLETE);
            assert_eq!(exit_status(&stopped, EXIT_DISAGREE), EXIT_INCOMPLETE);
            assert_eq!(exit_status(&stopped, EXIT_WRONG), EXIT_WRONG);
        }
        assert_eq!(
            exit_status(&f(Ending::Complete), EXIT_DISAGREE),
            EXIT_DISAGREE
        );
        assert_eq!(exit_status(&f(Ending::Interrupted), 0), EXIT_INTERRUPTED);
    }

    #[test]
    fn a_scan_of_an_index_its_set_turned_off_fails_the_exit_status_short_of_wrong() {
        let broken = |ending| Finished {
            ending,
            unlawful: vec!["t@btree q warm 0: a scan used lego.x_idx".into()],
        };
        assert_eq!(exit_status(&broken(Ending::Complete), 0), EXIT_UNLAWFUL);
        assert_eq!(
            exit_status(&broken(Ending::Complete), EXIT_INCOMPLETE),
            EXIT_UNLAWFUL
        );
        assert_eq!(
            exit_status(&broken(Ending::StoppedForLoader), 0),
            EXIT_UNLAWFUL
        );
        assert_eq!(
            exit_status(&broken(Ending::Complete), EXIT_WRONG),
            EXIT_WRONG
        );
        assert_eq!(
            exit_status(&broken(Ending::Interrupted), 0),
            EXIT_INTERRUPTED
        );
    }

    #[test]
    fn an_execution_its_explain_contradicts_fails_the_exit_status_and_old_results_still_read() {
        let dir = std::env::temp_dir().join(format!("warren-bench-law-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("results.parquet");
        let agree = [("rows_read_agree", "true"), ("leaves_agree", "true")];
        let rows_differ = [("rows_read_agree", "false"), ("leaves_agree", "true")];
        let leaves_differ = [("rows_read_agree", "true"), ("leaves_agree", "false")];
        for (lines, want) in [
            (vec![line_of(&report::RESULTS, &agree)], 0),
            (
                vec![
                    line_of(&report::RESULTS, &agree),
                    line_of(&report::RESULTS, &rows_differ),
                ],
                EXIT_DISAGREE,
            ),
            (
                vec![line_of(&report::RESULTS, &leaves_differ)],
                EXIT_DISAGREE,
            ),
        ] {
            tables::write(&path, &report::RESULTS, &lines).unwrap();
            assert_eq!(summarize(&path, None, None).unwrap(), want);
        }
        let not_alone = [
            ("witness_status", "NOT_ALONE"),
            ("rows_read_agree", ""),
            ("leaves_agree", ""),
        ];
        tables::write(
            &path,
            &report::RESULTS,
            &[line_of(&report::RESULTS, &not_alone)],
        )
        .unwrap();
        assert_eq!(summarize(&path, None, None).unwrap(), 0);
        tables::write(
            &path,
            &report::RESULTS_V2,
            &[line_of(&report::RESULTS_V2, &agree)],
        )
        .unwrap();
        assert_eq!(summarize(&path, None, None).unwrap(), 0);
        tables::write(
            &path,
            &report::RESULTS_V2,
            &[line_of(&report::RESULTS_V2, &leaves_differ)],
        )
        .unwrap();
        assert_eq!(summarize(&path, None, None).unwrap(), EXIT_DISAGREE);
        tables::write(
            &path,
            &report::RESULTS_V1,
            &[line_of(&report::RESULTS_V1, &[])],
        )
        .unwrap();
        assert_eq!(summarize(&path, None, None).unwrap(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
