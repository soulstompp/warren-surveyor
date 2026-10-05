// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Index sets: which of a database's indexes a run turns off. Every index of the database's own
//! schemas is its roster. A set, declared in a JSON file, names the indexes it turns off by
//! selectors, and every repetition under it runs as
//! `BEGIN; DROP INDEX …; SET LOCAL transaction_read_only = on; <question>; ROLLBACK`, so the
//! indexes are off only inside that transaction. A key is one index definition on one
//! table and on every class inheriting from it: the indexes that share their root relation through
//! `pg_inherits` and their definition after `USING <method>`. A table's surveyor is in no key.
//!
//! The file:
//!
//! ```text
//! { "sets":     { "<name>": { "off": [<selector>, …] }, … },
//!   "families": { "<name>": { "base": "<set>", "off": [<selector>, …] }, … } }
//! ```
//!
//! A selector is an object of any of `method` (an access method), `constraint` (true for an index
//! that backs a primary key, unique or exclusion constraint), `expression` (true for an index with
//! an expression among its keys), `schema`, `table` (the index's table, or the root relation it
//! inherits from) and `key`; an index matches when it matches every field given. A set turns
//! off every index some selector of its `off` matches. A family member, `<family>:<key>`,
//! turns off what its base set turns off and the indexes of that key the family's selectors
//! match. Unknown keys are refused.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::Row;

use crate::digest::fingerprint;
use crate::explain::Facts;
use crate::tables::{Declared, Kind};
use crate::target;

/// The name ending of an index whose name names its key.
pub const SOURCE_SUFFIX: &str = "_idx";

/// The surveyor's access method. A surveyor is its table's own, one to a table, whatever columns it
/// names, so a surveyor belongs to no key.
pub const SURVEYOR: &str = "surveyor";

/// One index of the roster.
#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub oid: i64,
    pub schema: String,
    pub table: String,
    /// The table's kind: `r` a plain table, `p` a partitioned one, `m` a materialized view.
    pub relkind: String,
    /// The relation the table inherits from, through `pg_inherits`, at the top: the table itself
    /// when it inherits from none; several are joined by `,`.
    pub root: String,
    /// `schema.table`, quoted where a name needs it.
    pub qualified_table: String,
    pub name: String,
    /// `schema.name`, quoted where a name needs it.
    pub qualified: String,
    pub method: String,
    /// `pg_get_indexdef`, read with `search_path` set to `pg_catalog`.
    pub definition: String,
    /// Each key part as `pg_get_indexdef(index, k, false)` prints it.
    pub keys: Vec<String>,
    pub include: Vec<String>,
    pub predicate: Option<String>,
    /// The constraint the index backs: a primary key, unique or exclusion constraint.
    pub constraint: Option<String>,
    pub expression: bool,
    pub unique: bool,
    pub valid: bool,
    /// Ready for inserts and live.
    pub ready: bool,
    /// Each key part's operator class, with ` (not default)` after one that is not its type's
    /// default.
    pub opclasses: Vec<String>,
    pub collations: String,
    pub options: String,
    pub reloptions: Option<String>,
    pub bytes: i64,
    /// The index this one is attached to: a partition's index belongs to its partitioned table's,
    /// and is dropped only with it.
    pub parent: Option<i64>,
}

impl Index {
    /// The definition after `USING <method> `: the keys, INCLUDE and predicate.
    pub fn key_text(&self) -> Option<&str> {
        let needle = format!(" USING {} ", self.method);
        self.definition
            .find(&needle)
            .map(|at| &self.definition[at + needle.len()..])
    }

    pub fn id(&self) -> (String, String) {
        (self.schema.clone(), self.name.clone())
    }

    /// The line the roster's fingerprint is taken over.
    fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            self.schema, self.table, self.name, self.method, self.definition, self.valid
        )
    }
}

/// A fingerprint of a roster: its indexes, their tables, methods, definitions and validity.
pub fn roster_fingerprint(roster: &[Index]) -> String {
    let mut lines: Vec<String> = roster.iter().map(Index::line).collect();
    lines.sort();
    let refs: Vec<&[u8]> = lines.iter().map(String::as_bytes).collect();
    fingerprint(&refs)
}

const USER_SCHEMAS: &str = "n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'";

