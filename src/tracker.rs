//! Motion estimation from timestamped position samples.
//!
//! A least-squares polynomial (degree 1 or 2) is fitted to the last few
//! samples. Fitting over several frames averages out the ±0.5 px quantisation
//! that plain frame-to-frame differences would amplify, and it handles uneven
//! frame spacing (dropped or duplicated frames) naturally.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Motion {
    /// Position at the time of the newest sample.
    pub x: f64,
    pub v: f64,
    pub a: f64,
    /// Time of the newest sample.
    pub t: f64,
}

impl Motion {
    /// Extrapolates to time `t`. `accel_weight` scales the acceleration term;
    /// `max_dt` caps how far ahead we trust the model.
    pub fn at(&self, t: f64, accel_weight: f64, max_dt: f64) -> (f64, f64) {
        let dt = (t - self.t).clamp(0.0, max_dt);
        let a = self.a * accel_weight;
        (self.x + self.v * dt + 0.5 * a * dt * dt, self.v + a * dt)
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    samples: VecDeque<(f64, f64)>,
    capacity: usize,
    degree: usize,
}

impl Track {
    pub fn new(capacity: usize, degree: usize) -> Track {
        Track {
            samples: VecDeque::with_capacity(capacity + 1),
            capacity: capacity.max(2),
            degree: degree.min(2),
        }
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn push(&mut self, t: f64, x: f64) {
        if let Some(&(lt, _)) = self.samples.back()
            && t <= lt
        {
            return;
        }
        self.samples.push_back((t, x));
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }

    pub fn motion(&self) -> Option<Motion> {
        let &(t0, x0) = self.samples.back()?;
        let n = self.samples.len();
        let degree = self.degree.min(n - 1);
        if degree == 0 {
            return Some(Motion {
                x: x0,
                v: 0.0,
                a: 0.0,
                t: t0,
            });
        }
        // Normal equations in tau = t - t0 (tau <= 0) keep the numbers small.
        let mut s = [0.0f64; 5];
        let mut r = [0.0f64; 3];
        for &(t, x) in &self.samples {
            let tau = t - t0;
            let mut p = 1.0;
            for (k, sk) in s.iter_mut().enumerate() {
                *sk += p;
                if k < 3 {
                    r[k] += x * p;
                }
                p *= tau;
            }
        }
        if degree == 1 {
            let det = s[0] * s[2] - s[1] * s[1];
            if det.abs() < 1e-18 {
                return Some(Motion {
                    x: x0,
                    v: 0.0,
                    a: 0.0,
                    t: t0,
                });
            }
            let c0 = (r[0] * s[2] - r[1] * s[1]) / det;
            let c1 = (s[0] * r[1] - s[1] * r[0]) / det;
            return Some(Motion {
                x: c0,
                v: c1,
                a: 0.0,
                t: t0,
            });
        }
        let m = [[s[0], s[1], s[2]], [s[1], s[2], s[3]], [s[2], s[3], s[4]]];
        match solve3(m, r) {
            Some([c0, c1, c2]) => Some(Motion {
                x: c0,
                v: c1,
                a: 2.0 * c2,
                t: t0,
            }),
            None => {
                let mut lin = self.clone();
                lin.degree = 1;
                lin.motion()
            }
        }
    }
}

fn solve3(m: [[f64; 3]; 3], r: [f64; 3]) -> Option<[f64; 3]> {
    let det3 = |m: &[[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let d = det3(&m);
    if d.abs() < 1e-24 {
        return None;
    }
    let mut out = [0.0; 3];
    for (col, o) in out.iter_mut().enumerate() {
        let mut mc = m;
        for row in 0..3 {
            mc[row][col] = r[row];
        }
        *o = det3(&mc) / d;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_fit_recovers_velocity() {
        let mut t = Track::new(4, 1);
        for i in 0..6 {
            let time = i as f64 / 60.0;
            t.push(time, 100.0 + 300.0 * time);
        }
        let m = t.motion().unwrap();
        assert!((m.v - 300.0).abs() < 1e-6);
        assert!((m.x - (100.0 + 300.0 * 5.0 / 60.0)).abs() < 1e-6);
    }

    #[test]
    fn quadratic_fit_recovers_acceleration() {
        let mut t = Track::new(6, 2);
        for i in 0..8 {
            let time = i as f64 * 0.017 + 3.0;
            let tau = time - 3.0;
            t.push(time, 50.0 + 120.0 * tau + 0.5 * 900.0 * tau * tau);
        }
        let m = t.motion().unwrap();
        let tau = 7.0 * 0.017;
        assert!((m.a - 900.0).abs() < 1e-3, "{}", m.a);
        assert!((m.v - (120.0 + 900.0 * tau)).abs() < 1e-3);
        let (x, _) = m.at(m.t + 0.02, 1.0, 1.0);
        let tau2 = tau + 0.02;
        assert!((x - (50.0 + 120.0 * tau2 + 450.0 * tau2 * tau2)).abs() < 1e-3);
    }

    #[test]
    fn ignores_duplicate_timestamps() {
        let mut t = Track::new(4, 1);
        t.push(1.0, 10.0);
        t.push(1.0, 99.0);
        assert_eq!(t.motion().unwrap().x, 10.0);
    }
}
