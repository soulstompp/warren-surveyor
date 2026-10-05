// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The part of a GiST of cubes a question names, and the share of a key's entries it takes.
//!
//! A question names a box, a distance from a point (every value within a chord of the point: the
//! ball), or both, and the read follows the keys reaching all it names. A key wholly inside takes
//! all its entries, and a key wholly outside none. A key across the edge is counted in the
//! dimensions its entries fill, since its union key bounds them in every dimension and they fill
//! only some. Where the index's values lie on a sphere about the origin, as earthdistance's earths
//! do:
//!
//! - a key with volume takes, against a box, the share of its volume inside the box; against a
//!   ball, the share of the sphere's surface inside the key that lies inside the question;
//! - a key with no extent on one axis takes the share of the circle it lies on, inside the key,
//!   that lies inside the question;
//! - a key with extent on one axis alone takes its two ends;
//! - a point takes itself.
//!
//! On any other GiST of cubes a key takes the share of its extent inside the box, on the axes
//! where it has one, and a point itself.

use std::f64::consts::PI;

/// The steps a key's surface is added up in.
const STEPS: usize = 200;

const POINT_BIT: u32 = 0x8000_0000;
const DIM_MASK: u32 = 0x7fff_ffff;

/// A cube: its lower and upper corner in each dimension.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cube {
    pub lo: Vec<f64>,
    pub hi: Vec<f64>,
}

impl Cube {
    /// Reads a cube from its datum: None where the value is too short for the corners its header
    /// names, so that no read passes its end.
    pub(crate) unsafe fn of(datum: pgrx::pg_sys::Datum) -> Option<Cube> {
        let v = pgrx::pg_sys::pg_detoast_datum(datum.cast_mut_ptr()) as *const u8;
        let size = pgrx::varlena::varsize_any(v as *const pgrx::pg_sys::varlena);
        if size < 8 {
            return None;
        }
        let header = std::ptr::read_unaligned(v.add(4) as *const u32);
        let dim = (header & DIM_MASK) as usize;
        let point = header & POINT_BIT != 0;
        let corners = if point { dim } else { 2 * dim };
        if size < 8 + 8 * corners {
            return None;
        }
        let x = |i: usize| std::ptr::read_unaligned(v.add(8 + 8 * i) as *const f64);
        let (mut lo, mut hi) = (Vec::with_capacity(dim), Vec::with_capacity(dim));
        for i in 0..dim {
            let a = x(i);
            let b = if point { a } else { x(i + dim) };
            lo.push(a.min(b));
            hi.push(a.max(b));
        }
        Some(Cube { lo, hi })
    }

    /// The cube of one point.
    pub(crate) fn point(p: &[f64]) -> Cube {
        Cube {
            lo: p.to_vec(),
            hi: p.to_vec(),
        }
    }

    pub(crate) fn dim(&self) -> usize {
        self.lo.len()
    }

    /// The corners in dimension `i`, a dimension the cube lacks being 0.
    pub(crate) fn at(&self, i: usize) -> (f64, f64) {
        (
            self.lo.get(i).copied().unwrap_or(0.0),
            self.hi.get(i).copied().unwrap_or(0.0),
        )
    }

    pub(crate) fn overlaps(&self, other: &Cube) -> bool {
        (0..self.dim().max(other.dim())).all(|i| {
            let ((a, b), (c, d)) = (self.at(i), other.at(i));
            a <= d && c <= b
        })
    }

    /// Whether `other` lies inside this cube, in the dimensions both have, and at 0 in those only
    /// `other` has.
    pub(crate) fn contains(&self, other: &Cube) -> bool {
        (self.dim()..other.dim()).all(|i| other.at(i) == (0.0, 0.0))
            && (0..self.dim().min(other.dim())).all(|i| {
                let ((a, b), (c, d)) = (self.at(i), other.at(i));
                a <= c && d <= b
            })
    }

    /// The cube this cube and `other` both hold; its lower corner passes its upper where they hold
    /// none together.
    pub(crate) fn meet(&self, other: &Cube) -> Cube {
        let n = self.dim().max(other.dim());
        let (mut lo, mut hi) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for i in 0..n {
            let ((a, b), (c, d)) = (self.at(i), other.at(i));
            lo.push(a.max(c));
            hi.push(b.min(d));
        }
        Cube { lo, hi }
    }