/// The roster, read in the transaction already open on `conn`, whose `search_path` is set to
/// `pg_catalog` for it (with `SET LOCAL`), so every name a definition holds is printed qualified.
pub async fn read_roster_here(conn: &mut PgConnection) -> Result<Vec<Index>, String> {
    let e = |e: sqlx::Error| format!("reading the indexes: {e}");
    sqlx::raw_sql("SET LOCAL search_path = pg_catalog")
        .execute(&mut *conn)
        .await
        .map_err(e)?;
    let sql = format!(
        "WITH RECURSIVE up(rel, anc, depth) AS (
             SELECT DISTINCT x.indrelid, x.indrelid, 0
             FROM pg_index x JOIN pg_class t ON t.oid = x.indrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace
             WHERE {USER_SCHEMAS}
             UNION
             SELECT u.rel, i.inhparent, u.depth + 1
             FROM up u JOIN pg_inherits i ON i.inhrelid = u.anc
             WHERE u.depth < 100
         ), roots AS (
             SELECT u.rel, string_agg(DISTINCT c.relname::text, ',' ORDER BY c.relname::text) AS root
             FROM up u JOIN pg_class c ON c.oid = u.anc
             WHERE NOT EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhrelid = u.anc)
             GROUP BY u.rel
         )
         SELECT x.indexrelid::int8 AS oid, n.nspname::text AS schema, t.relname::text AS tbl,
                t.relkind::text AS relkind, coalesce(r.root, t.relname::text) AS root,
                format('%I.%I', n.nspname, t.relname) AS qualified_table,
                i.relname::text AS name, format('%I.%I', n.nspname, i.relname) AS qualified,
                a.amname::text AS method, pg_get_indexdef(x.indexrelid) AS definition,
                ARRAY(SELECT pg_get_indexdef(x.indexrelid, k, false)
                      FROM generate_series(1, x.indnkeyatts::int) k ORDER BY k) AS keys,
                ARRAY(SELECT pg_get_indexdef(x.indexrelid, k, false)
                      FROM generate_series(x.indnkeyatts::int + 1, x.indnatts::int) k
                      ORDER BY k) AS include,
                pg_get_expr(x.indpred, x.indrelid) AS predicate,
                (SELECT c.conname::text FROM pg_constraint c
                 WHERE c.conindid = x.indexrelid AND c.conrelid = x.indrelid
                   AND c.contype IN ('p', 'u', 'x')
                 ORDER BY c.conname LIMIT 1) AS constraint_name,
                x.indexprs IS NOT NULL AS expression, x.indisunique AS is_unique,
                x.indisvalid AS valid, x.indisready AND x.indislive AS ready,
                ARRAY(SELECT oc.opcname::text
                             || CASE WHEN oc.opcdefault THEN '' ELSE ' (not default)' END
                      FROM unnest(x.indclass::oid[]) WITH ORDINALITY AS u(o, k)
                      JOIN pg_opclass oc ON oc.oid = u.o ORDER BY u.k) AS opclasses,
                x.indcollation::text AS collations, x.indoption::text AS options,
                i.reloptions::text AS reloptions,
                pg_relation_size(x.indexrelid)::int8 AS bytes,
                (SELECT h.inhparent::int8 FROM pg_inherits h
                 WHERE h.inhrelid = x.indexrelid) AS parent
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         JOIN pg_class t ON t.oid = x.indrelid
         JOIN pg_namespace n ON n.oid = t.relnamespace
         JOIN pg_am a ON a.oid = i.relam
         LEFT JOIN roots r ON r.rel = x.indrelid
         WHERE {USER_SCHEMAS}
         ORDER BY 2, 3, 6"
    );
    let rows = sqlx::query(&sql).fetch_all(&mut *conn).await.map_err(e)?;
    rows.iter()
        .map(|r| {
            let g = |e: sqlx::Error| format!("reading the indexes: {e}");
            Ok(Index {
                oid: r.try_get("oid").map_err(g)?,
                schema: r.try_get("schema").map_err(g)?,
                table: r.try_get("tbl").map_err(g)?,
                relkind: r.try_get("relkind").map_err(g)?,
                root: r.try_get("root").map_err(g)?,
                qualified_table: r.try_get("qualified_table").map_err(g)?,
                name: r.try_get("name").map_err(g)?,
                qualified: r.try_get("qualified").map_err(g)?,
                method: r.try_get("method").map_err(g)?,
                definition: r.try_get("definition").map_err(g)?,
                keys: r.try_get("keys").map_err(g)?,
                include: r.try_get("include").map_err(g)?,
                predicate: r.try_get("predicate").map_err(g)?,
                constraint: r.try_get("constraint_name").map_err(g)?,
                expression: r.try_get("expression").map_err(g)?,
                unique: r.try_get("is_unique").map_err(g)?,
                valid: r.try_get("valid").map_err(g)?,
                ready: r.try_get("ready").map_err(g)?,
                opclasses: r.try_get("opclasses").map_err(g)?,
                collations: r.try_get("collations").map_err(g)?,
                options: r.try_get("options").map_err(g)?,
                reloptions: r.try_get("reloptions").map_err(g)?,
                bytes: r.try_get("bytes").map_err(g)?,
                parent: r.try_get("parent").map_err(g)?,
            })
        })
        .collect()
}

/// The committed roster: read in a transaction of its own, so `conn`'s own `search_path` is left
/// as it was.
pub async fn read_roster(conn: &mut PgConnection) -> Result<Vec<Index>, String> {
    sqlx::raw_sql("BEGIN")
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("BEGIN: {e}"))?;
    let r = read_roster_here(conn).await;
    let end = if r.is_ok() { "COMMIT" } else { "ROLLBACK" };
    sqlx::raw_sql(end)
        .execute(&mut *conn)
        .await
        .map_err(|e| format!("{end}: {e}"))?;
    r
}

