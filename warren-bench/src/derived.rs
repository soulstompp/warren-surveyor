// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Derived statements: plain `.sql` files written for each target by some other program, each one
//! answering as a question of the question set.
//!
//! A family is a directory `DIR` holding one directory per target, named as the target is named,
//! and in each the same files `<question>.sql`, where `<question>` is the name of a question of the
//! set (for example `directive/combos/in_house.sql`). The statement is recorded under the name
//! `<family>/<selection of that question>`, the family being `DIR`'s own name, and its answer is
//! checked against that question's expected answer, with that question's bound values.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::digest::fingerprint;
use crate::questions::{files, leftover_directive, placeholders, Question, QuestionSet};

pub struct Family {
    pub name: String,
    pub dir: PathBuf,
    /// One per statement, named `<family>/<selection>`, each answering as its source question.
    pub questions: Vec<Question>,
    /// Per target, the statement text of each derived question.
    pub sql: BTreeMap<String, BTreeMap<String, String>>,
    /// Per target, a fingerprint of that target's files and their text.
    pub fingerprints: BTreeMap<String, String>,
}

/// Reads a family, refusing anything that does not bind one statement per target to one question.
pub fn load(dir: &Path, set: &QuestionSet) -> Result<Family, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| at(&"a derived family is a named directory"))?
        .to_string();
    if set.questions.iter().any(|q| q.spelling() == name) {
        return Err(at(&format!(
            "`{name}` is already the spelling of questions in the set; name the family apart"
        )));
    }
    let by_name: BTreeMap<&str, &Question> = set
        .questions
        .iter()
        .filter(|q| q.derived_from.is_none())
        .map(|q| (q.name.as_str(), q))
        .collect();

    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| at(&e))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| at(&e))?;
    entries.sort();
    let mut texts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for p in &entries {
        let target = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !p.is_dir() {
            return Err(at(&format!(
                "`{target}` is a file; want one directory per target"
            )));
        }
        let mut by_question = BTreeMap::new();
        for f in files(p)? {
            let rel = f.to_string_lossy().to_string();
            let source = rel.strip_suffix(".sql").ok_or_else(|| {
                at(&format!(
                    "{target}/{rel}: only .sql files belong in a family"
                ))
            })?;
            if !by_name.contains_key(source) {
                return Err(at(&format!(
                    "{target}/{rel}: `{source}` is not a question of the set"
                )));
            }
            let text = fs::read_to_string(p.join(&f)).map_err(|e| at(&e))?;
            by_question.insert(source.to_string(), text);
        }
        texts.insert(target, by_question);
    }
    let Some((first, first_texts)) = texts.iter().next() else {
        return Err(at(&"no target directories"));
    };
    let names: BTreeSet<&String> = first_texts.keys().collect();
    if names.is_empty() {
        return Err(at(&format!("{first}: no statements")));
    }
    for (t, m) in &texts {
        let here: BTreeSet<&String> = m.keys().collect();
        if here != names {
            return Err(at(&format!(
                "the targets hold different statements: {first} only {:?}; {t} only {:?}",
                names.difference(&here).collect::<Vec<_>>(),
                here.difference(&names).collect::<Vec<_>>()
            )));
        }
    }

    let mut questions = Vec::new();
    let mut derived_name: BTreeMap<String, String> = BTreeMap::new();
    for source in &names {
        let q = by_name[source.as_str()];
        let n = format!("{name}/{}", q.selection());
        if let Some(other) = derived_name.insert(n.clone(), source.to_string()) {
            return Err(at(&format!(
                "{other} and {source} would both be recorded as {n}"
            )));
        }
        questions.push(Question {
            name: n,
            binds: q.binds.clone(),
            derived_from: Some(q.name.clone()),
        });
    }

    let mut sql = BTreeMap::new();
    let mut fingerprints = BTreeMap::new();
    for (t, m) in &texts {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut out = BTreeMap::new();
        for q in &questions {
            let source = q.answers_as();
            let text = &m[source];
            if let Some(d) = leftover_directive(text) {
                return Err(at(&format!("{t}/{source}.sql: holds the directive {d}")));
            }
            let want: BTreeSet<u32> = (1..=q.binds.len() as u32).collect();
            if placeholders(text) != want {
                return Err(at(&format!(
                    "{t}/{source}.sql: placeholders {:?}, but {source} binds {} value(s)",
                    placeholders(text),
                    q.binds.len()
                )));
            }
            parts.push(source.as_bytes());
            parts.push(text.as_bytes());
            out.insert(q.name.clone(), text.clone());
        }
        fingerprints.insert(t.clone(), fingerprint(&parts));
        sql.insert(t.clone(), out);
    }
    Ok(Family {
        name,
        dir: dir.to_path_buf(),
        questions,
        sql,
        fingerprints,
    })
}

/// Adds a family's statements to the question set, refusing a name the set already holds.
pub fn extend(set: &mut QuestionSet, family: &Family) -> Result<(), String> {
    for q in &family.questions {
        if set.questions.iter().any(|x| x.name == q.name) {
            return Err(format!(
                "{}: {} is already a question of the set",
                family.dir.display(),
                q.name
            ));
        }
    }
    set.questions.extend(family.questions.iter().cloned());
    Ok(())
}