    /// The square of the distance from `p` to the cube's nearest point.
    fn nearest(&self, p: &[f64]) -> f64 {
        (0..self.dim().max(p.len()))
            .map(|i| {
                let (a, b) = self.at(i);
                let x = p.get(i).copied().unwrap_or(0.0);
                (a - x).max(x - b).max(0.0).powi(2)
            })
            .sum()
    }

    /// The square of the distance from `p` to the cube's farthest point.
    fn farthest(&self, p: &[f64]) -> f64 {
        (0..self.dim().max(p.len()))
            .map(|i| {
                let (a, b) = self.at(i);
                let x = p.get(i).copied().unwrap_or(0.0);
                (x - a).powi(2).max((b - x).powi(2))
            })
            .sum()
    }

    /// The axes on which the cube has extent.
    fn spread(&self) -> Vec<usize> {
        (0..self.dim())
            .filter(|&i| self.hi[i] > self.lo[i])
            .collect()
    }
}

/// Every value within a chord of a point: the point, the chord, and whether a value at the chord
/// is taken.
#[derive(Clone, Debug)]
pub(crate) struct Ball {
    pub centre: Vec<f64>,
    pub chord: f64,
    pub closed: bool,
}

impl Ball {
    /// Whether a value whose distance from the centre has the square `d2` lies within the chord.
    fn within(&self, d2: f64) -> bool {
        if self.closed {
            self.chord >= 0.0 && d2 <= self.chord * self.chord
        } else {
            self.chord > 0.0 && d2 < self.chord * self.chord
        }
    }

    /// The cube that just holds the ball.
    pub(crate) fn cube(&self) -> Cube {
        Cube {
            lo: self.centre.iter().map(|x| x - self.chord).collect(),
            hi: self.centre.iter().map(|x| x + self.chord).collect(),
        }
    }

    /// Where the circle of radius `rho` about axis `k`, at `v` on it, on the sphere of radius
    /// `radius`, lies within the chord, as the circle's turn on axes `u` and `w` against a bound.
    fn cap(&self, radius: f64, rho: f64, k: usize, u: usize, w: usize, v: f64) -> Cap {
        let p = |i: usize| self.centre.get(i).copied().unwrap_or(0.0);
        let norm2: f64 = self.centre.iter().map(|x| x * x).sum();
        let h = (radius * radius + norm2 - self.chord * self.chord) / 2.0;
        Cap {
            reach: rho * p(u).hypot(p(w)),
            turn: p(w).atan2(p(u)),
            bound: h - v * p(k),
        }
    }
}

/// Where on a circle a ball takes it: the turns at which `reach` times the cosine of the turn less
/// `turn` passes `bound`.
#[derive(Clone, Copy, Debug)]
struct Cap {
    reach: f64,
    turn: f64,
    bound: f64,
}

/// The part of a GiST of cubes a question names: a box, a ball, or both; where the read follows the
/// keys; and the radius of the sphere the index's values lie on, where they lie on one.
#[derive(Clone, Debug)]
pub(crate) struct Region {
    /// The box, or the cube that just holds the ball, or the cube both hold.
    pub reach: Cube,
    /// The box, and whether an entry must lie inside it rather than overlap it.
    boxed: Option<(Cube, bool)>,
    ball: Option<Ball>,
    sphere: Option<f64>,
}

impl Region {
    pub(crate) fn new(
        boxed: Option<(Cube, bool)>,
        ball: Option<Ball>,
        sphere: Option<f64>,
    ) -> Option<Region> {
        let reach = match (&boxed, &ball) {
            (Some((b, _)), None) => b.clone(),
            (None, Some(ball)) => ball.cube(),
            (Some((b, _)), Some(ball)) => b.meet(&ball.cube()),
            (None, None) => return None,
        };
        Some(Region {
            reach,
            boxed,
            ball,
            sphere,
        })
    }

    /// Whether an entry under `key` may lie in the region.
    pub(crate) fn reaches(&self, key: &Cube) -> bool {
        key.overlaps(&self.reach)
            && self
                .ball
                .as_ref()
                .is_none_or(|b| b.within(key.nearest(&b.centre)))
    }

    /// Whether every entry under `key` lies in the region.
    pub(crate) fn holds(&self, key: &Cube) -> bool {
        self.boxed.as_ref().is_none_or(|(b, _)| b.contains(key))
            && self
                .ball
                .as_ref()
                .is_none_or(|b| b.within(key.farthest(&b.centre)))
    }