/// The versions of the extensions installed in the database, by name.
pub async fn extensions(conn: &mut PgConnection) -> Result<Vec<(String, String)>, String> {
    sqlx::query_as("SELECT extname::text, extversion FROM pg_extension ORDER BY 1")
        .fetch_all(conn)
        .await
        .map_err(|e| format!("reading pg_extension: {e}"))
}

/// One key: the non-constraint indexes that share their root relation and their key text.
#[derive(Clone, Debug)]
pub struct Key {
    pub name: String,
    pub root: String,
    pub key: String,
    /// Positions in the roster.
    pub members: Vec<usize>,
}

/// An index's name without its `_idx` ending, and without its table's name in front: what names
/// its key.
fn key_part_of_name(i: &Index) -> String {
    let base = i.name.strip_suffix(SOURCE_SUFFIX).unwrap_or(&i.name);
    match base.strip_prefix(&format!("{}_", i.table)) {
        Some(part) if !part.is_empty() => format!("{}_{part}", i.root),
        _ => base.to_string(),
    }
}

/// The keys of a roster. A key is named by its root and the part of its indexes' names that
/// names the key (`lego_sets_theme_id_set_num` for `lego_sets_theme_id_set_num_idx` on
/// `lego.lego_sets` and `lego_sets_town_theme_id_set_num_idx` on the class `lego_oo.lego_sets_town`);
/// where its indexes' names differ, by the first of them in order. An index backing a constraint
/// and a surveyor belong to no key, whatever their columns.
pub fn keys(roster: &[Index]) -> Vec<Key> {
    let mut by: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    for (at, i) in roster.iter().enumerate() {
        if i.constraint.is_some() || i.method == SURVEYOR {
            continue;
        }
        if let Some(k) = i.key_text() {
            by.entry((i.root.clone(), k.to_string()))
                .or_default()
                .push(at);
        }
    }
    by.into_iter()
        .map(|((root, key), members)| {
            let name = members
                .iter()
                .map(|m| key_part_of_name(&roster[*m]))
                .min()
                .unwrap_or_default();
            Key {
                name,
                root,
                key,
                members,
            }
        })
        .collect()
}

/// Which indexes a selector matches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selector {
    pub method: Option<String>,
    pub constraint: Option<bool>,
    pub expression: Option<bool>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub key: Option<String>,
}

impl Selector {
    fn matches(&self, i: &Index, key: Option<&str>) -> bool {
        self.method.as_ref().is_none_or(|m| *m == i.method)
            && self.constraint.is_none_or(|c| c == i.constraint.is_some())
            && self.expression.is_none_or(|e| e == i.expression)
            && self.schema.as_ref().is_none_or(|s| *s == i.schema)
            && self
                .table
                .as_ref()
                .is_none_or(|t| *t == i.table || i.root.split(',').any(|r| r == t))
            && self.key.as_ref().is_none_or(|s| Some(s.as_str()) == key)
    }
}

#[derive(Clone, Debug)]
struct SetDecl {
    off: Vec<Selector>,
}

#[derive(Clone, Debug)]
struct FamilyDecl {
    base: String,
    off: Vec<Selector>,
}

/// The declared sets.
#[derive(Clone, Debug)]
pub struct Sets {
    pub file: PathBuf,
    sets: BTreeMap<String, SetDecl>,
    families: BTreeMap<String, FamilyDecl>,
}

fn set_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn only_keys(o: &serde_json::Map<String, Json>, keys: &[&str], what: &str) -> Result<(), String> {
    match o.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(k) => Err(format!("{what}: unknown key `{k}`")),
        None => Ok(()),
    }
}

fn selector(v: &Json, what: &str) -> Result<Selector, String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("{what}: a selector is an object"))?;
    only_keys(
        o,
        &[
            "method",
            "constraint",
            "expression",
            "schema",
            "table",
            "key",
        ],
        what,
    )?;
    let text = |k: &str| -> Result<Option<String>, String> {
        o.get(k)
            .map(|x| {
                x.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| format!("{what}: `{k}` is a name"))
            })
            .transpose()
    };
    let flag = |k: &str| -> Result<Option<bool>, String> {
        o.get(k)
            .map(|x| {
                x.as_bool()
                    .ok_or_else(|| format!("{what}: `{k}` is true or false"))
            })
            .transpose()
    };
    Ok(Selector {
        method: text("method")?,
        constraint: flag("constraint")?,
        expression: flag("expression")?,
        schema: text("schema")?,
        table: text("table")?,
        key: text("key")?,
    })
}

fn selectors(o: &serde_json::Map<String, Json>, what: &str) -> Result<Vec<Selector>, String> {
    o.get("off")
        .ok_or_else(|| format!("{what}: no `off`"))?
        .as_array()
        .ok_or_else(|| format!("{what}: `off` is a list of selectors"))?
        .iter()
        .enumerate()
        .map(|(n, s)| selector(s, &format!("{what}: selector {}", n + 1)))
        .collect()
}

