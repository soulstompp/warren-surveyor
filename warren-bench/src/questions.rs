// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The question set: every `.sql` file under the questions directory, sent as written; or every
//! `.sqlc` template under it, composed once per target with `cargo sqlc`, after writing the one
//! template that names the target's table.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{debug, info};

use crate::digest::fingerprint;

/// The template every question reads its table through. It is not in the questions directory: it
/// is written for each target, naming that target's table, before composing.
pub const TABLE_TEMPLATE: &str = "target/table";

#[derive(Clone, Debug)]
pub struct Question {
    /// The file's path under the questions directory, without `.sql` or `.sqlc`; for a derived
    /// statement, its family and the selection of the question it was derived from.
    pub name: String,
    /// Bind values in the order of their placeholders.
    pub binds: Vec<String>,
    /// For a derived statement, the question it was derived from.
    pub derived_from: Option<String>,
}

impl Question {
    /// The question whose expected answer this one must give: itself, or the question a derived
    /// statement was derived from.
    pub fn answers_as(&self) -> &str {
        self.derived_from.as_deref().unwrap_or(&self.name)
    }

    /// The first path component: how the question is spelled.
    pub fn spelling(&self) -> &str {
        self.name.split('/').next().unwrap_or("")
    }

    /// The rest of the path: which selection it is.
    pub fn selection(&self) -> &str {
        self.name.split_once('/').map_or("", |(_, r)| r)
    }
}

pub struct QuestionSet {
    pub dir: PathBuf,
    /// The binds file the questions were read with.
    pub binds: PathBuf,
    pub questions: Vec<Question>,
    /// A fingerprint of every file of the questions directory and every bind value.
    pub hash: String,
    /// Per question, its text, where the questions are plain `.sql` files sent as written; empty
    /// where they are templates.
    pub plain: BTreeMap<String, String>,
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            walk(&p, base, out)?;
        } else {
            out.push(p.strip_prefix(base).unwrap().to_path_buf());
        }
    }
    Ok(())
}

pub(crate) fn files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    Ok(out)
}

fn with_ext(dir: &Path, ext: &str) -> Result<BTreeSet<String>, String> {
    Ok(files(dir)?
        .into_iter()
        .filter_map(|p| {
            let s = p.to_string_lossy().to_string();
            s.strip_suffix(&format!(".{ext}")).map(str::to_string)
        })
        .collect())
}

/// Whether the template `name` declares an open slot, `:compose(@slot)`, outside a comment: a
/// shape, which the composer emits no file for and which is no question.
fn is_shape(dir: &Path, name: &str) -> Result<bool, String> {
    let path = dir.join(format!("{name}.sqlc"));
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(strip_comments(&text)
        .split(":compose(")
        .skip(1)
        .any(|piece| piece.trim_start().starts_with('@')))
}

/// The templates under `dir` that are not shapes: each one composes to a file of its own.
fn composed_templates(dir: &Path) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for name in with_ext(dir, "sqlc")? {
        if !is_shape(dir, &name)? {
            out.insert(name);
        }
    }
    Ok(out)
}

/// Reads `question  name  value` lines. Values are given to each question in the order of the
/// names sorted alphabetically, which is how the composer numbers its placeholders.
fn read_binds(path: &Path) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 3 {
            return Err(format!(
                "{}:{}: want question, name, value",
                path.display(),
                i + 1
            ));
        }
        if out
            .entry(f[0].to_string())
            .or_default()
            .insert(f[1].to_string(), f[2].to_string())
            .is_some()
        {
            return Err(format!(
                "{}:{}: {} {} bound twice",
                path.display(),
                i + 1,
                f[0],
                f[1]
            ));
        }
    }
    Ok(out)
}

/// Strips `#` comments the way the composer does: from the `#` to the end of the line.
fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The names in `:bind(name)` in a template and every template it composes.
fn bind_names(
    dir: &Path,
    name: &str,
    seen: &mut BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    if !seen.insert(name.to_string()) || name == TABLE_TEMPLATE {
        return Ok(out);
    }
    let path = dir.join(format!("{name}.sqlc"));
    let text =
        strip_comments(&fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?);
    for piece in text.split(":bind(").skip(1) {
        if let Some((n, _)) = piece.split_once(')') {
            out.insert(n.trim().to_string());
        }
    }
    for word in text.split(|c: char| !(c.is_ascii_alphanumeric() || "_/.-".contains(c))) {
        if let Some(child) = word.strip_suffix(".sqlc") {
            out.extend(bind_names(dir, child, seen)?);
        }
    }
    Ok(out)
}