    /// Whether the entry `entry` lies in the region.
    pub(crate) fn takes(&self, entry: &Cube) -> bool {
        self.boxed.as_ref().is_none_or(|(b, inside)| {
            if *inside {
                b.contains(entry)
            } else {
                entry.overlaps(b)
            }
        }) && self
            .ball
            .as_ref()
            .is_none_or(|b| b.within(entry.nearest(&b.centre)))
    }

    /// The share of the entries under `key` the region takes.
    pub(crate) fn share(&self, key: &Cube) -> f64 {
        if !self.reaches(key) {
            return 0.0;
        }
        if self.holds(key) {
            return 1.0;
        }
        let spread = key.spread();
        let on_sphere = match self.sphere {
            Some(radius) if key.dim() == 3 => Some(radius),
            _ => None,
        };
        let shared = match (on_sphere, spread.len()) {
            (_, 0) => Some(if self.takes(key) { 1.0 } else { 0.0 }),
            (Some(_), 1) => Some(self.ends(key, spread[0])),
            (Some(radius), 2) => {
                let flat = (0..3).find(|i| !spread.contains(i)).unwrap_or(2);
                self.on_circle(key, radius, flat)
            }
            (Some(radius), 3) => self
                .ball
                .as_ref()
                .and_then(|ball| self.on_surface(key, radius, ball)),
            _ => None,
        };
        shared
            .unwrap_or_else(|| self.extent(key, &spread))
            .clamp(0.0, 1.0)
    }

    /// The share of `key`'s extent inside the region's reach, on the axes `spread` where it has
    /// one.
    fn extent(&self, key: &Cube, spread: &[usize]) -> f64 {
        spread
            .iter()
            .map(|&i| {
                let ((a, b), (c, d)) = (key.at(i), self.reach.at(i));
                (b.min(d) - a.max(c)).max(0.0) / (b - a)
            })
            .product()
    }

    /// The share of the two ends of `key` on axis `i` the region takes.
    fn ends(&self, key: &Cube, i: usize) -> f64 {
        let end = |x: f64| {
            let mut p = key.lo.clone();
            p[i] = x;
            if self.takes(&Cube::point(&p)) {
                0.5
            } else {
                0.0
            }
        };
        end(key.lo[i]) + end(key.hi[i])
    }

    /// The share of the circle the sphere of radius `radius` shares with the plane of `key`, a key
    /// with no extent on axis `flat`, inside the key, that lies in the region.
    fn on_circle(&self, key: &Cube, radius: f64, flat: usize) -> Option<f64> {
        let (u, w) = match flat {
            0 => (1, 2),
            1 => (0, 2),
            _ => (0, 1),
        };
        let v = key.lo[flat];
        let rho = (radius * radius - v * v).max(0.0).sqrt();
        let whole = circle(rho, key.at(u), key.at(w), None);
        if whole <= 0.0 {
            return None;
        }
        let cut = key.meet(&self.reach);
        let cap = self
            .ball
            .as_ref()
            .map(|b| b.cap(radius, rho, flat, u, w, v));
        Some(circle(rho, cut.at(u), cut.at(w), cap) / whole)
    }

    /// The share of the surface of the sphere of radius `radius` inside `key` that lies in the
    /// region, the ball `ball` among it.
    fn on_surface(&self, key: &Cube, radius: f64, ball: &Ball) -> Option<f64> {
        let whole = slices(radius, key, None);
        if whole <= 0.0 {
            return None;
        }
        let cut = key.meet(&self.reach);
        Some(slices(radius, &cut, Some(ball)) / whole)
    }
}

/// The surface of the sphere of radius `radius` inside `cube`, within `ball` where there is one,
/// added up over `STEPS` slices of the third axis, each slice's circle counted exactly; in units
/// of the radius.
fn slices(radius: f64, cube: &Cube, ball: Option<&Ball>) -> f64 {
    let (lo, hi) = cube.at(2);
    let (z1, z2) = (lo.max(-radius), hi.min(radius));
    if z2 <= z1 || cube.at(0).1 < cube.at(0).0 || cube.at(1).1 < cube.at(1).0 {
        return 0.0;
    }
    let dz = (z2 - z1) / STEPS as f64;
    (0..STEPS)
        .map(|j| {
            let z = z1 + (j as f64 + 0.5) * dz;
            let rho = (radius * radius - z * z).max(0.0).sqrt();
            let cap = ball.map(|b| b.cap(radius, rho, 2, 0, 1, z));
            circle(rho, cube.at(0), cube.at(1), cap) * dz
        })
        .sum()
}