/// Reads the declared sets, refusing any key it does not know.
pub fn parse_sets(text: &str, file: &Path) -> Result<Sets, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", file.display());
    let j: Json = serde_json::from_str(text).map_err(|e| at(&e))?;
    let top = j.as_object().ok_or_else(|| at(&"want a JSON object"))?;
    only_keys(top, &["sets", "families"], "the file").map_err(|e| at(&e))?;
    let mut sets = BTreeMap::new();
    for (name, v) in top
        .get("sets")
        .ok_or_else(|| at(&"no `sets`"))?
        .as_object()
        .ok_or_else(|| at(&"`sets` is an object"))?
    {
        let what = format!("set {name}");
        if !set_name(name) {
            return Err(at(&format!(
                "{what}: a set's name is lowercase letters, digits, - and _"
            )));
        }
        let o = v
            .as_object()
            .ok_or_else(|| at(&format!("{what}: want an object")))?;
        only_keys(o, &["off"], &what).map_err(|e| at(&e))?;
        sets.insert(
            name.clone(),
            SetDecl {
                off: selectors(o, &what).map_err(|e| at(&e))?,
            },
        );
    }
    let mut families = BTreeMap::new();
    if let Some(f) = top.get("families") {
        for (name, v) in f
            .as_object()
            .ok_or_else(|| at(&"`families` is an object"))?
        {
            let what = format!("family {name}");
            if !set_name(name) || sets.contains_key(name) {
                return Err(at(&format!(
                    "{what}: a family's name is lowercase letters, digits, - and _, and no set's"
                )));
            }
            let o = v
                .as_object()
                .ok_or_else(|| at(&format!("{what}: want an object")))?;
            only_keys(o, &["base", "off"], &what).map_err(|e| at(&e))?;
            let base = o
                .get("base")
                .and_then(Json::as_str)
                .ok_or_else(|| at(&format!("{what}: `base` names a set")))?
                .to_string();
            if !sets.contains_key(&base) {
                return Err(at(&format!(
                    "{what}: its base {base} is not a declared set"
                )));
            }
            let off = selectors(o, &what).map_err(|e| at(&e))?;
            if off.is_empty() {
                return Err(at(&format!("{what}: `off` names nothing of the key")));
            }
            families.insert(name.clone(), FamilyDecl { base, off });
        }
    }
    Ok(Sets {
        file: file.to_path_buf(),
        sets,
        families,
    })
}

pub fn read_sets(file: &Path) -> Result<Sets, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    parse_sets(&text, file)
}

/// The index sets the bench declares itself, in its own `index_sets.json`: those it runs under
/// where no file is given.
pub fn built_in() -> Result<Sets, String> {
    parse_sets(
        include_str!("../index_sets.json"),
        Path::new("the bench's own index_sets.json"),
    )
}

/// A resolved set: the committed roster, and the indexes the set turns off in it.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub set: String,
    pub roster: Vec<Index>,
    /// Positions in the roster.
    pub off: BTreeSet<usize>,
    pub fingerprint: String,
}

impl Resolved {
    /// What opens a repetition: the transaction, and the indexes dropped in it.
    pub fn open(&self) -> Vec<String> {
        // an index attached to one the set also turns off goes with it: PostgreSQL drops a
        // partition's index only through its partitioned table's
        let off_oids: BTreeSet<i64> = self.off.iter().map(|at| self.roster[*at].oid).collect();
        let mut drops: Vec<String> = self
            .off
            .iter()
            .filter(|at| {
                self.roster[**at]
                    .parent
                    .is_none_or(|p| !off_oids.contains(&p))
            })
            .map(|at| format!("DROP INDEX {}", self.roster[*at].qualified))
            .collect();
        drops.sort();
        let mut out = vec!["BEGIN".to_string()];
        out.extend(drops);
        out
    }

    /// What closes a repetition: the rollback.
    pub fn close(&self) -> Vec<String> {
        vec!["ROLLBACK".to_string()]
    }

    /// The indexes turned off, as `schema.index`, in order, joined by `,`.
    pub fn off_names(&self) -> String {
        let mut n: Vec<String> = self
            .off
            .iter()
            .map(|at| format!("{}.{}", self.roster[*at].schema, self.roster[*at].name))
            .collect();
        n.sort();
        n.join(",")
    }

    /// The indexes present under the set, as (schema, index).
    pub fn present(&self) -> BTreeSet<(String, String)> {
        self.roster
            .iter()
            .enumerate()
            .filter(|(at, _)| !self.off.contains(at))
            .map(|(_, i)| i.id())
            .collect()
    }

    /// Every scan of an EXPLAIN that used an index the set does not hold present.
    pub fn unlawful(&self, facts: &Facts) -> Vec<String> {
        let present = self.present();
        let mut out = Vec::new();
        for s in &facts.scans {
            for ix in &s.indexes {
                let schema = s.schema.clone().unwrap_or_default();
                if schema.starts_with("pg_") || schema == "information_schema" {
                    continue;
                }
                if !present.contains(&(schema.clone(), ix.clone())) {
                    out.push(format!(
                        "a scan of {schema}.{} used {schema}.{ix}, which the set {} does not hold present",
                        s.relation, self.set
                    ));
                }
            }
        }
        out
    }
}

