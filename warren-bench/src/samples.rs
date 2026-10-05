// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Samples: a few questions, a few index sets and a few repetitions, declared in
//! `samples/<name>/sample.json`:
//!
//! ```text
//! { "questions": "<questions directory>", "binds": "<binds file>", "only": ["<prefix>", …],
//!   "index_sets": ["<set>", …], "cold": 1, "warm": 1, "statement_timeout": "60s" }
//! ```
//!
//! Paths are relative to the sample's own directory. `only` keeps the questions whose names start
//! with one of its prefixes (every question when it is empty). Unknown keys are refused.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use crate::digest::fingerprint;
use crate::questions::QuestionSet;

pub struct Sample {
    pub name: String,
    pub questions: PathBuf,
    pub binds: PathBuf,
    pub only: Vec<String>,
    pub index_sets: Vec<String>,
    pub cold: u32,
    pub warm: u32,
    pub statement_timeout: String,
}

const KEYS: [&str; 7] = [
    "questions",
    "binds",
    "only",
    "index_sets",
    "cold",
    "warm",
    "statement_timeout",
];

/// Whether `s` names a sample: lowercase letters, digits, - and _.
pub fn sample_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Reads the sample `name` from `samples/<name>/sample.json`, refusing any key it does not know
/// and any it lacks.
pub fn read(samples: &Path, name: &str) -> Result<Sample, String> {
    if !sample_name(name) {
        return Err(format!(
            "sample `{name}`: a sample's name is lowercase letters, digits, - and _"
        ));
    }
    let dir = samples.join(name);
    let file = dir.join("sample.json");
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", file.display());
    let text = std::fs::read_to_string(&file).map_err(|e| at(&e))?;
    let j: Json = serde_json::from_str(&text).map_err(|e| at(&e))?;
    let o = j.as_object().ok_or_else(|| at(&"want a JSON object"))?;
    if let Some(k) = o.keys().find(|k| !KEYS.contains(&k.as_str())) {
        return Err(at(&format!("unknown key `{k}`")));
    }
    if let Some(k) = KEYS.iter().find(|k| !o.contains_key(**k)) {
        return Err(at(&format!("no `{k}`")));
    }
    let text_of = |k: &str| {
        o[k].as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| at(&format!("`{k}` is a string")))
    };
    let list = |k: &str| -> Result<Vec<String>, String> {
        o[k].as_array()
            .ok_or_else(|| at(&format!("`{k}` is a list of strings")))?
            .iter()
            .map(|x| {
                x.as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| at(&format!("`{k}` is a list of strings")))
            })
            .collect()
    };
    let count = |k: &str| {
        o[k].as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| at(&format!("`{k}` is a count")))
    };
    let cold = count("cold")?;
    if cold == 0 {
        return Err(at(&"`cold` is at least 1"));
    }
    let index_sets = list("index_sets")?;
    if index_sets.is_empty() {
        return Err(at(&"`index_sets` names at least one set"));
    }
    Ok(Sample {
        name: name.to_string(),
        questions: dir.join(text_of("questions")?),
        binds: dir.join(text_of("binds")?),
        only: list("only")?,
        index_sets,
        cold,
        warm: count("warm")?,
        statement_timeout: text_of("statement_timeout")?,
    })
}

/// Keeps the questions whose names start with one of `only` (all of them when it is empty), and
/// adds the prefixes to the set's fingerprint. Refuses a prefix no question starts with.
pub fn restrict(set: &mut QuestionSet, only: &[String]) -> Result<(), String> {
    if only.is_empty() {
        return Ok(());
    }
    for p in only {
        if !set.questions.iter().any(|q| q.name.starts_with(p.as_str())) {
            return Err(format!(
                "`only` holds {p}, and no question's name starts with it"
            ));
        }
    }
    set.questions
        .retain(|q| only.iter().any(|p| q.name.starts_with(p.as_str())));
    let mut parts: Vec<&[u8]> = vec![set.hash.as_bytes(), b"only"];
    parts.extend(only.iter().map(|p| p.as_bytes()));
    set.hash = fingerprint(&parts);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("warren-bench-sample-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(d.join("s")).unwrap();
        d
    }

    #[test]
    fn a_sample_refuses_what_it_does_not_know_and_what_it_lacks() {
        let d = scratch("keys");
        let full = r#"{"questions": "../sqlc", "binds": "../binds.tsv", "only": ["a/"],
            "index_sets": ["all"], "cold": 1, "warm": 0, "statement_timeout": "60s"}"#;
        std::fs::write(d.join("s/sample.json"), full).unwrap();
        let s = read(&d, "s").unwrap();
        assert_eq!((s.cold, s.warm, s.only.len()), (1, 0, 1));
        assert!(s.questions.ends_with("s/../sqlc"));
        assert_eq!(s.index_sets, ["all"]);
        for (bad, want) in [
            (
                full.replace("\"cold\": 1", "\"cold\": 1, \"hot\": 2"),
                "unknown key `hot`",
            ),
            (full.replace("\"warm\": 0, ", ""), "no `warm`"),
            (full.replace("\"only\": [\"a/\"],", ""), "no `only`"),
            (full.replace("\"cold\": 1", "\"cold\": 0"), "at least 1"),
            (full.replace("[\"all\"]", "[]"), "at least one set"),
            (
                full.replace("\"60s\"", "60"),
                "`statement_timeout` is a string",
            ),
        ] {
            std::fs::write(d.join("s/sample.json"), bad).unwrap();
            let e = read(&d, "s").err().unwrap_or_default();
            assert!(e.contains(want), "{want}: {e}");
        }
        assert!(read(&d, "../s").err().unwrap().contains("a sample's name"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_keeps_its_prefixes_and_changes_the_fingerprint() {
        let q = |n: &str| crate::questions::Question {
            name: n.into(),
            binds: vec![],
            derived_from: None,
        };
        let mut set = QuestionSet {
            dir: PathBuf::from("x"),
            binds: PathBuf::from("x.tsv"),
            questions: vec![q("lines/set"), q("parts/category"), q("sets/nested")],
            hash: "h".into(),
            plain: Default::default(),
        };
        restrict(&mut set, &["lines/".into(), "parts/".into()]).unwrap();
        assert_eq!(set.questions.len(), 2);
        assert_ne!(set.hash, "h");
        assert!(restrict(&mut set, &["nope/".into()]).is_err());
    }
}
