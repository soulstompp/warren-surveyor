// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The canonical text of a typed value: one text for one value, however the server's session prints
//! it. Each column of an answer is read as one of the kinds below, from the type the server reports
//! for it, and its values are rewritten into the kind's form before the digest is taken. A value
//! whose text does not have its kind's form is kept as it was sent.
//!
//! Where Postgres's own text for a type already has the kind's form (integers, booleans, text,
//! dates, timestamps without a time zone), the canonical text is that text unchanged.

use std::borrow::Cow;

/// How a column's values are written for the digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Canon {
    /// As sent: text, dates, and every type with no canonical form here.
    AsSent,
    /// An integer in decimal: `-` before a negative one, no `+`, no leading zeros.
    Integer,
    /// `t` or `f`: a boolean sent as `t` or `f`, or as `true` or `false`.
    Boolean,
    /// An exact decimal: no leading zeros, no trailing zeros after the point, no point when it is
    /// whole, and `0` for zero of either sign.
    Decimal,
    /// A single-precision number, in the shortest form that reads back to the same number.
    Float4,
    /// A double-precision number, in the shortest form that reads back to the same number.
    Float8,
    /// `YYYY-MM-DD HH:MM:SS`, and `.` with the fraction when the fraction is not zero, without its
    /// trailing zeros.
    Timestamp,
    /// An instant written with its offset from UTC, rewritten as its time in UTC in the form of
    /// `Timestamp`.
    Instant,
    /// `HH:MM:SS`, with the fraction as for `Timestamp`.
    Time,
}

/// The kind of a Postgres column, by its type's OID.
pub fn postgres(oid: u32) -> Canon {
    match oid {
        16 => Canon::Boolean,
        20 | 21 | 23 => Canon::Integer,
        700 => Canon::Float4,
        701 => Canon::Float8,
        1700 => Canon::Decimal,
        1083 => Canon::Time,
        1114 => Canon::Timestamp,
        1184 => Canon::Instant,
        _ => Canon::AsSent,
    }
}

/// The canonical text of one value of `kind`, sent as `sent`.
pub fn write(kind: Canon, sent: &[u8]) -> Cow<'_, [u8]> {
    let Ok(text) = std::str::from_utf8(sent) else {
        return Cow::Borrowed(sent);
    };
    let out = match kind {
        Canon::AsSent => None,
        Canon::Integer => integer(text),
        Canon::Boolean => match text {
            "t" | "true" | "TRUE" => Some("t".to_string()),
            "f" | "false" | "FALSE" => Some("f".to_string()),
            _ => None,
        },
        Canon::Decimal => decimal(text),
        Canon::Float4 => text.parse::<f32>().ok().map(|x| x.to_string()),
        Canon::Float8 => text.parse::<f64>().ok().map(|x| x.to_string()),
        Canon::Timestamp => timestamp(text).map(|t| t.text()),
        Canon::Instant => instant(text),
        Canon::Time => time(text),
    };
    match out {
        Some(s) if s.as_bytes() != sent => Cow::Owned(s.into_bytes()),
        _ => Cow::Borrowed(sent),
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn split_sign(s: &str) -> (bool, &str) {
    match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    }
}

fn integer(text: &str) -> Option<String> {
    let (negative, body) = split_sign(text);
    if !digits(body) {
        return None;
    }
    let body = body.trim_start_matches('0');
    Some(if body.is_empty() {
        "0".to_string()
    } else if negative {
        format!("-{body}")
    } else {
        body.to_string()
    })
}

fn decimal(text: &str) -> Option<String> {
    let (negative, body) = split_sign(text);
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    if !digits(whole) || !(fraction.is_empty() || digits(fraction)) {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    if whole.is_empty() && fraction.is_empty() {
        return Some("0".to_string());
    }
    let whole = if whole.is_empty() { "0" } else { whole };
    let sign = if negative { "-" } else { "" };
    Some(if fraction.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction}")
    })
}

/// A fraction of a second without its trailing zeros; empty when it is zero or absent.
fn fraction(f: &str) -> Option<&str> {
    if f.is_empty() {
        return Some("");
    }
    digits(f).then(|| f.trim_end_matches('0'))
}

fn with_fraction(base: String, f: &str) -> String {
    if f.is_empty() {
        base
    } else {
        format!("{base}.{f}")
    }
}

fn two(s: &str) -> Option<u32> {
    (s.len() == 2 && digits(s))
        .then(|| s.parse().ok())
        .flatten()
}

