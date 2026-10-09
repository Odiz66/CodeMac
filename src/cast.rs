//! Perfect casts: find the power bar while charging and release at the top.
//!
//! Fisch draws the power bar next to the character: a thin vertical bar with
//! a green cap at the top and a white fill growing up from the bottom. It is
//! attached to the character, so it shifts a little whenever the camera
//! moves. Instead of fixed coordinates the bar is searched for in an area:
//! green blobs are candidates for the cap, and a candidate only counts when a
//! white fill of the cap's width sits in the column below it.

use std::collections::VecDeque;

use crate::capture::{Frame, Rect};
use crate::config::CastCfg;

/// Longest bar (cap to bottom) that is searched for, in pixels.
const MAX_BAR_LEN: u32 = 700;
/// After this many frames without the bar a charge counts as "bar gone".
pub const GONE_AFTER_MISSES: u32 = 30;

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
    /// Cap position (centre column, bottom row), relative to the search area.
    pub cap: (u32, u32),
    /// Bottom of the bar, relative to the search area.
    pub bottom: u32,
}

/// A connected group of green pixels.
#[derive(Debug, Clone, Copy)]
struct Blob {
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
    size: u32,
    sum_x: u64,
}

impl Blob {
    fn center_x(&self) -> u32 {
        (self.sum_x / self.size as u64) as u32
    }
    fn width(&self) -> u32 {
        self.max_x - self.min_x + 1
    }
    fn height(&self) -> u32 {
        self.max_y - self.min_y + 1
    }
}

fn green_blobs(frame: &Frame) -> Vec<Blob> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let mut mask: Vec<bool> = (0..w * h)
        .map(|i| is_cap(frame.px((i % w) as u32, (i / w) as u32)))
        .collect();
    let mut blobs = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !mask[start] {
            continue;
        }
        mask[start] = false;
        stack.push(start);
        let (sx, sy) = ((start % w) as u32, (start / w) as u32);
        let mut b = Blob {
            min_x: sx,
            max_x: sx,
            min_y: sy,
            max_y: sy,
            size: 0,
            sum_x: 0,
        };
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            b.size += 1;
            b.sum_x += x as u64;
            b.min_x = b.min_x.min(x as u32);
            b.max_x = b.max_x.max(x as u32);
            b.min_y = b.min_y.min(y as u32);
            b.max_y = b.max_y.max(y as u32);
            for (dx, dy) in [
                (-1i32, 0i32),
                (1, 0),
                (0, -1),
                (0, 1),
                (-1, -1),
                (1, -1),
                (-1, 1),
                (1, 1),
            ] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                    let j = ny as usize * w + nx as usize;
                    if mask[j] {
                        mask[j] = false;
                        stack.push(j);
                    }
                }
            }
        }
        blobs.push(b);
    }
    blobs
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

    /// Finds the bar in a frame of `rect`. `hint` is the cap column and bar
    /// bottom found earlier in this charge: the bar barely moves while
    /// charging, so candidates near it are tried first, readings whose bottom
    /// jumps away from it are rejected, and an empty bar can still be read.
    pub fn read(&self, frame: &Frame, hint: Option<(u32, u32)>) -> Option<CastReading> {
        let mut blobs: Vec<Blob> = green_blobs(frame)
            .into_iter()
            .filter(|b| {
                (4..=3000).contains(&b.size) && (3..=60).contains(&b.width()) && b.height() <= 40
            })
            .collect();
        blobs.sort_by_key(|b| {
            let near = hint.is_some_and(|(hx, _)| b.center_x().abs_diff(hx) <= 12);
            (!near, std::cmp::Reverse(b.size))
        });
        blobs.iter().find_map(|b| self.measure(frame, b, hint))
    }

    /// Checks the column below a green blob for a bar and measures its fill.
    fn measure(&self, frame: &Frame, cap: &Blob, hint: Option<(u32, u32)>) -> Option<CastReading> {
        let cx = cap.center_x();
        let cap_y = cap.max_y;
        let cap_w = cap.width();
        if cap_y + 1 >= frame.height {
            return None;
        }
        let same_bar = hint.filter(|&(hx, _)| hx.abs_diff(cx) <= 12);
        // Once the bottom of this bar is known, nothing below it is searched.
        let mut end = (cap_y + MAX_BAR_LEN).min(frame.height - 1);
        if let Some((_, hb)) = same_bar {
            end = end.min(hb + 8);
        }

        // The fill top is the first row below the cap with a white run about
        // as wide as the cap; anything much wider (clothes, foam) is not the bar.
        let top = (cap_y + 1..=end).find(|&y| self.is_fill_row(frame, cx, y, cap_w));
        let (top, bottom) = match top {
            Some(top) => {
                let mut bottom = top;
                let mut y = top + 1;
                while y <= end {
                    if self.is_fill_row(frame, cx, y, cap_w) {
                        bottom = y;
                    } else if !(y < end && self.is_fill_row(frame, cx, y + 1, cap_w)) {
                        break;
                    }
                    y += 1;
                }
                match same_bar {
                    // The bottom of a bar does not move during a charge: a
                    // white run ending elsewhere is something else.
                    Some((_, hb)) if bottom.abs_diff(hb) > 8 => (hb + 1, hb),
                    _ => (top, bottom),
                }
            }
            None => {
                let (_, hb) = same_bar?;
                (hb + 1, hb)
            }
        };
        if bottom <= cap_y + 20 {
            return None;
        }
        let len = (bottom - cap_y) as f32;
        let fill = ((bottom + 1).saturating_sub(top) as f32 / len).clamp(0.0, 1.0);
        Some(CastReading {
            fill,
            cap: (cx, cap_y),
            bottom,
        })
    }

    fn is_fill_row(&self, frame: &Frame, cx: u32, y: u32, cap_w: u32) -> bool {
        if cx >= frame.width || y >= frame.height || !is_fill(frame.px(cx, y)) {
            return false;
        }
        let limit = cap_w * 2 + 4;
        let mut l = cx;
        while l > 0 && cx - l < limit && is_fill(frame.px(l - 1, y)) {
            l -= 1;
        }
        let mut r = cx;
        while r + 1 < frame.width && r - cx < limit && is_fill(frame.px(r + 1, y)) {
            r += 1;
        }
        let width = r - l + 1;
        width >= (cap_w * 4 / 10).max(2) && width <= cap_w * 16 / 10 + 3
    }
}