/// The turn, in radians, of the circle of radius `rho` about the origin that lies inside `u` on
/// its first axis and `w` on its second, and where `cap` is given, on its side of the bound.
fn circle(rho: f64, u: (f64, f64), w: (f64, f64), cap: Option<Cap>) -> f64 {
    if rho <= 0.0 || u.1 < u.0 || w.1 < w.0 {
        return 0.0;
    }
    let mut turns = meet(
        &by_cosine(u.0 / rho, u.1 / rho),
        &by_sine(w.0 / rho, w.1 / rho),
    );
    if let Some(cap) = cap {
        let on_side = if cap.reach <= 0.0 {
            if cap.bound < 0.0 {
                vec![(-PI, PI)]
            } else {
                Vec::new()
            }
        } else {
            let k = cap.bound / cap.reach;
            if k >= 1.0 {
                Vec::new()
            } else if k <= -1.0 {
                vec![(-PI, PI)]
            } else {
                let d = k.acos();
                arc(cap.turn - d, cap.turn + d)
            }
        };
        turns = meet(&turns, &on_side);
    }
    turns.iter().map(|(a, b)| b - a).sum()
}

/// The turns, from −π to π, whose cosine lies from `lo` to `hi`.
fn by_cosine(lo: f64, hi: f64) -> Vec<(f64, f64)> {
    let (lo, hi) = (snapped(lo), snapped(hi));
    if lo > 1.0 || hi < -1.0 {
        return Vec::new();
    }
    let (a, b) = (hi.min(1.0).acos(), lo.max(-1.0).acos());
    vec![(-b, -a), (a, b)]
}

/// The turns, from −π to π, whose sine lies from `lo` to `hi`.
fn by_sine(lo: f64, hi: f64) -> Vec<(f64, f64)> {
    let (lo, hi) = (snapped(lo), snapped(hi));
    if lo > 1.0 || hi < -1.0 {
        return Vec::new();
    }
    let (a, b) = (lo.max(-1.0).asin(), hi.min(1.0).asin());
    let mut out = vec![(a, b)];
    out.extend(arc(PI - b, PI - a));
    out
}

/// `x`, taken as 1 or −1 where it lies within rounding of either.
fn snapped(x: f64) -> f64 {
    if (x - 1.0).abs() < 1e-15 {
        1.0
    } else if (x + 1.0).abs() < 1e-15 {
        -1.0
    } else {
        x
    }
}

/// The turns from `start` to `end`, at most one whole turn on, laid from −π to π.
fn arc(start: f64, end: f64) -> Vec<(f64, f64)> {
    let length = (end - start).min(2.0 * PI);
    let s = start - 2.0 * PI * ((start + PI) / (2.0 * PI)).floor();
    let e = s + length;
    if e <= PI {
        vec![(s, e)]
    } else {
        vec![(s, PI), (-PI, e - 2.0 * PI)]
    }
}

