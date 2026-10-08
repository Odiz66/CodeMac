//! Finds the player bar, the fish and the progress fill in a captured frame.
//!
//! Detection is column based: several rows across the middle of the track are
//! sampled and a column only counts as "bar" or "fish" when enough of those rows
//! agree. Thin slash effects, particles and glow that cover part of the height
//! therefore do not create false columns, and colours are matched against a list
//! of calibrated references with a tolerance instead of exact values.

use anyhow::{Result, bail};

use crate::capture::{Frame, Rect};
use crate::config::{ColorsCfg, Config, DetectionCfg, LayoutCfg, Rgb};

/// Positions relative to the capture rectangle.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub capture: Rect,
    pub track_x0: u32,
    pub track_x1: u32,
    pub track_y0: u32,
    pub track_y1: u32,
    pub progress: Option<ProgressRow>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProgressRow {
    pub y: u32,
    pub x0: u32,
    pub x1: u32,
}

impl Layout {
    pub fn from_config(l: &LayoutCfg) -> Result<Layout> {
        if !l.is_calibrated() {
            bail!(
                "the reel bar position is not calibrated yet; run `cargo run --release -- calibrate` first"
            );
        }
        let progress = (l.progress_y > 0).then(|| {
            let pl = if l.progress_left > 0 {
                l.progress_left
            } else {
                l.track_left
            };
            let pr = if l.progress_right > 0 {
                l.progress_right
            } else {
                l.track_right
            };
            (l.progress_y, pl.min(pr), pl.max(pr))
        });
        let pad = l.padding.max(0);
        let mut left = l.track_left;
        let mut right = l.track_right;
        let mut top = l.track_top;
        let mut bottom = l.track_bottom;
        if let Some((py, pl, pr)) = progress {
            left = left.min(pl);
            right = right.max(pr);
            top = top.min(py - 1);
            bottom = bottom.max(py + 1);
        }
        let x = (left - pad).max(0);
        let y = (top - pad).max(0);
        let capture = Rect {
            x,
            y,
            w: right + pad - x + 1,
            h: bottom + pad - y + 1,
        };
        let rel_x = |v: i32| (v - x) as u32;
        let rel_y = |v: i32| (v - y) as u32;
        Ok(Layout {
            capture,
            track_x0: rel_x(l.track_left),
            track_x1: rel_x(l.track_right),
            track_y0: rel_y(l.track_top),
            track_y1: rel_y(l.track_bottom),
            progress: progress.map(|(py, pl, pr)| ProgressRow {
                y: rel_y(py),
                x0: rel_x(pl),
                x1: rel_x(pr),
            }),
        })
    }

    pub fn track_width(&self) -> f32 {
        (self.track_x1 - self.track_x0 + 1) as f32
    }

    /// Rows sampled for column classification: evenly spread over the middle
    /// `band` fraction of the track height.
    pub fn sample_rows(&self, count: u32, band: f32) -> Vec<u32> {
        let h = (self.track_y1 - self.track_y0) as f32;
        let mid = self.track_y0 as f32 + h / 2.0;
        let half = h * band / 2.0;
        let count = count.max(1);
        let mut rows: Vec<u32> = (0..count)
            .map(|i| {
                let t = if count == 1 {
                    0.5
                } else {
                    i as f32 / (count - 1) as f32
                };
                (mid - half + t * 2.0 * half).round() as u32
            })
            .collect();
        rows.dedup();
        rows
    }
}

/// A set of reference colours with a Euclidean RGB tolerance.
#[derive(Debug, Clone)]
pub struct Palette {
    colors: Vec<[i32; 3]>,
    tol_sq: i32,
}

impl Palette {
    pub fn new(colors: &[Rgb], tolerance: f32) -> Palette {
        Palette {
            colors: colors
                .iter()
                .map(|c| [c.0[0] as i32, c.0[1] as i32, c.0[2] as i32])
                .collect(),
            tol_sq: (tolerance * tolerance).round() as i32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.colors.is_empty()
    }

    #[inline]
    pub fn matches(&self, p: [u8; 3]) -> bool {
        self.colors.iter().any(|c| dist_sq(p, *c) <= self.tol_sq)
    }
}

#[inline]
pub fn dist_sq(p: [u8; 3], c: [i32; 3]) -> i32 {
    let dr = p[0] as i32 - c[0];
    let dg = p[1] as i32 - c[1];
    let db = p[2] as i32 - c[2];
    dr * dr + dg * dg + db * db
}

/// A horizontal extent in track coordinates (0 = left edge of the track).
/// `left` is the left edge of the first pixel, `right` the right edge of the last.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub left: f32,
    pub right: f32,
}

