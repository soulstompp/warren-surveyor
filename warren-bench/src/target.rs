// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Targets: a connection URL and the table the questions are read from. Sessions, and what each
//! target is recorded as: its server, its settings, its partitioning and its size.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection};

use crate::digest::fingerprint;

pub const APPLICATION_NAME: &str = "warren-bench";

/// The oldest server version (`server_version_num`) this program runs against; an older server
/// is refused.
pub const OLDEST_SERVER: u32 = 130_000;

/// The columns of the target database's `pg_database` row recorded for every target, each under
/// the key `database.<column>`. Which of them exist depends on the server's version; a column the
/// server does not have is recorded as absent (null), as is a column whose value is null.
pub const DATABASE_COLUMNS: &[&str] = &[
    "datlocprovider",
    "datcollate",
    "datctype",
    "daticulocale",
    "datlocale",
];

/// The settings recorded for every target, each under the key `setting.<name>`, read with
/// `current_setting(name, true)`. A setting the server does not have is recorded as absent (null).
pub const SETTINGS: &[&str] = &[
    "server_version",
    "server_version_num",
    "shared_buffers",
    "work_mem",
    "effective_cache_size",
    "maintenance_work_mem",
    "max_parallel_workers_per_gather",
    "max_parallel_workers",
    "max_worker_processes",
    "parallel_setup_cost",
    "parallel_tuple_cost",
    "jit",
    "jit_above_cost",
    "enable_partition_pruning",
    "enable_partitionwise_join",
    "enable_partitionwise_aggregate",
    "constraint_exclusion",
    "plan_cache_mode",
    "random_page_cost",
    "seq_page_cost",
    "cpu_tuple_cost",
    "effective_io_concurrency",
    "io_method",
    "io_combine_limit",
    "default_statistics_target",
    "from_collapse_limit",
    "join_collapse_limit",
    "geqo_threshold",
    "track_counts",
    "track_io_timing",
    "huge_pages",
    "statement_timeout",
    "application_name",
    "TimeZone",
    "DateStyle",
    "IntervalStyle",
    "extra_float_digits",
    "server_encoding",
    "search_path",
    "shared_preload_libraries",
    "session_preload_libraries",
];

/// A connection URL, checked to name its host, port and database explicitly, so that nothing is
/// filled in from the environment.
#[derive(Clone, Debug)]
pub struct Url {
    pub text: String,
    pub redacted: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

impl Url {
    pub fn parse(text: &str) -> Result<Url, String> {
        let rest = text
            .strip_prefix("postgres://")
            .or_else(|| text.strip_prefix("postgresql://"))
            .ok_or("a target URL starts with postgres:// or postgresql://")?;
        let (authority, path) = rest
            .split_once('/')
            .ok_or("a target URL names its database: postgres://host:port/database")?;
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (h, p) = h.split_once(']').ok_or("unclosed [ in the host")?;
            (h.to_string(), p.strip_prefix(':').unwrap_or(""))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p),
                None => (hostport.to_string(), ""),
            }
        };
        if host.is_empty() {
            return Err("a target URL names its host".into());
        }
        let port: u16 = port
            .parse()
            .map_err(|_| "a target URL names its port: postgres://host:port/database")?;
        let database = path.split('?').next().unwrap_or("").to_string();
        if database.is_empty() {
            return Err("a target URL names its database: postgres://host:port/database".into());
        }
        let q = path
            .split_once('?')
            .map(|(_, q)| redact_query(q))
            .unwrap_or_default();
        let redacted = match userinfo {
            Some(u) => {
                let user = u.split(':').next().unwrap_or("");
                let pw = if u.contains(':') { ":***" } else { "" };
                format!("postgres://{user}{pw}@{hostport}/{database}{q}")
            }
            None => format!("postgres://{hostport}/{database}{q}"),
        };
        Ok(Url {
            text: text.to_string(),
            redacted,
            host,
            port,
            database,
        })
    }

    /// The options every session on this URL is opened with.
    pub fn session_options(&self) -> Result<PgConnectOptions, String> {
        let o = PgConnectOptions::from_str(&self.text)
            .map_err(|e| format!("{}: {e}", self.redacted))?;
        Ok(o.application_name(APPLICATION_NAME)
            .log_statements(log::LevelFilter::Trace)
            .log_slow_statements(log::LevelFilter::Off, Duration::from_secs(3600)))
    }

    /// The server this URL reaches, for grouping targets by server.
    pub fn server(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn redact_query(q: &str) -> String {
    let parts: Vec<String> = q
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if k.eq_ignore_ascii_case("password") => format!("{k}=***"),
            _ => kv.to_string(),
        })
        .collect();
    format!("?{}", parts.join("&"))
}