/// A family must hold a directory for every target of the run, and none for any other.
pub fn same_targets(family: &Family, targets: &BTreeSet<String>) -> Result<(), String> {
    let have: BTreeSet<String> = family.sql.keys().cloned().collect();
    if &have != targets {
        return Err(format!(
            "{}: targets without a directory {:?}; directories without a target {:?}",
            family.dir.display(),
            targets.difference(&have).collect::<Vec<_>>(),
            have.difference(targets).collect::<Vec<_>>()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("warren-bench-derived-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn set() -> QuestionSet {
        let q = |n: &str, b: &[&str]| Question {
            name: n.into(),
            binds: b.iter().map(|s| s.to_string()).collect(),
            derived_from: None,
        };
        QuestionSet {
            dir: PathBuf::from("sqlc"),
            binds: PathBuf::from("binds.tsv"),
            questions: vec![
                q("directive/scopes/town", &[]),
                q("predicate/scopes/town", &[]),
                q("directive/text/set_parts", &["75053-1"]),
            ],
            hash: "h".into(),
            plain: Default::default(),
        }
    }

    #[test]
    fn a_derived_statement_answers_as_the_question_it_names() {
        let d = scratch("names").join("derived");
        for t in ["t1", "t2"] {
            put(&d, &format!("{t}/directive/scopes/town.sql"), "SELECT 1");
            put(
                &d,
                &format!("{t}/directive/text/set_parts.sql"),
                "SELECT 1 WHERE $1 = $1",
            );
        }
        let f = load(&d, &set()).unwrap();
        assert_eq!(f.name, "derived");
        let names: Vec<(&str, &str, usize)> = f
            .questions
            .iter()
            .map(|q| (q.name.as_str(), q.answers_as(), q.binds.len()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("derived/scopes/town", "directive/scopes/town", 0),
                ("derived/text/set_parts", "directive/text/set_parts", 1),
            ]
        );
        assert_eq!(f.questions[0].spelling(), "derived");
        assert_eq!(f.sql["t2"]["derived/scopes/town"], "SELECT 1");
    }

    #[test]
    fn every_target_must_hold_the_same_statements() {
        let d = scratch("same").join("derived");
        put(&d, "t1/directive/scopes/town.sql", "SELECT 1");
        put(&d, "t1/predicate/scopes/town.sql", "SELECT 1");
        put(&d, "t2/directive/scopes/town.sql", "SELECT 1");
        let e = load(&d, &set()).err().unwrap();
        assert!(e.contains("different statements"), "{e}");
    }

    #[test]
    fn a_statement_must_name_a_question_of_the_set() {
        let d = scratch("unknown").join("derived");
        put(&d, "t1/directive/scopes/castle.sql", "SELECT 1");
        let e = load(&d, &set()).err().unwrap();
        assert!(e.contains("not a question of the set"), "{e}");
    }

    #[test]
    fn two_statements_may_not_share_a_recorded_name() {
        let d = scratch("clash").join("derived");
        put(&d, "t1/directive/scopes/town.sql", "SELECT 1");
        put(&d, "t1/predicate/scopes/town.sql", "SELECT 1");
        let e = load(&d, &set()).err().unwrap();
        assert!(e.contains("would both be recorded"), "{e}");
    }

    #[test]
    fn a_family_may_not_take_the_name_of_a_spelling() {
        let d = scratch("spelling").join("directive");
        put(&d, "t1/directive/scopes/town.sql", "SELECT 1");
        let e = load(&d, &set()).err().unwrap();
        assert!(e.contains("already the spelling"), "{e}");
    }

    #[test]
    fn placeholders_must_match_the_bound_values() {
        let d = scratch("binds").join("derived");
        put(&d, "t1/directive/scopes/town.sql", "SELECT $1");
        let e = load(&d, &set()).err().unwrap();
        assert!(e.contains("placeholders"), "{e}");
        let d = scratch("binds2").join("derived");
        put(&d, "t1/directive/text/set_parts.sql", "SELECT 1");
        assert!(load(&d, &set()).is_err());
    }

    #[test]
    fn the_family_and_the_targets_must_match_both_ways() {
        let d = scratch("targets").join("derived");
        put(&d, "t1/directive/scopes/town.sql", "SELECT 1");
        put(&d, "t2/directive/scopes/town.sql", "SELECT 1");
        let f = load(&d, &set()).unwrap();
        let ts = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        assert!(same_targets(&f, &ts(&["t1", "t2"])).is_ok());
        assert!(same_targets(&f, &ts(&["t1"])).is_err());
        assert!(same_targets(&f, &ts(&["t1", "t2", "t3"])).is_err());
    }

    #[test]
    fn a_family_is_added_once() {
        let d = scratch("extend").join("derived");
        put(&d, "t1/directive/scopes/town.sql", "SELECT 1");
        let f = load(&d, &set()).unwrap();
        let mut s = set();
        extend(&mut s, &f).unwrap();
        assert_eq!(s.questions.len(), 4);
        assert_eq!(s.questions[3].answers_as(), "directive/scopes/town");
        assert!(extend(&mut s, &f).is_err());
    }

    #[test]
    fn a_statement_changed_changes_its_targets_fingerprint() {
        let d = scratch("print").join("derived");
        put(&d, "t1/directive/scopes/town.sql", "SELECT 1");
        put(&d, "t2/directive/scopes/town.sql", "SELECT 1");
        let a = load(&d, &set()).unwrap();
        assert_eq!(a.fingerprints["t1"], a.fingerprints["t2"]);
        put(&d, "t2/directive/scopes/town.sql", "SELECT 2");
        let b = load(&d, &set()).unwrap();
        assert_eq!(a.fingerprints["t1"], b.fingerprints["t1"]);
        assert_ne!(b.fingerprints["t1"], b.fingerprints["t2"]);
    }
}