/// The turns both `a` and `b` hold.
fn meet(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &(p, q) in a {
        for &(r, s) in b {
            let (lo, hi) = (p.max(r), q.min(s));
            if hi > lo {
                out.push((lo, hi));
            }
        }
    }
    out
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::{Ball, Cube, Region};
    use pgrx::prelude::*;
    use std::f64::consts::PI;

    /// earthdistance's radius of the earth.
    const EARTH: f64 = 6_378_168.0;

    fn on_earth(lat: f64, lon: f64) -> Vec<f64> {
        let (p, l) = (lat.to_radians(), lon.to_radians());
        vec![
            EARTH * p.cos() * l.cos(),
            EARTH * p.cos() * l.sin(),
            EARTH * p.sin(),
        ]
    }

    /// The chord of an arc of `metres` on the earth.
    fn chord(metres: f64) -> f64 {
        2.0 * EARTH * (metres / (2.0 * EARTH)).sin()
    }

    fn ball(lat: f64, lon: f64, metres: f64) -> Ball {
        Ball {
            centre: on_earth(lat, lon),
            chord: chord(metres),
            closed: false,
        }
    }

    #[pg_test]
    fn a_key_that_is_the_cube_holding_a_small_ball_takes_the_ball_by_the_surface_inside_it() {
        // the earth within the distance over the earth inside the cube, which for a small ball
        // comes to a quarter turn times the height of the up axis in the ground's own direction
        for (lat, lon) in [(51.5085, -0.1115), (56.1572, 10.2107)] {
            for metres in [1_000.0, 8_000.0] {
                let b = ball(lat, lon, metres);
                let key = b.cube();
                let region = Region::new(None, Some(b), Some(EARTH)).unwrap();
                let share = region.share(&key);
                let expected = PI / 4.0 * lat.to_radians().sin();
                assert!(
                    (share - expected).abs() < 1e-3,
                    "{lat} {lon} {metres}: {share} against {expected}"
                );
            }
        }
    }

    #[pg_test]
    fn a_key_on_one_latitude_takes_the_stretch_of_its_latitude_inside_the_box_or_the_ball() {
        let lat: f64 = 51.5;
        // a key on one latitude from 0.4 degrees west to 0.4 degrees east
        let (w, e) = (on_earth(lat, -0.4), on_earth(lat, 0.4));
        let key = Cube {
            lo: vec![w[0].min(e[0]), w[1], w[2]],
            hi: vec![on_earth(lat, 0.0)[0], e[1], w[2]],
        };
        // a box holding the key's eastern half: half the stretch
        let east = Cube {
            lo: vec![0.0, 0.0, 0.0],
            hi: vec![EARTH, EARTH, EARTH],
        };
        let half = Region::new(Some((east, true)), None, Some(EARTH)).unwrap();
        assert!(
            (half.share(&key) - 0.5).abs() < 1e-9,
            "{}",
            half.share(&key)
        );
        // a box holding the key from 0.2 degrees west to 0.2 degrees east by its first axis alone:
        // half the stretch, where a quarter of the key's own area
        let middle = Cube {
            lo: vec![on_earth(lat, 0.2)[0], -EARTH, -EARTH],
            hi: vec![EARTH, EARTH, EARTH],
        };
        let half = Region::new(Some((middle, true)), None, Some(EARTH)).unwrap();
        assert!(
            (half.share(&key) - 0.5).abs() < 1e-9,
            "{}",
            half.share(&key)
        );
        // a ball about the key's middle: the longitudes within the distance, over the key's 0.8
        // degrees
        for metres in [5_000.0, 20_000.0] {
            let b = ball(lat, 0.0, metres);
            let region = Region::new(None, Some(b), Some(EARTH)).unwrap();
            let angle = metres / EARTH;
            let p = lat.to_radians();
            let across = ((angle.cos() - p.sin().powi(2)) / p.cos().powi(2)).acos();
            let expected = 2.0 * across / 0.8_f64.to_radians();
            let share = region.share(&key);
            assert!(
                (share - expected).abs() < 1e-9,
                "{metres}: {share} against {expected}"
            );
        }
    }

    #[pg_test]
    fn a_key_whole_inside_or_outside_takes_all_or_none_and_a_point_itself() {
        let b = ball(51.5, 0.0, 10_000.0);
        let region = Region::new(None, Some(b.clone()), Some(EARTH)).unwrap();
        let near = Cube::point(&on_earth(51.5, 0.01));
        let far = Cube::point(&on_earth(51.5, 1.0));
        assert_eq!(region.share(&near), 1.0);
        assert_eq!(region.share(&far), 0.0);
        let inside = Cube {
            lo: on_earth(51.49, -0.01)
                .iter()
                .zip(on_earth(51.51, 0.01))
                .map(|(a, b)| a.min(b))
                .collect(),
            hi: on_earth(51.49, -0.01)
                .iter()
                .zip(on_earth(51.51, 0.01))
                .map(|(a, b)| a.max(b))
                .collect(),
        };
        assert_eq!(region.share(&inside), 1.0);
        // a key with extent on one axis alone: its two ends, one inside
        let s = on_earth(51.5, 0.0);
        let mut end = s.clone();
        end[1] = -end[1] - 50_000.0;
        let ends = Cube {
            lo: vec![s[0], end[1], s[2]],
            hi: vec![s[0], s[1], s[2]],
        };
        assert_eq!(region.share(&ends), 0.5);
    }

    #[pg_test]
    fn a_key_with_no_extent_on_one_axis_off_a_sphere_takes_the_share_of_its_extent() {
        let key = Cube {
            lo: vec![0.0, 5.0],
            hi: vec![10.0, 5.0],
        };
        let half = Cube {
            lo: vec![0.0, 0.0],
            hi: vec![5.0, 10.0],
        };
        let region = Region::new(Some((half, true)), None, None).unwrap();
        assert_eq!(region.share(&key), 0.5);
    }
}