/// Resolves the set `name` (a declared set, or `<family>:<key>`) against a roster.
pub fn resolve_set(sets: &Sets, name: &str, roster: &[Index]) -> Result<Resolved, String> {
    let at = |e: &str| format!("index set {name}: {e}");
    let keys = keys(roster);
    let mut key_of: BTreeMap<usize, &str> = BTreeMap::new();
    let mut by_name: BTreeMap<&str, Vec<&Key>> = BTreeMap::new();
    for s in &keys {
        for m in &s.members {
            key_of.insert(*m, &s.name);
        }
        by_name.entry(s.name.as_str()).or_default().push(s);
    }
    let matching = |sels: &[Selector], only: Option<&Key>| -> BTreeSet<usize> {
        roster
            .iter()
            .enumerate()
            .filter(|(at, i)| {
                only.is_none_or(|s| s.members.contains(at))
                    && sels
                        .iter()
                        .any(|sel| sel.matches(i, key_of.get(at).copied()))
            })
            .map(|(at, _)| at)
            .collect()
    };
    let (off, declared_empty) = match name.split_once(':') {
        None => {
            let d = sets.sets.get(name).ok_or_else(|| {
                at(&format!(
                    "not declared in {}; the sets are {}, and the families {}",
                    sets.file.display(),
                    sets.sets.keys().cloned().collect::<Vec<_>>().join(", "),
                    sets.families.keys().cloned().collect::<Vec<_>>().join(", ")
                ))
            })?;
            (matching(&d.off, None), d.off.is_empty())
        }
        Some((family, key)) => {
            let f = sets.families.get(family).ok_or_else(|| {
                at(&format!(
                    "{family} is not a family declared in {}",
                    sets.file.display()
                ))
            })?;
            let s = match by_name.get(key).map(Vec::as_slice) {
                Some([one]) => *one,
                Some(_) => {
                    return Err(at(&format!(
                        "more than one key is named {key}, so it names none"
                    )))
                }
                None => {
                    return Err(at(&format!(
                        "no key is named {key}; the keys are {}",
                        keys.iter()
                            .map(|s| format!("{} ({} USING … {})", s.name, s.root, s.key))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )))
                }
            };
            let own = matching(&f.off, Some(s));
            if own.is_empty() {
                return Err(at(&format!(
                    "the key {key} has no index the family {family} turns off"
                )));
            }
            let base = &sets.sets[&f.base];
            let mut off = matching(&base.off, None);
            off.extend(own);
            (off, false)
        }
    };
    let constraints: Vec<&str> = off
        .iter()
        .filter(|at| roster[**at].constraint.is_some())
        .map(|at| roster[*at].qualified.as_str())
        .collect();
    if !constraints.is_empty() {
        return Err(at(&format!(
            "it would turn off indexes that back constraints: {}",
            constraints.join(", ")
        )));
    }
    if off.is_empty() && !declared_empty {
        return Err(at("it turns nothing off in this database"));
    }
    let off_oids: BTreeSet<i64> = off.iter().map(|at| roster[*at].oid).collect();
    let alone: Vec<String> = off
        .iter()
        .filter_map(|at| {
            let i = &roster[*at];
            let p = i.parent?;
            if off_oids.contains(&p) {
                return None;
            }
            let parent = roster
                .iter()
                .find(|r| r.oid == p)
                .map_or_else(|| p.to_string(), |r| r.qualified.clone());
            Some(format!("{} (attached to {parent})", i.qualified))
        })
        .collect();
    if !alone.is_empty() {
        return Err(at(&format!(
            "it would turn off a partition's index without its partitioned table's, which PostgreSQL drops only together: {}",
            alone.join(", ")
        )));
    }
    Ok(Resolved {
        set: name.to_string(),
        fingerprint: roster_fingerprint(roster),
        roster: roster.to_vec(),
        off,
    })
}

