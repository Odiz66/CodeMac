//! Perfect casts: find the power bar while charging and release at the top.
//!
//! Fisch draws the power bar next to the character: a thin vertical bar with
//! a green cap at the top and a white fill growing up from the bottom. It is
//! attached to the character, so it shifts a little whenever the camera
//! moves. Instead of fixed coordinates the bar is searched for in an area:
//! first the green cap, then the white fill in the column below it.

use crate::capture::{Frame, Rect};
use crate::config::CastCfg;
use crate::tracker::Track;

/// Green cap at the top of the bar.
fn is_cap(p: [u8; 3]) -> bool {
    let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
    g >= 90 && g - r >= 30 && g - b >= 25
}

/// White fill (bright and colourless, so the cyan rod effects do not count).
fn is_fill(p: [u8; 3]) -> bool {
    let mn = p[0].min(p[1]).min(p[2]);
    let mx = p[0].max(p[1]).max(p[2]);
    mn >= 170 && mx - mn <= 45
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CastReading {
    /// Filled fraction 0..1.
    pub fill: f32,
    /// Cap position (bottom of the cap), relative to the search area.
    pub cap: (u32, u32),
    /// Bottom of the bar, relative to the search area.
    pub bottom: u32,
}

pub struct CastFinder {
    pub rect: Rect,
}

impl CastFinder {
    pub fn new(cfg: &CastCfg) -> CastFinder {
        CastFinder {
            rect: Rect {
                x: cfg.search_x.max(0),
                y: cfg.search_y.max(0),
                w: cfg.search_w.max(16),
                h: cfg.search_h.max(16),
            },
        }
    }

    /// Finds the bar in a frame of `rect`. `bottom_hint` is the lowest bottom
    /// seen so far during this charge (the bottom of the bar never moves while
    /// charging, but a nearly empty fill is hard to see).
    pub fn read(&self, frame: &Frame, bottom_hint: Option<u32>) -> Option<CastReading> {
        let mut cap: Vec<(u32, u32)> = Vec::new();
        for y in 0..frame.height {
            for x in 0..frame.width {
                if is_cap(frame.px(x, y)) {
                    cap.push((x, y));
                }
            }
        }
        if cap.len() < 4 {
            return None;
        }
        let mut xs: Vec<u32> = cap.iter().map(|p| p.0).collect();
        xs.sort_unstable();
        let mid = xs[xs.len() / 2];
        let near: Vec<(u32, u32)> = cap.into_iter().filter(|p| p.0.abs_diff(mid) <= 8).collect();
        if near.len() < 4 {
            return None;
        }
        let mut near_x: Vec<u32> = near.iter().map(|p| p.0).collect();
        near_x.sort_unstable();
        let cap_x = near_x[near_x.len() / 2];
        let cap_y = near.iter().map(|p| p.1).max().unwrap_or(0);

        // Scan the bar column (and its neighbours) from the bottom of the
        // search area upwards: the lowest white pixel is the bottom of the
        // fill, and the white run above it is the fill.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for x in [cap_x.saturating_sub(1), cap_x, cap_x + 1] {
            if x >= frame.width {
                continue;
            }
            let Some(bottom) = (cap_y + 1..frame.height)
                .rev()
                .find(|&y| is_fill(frame.px(x, y)))
            else {
                continue;
            };
            let mut top = bottom;
            while top > cap_y + 1 {
                let up = top - 1;
                let gap_ok = up > cap_y + 1 && is_fill(frame.px(x, up - 1));
                if is_fill(frame.px(x, up)) || gap_ok {
                    top = up;
                } else {
                    break;
                }
            }
            runs.push((bottom, top));
        }
        runs.sort_unstable();
        let (bottom, top) = match runs.get(runs.len() / 2) {
            Some(&(b, t)) => (b.max(bottom_hint.unwrap_or(0)), t),
            None => {
                let b = bottom_hint?;
                (b, b + 1)
            }
        };
        if bottom <= cap_y + 20 {
            return None;
        }
        let len = (bottom - cap_y) as f32;
        let fill = ((bottom + 1).saturating_sub(top) as f32 / len).clamp(0.0, 1.0);
        Some(CastReading {
            fill,
            cap: (cap_x, cap_y),
            bottom,
        })
    }
}

/// Decides when to let go of the mouse button.
pub struct CastWatch {
    release_at: f32,
    lead_s: f64,
    track: Track,
    /// The bar has been seen nearly empty since the button went down, so a
    /// bar still full from the previous cast cannot trigger a release.
    armed: bool,
    seen: bool,
    peak: f32,
    peak_passed: bool,
}

impl CastWatch {
    pub fn new(cfg: &CastCfg) -> CastWatch {
        CastWatch {
            release_at: cfg.release_at,
            lead_s: cfg.lead_ms as f64 / 1000.0,
            track: Track::new(3, 1),
            armed: false,
            seen: false,
            peak: 0.0,
            peak_passed: false,
        }
    }

    /// Feed one measurement taken `t` seconds after pressing; returns true
    /// when the button should be released now.
    pub fn update(&mut self, t: f64, fill: f32) -> bool {
        self.track.push(t, fill as f64);
        if fill < 0.3 {
            self.armed = true;
        }
        if fill > 0.05 && self.armed {
            self.seen = true;
        }
        if !self.seen {
            return false;
        }
        self.peak = self.peak.max(fill);
        // A bar that swings back down before reaching release_at oscillates:
        // aim for just below the peak it actually reaches.
        if self.peak >= 0.5 && fill < self.peak - 0.08 {
            self.peak_passed = true;
        }
        let rate = self.track.motion().map_or(0.0, |m| m.v);
        let predicted = fill as f64 + rate.max(0.0) * self.lead_s;
        if predicted >= self.release_at as f64 {
            return true;
        }
        self.peak_passed && rate > 0.0 && predicted >= (self.peak - 0.03) as f64
    }

