// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every file this program writes for another program to read is Parquet, with each column's type
//! declared beside its name. A value that does not fit its column's type is an error, and a
//! missing value is null. Any such file exports to tab-separated text, and the text reads back
//! to the same values.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Float64Type, Int64Type};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

/// The type of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Int,
    Float,
    Bool,
}

impl Kind {
    fn data_type(self) -> DataType {
        match self {
            Kind::Text => DataType::Utf8,
            Kind::Int => DataType::Int64,
            Kind::Float => DataType::Float64,
            Kind::Bool => DataType::Boolean,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Int => "an integer",
            Kind::Float => "a number",
            Kind::Bool => "true or false",
        }
    }
}

/// The columns of one kind of file, each with its type, in order.
pub struct Declared {
    pub name: &'static str,
    pub columns: &'static [(&'static str, Kind)],
    /// The columns an export sorts by first; the rest follow, so the order depends on the rows
    /// alone and never on the order they were written in.
    pub key: &'static [&'static str],
}

impl Declared {
    pub fn names(&self) -> Vec<&'static str> {
        self.columns.iter().map(|(n, _)| *n).collect()
    }

    fn schema(&self) -> Arc<Schema> {
        Arc::new(Schema::new(
            self.columns
                .iter()
                .map(|(n, k)| Field::new(*n, k.data_type(), true))
                .collect::<Vec<_>>(),
        ))
    }

    fn same_columns(&self, schema: &Schema) -> bool {
        schema.fields().len() == self.columns.len()
            && schema
                .fields()
                .iter()
                .zip(self.columns)
                .all(|(f, (n, k))| f.name() == n && *f.data_type() == k.data_type())
    }

    fn describe(schema: &Schema) -> String {
        schema
            .fields()
            .iter()
            .map(|f| format!("{} {}", f.name(), f.data_type()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One value, as stored.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Value {
    /// The value's text: empty for null. Numbers are written in the shortest form that reads back
    /// to the same number.
    pub fn text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Text(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Float(x) => x.to_string(),
            Value::Bool(b) => b.to_string(),
        }
    }

    /// Reads a value of `kind` from its text: the empty text is null.
    fn parse(text: &str, kind: Kind, column: &str) -> Result<Value, String> {
        if text.is_empty() {
            return Ok(Value::Null);
        }
        let bad = || format!("column {column}: `{text}` is not {}", kind.word());
        Ok(match kind {
            Kind::Text => Value::Text(text.to_string()),
            Kind::Int => Value::Int(text.parse().map_err(|_| bad())?),
            Kind::Float => Value::Float(text.parse().map_err(|_| bad())?),
            Kind::Bool => Value::Bool(match text {
                "true" => true,
                "false" => false,
                _ => return Err(bad()),
            }),
        })
    }

    fn order(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Less,
            (_, Value::Null) => Ordering::Greater,
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

/// Rows read from a file, each a map from column name to the value's text (empty for null).
#[derive(Debug)]
pub struct Table {
    pub rows: Vec<BTreeMap<String, String>>,
}

fn array(values: Vec<&Value>, kind: Kind) -> ArrayRef {
    match kind {
        Kind::Text => Arc::new(
            values
                .iter()
                .map(|v| match v {
                    Value::Text(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect::<StringArray>(),
        ),
        Kind::Int => Arc::new(
            values
                .iter()
                .map(|v| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .collect::<Int64Array>(),
        ),
        Kind::Float => Arc::new(
            values
                .iter()
                .map(|v| match v {
                    Value::Float(x) => Some(*x),
                    _ => None,
                })
                .collect::<Float64Array>(),
        ),
        Kind::Bool => Arc::new(
            values
                .iter()
                .map(|v| match v {
                    Value::Bool(b) => Some(*b),
                    _ => None,
                })
                .collect::<BooleanArray>(),
        ),
    }
}

fn value(a: &ArrayRef, kind: Kind, i: usize) -> Value {
    if a.is_null(i) {
        return Value::Null;
    }
    match kind {
        Kind::Text => Value::Text(a.as_string::<i32>().value(i).to_string()),
        Kind::Int => Value::Int(a.as_primitive::<Int64Type>().value(i)),
        Kind::Float => Value::Float(a.as_primitive::<Float64Type>().value(i)),
        Kind::Bool => Value::Bool(a.as_boolean().value(i)),
    }
}

/// Reads rows given as text, one text per column: the empty text is null, and any other text
/// must read as its column's type.
pub fn parse_rows(d: &Declared, rows: &[Vec<String>]) -> Result<Vec<Vec<Value>>, String> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            if r.len() != d.columns.len() {
                return Err(format!(
                    "row {}: {} values for {} columns",
                    i + 1,
                    r.len(),
                    d.columns.len()
                ));
            }
            r.iter()
                .zip(d.columns)
                .map(|(t, (n, k))| Value::parse(t, *k, n))
                .collect()
        })
        .collect()
}

/// Writes rows given as text (see `parse_rows`). The file appears whole or not at all.
pub fn write(path: &Path, d: &Declared, rows: &[Vec<String>]) -> Result<(), String> {
    let values = parse_rows(d, rows).map_err(|e| format!("{}: {e}", path.display()))?;
    write_values(path, d, &values)
}

pub fn write_values(path: &Path, d: &Declared, rows: &[Vec<Value>]) -> Result<(), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let schema = d.schema();
    let arrays = d
        .columns
        .iter()
        .enumerate()
        .map(|(c, (_, k))| array(rows.iter().map(|r| &r[c]).collect(), *k))
        .collect();
    let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|e| at(&e))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    let file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut w = ArrowWriter::try_new(file, schema, Some(props)).map_err(|e| at(&e))?;
    w.write(&batch).map_err(|e| at(&e))?;
    w.close().map_err(|e| at(&e))?;
    fs::rename(&tmp, path).map_err(|e| at(&e))
}

/// The Parquet files at `path`: the file itself, or every `.parquet` file in the directory, in
/// name order.
fn files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut out: Vec<PathBuf> = fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    out.sort();
    if out.is_empty() {
        return Err(format!("{}: no .parquet files", path.display()));
    }
    Ok(out)
}

/// Reads a file of declared columns, or a directory of them, refusing any file whose columns are
/// not exactly the declared ones.
pub fn read_values(path: &Path, d: &Declared) -> Result<Vec<Vec<Value>>, String> {
    let mut rows = Vec::new();
    for f in files(path)? {
        let at = |e: &dyn std::fmt::Display| format!("{}: {e}", f.display());
        let file = File::open(&f).map_err(|e| at(&e))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| at(&e))?;
        if !d.same_columns(builder.schema()) {
            return Err(at(&format!(
                "its columns are {}, and a {} file's are {}",
                Declared::describe(builder.schema()),
                d.name,
                Declared::describe(&d.schema())
            )));
        }
        for batch in builder.build().map_err(|e| at(&e))? {
            let batch = batch.map_err(|e| at(&e))?;
            for i in 0..batch.num_rows() {
                rows.push(
                    d.columns
                        .iter()
                        .enumerate()
                        .map(|(c, (_, k))| value(batch.column(c), *k, i))
                        .collect(),
                );
            }
        }
    }
    Ok(rows)
}

pub fn read(path: &Path, d: &Declared) -> Result<Table, String> {
    let rows = read_values(path, d)?;
    Ok(Table {
        rows: rows
            .into_iter()
            .map(|r| {
                d.columns
                    .iter()
                    .zip(r)
                    .map(|((n, _), v)| (n.to_string(), v.text()))
                    .collect()
            })
            .collect(),
    })
}

/// Which declared kind of file the one at `path` is, by its columns.
pub fn recognise<'a>(path: &Path, known: &[&'a Declared]) -> Result<&'a Declared, String> {
    let f = files(path)?.remove(0);
    let file = File::open(&f).map_err(|e| format!("{}: {e}", f.display()))?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| format!("{}: {e}", f.display()))?;
    known
        .iter()
        .copied()
        .find(|d| d.same_columns(builder.schema()))
        .ok_or_else(|| {
            format!(
                "{}: its columns ({}) are not those of any file this program writes",
                f.display(),
                Declared::describe(builder.schema())
            )
        })
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        out.push(match chars.next() {
            Some('\\') => '\\',
            Some('t') => '\t',
            Some('n') => '\n',
            Some('r') => '\r',
            other => {
                return Err(format!(
                    "`\\{}` is not an escape",
                    other.map(String::from).unwrap_or_default()
                ))
            }
        });
    }
    Ok(out)
}