#[derive(Clone)]
pub struct Target {
    pub name: String,
    /// The table as the target's catalog spells it.
    pub table: String,
    pub url: Url,
    pub opts: PgConnectOptions,
    /// The relations a plan may scan for this table: its leaves, or the table itself.
    pub leaves: BTreeSet<String>,
    /// The server's `server_version_num`.
    pub server_version_num: u32,
    /// The EXPLAIN command this server is sent (see `explain::command`).
    pub explain: &'static str,
    /// Each (key, value); an empty value is recorded as null.
    pub config: Vec<(String, String)>,
    pub config_id: String,
}

impl Target {
    /// The value recorded under `key`; empty when it is absent.
    pub fn get(&self, key: &str) -> &str {
        self.config
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str())
    }
}

pub struct Session {
    pub conn: PgConnection,
    /// The backend's pid.
    pub pid: i32,
    /// Set when the transaction a repetition opened could not be rolled back: the session is then
    /// closed, never used again.
    pub broken: bool,
}

impl Session {
    pub async fn close(self) {
        let _ = self.conn.close().await;
    }
}

pub async fn simple(conn: &mut PgConnection, sql: &str) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(sql).execute(conn).await.map(|_| ())
}

/// The first column of the first row of a statement, as text.
pub async fn scalar(conn: &mut PgConnection, sql: &str) -> Result<Option<String>, String> {
    let rows = sqlx::raw_sql(sql)
        .fetch_all(conn)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match rows.first() {
        None => Ok(None),
        Some(r) => {
            let v = sqlx::Row::try_get_raw(r, 0).map_err(|e| e.to_string())?;
            if sqlx::ValueRef::is_null(&v) {
                Ok(None)
            } else {
                Ok(Some(v.as_str().map_err(|e| e.to_string())?.to_string()))
            }
        }
    }
}

/// A statement timeout of digits and a unit (ms, s, min, h, d; none is ms, as for Postgres's
/// `statement_timeout`), in milliseconds.
pub fn timeout_ms(s: &str) -> Option<u64> {
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    let n: u64 = s[..digits].parse().ok()?;
    let unit = match &s[digits..] {
        "" | "ms" => 1,
        "s" => 1_000,
        "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(unit)
}

/// Opens a session named `warren-bench`, with the statement timeout set. An index set's DROP INDEX
/// waits at most 10 s for its table's lock, so it never holds the table's other queries back for
/// long, and a session left inside a transaction by a client that went away is ended by the server
/// after the statement timeout, its locks with it.
pub async fn open(opts: &PgConnectOptions, statement_timeout: &str) -> Result<Session, String> {
    let mut conn = PgConnection::connect_with(opts)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let name = format!("SET application_name = '{APPLICATION_NAME}'");
    let pid_sql = "SELECT pg_backend_pid()";
    let timeouts = [
        format!("SET statement_timeout = '{statement_timeout}'"),
        "SET lock_timeout = '10s'".to_string(),
        format!("SET idle_in_transaction_session_timeout = '{statement_timeout}'"),
    ];
    simple(&mut conn, &name)
        .await
        .map_err(|e| format!("{name}: {e}"))?;
    let pid = scalar(&mut conn, pid_sql)
        .await?
        .and_then(|p| p.parse().ok())
        .ok_or(format!("{pid_sql} returned nothing"))?;
    for timeout in &timeouts {
        simple(&mut conn, timeout)
            .await
            .map_err(|e| format!("{timeout}: {e}"))?;
    }
    Ok(Session {
        conn,
        pid,
        broken: false,
    })
}

/// Checks that the server lists `pid` as a session of this program, and returns it.
pub async fn confirm_pid(control: &mut PgConnection, pid: i32) -> Result<i32, String> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT pid FROM pg_stat_activity WHERE pid = $1 AND application_name = $2",
    )
    .bind(pid)
    .bind(APPLICATION_NAME)
    .fetch_optional(control)
    .await
    .map_err(|e| format!("reading pg_stat_activity: {e}"))?;
    found
        .ok_or_else(|| format!("pid {pid} is not listed in pg_stat_activity as {APPLICATION_NAME}"))
}

