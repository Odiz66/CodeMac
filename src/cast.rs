//! Perfect casts: watch the power bar while charging and release at the top.
//!
//! The bar is described by two screen points (its empty end and its full end)
//! and the colours of its fill. The filled fraction is measured along the line
//! between the points; each sample also looks at its two neighbours across the
//! line so a 1 px misalignment does not matter.

use crate::capture::{Frame, Rect};
use crate::config::CastCfg;
use crate::detect::Palette;
use crate::tracker::Track;

pub struct CastBar {
    pub rect: Rect,
    /// Sample points relative to `rect`, from the empty end to the full end.
    points: Vec<(u32, u32)>,
    /// Offset to the neighbours across the bar.
    across: (i32, i32),
    palette: Palette,
}

impl CastBar {
    /// `None` when the power bar has not been calibrated.
    pub fn new(cfg: &CastCfg) -> Option<CastBar> {
        if !cfg.is_calibrated() {
            return None;
        }
        Some(Self::from_points(
            (cfg.empty_x, cfg.empty_y),
            (cfg.full_x, cfg.full_y),
            Palette::new(&cfg.fill, cfg.tolerance),
        ))
    }

    pub fn from_points(empty: (i32, i32), full: (i32, i32), palette: Palette) -> CastBar {
        let (dx, dy) = (full.0 - empty.0, full.1 - empty.1);
        let n = dx.abs().max(dy.abs()).max(1) as usize + 1;
        let pad = 2;
        let rect = Rect {
            x: (empty.0.min(full.0) - pad).max(0),
            y: (empty.1.min(full.1) - pad).max(0),
            w: dx.abs() + 2 * pad + 1,
            h: dy.abs() + 2 * pad + 1,
        };
        let points = (0..n)
            .map(|i| {
                let f = i as f32 / (n - 1) as f32;
                let x = empty.0 as f32 + dx as f32 * f;
                let y = empty.1 as f32 + dy as f32 * f;
                (
                    (x.round() as i32 - rect.x) as u32,
                    (y.round() as i32 - rect.y) as u32,
                )
            })
            .collect();
        let across = if dx.abs() >= dy.abs() { (0, 1) } else { (1, 0) };
        CastBar {
            rect,
            points,
            across,
            palette,
        }
    }

    /// Filled fraction of the bar (0..1).
    pub fn fill(&self, frame: &Frame) -> f32 {
        let filled = self
            .points
            .iter()
            .filter(|&&p| self.is_filled(frame, p))
            .count();
        filled as f32 / self.points.len() as f32
    }

    fn is_filled(&self, frame: &Frame, (x, y): (u32, u32)) -> bool {
        let mut hits = 0;
        for k in [-1, 0, 1] {
            let px = x as i32 + self.across.0 * k;
            let py = y as i32 + self.across.1 * k;
            if px >= 0
                && py >= 0
                && (px as u32) < frame.width
                && (py as u32) < frame.height
                && self.palette.matches(frame.px(px as u32, py as u32))
            {
                hits += 1;
            }
        }
        hits >= 2
    }

    /// Sample points in screen coordinates (for drawing).
    pub fn screen_points(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.points
            .iter()
            .map(|&(x, y)| (x as i32 + self.rect.x, y as i32 + self.rect.y))
    }

    /// Pixels at the empty end of the bar (first 15 %), used to learn the fill colour.
    pub fn empty_end_pixels(&self, frame: &Frame) -> Vec<[u8; 3]> {
        let n = (self.points.len() * 15 / 100).max(3).min(self.points.len());
        self.points[..n]
            .iter()
            .filter(|&&(x, y)| x < frame.width && y < frame.height)
            .map(|&(x, y)| frame.px(x, y))
            .collect()
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
    use crate::config::Rgb;

    const FILL: [u8; 3] = [60, 220, 90];

    fn bar() -> CastBar {
        // Vertical bar: empty end at the bottom (y 300), full end at the top (y 200).
        CastBar::from_points((50, 300), (50, 200), Palette::new(&[Rgb(FILL)], 40.0))
    }

    fn draw(b: &CastBar, fill: f32) -> Frame {
        let mut f = Frame::new(b.rect.w as u32, b.rect.h as u32);
        let n = b.points.len();
        let filled = (n as f32 * fill).round() as usize;
        for (i, &(x, y)) in b.points.iter().enumerate() {
            let c = if i < filled { FILL } else { [30, 30, 30] };
            for k in -1..=1 {
                f.set((x as i32 + k) as u32, y, c);
            }
        }
        f
    }

    #[test]
    fn measures_fill_from_the_empty_end() {
        let b = bar();
        assert_eq!(
            b.rect,
            Rect {
                x: 48,
                y: 198,
                w: 5,
                h: 105
            }
        );
        for fill in [0.0, 0.25, 0.5, 1.0] {
            assert!((b.fill(&draw(&b, fill)) - fill).abs() < 0.02);
        }
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
