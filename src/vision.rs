//! Reading the minigame state out of a captured frame.

use crate::capture::Frame;
use crate::config::{Axis, ColorTarget, Config, Rgb};

/// Where something sits along the bar, as fractions 0.0..=1.0 of its length.
/// 0.0 is the end the zone moves toward when the button is released.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub lo: f32,
    pub hi: f32,
}

impl Span {
    pub fn center(&self) -> f32 {
        (self.lo + self.hi) / 2.0
    }
}

#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub active_pixels: usize,
    pub done_pixels: usize,
    pub marker_pixels: usize,
    pub zone_pixels: usize,
    /// The minigame UI is on screen.
    pub minigame_visible: bool,
    /// The configured "catch succeeded" color is on screen.
    pub catch_done: bool,
    /// Marker (fish) position along the bar.
    pub marker: Option<f32>,
    /// Extent of your controllable zone along the bar.
    pub zone: Option<Span>,
}

#[derive(Debug, Clone)]
pub struct Detector {
    active: Option<ColorTarget>,
    done: Option<ColorTarget>,
    marker: Option<ColorTarget>,
    zone: Option<ColorTarget>,
    axis: Axis,
    invert: bool,
}

impl Detector {
    pub fn from_config(cfg: &Config) -> Detector {
        Detector {
            active: cfg.active,
            done: cfg.done,
            marker: cfg.track.marker,
            zone: cfg.track.zone,
            axis: cfg.track.axis,
            invert: cfg.track.invert,
        }
    }

    pub fn analyze(&self, frame: &Frame) -> Analysis {
        let len = match self.axis {
            Axis::Horizontal => frame.width,
            Axis::Vertical => frame.height,
        };
        let mut marker_hist = vec![0u32; if self.marker.is_some() { len } else { 0 }];
        let mut zone_hist = vec![0u32; if self.zone.is_some() { len } else { 0 }];
        let mut a = Analysis::default();

        for y in 0..frame.height {
            for x in 0..frame.width {
                let px = frame.pixel(x, y);
                if self.active.is_some_and(|t| t.matches(px)) {
                    a.active_pixels += 1;
                }
                if self.done.is_some_and(|t| t.matches(px)) {
                    a.done_pixels += 1;
                }
                let is_marker = self.marker.is_some_and(|t| t.matches(px));
                let is_zone = self.zone.is_some_and(|t| t.matches(px));
                if !is_marker && !is_zone {
                    continue;
                }
                // Index along the bar, growing in the direction holding moves the zone.
                let mut i = match self.axis {
                    Axis::Horizontal => x,
                    Axis::Vertical => frame.height - 1 - y,
                };
                if self.invert {
                    i = len - 1 - i;
                }
                if is_marker {
                    marker_hist[i] += 1;
                    a.marker_pixels += 1;
                }
                if is_zone {
                    zone_hist[i] += 1;
                    a.zone_pixels += 1;
                }
            }
        }

        let scale = (len.max(2) - 1) as f32;
        if self.marker.is_some_and(|t| a.marker_pixels >= t.min_pixels) {
            a.marker = Some(percentile(&marker_hist, a.marker_pixels, 0.5) as f32 / scale);
        }
        if self.zone.is_some_and(|t| a.zone_pixels >= t.min_pixels) {
            // 5th..95th percentile: ignores a few stray matching pixels elsewhere
            // and still spans the zone when the marker covers part of it.
            a.zone = Some(Span {
                lo: percentile(&zone_hist, a.zone_pixels, 0.05) as f32 / scale,
                hi: percentile(&zone_hist, a.zone_pixels, 0.95) as f32 / scale,
            });
        }
        a.minigame_visible = match self.active {
            Some(t) => a.active_pixels >= t.min_pixels,
            None => a.marker.is_some() || a.zone.is_some(),
        };
        a.catch_done = self.done.is_some_and(|t| a.done_pixels >= t.min_pixels);
        a
    }
}

/// Smallest index whose cumulative count reaches `q` of `total`.
fn percentile(hist: &[u32], total: usize, q: f32) -> usize {
    let target = ((q * total as f32).ceil() as usize).clamp(1, total.max(1));
    let mut seen = 0usize;
    for (i, &count) in hist.iter().enumerate() {
        seen += count as usize;
        if seen >= target {
            return i;
        }
    }
    hist.len().saturating_sub(1)
}