/// Resolves a table name to the catalog's own spelling of it.
pub async fn canonical_table(control: &mut PgConnection, table: &str) -> Result<String, String> {
    // qualified whatever the session's search_path, so a table is named alike on every target
    sqlx::query_scalar::<_, String>(
        "SELECT format('%I.%I', n.nspname, c.relname) FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1::regclass",
    )
    .bind(table)
    .fetch_one(control)
    .await
    .map_err(|e| format!("table {table}: {e}"))
}

/// Reads one value per name with `sql`, which takes the names as `$1` and returns (name, value)
/// in the order given. Refuses an answer that is not exactly one line per name.
async fn by_name(
    control: &mut PgConnection,
    sql: &str,
    names: &[&str],
) -> Result<Vec<(String, Option<String>)>, String> {
    let asked: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let got: Vec<(String, Option<String>)> = sqlx::query_as(sql)
        .bind(&asked)
        .fetch_all(control)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    let answered: Vec<&String> = got.iter().map(|(n, _)| n).collect();
    if answered != asked.iter().collect::<Vec<_>>() {
        return Err(format!("{sql}: asked for {asked:?}, answered {answered:?}"));
    }
    Ok(got)
}

/// Configuration lines `<prefix>.<name>` for values read by name. An absent value is recorded as
/// null (the empty text, which a file records as null).
pub fn named(prefix: &str, values: Vec<(String, Option<String>)>) -> Vec<(String, String)> {
    values
        .into_iter()
        .map(|(n, v)| (format!("{prefix}.{n}"), v.unwrap_or_default()))
        .collect()
}

/// The server's `server_version_num`, from the setting a configuration recorded. Refuses a
/// server older than `OLDEST_SERVER`.
pub fn server_version(config: &[(String, String)]) -> Result<u32, String> {
    let text = config
        .iter()
        .find(|(k, _)| k == "setting.server_version_num")
        .map_or("", |(_, v)| v.as_str());
    let v: u32 = text
        .parse()
        .map_err(|_| format!("server_version_num `{text}` is not a version number"))?;
    if v < OLDEST_SERVER {
        return Err(format!(
            "the server's server_version_num is {v}; servers older than {OLDEST_SERVER} are refused"
        ));
    }
    Ok(v)
}

/// The surveyor's library: the one whose access method is `surveyor`.
pub const SURVEYOR_LIBRARY: &str = "warren_surveyor_pg";

/// The libraries a Postgres session loads as it starts, as recorded: those
/// `shared_preload_libraries` and `session_preload_libraries` name, each once, in order.
pub fn preloaded(config: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in [
        "setting.shared_preload_libraries",
        "setting.session_preload_libraries",
    ] {
        let v = config
            .iter()
            .find(|(k, _)| k == key)
            .map_or("", |(_, v)| v.as_str());
        for e in v.split(',') {
            let e = e.trim();
            let e = e
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .unwrap_or(e);
            if !e.is_empty() && !out.iter().any(|x| x == e) {
                out.push(e.to_string());
            }
        }
    }
    out
}

/// A library's name as a session loads it, without a directory or `.so`.
fn library_name(entry: &str) -> &str {
    let base = entry.rsplit('/').next().unwrap_or(entry);
    base.strip_suffix(".so").unwrap_or(base)
}