/// One question per name, each with its bound values in the order of their names.
fn questions_of(
    names: &BTreeSet<String>,
    binds: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<Question> {
    names
        .iter()
        .map(|n| Question {
            name: n.clone(),
            binds: binds
                .get(n)
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default(),
            derived_from: None,
        })
        .collect()
}

/// A fingerprint of every file under `dir`, by path and content, and of the binds file.
fn fingerprint_of(dir: &Path, binds_file: &Path) -> Result<String, String> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for p in files(dir)? {
        parts.push(p.to_string_lossy().as_bytes().to_vec());
        parts.push(fs::read(dir.join(&p)).map_err(|e| format!("{}: {e}", p.display()))?);
    }
    parts.push(fs::read(binds_file).map_err(|e| format!("{}: {e}", binds_file.display()))?);
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    Ok(fingerprint(&refs))
}

impl QuestionSet {
    /// Whether the questions are plain `.sql` files, sent as written.
    pub fn is_plain(&self) -> bool {
        !self.plain.is_empty()
    }

    /// Reads the questions directory: plain `.sql` files, or `.sqlc` templates, never both.
    pub fn load(dir: &Path, binds_file: &Path) -> Result<Self, String> {
        let plain = with_ext(dir, "sql")?;
        let templates = with_ext(dir, "sqlc")?;
        match (plain.is_empty(), templates.is_empty()) {
            (false, false) => Err(format!(
                "{}: holds both .sql files and .sqlc templates; a questions directory is one or the other",
                dir.display()
            )),
            (false, true) => Self::load_plain(dir, binds_file, plain),
            _ => Self::load_templates(dir, binds_file),
        }
    }

    /// Plain questions: each `.sql` file is a question, its text sent as written. Its bound values
    /// are its `$1`, `$2`, … in the order of their names in the binds file.
    fn load_plain(dir: &Path, binds_file: &Path, names: BTreeSet<String>) -> Result<Self, String> {
        let binds = read_binds(binds_file)?;
        for q in binds.keys() {
            if !names.contains(q) {
                return Err(format!(
                    "{}: binds a value for {q}, which is not a question",
                    binds_file.display()
                ));
            }
        }
        let mut texts = BTreeMap::new();
        for n in &names {
            let path = dir.join(format!("{n}.sql"));
            let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if text.trim().is_empty() {
                return Err(format!("{}: empty", path.display()));
            }
            let declared = binds.get(n).map_or(0, BTreeMap::len) as u32;
            let used = placeholders(&text);
            if used != (1..=declared).collect::<BTreeSet<u32>>() {
                return Err(format!(
                    "{}: placeholders {used:?}, but {} declares {declared} bound value(s)",
                    path.display(),
                    binds_file.display()
                ));
            }
            texts.insert(n.clone(), text);
        }
        Ok(QuestionSet {
            dir: dir.to_path_buf(),
            binds: binds_file.to_path_buf(),
            questions: questions_of(&names, &binds),
            hash: fingerprint_of(dir, binds_file)?,
            plain: texts,
        })
    }

    fn load_templates(dir: &Path, binds_file: &Path) -> Result<Self, String> {
        let names = composed_templates(dir)?;
        if names.contains(TABLE_TEMPLATE) {
            return Err(format!(
                "{}: {TABLE_TEMPLATE}.sqlc is written per target and must not be in the questions directory",
                dir.display()
            ));
        }
        if names.is_empty() {
            return Err(format!(
                "{}: no .sql questions and no .sqlc templates",
                dir.display()
            ));
        }
        let binds = read_binds(binds_file)?;
        for q in binds.keys() {
            if !names.contains(q) {
                return Err(format!(
                    "{}: binds a value for {q}, which is not a template",
                    binds_file.display()
                ));
            }
        }
        for n in &names {
            let used = bind_names(dir, n, &mut BTreeSet::new())?;
            let declared: BTreeSet<String> = binds
                .get(n)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            if used != declared {
                return Err(format!(
                    "{n}: the templates bind {used:?}, and {} declares {declared:?}",
                    binds_file.display()
                ));
            }
        }
        Ok(QuestionSet {
            dir: dir.to_path_buf(),
            binds: binds_file.to_path_buf(),
            questions: questions_of(&names, &binds),
            hash: fingerprint_of(dir, binds_file)?,
            plain: BTreeMap::new(),
        })
    }
}

