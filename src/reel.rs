//! Per-minigame logic without any I/O: turns detections into hold/release
//! decisions, decides when the minigame is over and keeps the statistics that
//! tell whether the catch looked perfect.

use crate::config::Config;
use crate::controller::{Controller, CtlInput, CtlOutput};
use crate::detect::{Detection, Span};
use crate::tracker::Track;

/// Outside-the-bar frames and progress drops this close to the end of the
/// minigame are ignored: they come from the catch / fade-out animation.
const TAIL_IGNORE_S: f64 = 0.12;
/// A fish track that has not been updated for this long is not used for gating.
const STALE_TRACK_S: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// Bar and fish disappeared.
    Vanished,
    /// The fish has not been seen for `fish_lost_end_ms`.
    FishLost,
    Timeout,
}

#[derive(Debug, Clone, Default)]
pub struct ReelStats {
    pub frames: u32,
    pub duplicates: u32,
    /// Frames where both bar and fish were measured.
    pub measured: u32,
    pub outside: u32,
    pub longest_outside_frames: u32,
    pub worst_outside_px: f32,
    pub min_margin_px: f32,
    pub progress_drops: u32,
    pub last_progress: Option<f32>,
    pub max_progress: f32,
    pub hold_frames: u32,
    pub guard_frames: u32,
    pub duration_s: f64,
    /// Average time between captured frames.
    pub frame_ms: f32,
    outside_events: Vec<(f64, f32)>,
    drop_events: Vec<f64>,
    outside_streak: u32,
    progress_ref: Option<f32>,
    drop_candidate: u32,
}

impl ReelStats {
    /// The fish never left the bar and the progress never went down.
    pub fn perfect(&self) -> bool {
        self.measured > 0 && self.outside == 0 && self.progress_drops == 0
    }

    pub fn inside_ratio(&self) -> f32 {
        if self.measured == 0 {
            0.0
        } else {
            1.0 - self.outside as f32 / self.measured as f32
        }
    }