/// Refuses a Postgres database whose sessions would not load the surveyor alike: a surveyor
/// standing where its library is not preloaded, so a session loads it partway through planning
/// its first statement on a table with one, and plans that statement unlike the rest.
pub fn refuse_unlike_sessions(
    config: &[(String, String)],
    roster: &[crate::index_sets::Index],
) -> Result<(), String> {
    let loaded = preloaded(config);
    let has = |lib: &str| loaded.iter().any(|e| library_name(e) == lib);
    let new = has(SURVEYOR_LIBRARY);
    let surveyors: Vec<&str> = roster
        .iter()
        .filter(|i| i.method == crate::index_sets::SURVEYOR)
        .map(|i| i.qualified.as_str())
        .collect();
    let some = || {
        let mut s = surveyors
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if surveyors.len() > 3 {
            s.push_str(&format!(" and {} more", surveyors.len() - 3));
        }
        s
    };
    if !new && !surveyors.is_empty() {
        return Err(format!(
            "the database holds surveyors ({}) and its sessions do not preload {SURVEYOR_LIBRARY}, so a session loads it partway through planning its first statement on a table with one; ALTER DATABASE … SET session_preload_libraries = '{SURVEYOR_LIBRARY}'",
            some()
        ));
    }
    Ok(())
}

/// The file a preloaded library entry loads: `$libdir` is the server's package library directory,
/// a bare name is looked for there, and `.so` is added to a name without it.
fn library_file(entry: &str, pkglibdir: &str) -> std::path::PathBuf {
    let n = entry.replace("$libdir", pkglibdir);
    let p = if n.contains('/') {
        std::path::PathBuf::from(n)
    } else {
        std::path::Path::new(pkglibdir).join(n)
    };
    if p.extension().is_some_and(|x| x == "so") {
        p
    } else {
        let mut s = p.into_os_string();
        s.push(".so");
        s.into()
    }
}

/// The sha256 of each library a Postgres session preloads, as `library.<name>.sha256`, read on
/// this machine where the server runs on it; otherwise, and where it cannot be read, why not.
pub async fn library_hashes(
    c: &mut PgConnection,
    config: &[(String, String)],
    local: bool,
) -> Vec<(String, String)> {
    let loaded = preloaded(config);
    if loaded.is_empty() {
        return vec![];
    }
    let dir: Result<String, String> = if local {
        sqlx::query_scalar("SELECT setting FROM pg_config WHERE name = 'PKGLIBDIR'")
            .fetch_one(&mut *c)
            .await
            .map_err(|e| format!("not read: the server's library directory: {e}"))
    } else {
        Err("not read: the server is not on this machine".into())
    };
    loaded
        .iter()
        .map(|e| {
            let key = format!("library.{}.sha256", library_name(e));
            let value = match &dir {
                Err(why) => why.clone(),
                Ok(d) => {
                    let f = library_file(e, d);
                    match std::process::Command::new("sha256sum").arg(&f).output() {
                        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_string(),
                        Ok(o) => format!(
                            "not read: {}: {}",
                            f.display(),
                            String::from_utf8_lossy(&o.stderr).trim()
                        ),
                        Err(err) => format!("not read: sha256sum: {err}"),
                    }
                }
            };
            (key, value)
        })
        .collect()
}

/// Whether a host names this machine: a loopback address, `localhost`, or a socket directory.
pub fn local_host(host: &str) -> bool {
    host == "localhost"
        || host.starts_with('/')
        || host.starts_with("127.")
        || host == "::1"
        || host == "[::1]"
}

/// What a target is recorded as.
pub struct Described {
    pub config: Vec<(String, String)>,
    /// The names an EXPLAIN gives the relations of the target's own table.
    pub leaves: BTreeSet<String>,
    pub version: u32,
    /// Every index of the database's own schemas.
    pub roster: Option<Vec<crate::index_sets::Index>>,
}

/// Reads the target's server, settings, partitioning, sizes and statistics, and the server's version.
/// A server older than the oldest this program takes is refused before anything else is read.
pub async fn describe(control: &mut PgConnection, table: &str) -> Result<Described, String> {
    let (mut config, leaves, version) = describe_pg(control, table).await?;
    let roster = crate::index_sets::read_roster(control).await?;
    config.push((
        "roster".into(),
        crate::index_sets::roster_fingerprint(&roster),
    ));
    config.push(("roster_indexes".into(), roster.len().to_string()));
    for (name, version) in crate::index_sets::extensions(control).await? {
        config.push((format!("extension.{name}"), version));
    }
    Ok(Described {
        config,
        leaves,
        version,
        roster: Some(roster),
    })
}