/// Decides when to let go of the mouse button.
pub struct CastWatch {
    release_at: f32,
    lead_s: f64,
    /// Recent (time, fill) samples.
    recent: VecDeque<(f64, f32)>,
    /// Consecutive frames that called for a release.
    ready: u32,
    /// The bar has been seen nearly empty since the button went down, so a
    /// bar still full from the previous cast cannot trigger a release.
    armed: bool,
    seen: bool,
    peak: f32,
    peak_passed: bool,
    drops: u32,
}

impl CastWatch {
    pub fn new(cfg: &CastCfg) -> CastWatch {
        CastWatch {
            release_at: cfg.release_at,
            lead_s: cfg.lead_ms as f64 / 1000.0,
            recent: VecDeque::with_capacity(6),
            ready: 0,
            armed: false,
            seen: false,
            peak: 0.0,
            peak_passed: false,
            drops: 0,
        }
    }

    /// Feed one measurement taken `t` seconds after pressing; returns true
    /// when the button should be released now.
    pub fn update(&mut self, t: f64, fill: f32) -> bool {
        self.recent.push_back((t, fill));
        if self.recent.len() > 5 {
            self.recent.pop_front();
        }
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
        // aim for just below the peak it actually reaches. Two frames in a row
        // must show the drop, so one bad reading cannot trigger it.
        if self.peak >= 0.5 && fill < self.peak - 0.08 {
            self.drops += 1;
            if self.drops >= 2 {
                self.peak_passed = true;
            }
        } else {
            self.drops = 0;
        }
        let rate = self.rate();
        // The two-frame confirmation below costs one frame: look that much further.
        let frame_dt = match (self.recent.front(), self.recent.back()) {
            (Some(a), Some(b)) if self.recent.len() > 1 => {
                (b.0 - a.0) / (self.recent.len() - 1) as f64
            }
            _ => 0.0,
        };
        let predicted = fill as f64 + rate.max(0.0) * (self.lead_s + frame_dt);
        let want = predicted >= self.release_at as f64
            || (self.peak_passed
                && self.drops == 0
                && rate > 0.0
                && predicted >= (self.peak - 0.03) as f64);
        // Two frames in a row, so a single wrong reading cannot release early.
        self.ready = if want { self.ready + 1 } else { 0 };
        self.ready >= 2
    }

    /// Fill speed per second: the median of the slopes between all pairs of
    /// recent samples, so one wrong reading barely moves it.
    fn rate(&self) -> f64 {
        let s: Vec<(f64, f32)> = self.recent.iter().copied().collect();
        let mut slopes = Vec::new();
        for i in 0..s.len() {
            for j in i + 1..s.len() {
                let dt = s[j].0 - s[i].0;
                if dt > 1e-6 {
                    slopes.push((s[j].1 - s[i].1) as f64 / dt);
                }
            }
        }
        if slopes.is_empty() {
            return 0.0;
        }
        slopes.sort_by(|a, b| a.total_cmp(b));
        slopes[slopes.len() / 2]
    }

    pub fn peak(&self) -> f32 {
        self.peak
    }
}

/// One charge: finder + release decision, shared by `run` and `debug --cast`
/// so both behave the same.
pub struct CastSession {
    watch: CastWatch,
    hint: Option<(u32, u32)>,
    seen: bool,
    misses: u32,
}

pub struct CastStep {
    pub reading: Option<CastReading>,
    pub release: bool,
}

impl CastSession {
    pub fn new(cfg: &CastCfg) -> CastSession {
        CastSession {
            watch: CastWatch::new(cfg),
            hint: None,
            seen: false,
            misses: 0,
        }
    }

    /// The bar has been found at least once during this charge.
    pub fn seen(&self) -> bool {
        self.seen
    }