/// The rows sorted by the declared key and then by every other column, so the order is the same
/// however the rows were written.
fn sorted(d: &Declared, mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let names = d.names();
    let mut order: Vec<usize> = d
        .key
        .iter()
        .map(|k| {
            names
                .iter()
                .position(|n| n == k)
                .expect("a key column is a column")
        })
        .collect();
    let rest: Vec<usize> = (0..names.len()).filter(|c| !order.contains(c)).collect();
    order.extend(rest);
    rows.sort_by(|a, b| {
        order
            .iter()
            .map(|&c| a[c].order(&b[c]))
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    });
    rows
}

/// The rows as tab-separated text: a header line of column names, then one line per row. A null
/// is an empty field; a backslash, tab, newline or carriage return inside a text is written `\\`,
/// `\t`, `\n` or `\r`.
pub fn to_tsv(d: &Declared, rows: Vec<Vec<Value>>) -> Result<String, String> {
    let mut out = d.names().join("\t");
    out.push('\n');
    for r in sorted(d, rows) {
        let mut fields = Vec::with_capacity(r.len());
        for (v, (n, _)) in r.iter().zip(d.columns) {
            if *v == Value::Text(String::new()) {
                return Err(format!(
                    "column {n}: an empty text, which an export cannot tell from null"
                ));
            }
            fields.push(escape(&v.text()));
        }
        out.push_str(&fields.join("\t"));
        out.push('\n');
    }
    Ok(out)
}