async fn describe_pg(
    control: &mut PgConnection,
    table: &str,
) -> Result<(Vec<(String, String)>, BTreeSet<String>, u32), String> {
    let settings = named(
        "setting",
        by_name(
            control,
            "SELECT s.name, current_setting(s.name, true)
             FROM unnest($1::text[]) WITH ORDINALITY AS s(name, n)
             ORDER BY s.n",
            SETTINGS,
        )
        .await?,
    );
    let version = server_version(&settings)?;
    let mut c: Vec<(String, String)> = Vec::new();
    for (k, q) in [
        ("version", "SELECT version()"),
        ("database", "SELECT current_database()"),
        ("server_address", "SELECT coalesce(inet_server_addr()::text, 'local socket') || ':' || coalesce(inet_server_port()::text, '')"),
        ("user", "SELECT current_user"),
    ] {
        c.push((k.into(), scalar(control, q).await?.unwrap_or_default()));
    }
    // Read by name from the row as JSON, so a column the server does not have reads as null.
    c.extend(named(
        "database",
        by_name(
            control,
            "SELECT k.name, to_jsonb(d) ->> k.name
             FROM pg_database d, unnest($1::text[]) WITH ORDINALITY AS k(name, n)
             WHERE d.datname = current_database()
             ORDER BY k.n",
            DATABASE_COLUMNS,
        )
        .await?,
    ));
    c.extend(settings);

    let relkind: String =
        sqlx::query_scalar("SELECT relkind::text FROM pg_class WHERE oid = $1::regclass")
            .bind(table)
            .fetch_one(&mut *control)
            .await
            .map_err(|e| format!("{table}: {e}"))?;
    // (relname, level, is leaf, relkind, partition key, partition bound)
    type Node = (String, i32, bool, String, Option<String>, Option<String>);
    let tree: Vec<Node> = if relkind == "p" {
        sqlx::query_as(
            "SELECT c.relname::text, t.level, t.isleaf, c.relkind::text,
                    CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END,
                    pg_get_expr(c.relpartbound, c.oid)
             FROM pg_partition_tree($1::regclass) t JOIN pg_class c ON c.oid = t.relid
             ORDER BY t.level, c.relname",
        )
        .bind(table)
        .fetch_all(&mut *control)
        .await
        .map_err(|e| format!("{table}: {e}"))?
    } else {
        vec![]
    };
    let leaves: BTreeSet<String> = if relkind == "p" {
        tree.iter().filter(|r| r.2).map(|r| r.0.clone()).collect()
    } else {
        let name: String =
            sqlx::query_scalar("SELECT relname::text FROM pg_class WHERE oid = $1::regclass")
                .bind(table)
                .fetch_one(&mut *control)
                .await
                .map_err(|e| e.to_string())?;
        [name].into_iter().collect()
    };

    let partitioning = match relkind.as_str() {
        "p" => {
            let mut levels: Vec<String> = Vec::new();
            let max_level = tree.iter().map(|r| r.1).max().unwrap_or(0);
            for lv in 0..=max_level {
                let parts: Vec<&str> = tree
                    .iter()
                    .filter(|r| r.1 == lv && r.3 == "p")
                    .filter_map(|r| r.4.as_deref())
                    .collect();
                let mut kinds: Vec<&str> = parts.clone();
                kinds.sort();
                kinds.dedup();
                for k in kinds {
                    let n = parts.iter().filter(|p| **p == k).count();
                    levels.push(format!("level {lv}: {k} x{n}"));
                }
            }
            let mut depths: Vec<String> = Vec::new();
            for lv in 0..=max_level {
                let n = tree.iter().filter(|r| r.1 == lv && r.2).count();
                if n > 0 {
                    depths.push(format!("{n} at level {lv}"));
                }
            }
            let defaults = tree
                .iter()
                .filter(|r| r.5.as_deref() == Some("DEFAULT"))
                .count();
            format!(
                "partitioned; {}; leaves {}; default partitions {defaults}",
                levels.join("; "),
                depths.join(", ")
            )
        }
        "r" => "table, not partitioned".into(),
        "v" => "view".into(),
        "m" => "materialized view".into(),
        other => format!("relkind {other}"),
    };
    c.push(("table".into(), table.into()));
    c.push(("partitioning".into(), partitioning));
    c.push(("leaves".into(), leaves.len().to_string()));

    type Sizes = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    let stats: Sizes =
        sqlx::query_as(
            "WITH rel AS (
                 SELECT t.relid FROM pg_partition_tree($1::regclass) t WHERE t.isleaf
                 UNION
                 SELECT $1::regclass WHERE NOT EXISTS (SELECT 1 FROM pg_partition_tree($1::regclass))
             )
             SELECT sum(pg_total_relation_size(r.relid))::bigint,
                    sum(pg_relation_size(r.relid))::bigint,
                    sum(pg_indexes_size(r.relid))::bigint,
                    sum(s.n_live_tup)::bigint,
                    count(*) FILTER (WHERE greatest(s.last_analyze, s.last_autoanalyze) IS NULL)::bigint,
                    min(greatest(s.last_analyze, s.last_autoanalyze))::text,
                    max(greatest(s.last_analyze, s.last_autoanalyze))::text,
                    (SELECT count(*) FROM pg_index i JOIN rel r2 ON r2.relid = i.indrelid)::bigint
             FROM rel r LEFT JOIN pg_stat_all_tables s ON s.relid = r.relid",
        )
        .bind(table)
        .fetch_one(&mut *control)
        .await
        .map_err(|e| format!("{table} sizes: {e}"))?;
    let show = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_default();
    c.push(("total_bytes".into(), show(stats.0)));
    c.push(("heap_bytes".into(), show(stats.1)));
    c.push(("index_bytes".into(), show(stats.2)));
    c.push(("indexes".into(), show(stats.7)));
    c.push(("live_rows_estimate".into(), show(stats.3)));
    c.push(("leaves_never_analyzed".into(), show(stats.4)));
    c.push(("analyzed_earliest".into(), stats.5.unwrap_or_default()));
    c.push(("analyzed_latest".into(), stats.6.unwrap_or_default()));

    Ok((c, leaves, version))
}