/// Certifies a resolved set: in a session of the target, the transaction every repetition opens, then
/// the roster read inside it, then ROLLBACK. The roster inside must be the committed one less the
/// indexes the set turns off.
pub async fn certify(
    opts: &PgConnectOptions,
    statement_timeout: &str,
    resolved: &Resolved,
) -> Result<(), String> {
    let at = |e: &str| format!("index set {}: {e}", resolved.set);
    let mut s = target::open(opts, statement_timeout).await?;
    let inside = async {
        for sql in resolved.open() {
            target::simple(&mut s.conn, &sql)
                .await
                .map_err(|e| format!("before the question: {sql}: {e}"))?;
        }
        read_roster_here(&mut s.conn).await
    }
    .await;
    let rolled = target::simple(&mut s.conn, "ROLLBACK").await;
    s.close().await;
    let inside = inside.map_err(|e| at(&e))?;
    rolled.map_err(|e| at(&format!("ROLLBACK: {e}")))?;
    let got: BTreeSet<(String, String, String)> = inside
        .iter()
        .map(|i| (i.schema.clone(), i.name.clone(), i.definition.clone()))
        .collect();
    let want: BTreeSet<(String, String, String)> = resolved
        .roster
        .iter()
        .enumerate()
        .filter(|(at, _)| !resolved.off.contains(at))
        .map(|(_, i)| (i.schema.clone(), i.name.clone(), i.definition.clone()))
        .collect();
    if got != want {
        let extra: Vec<String> = got
            .difference(&want)
            .map(|(s, n, _)| format!("{s}.{n}"))
            .collect();
        let missing: Vec<String> = want
            .difference(&got)
            .map(|(s, n, _)| format!("{s}.{n}"))
            .collect();
        return Err(at(&format!(
            "inside its transaction the indexes are not the roster less those it turns off: present and not expected {extra:?}; expected and not present {missing:?}"
        )));
    }
    Ok(())
}

/// One line per (index set, target, index): the roster, and which of its indexes each set held.
pub const INDEXES: Declared = Declared {
    name: "indexes",
    columns: &[
        ("index_set", Kind::Text),
        ("target", Kind::Text),
        ("schema", Kind::Text),
        ("table", Kind::Text),
        ("index", Kind::Text),
        ("method", Kind::Text),
        ("keys", Kind::Text),
        ("include", Kind::Text),
        ("predicate", Kind::Text),
        ("constraint", Kind::Text),
        ("expression", Kind::Bool),
        ("present", Kind::Bool),
        ("valid", Kind::Bool),
        ("bytes", Kind::Int),
    ],
    key: &["index_set", "target", "schema", "index"],
};