    /// Frames in a row without the bar.
    pub fn misses(&self) -> u32 {
        self.misses
    }

    pub fn peak(&self) -> f32 {
        self.watch.peak()
    }

    pub fn update(&mut self, finder: &CastFinder, frame: &Frame, t: f64) -> CastStep {
        let reading = finder.read(frame, self.hint);
        let release = match reading {
            Some(r) => {
                self.hint = Some((r.cap.0, r.bottom));
                self.seen = true;
                self.misses = 0;
                self.watch.update(t, r.fill)
            }
            None => {
                self.misses += 1;
                // Before the bar shows up "not visible" means empty (this arms
                // the watch). Afterwards a missed frame is just skipped: it
                // must not look like the fill dropped to zero.
                !self.seen && self.watch.update(t, 0.0)
            }
        };
        CastStep { reading, release }
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
        rect(&mut f, 40, 300, 20, 40, [120, 240, 255]);
        rect(&mut f, cap_x - 5, cap_top, 10, 6, [81, 141, 66]);
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

    fn rect(f: &mut Frame, x0: u32, y0: u32, w: u32, h: u32, c: [u8; 3]) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                f.set(x, y, c);
            }
        }
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
        assert!(r.cap.0.abs_diff(330) <= 1);
    }

    #[test]
    fn nothing_without_a_bar() {
        let finder = CastFinder::new(&CastCfg::default());
        let mut f = scene(200, 100, 400, 0.5);
        rect(&mut f, 190, 90, 20, 330, [30, 28, 70]);
        assert_eq!(finder.read(&f, None), None);
    }

    #[test]
    fn empty_bar_uses_the_known_bottom() {
        let finder = CastFinder::new(&CastCfg::default());
        let f = scene(200, 100, 400, 0.0);
        assert_eq!(finder.read(&f, None), None);
        let r = finder.read(&f, Some((200, 400))).unwrap();
        assert!(r.fill < 0.01);
    }

    #[test]
    fn green_speck_next_to_the_bar_is_ignored() {
        let finder = CastFinder::new(&CastCfg::default());
        let mut f = scene(200, 100, 400, 0.3);
        rect(&mut f, 206, 330, 2, 2, [70, 150, 60]);
        let r = finder.read(&f, None).unwrap();
        assert!((r.fill - 0.3).abs() < 0.02, "{}", r.fill);
    }

    #[test]
    fn grass_elsewhere_does_not_hide_the_bar() {
        let finder = CastFinder::new(&CastCfg::default());
        let mut f = scene(200, 100, 400, 0.5);
        rect(&mut f, 0, 560, 60, 40, [70, 140, 60]);
        rect(&mut f, 300, 20, 40, 30, [60, 150, 50]);
        let r = finder.read(&f, None).unwrap();
        assert!((r.fill - 0.5).abs() < 0.02, "{}", r.fill);
    }

    #[test]
    fn white_things_below_the_bar_are_ignored() {
        let finder = CastFinder::new(&CastCfg::default());
        // A wide white object (clothes) below the bar.
        let mut f = scene(200, 100, 400, 0.5);
        rect(&mut f, 150, 600, 120, 6, [240, 240, 240]);
        let r = finder.read(&f, None).unwrap();
        assert!((r.fill - 0.5).abs() < 0.02, "{}", r.fill);
        // A bar-wide glint far below is rejected once the bottom is known.
        let mut g = scene(200, 100, 400, 0.0);
        rect(&mut g, 195, 600, 10, 6, [240, 240, 240]);
        let r = finder.read(&g, Some((200, 400))).unwrap();
        assert!(r.fill < 0.01, "{}", r.fill);
    }

    #[test]
    fn a_missed_frame_does_not_release_early() {
        let cfg = CastCfg::default();
        let finder = CastFinder::new(&cfg);
        let mut s = CastSession::new(&cfg);
        let blank = Frame::new(420, 660);
        let mut released_at = None;
        for i in 0..400 {
            let t = i as f64 / 165.0;
            let fill = (t * 0.9) as f32;
            // The bar is missed for two frames at ~33 % and one at ~43 %.
            let frame = if [60, 61, 79].contains(&i) {
                blank.clone()
            } else {
                scene(200, 100, 400, fill.min(1.0))
            };
            if s.update(&finder, &frame, t).release {
                released_at = Some(fill);
                break;
            }
        }
        let fill = released_at.unwrap();
        assert!(fill > 0.9, "released at {fill}");
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
    fn a_single_bad_reading_does_not_count_as_a_peak() {
        let cfg = CastCfg::default();
        let mut w = CastWatch::new(&cfg);
        let mut released = None;
        for i in 0..200 {
            let t = i as f64 / 165.0;
            let mut fill = (t * 0.9) as f32;
            if i == 90 {
                fill = 0.1; // one wrong value at ~49 %
            }
            if w.update(t, fill.min(1.0)) {
                released = Some((t * 0.9) as f32);
                break;
            }
        }
        assert!(released.unwrap() > 0.9, "{released:?}");
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