/// A fingerprint of a configuration, leaving out what describes this program's own session.
pub fn config_id(config: &[(String, String)]) -> String {
    let own = ["setting.application_name", "setting.statement_timeout"];
    let parts: Vec<Vec<u8>> = config
        .iter()
        .filter(|(k, _)| !own.contains(&k.as_str()))
        .map(|(k, v)| format!("{k}={v}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    fingerprint(&refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_must_name_host_port_and_database() {
        assert!(Url::parse("postgres://localhost:5432/blocker").is_ok());
        assert!(Url::parse("postgres://localhost/blocker").is_err());
        assert!(Url::parse("postgres://localhost:5432").is_err());
        assert!(Url::parse("postgres://localhost:5432/").is_err());
        assert!(Url::parse("other://localhost:3306/x").is_err());
        assert!(Url::parse("localhost:3306/x").is_err());
    }

    #[test]
    fn a_statement_timeout_reads_in_milliseconds() {
        assert_eq!(timeout_ms("120s"), Some(120_000));
        assert_eq!(timeout_ms("500ms"), Some(500));
        assert_eq!(timeout_ms("1000"), Some(1000));
        assert_eq!(timeout_ms("5min"), Some(300_000));
        assert_eq!(timeout_ms("2h"), Some(7_200_000));
        assert_eq!(timeout_ms("1d"), Some(86_400_000));
        assert_eq!(timeout_ms("s"), None);
        assert_eq!(timeout_ms("10x"), None);
    }

    fn preloading(session: &str, shared: &str) -> Vec<(String, String)> {
        vec![
            ("setting.shared_preload_libraries".into(), shared.into()),
            ("setting.session_preload_libraries".into(), session.into()),
        ]
    }

    #[test]
    fn sessions_that_would_not_load_the_surveyor_alike_are_refused() {
        let plain = crate::index_sets::tests::roster();
        let mut surveyed = plain.clone();
        surveyed.push(crate::index_sets::tests::index(
            "lego",
            "lego_sets",
            "lego_sets",
            "lego_sets_surveyor",
            crate::index_sets::SURVEYOR,
            "set_num",
        ));
        let ok = |s: &str, sh: &str, r: &[crate::index_sets::Index]| {
            refuse_unlike_sessions(&preloading(s, sh), r)
        };
        assert!(ok("warren_surveyor_pg", "", &surveyed).is_ok());
        assert!(ok("\"$libdir/warren_surveyor_pg.so\"", "", &surveyed).is_ok());
        assert!(ok("", "warren_surveyor_pg", &surveyed).is_ok());
        // no surveyor stands: nothing need be preloaded
        assert!(ok("", "", &plain).is_ok());
        let e = ok("", "", &surveyed).unwrap_err();
        assert!(e.contains("do not preload warren_surveyor_pg"), "{e}");
    }

    #[test]
    fn a_preloaded_library_is_found_where_the_server_loads_it() {
        assert_eq!(
            preloaded(&preloading("a, \"$libdir/b.so\"", "c, a")),
            ["c", "a", "$libdir/b.so"]
        );
        let d = "/srv/pg/lib";
        assert_eq!(
            library_file("warren_surveyor_pg", d),
            std::path::Path::new("/srv/pg/lib/warren_surveyor_pg.so")
        );
        assert_eq!(
            library_file("$libdir/b.so", d),
            std::path::Path::new("/srv/pg/lib/b.so")
        );
        assert_eq!(library_name("$libdir/b.so"), "b");
        assert!(local_host("localhost") && local_host("127.0.0.1") && local_host("/tmp"));
        assert!(!local_host("db.example.org"));
    }

    #[test]
    fn passwords_are_redacted() {
        let u =
            Url::parse("postgres://blocker:s3cret@localhost:5432/blocker?sslmode=disable").unwrap();
        assert!(!u.redacted.contains("s3cret"));
        assert_eq!(
            u.redacted,
            "postgres://blocker:***@localhost:5432/blocker?sslmode=disable"
        );
        let q = Url::parse("postgres://localhost:5432/db?user=a&password=s3cret").unwrap();
        assert!(!q.redacted.contains("s3cret"));
        assert_eq!(q.server(), "localhost:5432");
    }

    fn with_version(v: &str) -> Vec<(String, String)> {
        vec![
            ("setting.server_version".into(), "x".into()),
            ("setting.server_version_num".into(), v.into()),
        ]
    }

    #[test]
    fn servers_from_version_13_on_are_taken() {
        for v in [
            130_000, 130_023, 140_024, 150_019, 160_015, 170_006, 180_006,
        ] {
            assert_eq!(server_version(&with_version(&v.to_string())), Ok(v));
        }
    }

    #[test]
    fn a_server_older_than_13_is_refused() {
        for v in ["120022", "129999", "110000", "90624"] {
            let e = server_version(&with_version(v)).unwrap_err();
            assert!(e.contains("older than 130000"), "{v}: {e}");
        }
    }

    #[test]
    fn a_server_version_that_is_absent_or_not_a_number_is_refused() {
        assert!(server_version(&with_version("")).is_err());
        assert!(server_version(&with_version("18.6")).is_err());
        assert!(server_version(&[]).is_err());
    }

    #[test]
    fn an_absent_value_is_recorded_empty_and_a_present_one_as_it_is() {
        assert_eq!(
            named(
                "setting",
                vec![
                    ("io_combine_limit".into(), None),
                    ("work_mem".into(), Some("4MB".into())),
                ]
            ),
            vec![
                ("setting.io_combine_limit".to_string(), String::new()),
                ("setting.work_mem".to_string(), "4MB".to_string()),
            ]
        );
    }

    #[test]
    fn configuration_fingerprint_ignores_this_programs_session_settings() {
        let a = vec![
            ("version".to_string(), "x".to_string()),
            ("setting.statement_timeout".to_string(), "2min".to_string()),
        ];
        let mut b = a.clone();
        b[1].1 = "5min".into();
        assert_eq!(config_id(&a), config_id(&b));
        b[0].1 = "y".into();
        assert_ne!(config_id(&a), config_id(&b));
    }
}