/// The most common colors in a frame (similar shades grouped), most frequent
/// first, as (average color, pixel count). Used by `fishmacro calibrate`.
pub fn dominant_colors(frame: &Frame, limit: usize) -> Vec<(Rgb, usize)> {
    // 5 bits per channel = 32768 buckets.
    let mut buckets = vec![(0usize, 0u64, 0u64, 0u64); 1 << 15];
    for px in frame.rgb.chunks_exact(3) {
        let key = ((px[0] as usize >> 3) << 10) | ((px[1] as usize >> 3) << 5) | (px[2] as usize >> 3);
        let b = &mut buckets[key];
        b.0 += 1;
        b.1 += px[0] as u64;
        b.2 += px[1] as u64;
        b.3 += px[2] as u64;
    }
    let mut colors: Vec<(Rgb, usize)> = buckets
        .into_iter()
        .filter(|b| b.0 > 0)
        .map(|(n, r, g, b)| {
            let avg = |sum: u64| (sum / n as u64) as u8;
            (Rgb { r: avg(r), g: avg(g), b: avg(b) }, n)
        })
        .collect();
    colors.sort_by_key(|c| std::cmp::Reverse(c.1));
    colors.truncate(limit);
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: [u8; 3] = [255, 255, 255];
    const GREEN: [u8; 3] = [0, 200, 0];
    const RED: [u8; 3] = [220, 30, 30];
    const GREY: [u8; 3] = [40, 40, 40];

    fn target(c: [u8; 3]) -> Option<ColorTarget> {
        Some(ColorTarget { color: Rgb { r: c[0], g: c[1], b: c[2] }, tolerance: 10, min_pixels: 2 })
    }

    fn detector(axis: Axis, invert: bool) -> Detector {
        Detector { active: target(GREY), done: target(WHITE), marker: target(RED), zone: target(GREEN), axis, invert }
    }

    /// 101 px wide, 4 px tall bar: grey background, green zone over x in
    /// `zone`, red marker at column `marker`.
    fn bar(zone: std::ops::Range<usize>, marker: usize) -> Frame {
        let (width, height) = (101, 4);
        let mut rgb = Vec::new();
        for _y in 0..height {
            for x in 0..width {
                let px = if x == marker {
                    RED
                } else if zone.contains(&x) {
                    GREEN
                } else {
                    GREY
                };
                rgb.extend_from_slice(&px);
            }
        }
        Frame { width, height, rgb }
    }

    #[test]
    fn finds_marker_and_zone_horizontally() {
        let a = detector(Axis::Horizontal, false).analyze(&bar(20..41, 70));
        assert!(a.minigame_visible);
        assert!(!a.catch_done);
        assert!((a.marker.unwrap() - 0.70).abs() < 0.011);
        let zone = a.zone.unwrap();
        assert!(zone.lo >= 0.20 && zone.lo <= 0.22, "{zone:?}");
        assert!(zone.hi >= 0.38 && zone.hi <= 0.40, "{zone:?}");
    }

    #[test]
    fn marker_inside_zone_keeps_zone_extent() {
        let a = detector(Axis::Horizontal, false).analyze(&bar(20..41, 30));
        let zone = a.zone.unwrap();
        assert!((zone.center() - 0.30).abs() < 0.02, "{zone:?}");
        assert!((a.marker.unwrap() - 0.30).abs() < 0.011);
    }

    #[test]
    fn invert_flips_positions() {
        let a = detector(Axis::Horizontal, true).analyze(&bar(20..41, 70));
        assert!((a.marker.unwrap() - 0.30).abs() < 0.011);
    }

    #[test]
    fn vertical_axis_counts_from_bottom() {
        // 1 px wide, 11 px tall; marker at the top row, zone at the bottom.
        let mut rgb = Vec::new();
        for y in 0..11 {
            rgb.extend_from_slice(match y {
                0 => &RED,
                9 | 10 => &GREEN,
                _ => &GREY,
            });
        }
        let mut d = detector(Axis::Vertical, false);
        d.marker = Some(ColorTarget { min_pixels: 1, ..d.marker.unwrap() });
        let a = d.analyze(&Frame { width: 1, height: 11, rgb });
        assert_eq!(a.marker, Some(1.0));
        assert!(a.zone.unwrap().center() < 0.11);
    }

    #[test]
    fn hidden_minigame_and_done_banner() {
        let blank = Frame { width: 10, height: 2, rgb: vec![0; 60] };
        let a = detector(Axis::Horizontal, false).analyze(&blank);
        assert!(!a.minigame_visible);
        assert!(a.marker.is_none() && a.zone.is_none());

        let banner = Frame { width: 10, height: 2, rgb: vec![255; 60] };
        assert!(detector(Axis::Horizontal, false).analyze(&banner).catch_done);
    }

    #[test]
    fn dominant_colors_sorted_by_count() {
        let colors = dominant_colors(&bar(20..41, 70), 3);
        assert_eq!(colors[0].0, Rgb { r: 40, g: 40, b: 40 });
        assert_eq!(colors[1].0, Rgb { r: 0, g: 200, b: 0 });
        assert_eq!(colors[1].1, 21 * 4);
    }
}