impl Span {
    pub fn center(&self) -> f32 {
        (self.left + self.right) / 2.0
    }
    pub fn width(&self) -> f32 {
        self.right - self.left
    }
}

#[derive(Debug, Clone, Default)]
pub struct Detection {
    pub bar: Option<Span>,
    /// Fish candidates, widest first.
    pub fish: Vec<Span>,
    /// Filled fraction of the progress bar, if a progress row is configured.
    pub progress: Option<f32>,
}

impl Detection {
    pub fn visible(&self) -> bool {
        self.bar.is_some() || !self.fish.is_empty()
    }
}

pub struct Detector {
    pub layout: Layout,
    pub bar: Palette,
    pub fish: Palette,
    pub progress: Palette,
    rows: Vec<u32>,
    min_hits: usize,
    cfg: DetectionCfg,
    bar_cols: Vec<bool>,
    fish_cols: Vec<bool>,
}

impl Detector {
    pub fn new(cfg: &Config) -> Result<Detector> {
        let layout = Layout::from_config(&cfg.layout)?;
        Detector::with_layout(layout, &cfg.colors, &cfg.detection)
    }

    pub fn with_layout(layout: Layout, colors: &ColorsCfg, det: &DetectionCfg) -> Result<Detector> {
        if colors.bar.is_empty() || colors.fish.is_empty() {
            bail!(
                "bar/fish colours are not calibrated yet; run `cargo run --release -- calibrate` first"
            );
        }
        let rows = layout.sample_rows(det.row_samples, det.row_band);
        let min_hits = ((rows.len() as f32 * det.column_min_fraction).ceil() as usize).max(1);
        let n = layout.track_width() as usize;
        Ok(Detector {
            bar: Palette::new(&colors.bar, colors.bar_tolerance),
            fish: Palette::new(&colors.fish, colors.fish_tolerance),
            progress: Palette::new(&colors.progress, colors.progress_tolerance),
            rows,
            min_hits,
            cfg: det.clone(),
            bar_cols: vec![false; n],
            fish_cols: vec![false; n],
            layout,
        })
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[u32] {
        &self.rows
    }

    pub fn detect(&mut self, frame: &Frame) -> Detection {
        let l = &self.layout;
        if frame.width <= l.track_x1 || frame.height <= l.track_y1 {
            return Detection::default();
        }
        for (i, x) in (l.track_x0..=l.track_x1).enumerate() {
            let mut bar_hits = 0;
            let mut fish_hits = 0;
            for &y in &self.rows {
                let p = frame.px(x, y);
                if self.fish.matches(p) {
                    fish_hits += 1;
                } else if self.bar.matches(p) {
                    bar_hits += 1;
                }
            }
            self.fish_cols[i] = fish_hits >= self.min_hits;
            self.bar_cols[i] = bar_hits >= self.min_hits;
        }

        let n = self.bar_cols.len();
        let bar = runs(&self.bar_cols, self.cfg.bar_merge_gap as usize)
            .into_iter()
            .filter(|r| {
                let w = r.end - r.start + 1;
                w >= self.cfg.bar_min_width as usize
                    && (w as f32) <= self.cfg.bar_max_width * n as f32
            })
            .max_by_key(|r| r.covered)
            .map(|r| Span {
                left: r.start as f32,
                right: (r.end + 1) as f32,
            });

        let mut fish: Vec<Span> = runs(&self.fish_cols, 2)
            .into_iter()
            .filter(|r| {
                let w = r.end - r.start + 1;
                w >= self.cfg.fish_min_width as usize && w <= self.cfg.fish_max_width as usize
            })
            .map(|r| Span {
                left: r.start as f32,
                right: (r.end + 1) as f32,
            })
            .collect();
        fish.sort_by(|a, b| b.width().total_cmp(&a.width()));

        let progress = self.read_progress(frame);
        Detection {
            bar,
            fish,
            progress,
        }
    }

    fn read_progress(&self, frame: &Frame) -> Option<f32> {
        let p = self.layout.progress?;
        if self.progress.is_empty() || p.x1 >= frame.width || p.y >= frame.height {
            return None;
        }
        let rows: Vec<u32> = [p.y.saturating_sub(1), p.y, p.y + 1]
            .into_iter()
            .filter(|&y| y < frame.height)
            .collect();
        let need = if rows.len() >= 3 { 2 } else { 1 };
        let filled = (p.x0..=p.x1)
            .filter(|&x| {
                rows.iter()
                    .filter(|&&y| self.progress.matches(frame.px(x, y)))
                    .count()
                    >= need
            })
            .count();
        Some(filled as f32 / (p.x1 - p.x0 + 1) as f32)
    }

    /// Draws the layout and a detection on top of a copy of `frame`.
    pub fn annotate(&self, frame: &Frame, det: &Detection) -> Frame {
        let mut out = frame.clone();
        let l = &self.layout;
        let yellow = [255, 220, 0];
        for x in l.track_x0..=l.track_x1 {
            out.set(x, l.track_y0.saturating_sub(1), yellow);
            out.set(x, l.track_y1 + 1, yellow);
        }
        for &y in &self.rows {
            out.set(l.track_x0.saturating_sub(2), y, [255, 0, 255]);
            out.set(l.track_x1 + 2, y, [255, 0, 255]);
        }
        let vline = |out: &mut Frame, x: f32, c: [u8; 3]| {
            let x = (l.track_x0 as f32 + x).round() as u32;
            for y in l.track_y0..=l.track_y1 {
                out.set(x, y, c);
            }
        };
        if let Some(b) = det.bar {
            vline(&mut out, b.left, [0, 255, 0]);
            vline(&mut out, b.right - 1.0, [0, 255, 0]);
        }
        for (i, f) in det.fish.iter().enumerate() {
            vline(
                &mut out,
                f.center(),
                if i == 0 { [255, 0, 0] } else { [255, 140, 140] },
            );
        }
        if let Some(p) = l.progress {
            let filled = det.progress.unwrap_or(0.0);
            let split = p.x0 + ((p.x1 - p.x0) as f32 * filled) as u32;
            for x in p.x0..=p.x1 {
                out.set(
                    x,
                    p.y + 2,
                    if x <= split {
                        [0, 255, 255]
                    } else {
                        [0, 80, 80]
                    },
                );
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Run {
    pub start: usize,
    pub end: usize,
    pub covered: usize,
}

/// Runs of `true`, merging runs separated by at most `merge_gap` `false` columns.
pub fn runs(cols: &[bool], merge_gap: usize) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let mut i = 0;
    while i < cols.len() {
        if !cols[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < cols.len() && cols[i] {
            i += 1;
        }
        let end = i - 1;
        let len = end - start + 1;
        match out.last_mut() {
            Some(prev) if start - prev.end - 1 <= merge_gap => {
                prev.end = end;
                prev.covered += len;
            }
            _ => out.push(Run {
                start,
                end,
                covered: len,
            }),
        }
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const BG: [u8; 3] = [20, 24, 40];
    pub const BAR: [u8; 3] = [200, 40, 60];
    pub const FISH: [u8; 3] = [240, 240, 250];
    pub const PROG: [u8; 3] = [90, 220, 90];

    /// Capture = 220x49 at (100,495); track x 10..=209 (200 px), y 10..=29; progress row y 37.
    pub fn test_config() -> Config {
        let mut cfg = Config {
            layout: LayoutCfg {
                track_left: 110,
                track_top: 505,
                track_right: 309,
                track_bottom: 524,
                progress_y: 532,
                progress_left: 0,
                progress_right: 0,
                padding: 10,
            },
            ..Default::default()
        };
        cfg.colors.bar = vec![Rgb(BAR)];
        cfg.colors.fish = vec![Rgb(FISH)];
        cfg.colors.progress = vec![Rgb(PROG)];
        cfg
    }

    /// Draws a synthetic minigame. Positions are track-relative pixel columns.
    pub fn render(
        layout: &Layout,
        bar: Option<(u32, u32)>,
        fish: Option<(u32, u32)>,
        progress: f32,
    ) -> Frame {
        let mut f = Frame::new(layout.capture.w as u32, layout.capture.h as u32);
        for y in 0..f.height {
            for x in 0..f.width {
                f.set(x, y, [5, 60, 90]); // "ocean"
            }
        }
        for y in layout.track_y0..=layout.track_y1 {
            for x in layout.track_x0..=layout.track_x1 {
                let tx = x - layout.track_x0;
                let mut c = BG;
                if let Some((a, b)) = bar
                    && tx >= a
                    && tx <= b
                {
                    c = BAR;
                }
                // The fish only covers the middle of the track height.
                let mid = (layout.track_y0 + layout.track_y1) / 2;
                if let Some((a, b)) = fish
                    && tx >= a
                    && tx <= b
                    && y.abs_diff(mid) <= 6
                {
                    c = FISH;
                }
                f.set(x, y, c);
            }
        }
        if let Some(p) = layout.progress {
            let split = p.x0 + ((p.x1 - p.x0 + 1) as f32 * progress) as u32;
            for y in p.y - 1..=p.y + 1 {
                for x in p.x0..=p.x1 {
                    f.set(x, y, if x < split { PROG } else { [40, 40, 40] });
                }
            }
        }
        f
    }

    #[test]
    fn layout_maps_coordinates() {
        let l = Layout::from_config(&test_config().layout).unwrap();
        assert_eq!(
            l.capture,
            Rect {
                x: 100,
                y: 495,
                w: 220,
                h: 49
            }
        );
        assert_eq!(
            (l.track_x0, l.track_x1, l.track_y0, l.track_y1),
            (10, 209, 10, 29)
        );
        assert_eq!(
            l.progress,
            Some(ProgressRow {
                y: 37,
                x0: 10,
                x1: 209
            })
        );
        assert_eq!(l.track_width(), 200.0);
    }

    #[test]
    fn finds_bar_fish_and_progress() {
        let cfg = test_config();
        let mut d = Detector::new(&cfg).unwrap();
        let f = render(&d.layout, Some((50, 79)), Some((60, 64)), 0.25);
        let det = d.detect(&f);
        let bar = det.bar.unwrap();
        assert_eq!((bar.left, bar.right), (50.0, 80.0));
        assert_eq!(det.fish.len(), 1);
        assert_eq!(det.fish[0].center(), 62.5);
        let p = det.progress.unwrap();
        assert!((p - 0.25).abs() < 0.02, "{p}");
    }

    #[test]
    fn bar_split_by_fish_is_merged() {
        let cfg = test_config();
        let mut d = Detector::new(&cfg).unwrap();
        // Fish in the middle of the bar covering most rows -> splits the bar run.
        let mut f = render(&d.layout, Some((40, 99)), None, 0.5);
        for y in d.layout.track_y0..=d.layout.track_y1 {
            for x in 70..76 {
                f.set(d.layout.track_x0 + x, y, FISH);
            }
        }
        let det = d.detect(&f);
        let bar = det.bar.unwrap();
        assert_eq!((bar.left, bar.right), (40.0, 100.0));
        assert_eq!(det.fish[0].center(), 73.0);
    }

    #[test]
    fn partial_height_effects_are_ignored() {
        let cfg = test_config();
        let mut d = Detector::new(&cfg).unwrap();
        let mut f = render(&d.layout, Some((20, 39)), Some((150, 154)), 0.5);
        // A thin bar-coloured slash across the track only touches one sampled row.
        let y = d.rows()[0];
        for x in 100..140 {
            f.set(d.layout.track_x0 + x, y, BAR);
        }
        let det = d.detect(&f);
        assert_eq!(det.bar.unwrap().left, 20.0);
        assert_eq!(det.bar.unwrap().right, 40.0);
    }

    #[test]
    fn nothing_visible_without_minigame() {
        let cfg = test_config();
        let mut d = Detector::new(&cfg).unwrap();
        let f = Frame::new(d.layout.capture.w as u32, d.layout.capture.h as u32);
        let det = d.detect(&f);
        assert!(!det.visible());
    }

    #[test]
    fn tolerance_absorbs_tint() {
        let mut cfg = test_config();
        cfg.colors.bar_tolerance = 30.0;
        let mut d = Detector::new(&cfg).unwrap();
        let mut f = render(&d.layout, Some((10, 29)), None, 0.0);
        for y in d.layout.track_y0..=d.layout.track_y1 {
            for x in 10..30 {
                f.set(d.layout.track_x0 + x, y, [215, 52, 70]); // tinted bar
            }
        }
        assert!(d.detect(&f).bar.is_some());
    }

    #[test]
    fn run_merging() {
        let cols = [
            true, true, false, false, true, false, false, false, false, true,
        ];
        let r = runs(&cols, 2);
        assert_eq!(
            r,
            vec![
                Run {
                    start: 0,
                    end: 4,
                    covered: 3
                },
                Run {
                    start: 9,
                    end: 9,
                    covered: 1
                }
            ]
        );
    }
}