/// The composed statements of every question for one target.
#[derive(Clone)]
pub struct Composed {
    pub dir: PathBuf,
    /// The statement sent to the target, per question.
    pub sql: BTreeMap<String, String>,
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    for p in files(from)? {
        let dst = to.join(&p);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        fs::copy(from.join(&p), &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    Ok(())
}

fn sqlc(source: &Path, target: &Path, verify: bool) -> Result<String, String> {
    let mut cmd = Command::new("cargo");
    cmd.args(["sqlc", "compose", "--source"])
        .arg(source)
        .arg("--target")
        .arg(target)
        .arg("--skip-prepare");
    if verify {
        cmd.arg("--verify");
    }
    let out = cmd
        .output()
        .map_err(|e| format!("running cargo sqlc: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        return Err(format!(
            "cargo sqlc compose{} failed ({}): {text}",
            if verify { " --verify" } else { "" },
            out.status
        ));
    }
    Ok(text)
}

/// Placeholders `$1`, `$2`, … in a composed statement.
pub(crate) fn placeholders(sql: &str) -> BTreeSet<u32> {
    let b = sql.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                if let Ok(n) = sql[i + 1..j].parse() {
                    out.insert(n);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Any directive text left in a composed statement.
pub(crate) fn leftover_directive(sql: &str) -> Option<&'static str> {
    [
        ":compose(",
        ":define(",
        ":union(",
        ":intersect(",
        ":except(",
        ":count(",
        ":bind(",
    ]
    .into_iter()
    .find(|d| sql.contains(d))
}

/// Composes `src` into `out`, verifies it with the same flags, and checks the templates and
/// composed files against each other in both directions.
fn compose_into(src: &Path, out: &Path) -> Result<(), String> {
    let composed = sqlc(src, out, false)?;
    debug!(output = %composed.trim(), "composed");
    let verified = sqlc(src, out, true)?;
    info!(dir = %out.display(), verify = %verified.trim().lines().last().unwrap_or(""), "composed and verified");

    let templates = composed_templates(src)?;
    let emitted = with_ext(out, "sql")?;
    let other: Vec<PathBuf> = files(out)?
        .into_iter()
        .filter(|p| p.extension().is_none_or(|e| e != "sql"))
        .collect();
    if templates != emitted || !other.is_empty() {
        return Err(format!(
            "templates and composed files differ: templates only {:?}; composed only {:?}; other files {:?}",
            templates.difference(&emitted).collect::<Vec<_>>(),
            emitted.difference(&templates).collect::<Vec<_>>(),
            other
        ));
    }
    Ok(())
}

/// A composed question's text, refused if a directive survived.
fn composed_text(out: &Path, q: &Question) -> Result<(PathBuf, String), String> {
    let path = out.join(format!("{}.sql", q.name));
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(d) = leftover_directive(&text) {
        return Err(format!(
            "{}: the directive {d} survived composition",
            path.display()
        ));
    }
    Ok((path, text))
}

/// Plain questions for one target: each text written to `<work>/sql/<name>.sql`, and sent as
/// written.
fn plain(set: &QuestionSet, work: &Path) -> Result<Composed, String> {
    let out = work.join("sql");
    if out.exists() {
        fs::remove_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    let mut sql = BTreeMap::new();
    for q in set.questions.iter().filter(|q| q.derived_from.is_none()) {
        let text = set
            .plain
            .get(&q.name)
            .ok_or_else(|| format!("{}: no text", q.name))?;
        let ph = placeholders(text);
        let want: BTreeSet<u32> = (1..=q.binds.len() as u32).collect();
        if ph != want {
            return Err(format!(
                "{}: placeholders {ph:?}, but {} bound value(s) are declared",
                q.name,
                q.binds.len()
            ));
        }
        let path = out.join(format!("{}.sql", q.name));
        fs::create_dir_all(path.parent().unwrap())
            .map_err(|e| format!("{}: {e}", out.display()))?;
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        sql.insert(q.name.clone(), text.clone());
    }
    info!(dir = %out.display(), questions = sql.len(), "plain questions, sent as written");
    Ok(Composed { dir: out, sql })
}

/// Composes the question set for one table, in `work`. The questions directory is copied, the
/// table template is written naming `table`, the copy is composed, then verified, and the
/// templates and composed files are checked against each other in both directions. Plain questions
/// are written to `work` as they are, and sent as written.
pub fn compose(set: &QuestionSet, table: &str, work: &Path) -> Result<Composed, String> {
    if set.is_plain() {
        return plain(set, work);
    }
    let src = work.join("src");
    let out = work.join("sql");
    if src.exists() {
        fs::remove_dir_all(&src).map_err(|e| format!("{}: {e}", src.display()))?;
    }
    copy_tree(&set.dir, &src)?;
    let fill = src.join(format!("{TABLE_TEMPLATE}.sqlc"));
    fs::create_dir_all(fill.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&fill, format!("SELECT * FROM {table}\n"))
        .map_err(|e| format!("{}: {e}", fill.display()))?;
    compose_into(&src, &out)?;

    let mut sql = BTreeMap::new();
    for q in set.questions.iter().filter(|q| q.derived_from.is_none()) {
        let (path, text) = composed_text(&out, q)?;
        let ph = placeholders(&text);
        let want: BTreeSet<u32> = (1..=q.binds.len() as u32).collect();
        if ph != want {
            return Err(format!(
                "{}: placeholders {:?}, but {} bound value(s) are declared",
                path.display(),
                ph,
                q.binds.len()
            ));
        }
        sql.insert(q.name.clone(), text);
    }
    Ok(Composed { dir: out, sql })
}

/// Composed text that differs between two tables anywhere but in the table's own name.
pub fn differs_beyond_table(
    a: &Composed,
    a_table: &str,
    b: &Composed,
    b_table: &str,
) -> Vec<String> {
    let mark = "\u{1}TABLE\u{1}";
    a.sql
        .iter()
        .filter(|(q, text)| {
            b.sql
                .get(*q)
                .is_none_or(|t| text.replace(a_table, mark) != t.replace(b_table, mark))
        })
        .map(|(q, _)| q.clone())
        .collect()
}

/// A statement literal for a bind value.
pub fn literal(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_found_by_number() {
        assert_eq!(
            placeholders("SELECT $2, $10 FROM t WHERE x = $1"),
            [1, 2, 10].into_iter().collect()
        );
        assert!(placeholders("SELECT 1").is_empty());
    }

    #[test]
    fn a_surviving_directive_is_found() {
        assert_eq!(
            leftover_directive("SELECT * FROM (x) :except(a.sqlc)"),
            Some(":except(")
        );
        assert_eq!(leftover_directive("SELECT 1 EXCEPT SELECT 2"), None);
    }

    #[test]
    fn comments_are_stripped_to_the_end_of_the_line() {
        assert_eq!(
            strip_comments("SELECT 1 # x\n:bind(a) # :bind(b)"),
            "SELECT 1 \n:bind(a) "
        );
    }

    #[test]
    fn a_template_with_an_open_slot_is_a_shape_and_no_question() {
        let dir = std::env::temp_dir().join(format!("warren-bench-shapes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("shapes")).unwrap();
        let write = |name: &str, text: &str| fs::write(dir.join(name), text).unwrap();
        // a slot named in a comment declares nothing
        write(
            "shapes/count.sqlc",
            "# fills :compose(@rows)\nSELECT count(*) FROM (\n    :compose( @rows)\n) r\n",
        );
        write(
            "rows.sqlc",
            "# :compose(@not_a_slot)\nSELECT :bind(n) AS n\n",
        );
        write(
            "counted.sqlc",
            ":compose(shapes/count.sqlc, @rows = rows.sqlc)\n",
        );
        let binds = dir.join("binds.tsv");
        fs::write(&binds, "question\tname\tvalue\nrows\tn\t1\ncounted\tn\t1\n").unwrap();
        let set = QuestionSet::load(&dir, &binds).unwrap();
        let names: Vec<&str> = set.questions.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, ["counted", "rows"]);
        assert!(is_shape(&dir, "shapes/count").unwrap());
        assert!(!is_shape(&dir, "rows").unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A scratch directory of its own for one test.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("warren-bench-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(dir: &Path, name: &str, text: &str) {
        let p = dir.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn plain_questions_are_sent_as_written() {
        let dir = scratch("plain");
        put(&dir, "q/a.sql", "SELECT 1 AS n\n");
        put(
            &dir,
            "q/sets/by_year.sql",
            "SELECT year FROM s WHERE year > $1\n",
        );
        put(&dir, "q/README.md", "not a question\n");
        put(
            &dir,
            "binds.tsv",
            "question\tname\tvalue\nsets/by_year\tfrom\t2000\n",
        );
        let set = QuestionSet::load(&dir.join("q"), &dir.join("binds.tsv")).unwrap();
        assert!(set.is_plain());
        let names: Vec<&str> = set.questions.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(names, ["a", "sets/by_year"]);
        assert_eq!(set.questions[1].binds, ["2000"]);
        let c = compose(&set, "s", &dir.join("work")).unwrap();
        assert_eq!(
            c.sql["sets/by_year"],
            "SELECT year FROM s WHERE year > $1\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("work/sql/a.sql")).unwrap(),
            "SELECT 1 AS n\n"
        );
        // the fingerprint follows every file, the README too
        let before = set.hash.clone();
        put(&dir, "q/README.md", "changed\n");
        let after = QuestionSet::load(&dir.join("q"), &dir.join("binds.tsv"))
            .unwrap()
            .hash;
        assert_ne!(before, after);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_of_plain_questions_and_templates_is_refused() {
        let dir = scratch("mixed");
        put(&dir, "q/a.sql", "SELECT 1\n");
        put(&dir, "q/b.sqlc", "SELECT 2\n");
        put(&dir, "binds.tsv", "question\tname\tvalue\n");
        let e = QuestionSet::load(&dir.join("q"), &dir.join("binds.tsv"))
            .err()
            .unwrap_or_default();
        assert!(e.contains("both .sql files and .sqlc templates"), "{e}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_plain_question_whose_placeholders_are_not_its_bound_values_is_refused() {
        let dir = scratch("plain-binds");
        put(&dir, "q/a.sql", "SELECT $1, $3\n");
        put(
            &dir,
            "binds.tsv",
            "question\tname\tvalue\na\tx\t1\na\ty\t2\n",
        );
        let e = QuestionSet::load(&dir.join("q"), &dir.join("binds.tsv"))
            .err()
            .unwrap_or_default();
        assert!(e.contains("placeholders {1, 3}"), "{e}");
        // a value bound for no question
        put(&dir, "q/a.sql", "SELECT 1\n");
        put(&dir, "binds.tsv", "question\tname\tvalue\nb\tx\t1\n");
        let e = QuestionSet::load(&dir.join("q"), &dir.join("binds.tsv"))
            .err()
            .unwrap_or_default();
        assert!(e.contains("which is not a question"), "{e}");
        // and none bound where the text has a placeholder
        put(&dir, "q/a.sql", "SELECT $1\n");
        put(&dir, "binds.tsv", "question\tname\tvalue\n");
        assert!(QuestionSet::load(&dir.join("q"), &dir.join("binds.tsv")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn literals_double_their_quotes() {
        assert_eq!(literal("75053-1"), "'75053-1'");
        assert_eq!(literal("it's"), "'it''s'");
    }

    #[test]
    fn spelling_and_selection_split_the_name() {
        let q = Question {
            name: "predicate/combos/golden_age".into(),
            binds: vec![],
            derived_from: None,
        };
        assert_eq!(q.spelling(), "predicate");
        assert_eq!(q.selection(), "combos/golden_age");
    }
}
