// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The answer digest: a fingerprint of the rows a statement returned, as a multiset. It does not
//! depend on the order the rows arrive in, and it changes when any row is added, removed, repeated
//! or has any value changed.
//!
//! Each row is encoded column by column: a NULL is the byte `0`; a value is the byte `1`, its length
//! as four little-endian bytes, then its canonical text (`canonical`): the text the server sent,
//! rewritten into one form per type, so that the same value gives the same bytes however the
//! server printed it. The encoding is hashed twice with 64-bit FNV-1a from two different offset bases, each
//! result is mixed with the SplitMix64 finalizer, and the digest is the pair of wrapping sums of
//! those hashes over every row, together with the row count.

use std::fmt;

use crate::canonical::{self, Canon};

const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
const OFFSET_B: u64 = 0x6c62_272e_07bb_0142;

/// Rows received from the server, encoded and packed end to end as they were sent. Filling it is
/// cheap, so it can happen while a statement is being timed; the canonical texts and the digest
/// are taken afterwards.
#[derive(Default)]
pub struct RowBuffer {
    bytes: Vec<u8>,
    ends: Vec<usize>,
    /// How each column's values are written for the digest; a column with no kind is read as sent.
    kinds: Vec<Canon>,
}

impl RowBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how each column's values are written for the digest, in result order.
    pub fn set_kinds(&mut self, kinds: Vec<Canon>) {
        self.kinds = kinds;
    }

    /// Appends one row, given its columns in result order (`None` is NULL).
    pub fn push_row<'a, I>(&mut self, columns: I)
    where
        I: IntoIterator<Item = Option<&'a [u8]>>,
    {
        for c in columns {
            self.push_value(c);
        }
        self.end_row();
    }

    /// Appends one column of the row being built.
    pub fn push_value(&mut self, value: Option<&[u8]>) {
        match value {
            None => self.bytes.push(0),
            Some(v) => {
                self.bytes.push(1);
                self.bytes
                    .extend_from_slice(&(v.len() as u32).to_le_bytes());
                self.bytes.extend_from_slice(v);
            }
        }
    }

    /// Ends the row being built.
    pub fn end_row(&mut self) {
        self.ends.push(self.bytes.len());
    }

    pub fn digest(&self) -> Digest {
        let mut d = Digest::empty();
        let as_sent = self.kinds.iter().all(|k| *k == Canon::AsSent);
        let mut row = Vec::new();
        let mut start = 0;
        for &end in &self.ends {
            let sent = &self.bytes[start..end];
            if as_sent {
                d.add_encoded(sent);
            } else {
                row.clear();
                self.canonical_row(sent, &mut row);
                d.add_encoded(&row);
            }
            start = end;
        }
        d
    }

    /// One encoded row, its values rewritten into their canonical texts.
    fn canonical_row(&self, sent: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        let mut column = 0;
        while i < sent.len() {
            if sent[i] == 0 {
                out.push(0);
                i += 1;
            } else {
                let len = u32::from_le_bytes([sent[i + 1], sent[i + 2], sent[i + 3], sent[i + 4]])
                    as usize;
                let value = &sent[i + 5..i + 5 + len];
                let kind = self.kinds.get(column).copied().unwrap_or(Canon::AsSent);
                let text = canonical::write(kind, value);
                out.push(1);
                out.extend_from_slice(&(text.len() as u32).to_le_bytes());
                out.extend_from_slice(&text);
                i += 5 + len;
            }
            column += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest {
    pub rows: u64,
    pub sum_a: u64,
    pub sum_b: u64,
}

impl Digest {
    pub fn empty() -> Self {
        Digest {
            rows: 0,
            sum_a: 0,
            sum_b: 0,
        }
    }

    /// Adds one row without keeping it: the same digest a `RowBuffer` holding the row gives.
    #[cfg(test)]
    pub fn add_row<'a, I>(&mut self, columns: I)
    where
        I: IntoIterator<Item = Option<&'a [u8]>>,
    {
        let mut b = RowBuffer::new();
        b.push_row(columns);
        self.add_encoded(&b.bytes);
    }

    fn add_encoded(&mut self, row: &[u8]) {
        self.rows += 1;
        self.sum_a = self.sum_a.wrapping_add(mix(fnv1a(OFFSET_A, row)));
        self.sum_b = self.sum_b.wrapping_add(mix(fnv1a(OFFSET_B, row)));
    }

    /// The two sums as 32 hexadecimal digits; the row count is kept beside them.
    pub fn hex(&self) -> String {
        format!("{:016x}{:016x}", self.sum_a, self.sum_b)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} rows, {}", self.rows, self.hex())
    }
}

pub fn fnv1a(offset: u64, bytes: &[u8]) -> u64 {
    let mut h = offset;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A short fingerprint of some text, for naming things such as a question set or a configuration.
pub fn fingerprint(parts: &[&[u8]]) -> String {
    let mut buf = RowBuffer::new();
    buf.push_row(parts.iter().map(|p| Some(*p)));
    let d = buf.digest();
    format!("{:016x}", d.sum_a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(rows: &[&[Option<&str>]]) -> Digest {
        let mut b = RowBuffer::new();
        for r in rows {
            b.push_row(r.iter().map(|c| c.map(str::as_bytes)));
        }
        b.digest()
    }

    #[test]
    fn row_order_does_not_matter() {
        let a = digest_of(&[&[Some("1"), Some("x")], &[Some("2"), Some("y")]]);
        let b = digest_of(&[&[Some("2"), Some("y")], &[Some("1"), Some("x")]]);
        assert_eq!(a, b);
    }

    #[test]
    fn a_repeated_row_changes_the_digest() {
        let once = digest_of(&[&[Some("1")], &[Some("2")]]);
        let twice = digest_of(&[&[Some("1")], &[Some("2")], &[Some("2")]]);
        assert_ne!(once, twice);
        assert_ne!(once.hex(), twice.hex());
    }

    #[test]
    fn a_changed_value_with_the_same_row_count_changes_the_digest() {
        let a = digest_of(&[&[Some("75053-1"), Some("4")], &[Some("6020-1"), Some("1")]]);
        let b = digest_of(&[&[Some("75053-1"), Some("5")], &[Some("6020-1"), Some("1")]]);
        assert_eq!(a.rows, b.rows);
        assert_ne!(a.hex(), b.hex());
    }

    #[test]
    fn null_and_empty_text_differ() {
        assert_ne!(digest_of(&[&[None]]), digest_of(&[&[Some("")]]));
    }

    #[test]
    fn column_boundaries_are_part_of_the_row() {
        let a = digest_of(&[&[Some("ab"), Some("c")]]);
        let b = digest_of(&[&[Some("a"), Some("bc")]]);
        assert_ne!(a, b);
    }

    #[test]
    fn swapping_values_between_rows_changes_the_digest() {
        let a = digest_of(&[&[Some("1"), Some("a")], &[Some("2"), Some("b")]]);
        let b = digest_of(&[&[Some("1"), Some("b")], &[Some("2"), Some("a")]]);
        assert_ne!(a, b);
    }

    #[test]
    fn adding_rows_one_at_a_time_gives_the_buffered_digest() {
        let rows: &[&[Option<&str>]] = &[&[Some("1"), None], &[Some("2"), Some("")]];
        let mut d = Digest::empty();
        for r in rows {
            d.add_row(r.iter().map(|c| c.map(str::as_bytes)));
        }
        assert_eq!(d, digest_of(rows));
    }

    #[test]
    fn the_empty_answer_has_a_fixed_digest() {
        let d = digest_of(&[]);
        assert_eq!(d.rows, 0);
        assert_eq!(d.hex(), "0".repeat(32));
    }

    fn digest_with(kinds: &[Canon], rows: &[&[Option<&str>]]) -> Digest {
        let mut b = RowBuffer::new();
        b.set_kinds(kinds.to_vec());
        for r in rows {
            b.push_row(r.iter().map(|c| c.map(str::as_bytes)));
        }
        b.digest()
    }

    // One row as Postgres's text protocol sends it: an integer, a text, a boolean, a decimal, a
    // double, a date, a timestamp, an instant, a NULL.
    const PG_ROW: [Option<&str>; 9] = [
        Some("7"),
        Some("naïve ☂"),
        Some("t"),
        Some("12.50"),
        Some("1e+100"),
        Some("2020-02-29"),
        Some("2020-02-29 23:59:59.5"),
        Some("2021-03-28 01:30:00+01"),
        None,
    ];

    fn pg_kinds() -> Vec<Canon> {
        [23, 1043, 16, 1700, 701, 1082, 1114, 1184, 23]
            .into_iter()
            .map(canonical::postgres)
            .collect()
    }

    #[test]
    fn one_value_changed_changes_the_digest() {
        let pg = digest_with(&pg_kinds(), &[&PG_ROW]);
        for i in 0..PG_ROW.len() {
            let mut changed = PG_ROW;
            changed[i] = match changed[i] {
                None => Some("1"),
                Some(_) => None,
            };
            let d = digest_with(&pg_kinds(), &[&changed]);
            assert_eq!(d.rows, pg.rows);
            assert_ne!(d, pg, "column {i}");
        }
        let mut quantity = PG_ROW;
        quantity[3] = Some("12.51");
        assert_ne!(digest_with(&pg_kinds(), &[&quantity]), pg);
    }

    #[test]
    fn a_null_stays_null_whatever_its_column_kind() {
        let kinds = [
            Canon::AsSent,
            Canon::Integer,
            Canon::Boolean,
            Canon::Decimal,
            Canon::Float4,
            Canon::Float8,
            Canon::Timestamp,
            Canon::Instant,
            Canon::Time,
        ];
        let null = digest_of(&[&[None]]);
        for k in kinds {
            assert_eq!(digest_with(&[k], &[&[None]]), null, "{k:?}");
            // no value stands in for it
            for v in ["", "0", "f", "NULL", "0000-00-00 00:00:00"] {
                assert_ne!(digest_with(&[k], &[&[Some(v)]]), null, "{k:?} {v:?}");
            }
        }
    }

    #[test]
    fn a_postgres_answer_of_integers_text_and_booleans_digests_as_it_did_as_sent() {
        let rows: &[&[Option<&str>]] = &[
            &[Some("1"), Some("48379c01"), Some("72"), Some("f"), None],
            &[Some("2"), Some("3001"), Some("0"), Some("t"), Some("")],
        ];
        let kinds: Vec<Canon> = [23, 1043, 23, 16, 1043]
            .into_iter()
            .map(canonical::postgres)
            .collect();
        assert_eq!(digest_with(&kinds, rows), digest_of(rows));
        assert_eq!(digest_with(&kinds, rows).hex(), KNOWN_HEX);
    }

    // Values computed by a separate implementation of the same encoding and hashes, written in
    // another language, so that the two can check each other.
    #[test]
    fn known_values() {
        let d = digest_of(&[
            &[Some("1"), Some("48379c01"), Some("72"), Some("f"), None],
            &[Some("2"), Some("3001"), Some("0"), Some("t"), Some("")],
        ]);
        assert_eq!(d.rows, 2);
        assert_eq!(d.hex(), KNOWN_HEX);
    }

    const KNOWN_HEX: &str = "a902184c0b59ad4e6b0e923d7126800a";
}