/// A time of day read from `HH:MM:SS[.f]`.
fn clock(s: &str) -> Option<(u32, u32, u32, &str)> {
    let (hms, f) = s.split_once('.').unwrap_or((s, ""));
    let mut p = hms.split(':');
    let (h, m, sec) = (two(p.next()?)?, two(p.next()?)?, two(p.next()?)?);
    if p.next().is_some() || m > 59 || sec > 60 {
        return None;
    }
    Some((h, m, sec, fraction(f)?))
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp<'a> {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    fraction: &'a str,
}

impl Stamp<'_> {
    fn text(&self) -> String {
        with_fraction(
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                self.year, self.month, self.day, self.hour, self.minute, self.second
            ),
            self.fraction,
        )
    }
}

/// `YYYY-MM-DD HH:MM:SS[.f]`, with a four-digit year.
fn timestamp(text: &str) -> Option<Stamp<'_>> {
    let (date, clock_text) = text.split_once(' ')?;
    let mut d = date.split('-');
    let (y, mo, da) = (d.next()?, d.next()?, d.next()?);
    if d.next().is_some() || y.len() != 4 || !digits(y) {
        return None;
    }
    let (month, day) = (two(mo)?, two(da)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hour, minute, second, fraction) = clock(clock_text)?;
    if hour > 23 {
        return None;
    }
    Some(Stamp {
        year: y.parse().ok()?,
        month,
        day,
        hour,
        minute,
        second,
        fraction,
    })
}

