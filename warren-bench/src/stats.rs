// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Median and spread over repetitions.

/// The summary of one sample: its size, median, extremes and median absolute deviation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spread {
    pub n: usize,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub mad: f64,
}

/// The median of a sample; the mean of the two middle values when the size is even. `None` for an
/// empty sample. Values that are not finite are refused rather than sorted.
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() || xs.iter().any(|x| !x.is_finite()) {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

pub fn spread(xs: &[f64]) -> Option<Spread> {
    let m = median(xs)?;
    let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
    let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let deviations: Vec<f64> = xs.iter().map(|x| (x - m).abs()).collect();
    let mad = median(&deviations)?;
    Some(Spread {
        n: xs.len(),
        median: m,
        min,
        max,
        mad,
    })
}

/// The one value every element shares, or `None` when they differ or there are none.
pub fn constant<T: PartialEq + Clone>(xs: &[T]) -> Option<T> {
    let first = xs.first()?;
    xs.iter().all(|x| x == first).then(|| first.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_an_odd_sample_is_its_middle_value() {
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
    }

    #[test]
    fn median_of_an_even_sample_is_the_mean_of_the_middle_pair() {
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn median_of_nothing_is_none() {
        assert_eq!(median(&[]), None);
        assert_eq!(spread(&[]), None);
    }

    #[test]
    fn a_value_that_is_not_finite_is_refused() {
        assert_eq!(median(&[1.0, f64::NAN, 2.0]), None);
        assert_eq!(median(&[1.0, f64::INFINITY]), None);
    }

    #[test]
    fn median_does_not_move_with_one_outlier() {
        let s = spread(&[10.0, 11.0, 12.0, 13.0, 1000.0]).unwrap();
        assert_eq!(s.median, 12.0);
        assert_eq!(s.min, 10.0);
        assert_eq!(s.max, 1000.0);
        assert_eq!(s.mad, 1.0);
        assert_eq!(s.n, 5);
    }

    #[test]
    fn a_single_value_has_no_spread() {
        let s = spread(&[7.5]).unwrap();
        assert_eq!((s.median, s.min, s.max, s.mad), (7.5, 7.5, 7.5, 0.0));
    }

    #[test]
    fn median_absolute_deviation_of_an_even_sample() {
        // deviations from 2.5 are 1.5, 0.5, 0.5, 1.5
        let s = spread(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(s.mad, 1.0);
    }

    #[test]
    fn constant_reports_a_shared_value_only() {
        assert_eq!(constant(&[3, 3, 3]), Some(3));
        assert_eq!(constant(&[3, 4]), None);
        assert_eq!(constant::<i32>(&[]), None);
    }
}