/// The lines of `indexes.parquet` for one target: under the set `index_set` (none when the run
/// named no set), each index of the roster and whether the set held it present.
pub fn index_rows(
    index_set: Option<&str>,
    target: &str,
    roster: &[Index],
    off: &BTreeSet<usize>,
) -> Vec<Vec<String>> {
    roster
        .iter()
        .enumerate()
        .map(|(at, i)| {
            vec![
                index_set.unwrap_or_default().to_string(),
                target.to_string(),
                i.schema.clone(),
                i.table.clone(),
                i.name.clone(),
                i.method.clone(),
                i.keys.join(", "),
                i.include.join(", "),
                i.predicate.clone().unwrap_or_default(),
                i.constraint.clone().unwrap_or_default(),
                i.expression.to_string(),
                (!off.contains(&at)).to_string(),
                i.valid.to_string(),
                i.bytes.to_string(),
            ]
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn index(
        schema: &str,
        table: &str,
        root: &str,
        name: &str,
        method: &str,
        keys: &str,
    ) -> Index {
        Index {
            oid: 0,
            schema: schema.into(),
            table: table.into(),
            relkind: "r".into(),
            root: root.into(),
            qualified_table: format!("{schema}.{table}"),
            name: name.into(),
            qualified: format!("{schema}.{name}"),
            method: method.into(),
            definition: format!("CREATE INDEX {name} ON {schema}.{table} USING {method} ({keys})"),
            keys: keys.split(", ").map(str::to_string).collect(),
            include: vec![],
            predicate: None,
            constraint: None,
            expression: keys.contains('('),
            unique: false,
            valid: true,
            ready: true,
            opclasses: vec!["int4_ops".into(); keys.split(", ").count()],
            collations: "0".into(),
            options: "0".into(),
            reloptions: None,
            bytes: 8192,
            parent: None,
        }
    }

    fn pkey(schema: &str, table: &str, root: &str) -> Index {
        let name = format!("{table}_pkey");
        Index {
            constraint: Some(name.clone()),
            unique: true,
            definition: format!(
                "CREATE UNIQUE INDEX {name} ON {schema}.{table} USING btree (set_num)"
            ),
            ..index(schema, table, root, &name, "btree", "set_num")
        }
    }

    /// A roster like a load's: a flat table and two classes of its hierarchy, each with its
    /// primary key and a key, and an expression key on another table.
    pub fn roster() -> Vec<Index> {
        let mut r = Vec::new();
        for (schema, table, root) in [
            ("lego", "lego_sets", "lego_sets"),
            ("lego_oo", "lego_sets_town", "lego_sets"),
            ("lego_oo", "lego_sets_space", "lego_sets"),
        ] {
            r.push(pkey(schema, table, root));
            r.push(index(
                schema,
                table,
                root,
                &format!("{table}_theme_id_set_num_idx"),
                "btree",
                "theme_id, set_num",
            ));
        }
        r.push(index(
            "lego",
            "lego_purchases",
            "lego_purchases",
            "lego_purchases_month_clock_idx",
            "btree",
            "EXTRACT(month FROM lego.clock(ordered_at)), lego.clock(ordered_at)",
        ));
        r
    }

    pub fn standard() -> Sets {
        built_in().unwrap()
    }

    fn off_of(l: &Resolved) -> Vec<String> {
        l.off_names()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_key_is_one_on_a_table_and_every_class_under_its_root() {
        let r = roster();
        let s = keys(&r);
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["lego_purchases_month_clock", "lego_sets_theme_id_set_num"]
        );
        // three tables, each with the key's B-tree
        assert_eq!(s[1].members.len(), 3);
        assert!(s
            .iter()
            .all(|s| s.members.iter().all(|m| r[*m].constraint.is_none())));
    }

    /// The roster with one surveyor on the flat sets, on the same columns as their key's B-tree.
    fn roster_with_a_surveyor() -> Vec<Index> {
        let mut r = roster();
        r.push(index(
            "lego",
            "lego_sets",
            "lego_sets",
            "lego_sets_surveyor",
            SURVEYOR,
            "theme_id, set_num",
        ));
        r
    }

    #[test]
    fn the_standard_sets_turn_off_what_they_name() {
        let sets = standard();
        let r = roster_with_a_surveyor();
        let off = |name: &str| off_of(&resolve_set(&sets, name, &r).unwrap());
        assert!(off("all").is_empty());
        assert_eq!(off("dba"), ["lego.lego_sets_surveyor"]);
        assert_eq!(off("keys").len(), 5);
        assert!(off("keys").iter().all(|n| !n.ends_with("_pkey")));
        let without = off("without:lego_sets_theme_id_set_num");
        assert_eq!(without.len(), 3);
        assert!(without.contains(&"lego_oo.lego_sets_town_theme_id_set_num_idx".to_string()));
        assert!(!without.contains(&"lego.lego_purchases_month_clock_idx".to_string()));
        // on a roster with no surveyor, the DBA's indexes alone turn nothing off
        let e = resolve_set(&sets, "dba", &roster()).unwrap_err();
        assert!(e.contains("turns nothing off"), "{e}");
    }

    #[test]
    fn a_surveyor_on_a_keys_columns_is_in_no_key() {
        let r = roster_with_a_surveyor();
        let s = keys(&r);
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["lego_purchases_month_clock", "lego_sets_theme_id_set_num"]
        );
        assert!(s
            .iter()
            .all(|s| s.members.iter().all(|m| r[*m].method != SURVEYOR)));
        // turning off a key leaves the table's surveyor present
        let without = resolve_set(&standard(), "without:lego_sets_theme_id_set_num", &r).unwrap();
        assert!(without
            .present()
            .contains(&("lego".into(), "lego_sets_surveyor".into())));
        let e = resolve_set(&standard(), "without:lego_sets_surveyor", &r).unwrap_err();
        assert!(e.contains("no key is named lego_sets_surveyor"), "{e}");
    }

    #[test]
    fn a_set_that_would_drop_a_constraint_or_nothing_is_refused() {
        let r = roster();
        let text = r#"{"sets": {"all": {"off": []},
            "everything": {"off": [{}]},
            "nothing": {"off": [{"method": "hash"}]}}}"#;
        let sets = parse_sets(text, Path::new("t.json")).unwrap();
        let e = resolve_set(&sets, "everything", &r).unwrap_err();
        assert!(e.contains("back constraints"), "{e}");
        let e = resolve_set(&sets, "nothing", &r).unwrap_err();
        assert!(e.contains("turns nothing off"), "{e}");
        assert!(resolve_set(&sets, "all", &r).is_ok());
        let e = resolve_set(&standard(), "without:no_such", &r).unwrap_err();
        assert!(e.contains("no key is named no_such"), "{e}");
        let e = resolve_set(&standard(), "nonesuch", &r).unwrap_err();
        assert!(e.contains("not declared"), "{e}");
    }

    #[test]
    fn a_family_member_turns_off_its_key_on_every_class() {
        let r = roster();
        let l = resolve_set(&standard(), "without:lego_sets_theme_id_set_num", &r).unwrap();
        let open = l.open();
        assert_eq!(open[0], "BEGIN");
        assert!(
            open.contains(&"DROP INDEX lego_oo.lego_sets_space_theme_id_set_num_idx".to_string())
        );
        assert!(open.contains(&"DROP INDEX lego.lego_sets_theme_id_set_num_idx".to_string()));
        assert_eq!(l.close(), ["ROLLBACK"]);
        assert!(!l.present().contains(&(
            "lego_oo".into(),
            "lego_sets_town_theme_id_set_num_idx".into()
        )));
        assert!(l
            .present()
            .contains(&("lego".into(), "lego_purchases_month_clock_idx".into())));
    }

    #[test]
    fn the_sets_file_refuses_what_it_does_not_know() {
        let e = |t: &str| parse_sets(t, Path::new("t.json")).err().unwrap_or_default();
        assert!(e(r#"{"sets": {}, "extra": 1}"#).contains("unknown key `extra`"));
        assert!(e(r#"{"sets": {"a": {"off": [], "of": 1}}}"#).contains("unknown key `of`"));
        assert!(
            e(r#"{"sets": {"a": {"off": [{"metod": "btree"}]}}}"#).contains("unknown key `metod`")
        );
        assert!(e(r#"{"sets": {"a": {"off": [{"constraint": "no"}]}}}"#).contains("true or false"));
        assert!(e(r#"{"sets": {"A b": {"off": []}}}"#).contains("lowercase"));
        assert!(e(
            r#"{"sets": {"a": {"off": []}}, "families": {"f": {"base": "b", "off": [{}]}}}"#
        )
        .contains("not a declared set"));
        assert!(e(r#"{"sets": {"a": {}}}"#).contains("no `off`"));
    }

    #[test]
    fn a_scan_of_an_index_its_set_turned_off_breaks_the_law() {
        let l = resolve_set(&standard(), "keys", &roster()).unwrap();
        let scan = |ix: &str| crate::explain::Scan {
            relation: "lego_sets".into(),
            schema: Some("lego".into()),
            indexes: vec![ix.to_string()],
            ..crate::explain::Scan::default()
        };
        let facts = |ix: &str| Facts {
            scans: vec![scan(ix)],
            ..Facts::default()
        };
        assert!(l.unlawful(&facts("lego_sets_pkey")).is_empty());
        let e = l.unlawful(&facts("lego_sets_theme_id_set_num_idx"));
        assert_eq!(e.len(), 1);
        assert!(
            e[0].contains("lego.lego_sets_theme_id_set_num_idx"),
            "{e:?}"
        );
    }

    #[test]
    fn index_lines_round_trip_through_their_export() {
        let r = roster();
        let l = resolve_set(&standard(), "keys", &r).unwrap();
        let mut rows = index_rows(Some("keys"), "flat", &r, &l.off);
        rows.extend(index_rows(None, "plain", &r, &BTreeSet::new()));
        let dir = std::env::temp_dir().join(format!("warren-bench-indexes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("indexes.parquet");
        crate::tables::write(&p, &INDEXES, &rows).unwrap();
        let text =
            crate::tables::to_tsv(&INDEXES, crate::tables::read_values(&p, &INDEXES).unwrap())
                .unwrap();
        let back = crate::tables::from_tsv(&INDEXES, &text).unwrap();
        let q = dir.join("again.parquet");
        crate::tables::write_values(&q, &INDEXES, &back).unwrap();
        let again =
            crate::tables::to_tsv(&INDEXES, crate::tables::read_values(&q, &INDEXES).unwrap())
                .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(text, again);
        let names = INDEXES.names();
        let col = |c: &str| names.iter().position(|n| *n == c).unwrap();
        let off: Vec<String> = back
            .iter()
            .filter(|r| r[col("index_set")] == crate::tables::Value::Text("keys".into()))
            .filter(|r| r[col("present")] == crate::tables::Value::Bool(false))
            .map(|r| r[col("index")].text())
            .collect();
        assert_eq!(off.len(), 4);
        assert!(off.iter().all(|n| n.ends_with("_idx")));
        // a run that named no set holds every index present, under no index set
        assert!(back
            .iter()
            .filter(|r| r[col("index_set")] == crate::tables::Value::Null)
            .all(|r| r[col("present")] == crate::tables::Value::Bool(true)));
    }

    #[test]
    fn a_partitions_index_is_dropped_through_its_partitioned_tables_and_never_alone() {
        // a partitioned table with a surveyor and a key, each attached to its one partition's
        let at = |oid, table: &str, name: &str, method, keys, parent| Index {
            oid,
            relkind: if table == "t" { "p".into() } else { "r".into() },
            parent,
            ..index("s", table, "t", name, method, keys)
        };
        let roster = vec![
            at(1, "t", "t_surveyor", SURVEYOR, "id", None),
            at(2, "t_p0", "t_p0_surveyor", SURVEYOR, "id", Some(1)),
            at(3, "t", "t_a_idx", "btree", "a", None),
            at(4, "t_p0", "t_p0_a_idx", "btree", "a", Some(3)),
        ];
        let dba = resolve_set(&standard(), "dba", &roster).unwrap();
        assert_eq!(dba.off_names(), "s.t_p0_surveyor,s.t_surveyor");
        assert_eq!(dba.open(), vec!["BEGIN", "DROP INDEX s.t_surveyor"]);
        let keys = resolve_set(&standard(), "keys", &roster).unwrap();
        assert_eq!(
            keys.open(),
            vec!["BEGIN", "DROP INDEX s.t_a_idx", "DROP INDEX s.t_surveyor"]
        );
        // a selector that reaches the partition alone is refused
        let sets = parse_sets(
            r#"{ "sets": { "part": { "off": [ { "table": "t_p0", "method": "btree" } ] } }, "families": {} }"#,
            Path::new("x.json"),
        )
        .unwrap();
        let e = resolve_set(&sets, "part", &roster).unwrap_err();
        assert!(e.contains("s.t_p0_a_idx (attached to s.t_a_idx)"), "{e}");
    }
}