/// Reads text written by `to_tsv` back into values.
pub fn from_tsv(d: &Declared, text: &str) -> Result<Vec<Vec<Value>>, String> {
    let mut lines = text.lines();
    let head = lines.next().ok_or("empty text")?;
    if head.split('\t').collect::<Vec<_>>() != d.names() {
        return Err(format!("the header is not a {} file's columns", d.name));
    }
    lines
        .enumerate()
        .map(|(i, l)| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() != d.columns.len() {
                return Err(format!(
                    "line {}: {} fields for {} columns",
                    i + 2,
                    f.len(),
                    d.columns.len()
                ));
            }
            f.iter()
                .zip(d.columns)
                .map(|(t, (n, k))| {
                    let t = unescape(t).map_err(|e| format!("line {}: column {n}: {e}", i + 2))?;
                    Value::parse(&t, *k, n)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Declared = Declared {
        name: "test",
        columns: &[
            ("name", Kind::Text),
            ("n", Kind::Int),
            ("x", Kind::Float),
            ("ok", Kind::Bool),
        ],
        key: &["name"],
    };

    fn dir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warren-bench-{label}-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn awkward() -> Vec<Vec<Value>> {
        vec![
            vec![
                Value::Text("tab\there, newline\nthere, back\\slash, \\t, cr\r".into()),
                Value::Int(i64::MIN),
                Value::Float(0.1 + 0.2),
                Value::Bool(true),
            ],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
            vec![
                Value::Text("naïve ☂".into()),
                Value::Int(i64::MAX),
                Value::Float(1e-300),
                Value::Bool(false),
            ],
            vec![
                Value::Text("a".into()),
                Value::Int(-1),
                Value::Float(f64::NEG_INFINITY),
                Value::Null,
            ],
            vec![
                Value::Text("a".into()),
                Value::Int(-1),
                Value::Float(123456.789),
                Value::Null,
            ],
        ]
    }

    #[test]
    fn an_export_reads_back_to_the_same_values() {
        let d = dir("roundtrip");
        let p = d.join("t.parquet");
        write_values(&p, &D, &awkward()).unwrap();
        let text = to_tsv(&D, read_values(&p, &D).unwrap()).unwrap();
        let back = from_tsv(&D, &text).unwrap();
        assert_eq!(back, sorted(&D, awkward()));
        let q = d.join("u.parquet");
        write_values(&q, &D, &back).unwrap();
        assert_eq!(to_tsv(&D, read_values(&q, &D).unwrap()).unwrap(), text);
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_export_does_not_depend_on_the_order_rows_were_written() {
        let mut rows = awkward();
        let one = to_tsv(&D, rows.clone()).unwrap();
        rows.reverse();
        rows.swap(0, 2);
        assert_eq!(to_tsv(&D, rows).unwrap(), one);
    }

    #[test]
    fn a_value_that_is_not_its_columns_type_is_refused() {
        let d = dir("refused");
        let e = write(
            &d.join("t.parquet"),
            &D,
            &[vec!["a".into(), "1.5".into(), "".into(), "".into()]],
        )
        .unwrap_err();
        assert!(e.contains("column n: `1.5` is not an integer"), "{e}");
        let e = write(
            &d.join("t.parquet"),
            &D,
            &[vec!["a".into(), "".into(), "".into(), "yes".into()]],
        )
        .unwrap_err();
        assert!(e.contains("column ok: `yes` is not true or false"), "{e}");
        assert!(!d.join("t.parquet").exists());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_file_of_other_columns_is_refused() {
        const OTHER: Declared = Declared {
            name: "other",
            columns: &[("name", Kind::Text), ("n", Kind::Float)],
            key: &["name"],
        };
        let d = dir("other");
        let p = d.join("t.parquet");
        write(&p, &OTHER, &[vec!["a".into(), "1".into()]]).unwrap();
        let e = read(&p, &D).unwrap_err();
        assert!(e.contains("its columns are name Utf8, n Float64"), "{e}");
        assert_eq!(recognise(&p, &[&D, &OTHER]).unwrap().name, "other");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_directory_reads_as_its_files_in_name_order() {
        let d = dir("parts");
        write(
            &d.join("part-0002.parquet"),
            &D,
            &[vec!["b".into(), "2".into(), "".into(), "".into()]],
        )
        .unwrap();
        write(
            &d.join("part-0001.parquet"),
            &D,
            &[vec!["a".into(), "1".into(), "".into(), "".into()]],
        )
        .unwrap();
        let t = read(&d, &D).unwrap();
        assert_eq!(
            t.rows
                .iter()
                .map(|r| r["name"].as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(t.rows[0]["x"], "");
        fs::remove_dir_all(&d).unwrap();
    }
}