    pub fn peak(&self) -> f32 {
        self.peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Night-time background with some noise, a bar with a green cap at
    /// (cap_x, cap_top..cap_top+6) and a 10 px wide body down to `bottom`,
    /// filled from the bottom to `fill`.
    fn scene(cap_x: u32, cap_top: u32, bottom: u32, fill: f32) -> Frame {
        let mut f = Frame::new(420, 660);
        for y in 0..f.height {
            for x in 0..f.width {
                let n = ((x * 7 + y * 13) % 17) as u8;
                f.set(x, y, [30 + n, 28 + n, 70 + n * 2]);
            }
        }
        // Some cyan rod sparkle, which must not count as fill.
        for y in 300..340 {
            for x in 40..60 {
                f.set(x, y, [120, 240, 255]);
            }
        }
        for y in cap_top..cap_top + 6 {
            for x in cap_x - 5..cap_x + 5 {
                f.set(x, y, [81, 141, 66]);
            }
        }
        let body_top = cap_top + 6;
        let len = bottom - (cap_top + 5);
        let fill_top = bottom + 1 - (len as f32 * fill).round() as u32;
        for y in body_top..=bottom {
            for x in cap_x - 5..cap_x + 5 {
                f.set(
                    x,
                    y,
                    if y >= fill_top {
                        [238, 236, 230]
                    } else {
                        [26, 22, 35]
                    },
                );
            }
        }
        f
    }

    #[test]
    fn reads_the_fill() {
        let finder = CastFinder::new(&CastCfg::default());
        for fill in [0.1, 0.25, 0.5, 0.8, 1.0] {
            let r = finder.read(&scene(200, 100, 400, fill), None).unwrap();
            assert!((r.fill - fill).abs() < 0.02, "{fill} -> {}", r.fill);
            assert_eq!(r.bottom, 400);
        }
    }

    #[test]
    fn follows_the_bar_when_the_camera_moves() {
        let finder = CastFinder::new(&CastCfg::default());
        let r = finder.read(&scene(330, 40, 420, 0.5), None).unwrap();
        assert!((r.fill - 0.5).abs() < 0.02);
        assert_eq!(r.cap.0, 330);
    }

    #[test]
    fn nothing_without_a_bar() {
        let finder = CastFinder::new(&CastCfg::default());
        let mut f = scene(200, 100, 400, 0.5);
        for y in 90..420 {
            for x in 190..210 {
                f.set(x, y, [30, 28, 70]);
            }
        }
        assert_eq!(finder.read(&f, None), None);
    }

    #[test]
    fn empty_bar_uses_the_known_bottom() {
        let finder = CastFinder::new(&CastCfg::default());
        let f = scene(200, 100, 400, 0.0);
        assert_eq!(finder.read(&f, None), None);
        let r = finder.read(&f, Some(400)).unwrap();
        assert!(r.fill < 0.01);
    }

    #[test]
    fn releases_just_before_full_with_lead() {
        let cfg = CastCfg {
            release_at: 0.96,
            lead_ms: 20.0,
            ..Default::default()
        };
        let mut w = CastWatch::new(&cfg);
        // Fills at 1.0 per second, sampled at 60 Hz.
        let mut released_at = None;
        for i in 0..120 {
            let t = i as f64 / 60.0;
            if w.update(t, (t as f32).min(1.0)) {
                released_at = Some(t);
                break;
            }
        }
        let t = released_at.unwrap();
        assert!((0.93..=0.96).contains(&t), "{t}");
    }

    #[test]
    fn handles_a_bar_that_never_reaches_release_at() {
        // Oscillates between 0 and 0.9 (e.g. the very top is drawn in another colour).
        let cfg = CastCfg {
            release_at: 0.96,
            lead_ms: 20.0,
            ..Default::default()
        };
        let mut w = CastWatch::new(&cfg);
        let mut released = None;
        for i in 0..400 {
            let t = i as f64 / 60.0;
            let phase = (t % 2.0) as f32;
            let fill = if phase < 1.0 {
                phase * 0.9
            } else {
                (2.0 - phase) * 0.9
            };
            if w.update(t, fill) {
                released = Some((t, fill));
                break;
            }
        }
        let (t, fill) = released.unwrap();
        // Missed the first peak, released on the way up to the second one.
        assert!(t > 2.0 && t < 3.0, "{t}");
        assert!(fill > 0.82, "{fill}");
    }

    #[test]
    fn ignores_a_bar_left_full_by_the_previous_cast() {
        let cfg = CastCfg::default();
        let mut w = CastWatch::new(&cfg);
        // Still full for 5 frames, then the new charge starts from empty.
        for i in 0..5 {
            assert!(!w.update(i as f64 / 60.0, 1.0));
        }
        let mut released = None;
        for i in 5..120 {
            let t = i as f64 / 60.0;
            if w.update(t, ((t - 5.0 / 60.0) as f32).clamp(0.0, 1.0)) {
                released = Some(t);
                break;
            }
        }
        assert!(released.unwrap() > 0.9);
    }

    #[test]
    fn waits_for_the_bar_to_appear() {
        let cfg = CastCfg::default();
        let mut w = CastWatch::new(&cfg);
        for i in 0..30 {
            assert!(!w.update(i as f64 / 60.0, 0.0));
        }
    }
}