fn time(text: &str) -> Option<String> {
    let (negative, body) = split_sign(text);
    let (hms, f) = body.split_once('.').unwrap_or((body, ""));
    let mut p = hms.split(':');
    let (h, m, s) = (p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || h.len() < 2 || !digits(h) || two(m)? > 59 || two(s)? > 60 {
        return None;
    }
    let sign = if negative { "-" } else { "" };
    Some(with_fraction(format!("{sign}{h}:{m}:{s}"), fraction(f)?))
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `YYYY-MM-DD HH:MM:SS[.f]±HH[:MM[:SS]]` as the same instant in UTC.
fn instant(text: &str) -> Option<String> {
    let at = text.rfind(['+', '-'])?;
    let (local, offset) = text.split_at(at);
    let stamp = timestamp(local)?;
    let (negative, off) = split_sign(offset);
    let mut parts = off.split(':');
    let oh = two(parts.next()?)?;
    let om = parts.next().map_or(Some(0), two)?;
    let os = parts.next().map_or(Some(0), two)?;
    if parts.next().is_some() || om > 59 || os > 59 {
        return None;
    }
    let shift = i64::from(oh * 3600 + om * 60 + os);
    let shift = if negative { -shift } else { shift };
    let seconds = days_from_civil(stamp.year, stamp.month, stamp.day) * 86_400
        + i64::from(stamp.hour * 3600 + stamp.minute * 60 + stamp.second.min(59))
        - shift;
    let (y, mo, d) = civil_from_days(seconds.div_euclid(86_400));
    if !(1..=9999).contains(&y) || stamp.second > 59 {
        return None;
    }
    let s = seconds.rem_euclid(86_400) as u32;
    Some(
        Stamp {
            year: y,
            month: mo,
            day: d,
            hour: s / 3600,
            minute: s / 60 % 60,
            second: s % 60,
            fraction: stamp.fraction,
        }
        .text(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(kind: Canon, sent: &str) -> String {
        String::from_utf8(write(kind, sent.as_bytes()).into_owned()).unwrap()
    }

    #[test]
    fn integers_lose_leading_zeros_and_a_plus_sign() {
        assert_eq!(c(Canon::Integer, "42"), "42");
        assert_eq!(c(Canon::Integer, "-42"), "-42");
        assert_eq!(c(Canon::Integer, "0042"), "42");
        assert_eq!(c(Canon::Integer, "+7"), "7");
        assert_eq!(c(Canon::Integer, "-0"), "0");
        assert_eq!(
            c(Canon::Integer, "18446744073709551615"),
            "18446744073709551615"
        );
        assert_eq!(c(Canon::Integer, "x"), "x");
    }

    #[test]
    fn a_boolean_is_t_or_f() {
        assert_eq!(c(Canon::Boolean, "t"), "t");
        assert_eq!(c(Canon::Boolean, "f"), "f");
        assert_eq!(c(Canon::Boolean, "true"), "t");
        assert_eq!(c(Canon::Boolean, "false"), "f");
        assert_eq!(c(Canon::Boolean, "TRUE"), "t");
        assert_eq!(c(Canon::Boolean, "FALSE"), "f");
        // anything else is kept as it was sent
        assert_eq!(c(Canon::Boolean, "1"), "1");
    }

    #[test]
    fn a_decimal_is_written_without_the_zeros_its_scale_adds() {
        assert_eq!(c(Canon::Decimal, "12.50"), "12.5");
        assert_eq!(c(Canon::Decimal, "12.5"), "12.5");
        assert_eq!(c(Canon::Decimal, "2.5000000000000000"), "2.5");
        assert_eq!(c(Canon::Decimal, "1234"), "1234");
        assert_eq!(c(Canon::Decimal, "1234.000"), "1234");
        assert_eq!(c(Canon::Decimal, "-0.10"), "-0.1");
        assert_eq!(c(Canon::Decimal, "-0.00"), "0");
        assert_eq!(c(Canon::Decimal, "0.00"), "0");
        assert_eq!(c(Canon::Decimal, "007.0100"), "7.01");
        assert_eq!(c(Canon::Decimal, "NaN"), "NaN");
        assert_eq!(c(Canon::Decimal, "1.2.3"), "1.2.3");
    }

    #[test]
    fn a_float_is_its_shortest_round_trip_whichever_way_it_was_printed() {
        assert_eq!(
            c(Canon::Float8, "0.30000000000000004"),
            "0.30000000000000004"
        );
        assert_eq!(c(Canon::Float8, "1e+100"), c(Canon::Float8, "1e100"));
        assert_eq!(c(Canon::Float8, "-2.5e-07"), c(Canon::Float8, "-2.5e-7"));
        assert_eq!(c(Canon::Float8, "-2.5e-07"), "-0.00000025");
        assert_eq!(c(Canon::Float8, "Infinity"), "inf");
        assert_eq!(c(Canon::Float8, "-Infinity"), "-inf");
        assert_eq!(c(Canon::Float4, "0.1"), "0.1");
        assert_eq!(c(Canon::Float4, "1e+10"), c(Canon::Float4, "1e10"));
        assert_eq!(c(Canon::Float8, "abc"), "abc");
    }

    #[test]
    fn a_timestamp_loses_the_zeros_of_its_fraction() {
        assert_eq!(
            c(Canon::Timestamp, "2020-01-01 00:00:00.000000"),
            "2020-01-01 00:00:00"
        );
        assert_eq!(
            c(Canon::Timestamp, "2020-02-29 23:59:59.500000"),
            "2020-02-29 23:59:59.5"
        );
        assert_eq!(
            c(Canon::Timestamp, "2020-02-29 23:59:59.5"),
            "2020-02-29 23:59:59.5"
        );
        assert_eq!(c(Canon::Timestamp, "infinity"), "infinity");
        assert_eq!(
            c(Canon::Timestamp, "0044-03-15 12:00:00 BC"),
            "0044-03-15 12:00:00 BC"
        );
    }

    #[test]
    fn an_instant_is_written_as_its_time_in_utc() {
        assert_eq!(
            c(Canon::Instant, "2021-03-28 01:30:00+01"),
            "2021-03-28 00:30:00"
        );
        assert_eq!(
            c(Canon::Instant, "2020-01-01 00:10:00.250+05:30"),
            "2019-12-31 18:40:00.25"
        );
        assert_eq!(
            c(Canon::Instant, "2019-12-31 22:00:00-03"),
            "2020-01-01 01:00:00"
        );
        assert_eq!(
            c(Canon::Instant, "2024-02-28 23:00:00-01"),
            "2024-02-29 00:00:00"
        );
        assert_eq!(
            c(Canon::Instant, "2020-01-01 00:00:00+00"),
            "2020-01-01 00:00:00"
        );
        assert_eq!(c(Canon::Instant, "infinity"), "infinity");
    }

    #[test]
    fn a_time_loses_the_zeros_of_its_fraction() {
        assert_eq!(c(Canon::Time, "12:34:56.789000"), "12:34:56.789");
        assert_eq!(c(Canon::Time, "12:34:56"), "12:34:56");
        assert_eq!(c(Canon::Time, "-838:59:59.000000"), "-838:59:59");
    }

    #[test]
    fn the_calendar_round_trips() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (1900, 3, 1),
            (2024, 12, 31),
            (1, 1, 1),
        ] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2
        );
    }

    #[test]
    fn postgres_text_of_the_questions_types_is_already_canonical() {
        // the types the question sets return today: what Postgres sends is the canonical text
        for (oid, sent) in [
            (23, "1234"),
            (23, "-7"),
            (20, "9223372036854775807"),
            (16, "t"),
            (16, "f"),
            (1043, "75053-1"),
            (25, "naïve ☂"),
            (1082, "2020-02-29"),
            (1114, "2020-02-29 23:59:59.5"),
        ] {
            assert_eq!(c(postgres(oid), sent), sent, "oid {oid}");
        }
    }
}