    /// Caught (Some(true)), lost (Some(false)) or unknown without a progress bar.
    pub fn caught(&self) -> Option<bool> {
        self.last_progress
            .map(|p| p >= 0.8 || self.max_progress >= 0.97)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Step {
    pub hold: bool,
    pub ctl: CtlOutput,
    pub bar: Option<Span>,
    pub fish: Option<f32>,
    pub fish_pred: Option<f32>,
    pub bar_pred: Option<f32>,
    pub inside: Option<bool>,
    pub ended: Option<EndReason>,
    pub lookahead_ms: f32,
}

pub struct Reel {
    track_w: f32,
    fish: Track,
    bar: Track,
    ctl: Controller,
    lookahead_s: f64,
    lookahead_frames: f64,
    /// Smoothed time between captured frames.
    frame_dt: Option<f64>,
    last_frame_t: Option<f64>,
    coast_s: f64,
    accel_weight: f64,
    max_jump_px: f32,
    end_s: f64,
    fish_lost_s: f64,
    timeout_s: f64,
    progress_drop: f32,
    pending_far: Option<f32>,
    bar_w_ema: Option<f32>,
    last_bar_w: f32,
    last_visible: f64,
    last_fish: f64,
    pub stats: ReelStats,
}

impl Reel {
    pub fn new(cfg: &Config, track_w: f32) -> Reel {
        let c = &cfg.controller;
        let d = &cfg.detection;
        Reel {
            track_w,
            fish: Track::new(c.fish_history, 2),
            bar: Track::new(c.bar_history, 1),
            ctl: Controller::new(c),
            lookahead_s: c.lookahead_ms as f64 / 1000.0,
            lookahead_frames: c.lookahead_frames as f64,
            frame_dt: None,
            last_frame_t: None,
            coast_s: c.coast_ms as f64 / 1000.0,
            accel_weight: c.fish_accel_weight as f64,
            max_jump_px: d.fish_max_jump * track_w,
            end_s: d.end_ms as f64 / 1000.0,
            fish_lost_s: d.fish_lost_end_ms as f64 / 1000.0,
            timeout_s: cfg.timing.reel_timeout_ms as f64 / 1000.0,
            progress_drop: d.progress_drop,
            pending_far: None,
            bar_w_ema: None,
            last_bar_w: 0.0,
            last_visible: 0.0,
            last_fish: 0.0,
            stats: ReelStats {
                min_margin_px: f32::INFINITY,
                ..Default::default()
            },
        }
    }

    /// Processes one frame captured at `t` seconds since the minigame started.
    pub fn step(&mut self, t: f64, det: &Detection, duplicate: bool) -> Step {
        let mut step = Step::default();
        self.stats.frames += 1;
        if duplicate {
            self.stats.duplicates += 1;
        }

        let fish = if duplicate {
            None
        } else {
            self.pick_fish(t, &det.fish)
        };
        let bar = det.bar.map(|b| self.fix_occluded_bar(b, fish));

        if !duplicate {
            if let Some(f) = fish {
                self.fish.push(t, f.center() as f64);
            }
            if let Some(b) = bar {
                self.bar.push(t, b.center() as f64);
                self.last_bar_w = b.width();
            }
        }
        if det.visible() {
            self.last_visible = t;
        }
        if !det.fish.is_empty() {
            self.last_fish = t;
        }
        step.bar = bar;
        step.fish = fish.map(|f| f.center());

        if !duplicate {
            self.record(t, det, bar, fish, &mut step);
        }

        // Control.
        if !duplicate {
            if let Some(prev) = self.last_frame_t {
                let dt = t - prev;
                if dt > 0.001 && dt < 0.1 {
                    self.frame_dt = Some(self.frame_dt.map_or(dt, |f| 0.9 * f + 0.1 * dt));
                }
            }
            self.last_frame_t = Some(t);
        }
        let frame_dt = self.frame_dt.unwrap_or(1.0 / 60.0);
        let lookahead = self.lookahead_s + self.lookahead_frames * frame_dt;
        step.lookahead_ms = (lookahead * 1000.0) as f32;
        let target = t + lookahead;
        let predict = |track: &Track, accel: f64| {
            track.motion().map(|m| {
                if t - m.t > self.coast_s {
                    (m.x, 0.0)
                } else {
                    m.at(target, accel, self.coast_s + lookahead)
                }
            })
        };
        let fish_pred = predict(&self.fish, self.accel_weight);
        let bar_pred = predict(&self.bar, 0.0);
        step.ctl = match (fish_pred, bar_pred) {
            (Some((fx, fv)), Some((bx, bv))) => {
                let fx = (fx as f32).clamp(0.0, self.track_w);
                step.fish_pred = Some(fx);
                step.bar_pred = Some(bx as f32);
                self.ctl.update(&CtlInput {
                    fish_x: fx,
                    fish_v: fv as f32,
                    bar_x: bx as f32,
                    bar_v: bv as f32,
                    bar_w: self.last_bar_w,
                    track_w: self.track_w,
                })
            }
            _ => self.ctl.idle(),
        };
        step.hold = step.ctl.hold;
        if step.hold {
            self.stats.hold_frames += 1;
        }
        if step.ctl.guard {
            self.stats.guard_frames += 1;
        }

        step.ended = if t - self.last_visible > self.end_s {
            Some(EndReason::Vanished)
        } else if t - self.last_fish > self.fish_lost_s {
            Some(EndReason::FishLost)
        } else if t > self.timeout_s {
            Some(EndReason::Timeout)
        } else {
            None
        };
        step
    }

    /// Picks the fish candidate that is consistent with the motion so far.
    /// A detection far from the prediction must show up twice in a row.
    fn pick_fish(&mut self, t: f64, candidates: &[Span]) -> Option<Span> {
        let first = *candidates.first()?;
        let pred = self
            .fish
            .motion()
            .filter(|m| t - m.t < STALE_TRACK_S)
            .map(|m| m.at(t, self.accel_weight, self.coast_s).0 as f32);
        let Some(pred) = pred else {
            self.fish.clear();
            self.pending_far = None;
            return Some(first);
        };
        let best = *candidates
            .iter()
            .min_by(|a, b| {
                (a.center() - pred)
                    .abs()
                    .total_cmp(&(b.center() - pred).abs())
            })
            .expect("non-empty");
        if (best.center() - pred).abs() <= self.max_jump_px {
            self.pending_far = None;
            return Some(best);
        }
        let confirm_px = (self.max_jump_px / 4.0).max(8.0);
        match self.pending_far {
            Some(p) if (best.center() - p).abs() <= confirm_px => {
                self.pending_far = None;
                self.fish.clear();
                Some(best)
            }
            _ => {
                self.pending_far = Some(best.center());
                None
            }
        }
    }

    /// When the fish sits on an edge of the bar it hides that edge. If the bar
    /// suddenly looks narrower and the fish touches one of its ends, rebuild it
    /// from the visible end and the recent width.
    fn fix_occluded_bar(&mut self, bar: Span, fish: Option<Span>) -> Span {
        let touches = |f: &Span, edge: f32| f.left - 3.0 <= edge && f.right + 3.0 >= edge;
        if let (Some(ema), Some(f)) = (self.bar_w_ema, fish)
            && bar.width() < 0.85 * ema
        {
            if touches(&f, bar.right) {
                return Span {
                    left: bar.left,
                    right: bar.left + ema,
                };
            }
            if touches(&f, bar.left) {
                return Span {
                    left: bar.right - ema,
                    right: bar.right,
                };
            }
        }
        // Only learn the width from unobstructed views; it shrinks slowly during the reel.
        if fish.is_none_or(|f| !touches(&f, bar.left) && !touches(&f, bar.right)) {
            self.bar_w_ema = Some(match self.bar_w_ema {
                Some(e) => 0.7 * e + 0.3 * bar.width(),
                None => bar.width(),
            });
        }
        bar
    }

    fn record(
        &mut self,
        t: f64,
        det: &Detection,
        bar: Option<Span>,
        fish: Option<Span>,
        step: &mut Step,
    ) {
        let s = &mut self.stats;
        if let (Some(b), Some(f)) = (bar, fish) {
            s.measured += 1;
            let c = f.center();
            let margin = (c - b.left).min(b.right - c);
            s.min_margin_px = s.min_margin_px.min(margin);
            let inside = margin >= 0.0;
            step.inside = Some(inside);
            if inside {
                s.outside_streak = 0;
            } else {
                s.outside_streak += 1;
                s.outside_events.push((t, -margin));
            }
        }
        if det.visible()
            && let Some(p) = det.progress
        {
            s.last_progress = Some(p);
            s.max_progress = s.max_progress.max(p);
            match s.progress_ref {
                Some(r) if p < r - self.progress_drop => {
                    s.drop_candidate += 1;
                    if s.drop_candidate >= 2 {
                        s.drop_events.push(t);
                        s.progress_ref = Some(p);
                        s.drop_candidate = 0;
                    }
                }
                Some(r) => {
                    s.drop_candidate = 0;
                    s.progress_ref = Some(r.max(p));
                }
                None => s.progress_ref = Some(p),
            }
        }
    }

    /// Finalises the statistics once the minigame has ended.
    pub fn finish(&mut self) -> ReelStats {
        let cutoff = self.last_visible - TAIL_IGNORE_S;
        let frame_dt = self.frame_dt.unwrap_or(1.0 / 60.0);
        let s = &mut self.stats;
        s.duration_s = self.last_visible;
        s.frame_ms = (frame_dt * 1000.0) as f32;
        let mut longest = 0;
        let mut streak = 0;
        let mut prev_t = f64::NEG_INFINITY;
        let mut worst: f32 = 0.0;
        let mut counted = 0;
        for &(t, px) in s.outside_events.iter().filter(|(t, _)| *t < cutoff) {
            counted += 1;
            worst = worst.max(px);
            // Consecutive events less than ~1.5 frames apart form one streak.
            streak = if t - prev_t < 1.5 * frame_dt {
                streak + 1
            } else {
                1
            };
            longest = longest.max(streak);
            prev_t = t;
        }
        s.outside = counted;
        s.longest_outside_frames = longest;
        s.worst_outside_px = worst;
        s.progress_drops = s.drop_events.iter().filter(|t| **t < cutoff).count() as u32;
        if !s.min_margin_px.is_finite() {
            s.min_margin_px = 0.0;
        }
        s.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn det(bar: Option<(f32, f32)>, fish: Option<(f32, f32)>, progress: Option<f32>) -> Detection {
        Detection {
            bar: bar.map(|(l, r)| Span { left: l, right: r }),
            fish: fish
                .map(|(l, r)| Span { left: l, right: r })
                .into_iter()
                .collect(),
            progress,
        }
    }

    #[test]
    fn perfect_when_fish_stays_inside() {
        let cfg = Config::default();
        let mut r = Reel::new(&cfg, 1000.0);
        for i in 0..120 {
            let t = i as f64 / 60.0;
            let s = r.step(
                t,
                &det(
                    Some((400.0, 460.0)),
                    Some((425.0, 435.0)),
                    Some(0.3 + t as f32 * 0.3),
                ),
                false,
            );
            assert_eq!(s.ended, None);
        }
        let mut t = 2.0;
        loop {
            t += 1.0 / 60.0;
            if r.step(t, &det(None, None, None), false).ended.is_some() {
                break;
            }
        }
        let s = r.finish();
        assert!(s.perfect());
        assert_eq!(s.caught(), Some(true));
        assert_eq!(s.measured, 120);
    }

    #[test]
    fn counts_outside_frames_and_drops() {
        let cfg = Config::default();
        let mut r = Reel::new(&cfg, 1000.0);
        for i in 0..60 {
            let t = i as f64 / 60.0;
            let fish_x = if (20..25).contains(&i) { 470.0 } else { 430.0 };
            let progress = if i < 30 { 0.5 } else { 0.4 };
            r.step(
                t,
                &det(
                    Some((400.0, 460.0)),
                    Some((fish_x - 5.0, fish_x + 5.0)),
                    Some(progress),
                ),
                false,
            );
        }
        let s = r.finish();
        assert!(!s.perfect());
        assert_eq!(s.outside, 5);
        assert_eq!(s.longest_outside_frames, 5);
        assert_eq!(s.progress_drops, 1);
        assert!((s.worst_outside_px - 10.0).abs() < 1e-3);
    }

    #[test]
    fn single_far_detection_is_not_trusted() {
        let cfg = Config::default();
        let mut r = Reel::new(&cfg, 1000.0);
        for i in 0..10 {
            r.step(
                i as f64 / 60.0,
                &det(Some((400.0, 460.0)), Some((425.0, 435.0)), None),
                false,
            );
        }
        // A one-frame glitch 400 px away is ignored...
        let s = r.step(
            10.0 / 60.0,
            &det(Some((400.0, 460.0)), Some((825.0, 835.0)), None),
            false,
        );
        assert_eq!(s.fish, None);
        // ...but accepted when it persists.
        let s = r.step(
            11.0 / 60.0,
            &det(Some((400.0, 460.0)), Some((826.0, 836.0)), None),
            false,
        );
        assert_eq!(s.fish, Some(831.0));
    }

    #[test]
    fn ends_when_fish_lost() {
        let cfg = Config::default();
        let mut r = Reel::new(&cfg, 1000.0);
        let mut t = 0.0;
        let mut ended = None;
        for _ in 0..400 {
            t += 1.0 / 60.0;
            ended = r
                .step(t, &det(Some((400.0, 460.0)), None, None), false)
                .ended;
            if ended.is_some() {
                break;
            }
        }
        assert_eq!(ended, Some(EndReason::FishLost));
        assert!(t > 1.9 && t < 2.1, "{t}");
    }

    #[test]
    fn rebuilds_bar_edge_hidden_by_fish() {
        let cfg = Config::default();
        let mut r = Reel::new(&cfg, 1000.0);
        for i in 0..5 {
            r.step(
                i as f64 / 60.0,
                &det(Some((400.0, 460.0)), Some((425.0, 435.0)), None),
                false,
            );
        }
        // Fish covers the right edge: the bar appears to end at 450.
        let s = r.step(
            5.0 / 60.0,
            &det(Some((400.0, 450.0)), Some((450.0, 462.0)), None),
            false,
        );
        let b = s.bar.unwrap();
        assert!((b.right - 460.0).abs() < 0.5, "{b:?}");
    }
}
