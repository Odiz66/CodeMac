//! One reeling session: from the toggle key until the catch is done.

use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use crate::capture::{CaptureThread, Capturer, Sample};
use crate::config::{Config, Mode, SpamConfig};
use crate::hypr;
use crate::input::VirtualInput;
use crate::signals;
use crate::vision::{Analysis, Detector};

/// Upper bound on how long the loop sleeps, so hotkeys are handled promptly.
const POLL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The minigame disappeared from the region.
    Caught,
    /// The configured "done" color appeared.
    DoneColorSeen,
    /// The minigame never showed up after the toggle.
    NeverStarted,
    /// `max_reel_ms` elapsed.
    TimedOut,
    /// Toggle key pressed again.
    Stopped,
    /// Kill-switch key pressed.
    KillSwitch,
    /// Ctrl+C / SIGTERM / SIGHUP.
    Quit,
}

impl Outcome {
    pub fn describe(self) -> &'static str {
        match self {
            Outcome::Caught => "catch done (minigame closed)",
            Outcome::DoneColorSeen => "catch done (done color seen)",
            Outcome::NeverStarted => {
                "minigame never appeared in the region; check it with `fishmacro calibrate` or raise start_timeout_ms"
            }
            Outcome::TimedOut => "gave up after max_reel_ms",
            Outcome::Stopped => "stopped by toggle key",
            Outcome::KillSwitch => "kill switch",
            Outcome::Quit => "exit requested",
        }
    }
}

/// Runs one session. Every button is released before this returns, whatever
/// the outcome. `capturer` is reused across sessions (one long-lived capture
/// connection); it is replaced if the capture thread died.
pub fn reel(cfg: &Config, input: &mut VirtualInput, capturer: &mut Option<Capturer>) -> Result<Outcome, String> {
    if let Some((x, y)) = cfg.cursor {
        hypr::move_cursor(x, y, cfg.hyprland_config)?;
        // Let the game see the pointer at its new position before clicking.
        thread::sleep(Duration::from_millis(30));
    }
    let cap = capturer.take().unwrap_or_else(|| Capturer::new(cfg.capture_backend));
    let capture = CaptureThread::start(cap, cfg.region, Detector::from_config(cfg), cfg.capture_interval);
    let result = run(cfg, input, &capture);
    let released = input.release_all();
    *capturer = capture.finish();
    let outcome = result?;
    released.map_err(|e| format!("failed to release buttons: {e}"))?;
    Ok(outcome)
}

fn run(cfg: &Config, input: &mut VirtualInput, capture: &CaptureThread) -> Result<Outcome, String> {
    let io = |e: std::io::Error| format!("uinput write failed: {e}");
    let start = Instant::now();
    let mut first_seen: Option<Instant> = None;
    let mut last_visible = start;
    // Minigame present in the newest frame.
    let mut visible = false;
    let mut tracker = Tracker::new(cfg.track.deadband, cfg.track.lookahead);
    let mut spam = SpamClock::new(&cfg.spam, start);

    loop {
        if signals::kill_requested() {
            return Ok(Outcome::KillSwitch);
        }
        if signals::quit_requested() {
            return Ok(Outcome::Quit);
        }
        if signals::take_toggle() {
            return Ok(Outcome::Stopped);
        }

        // Only press while the minigame is on screen (or before it first shows
        // up, if configured), so nothing is clicked once the catch is over:
        // in many games a click then casts again.
        let act = match first_seen {
            Some(_) => visible,
            None => cfg.press_before_visible && cfg.mode != Mode::Track,
        };
        let mut next_deadline = Instant::now() + POLL;
        match cfg.mode {
            Mode::Hold => input.set_held(act).map_err(io)?,
            Mode::Spam if act => next_deadline = next_deadline.min(spam.tick(Instant::now(), input).map_err(io)?),
            Mode::Spam => spam.pause(Instant::now(), input).map_err(io)?,
            // While visible, frames drive the button (below).
            Mode::Track if !visible => input.set_held(false).map_err(io)?,
            Mode::Track => {}
        }

        // Wait for the next frame (or the next deadline), then take every frame
        // that is queued so decisions use the newest one.
        let wait = next_deadline.saturating_duration_since(Instant::now());
        let mut samples = Vec::new();
        match capture.rx.recv_timeout(wait) {
            Ok(sample) => samples.push(sample),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("capture thread stopped unexpectedly".into()),
        }
        samples.extend(capture.rx.try_iter());

        for sample in samples {
            let Sample { at, analysis, .. } = sample?;
            if analysis.catch_done {
                return Ok(Outcome::DoneColorSeen);
            }
            visible = analysis.minigame_visible;
            if visible {
                first_seen.get_or_insert(at);
                last_visible = at;
            }
            if cfg.mode == Mode::Track && visible {
                if let Some(hold) = tracker.update(at, &analysis) {
                    input.set_held(hold).map_err(io)?;
                }
            }
        }

        let now = Instant::now();
        match first_seen {
            None if now - start > cfg.start_timeout => return Ok(Outcome::NeverStarted),
            Some(_) if now.saturating_duration_since(last_visible) > cfg.end_grace => return Ok(Outcome::Caught),
            _ => {}
        }
        if now - start > cfg.max_reel {
            return Ok(Outcome::TimedOut);
        }
    }
}

/// Press/release timing for `spam` mode.
struct SpamClock {
    period: Duration,
    press: Duration,
    next_press: Instant,
    release_at: Option<Instant>,
}

impl SpamClock {
    fn new(cfg: &SpamConfig, start: Instant) -> SpamClock {
        SpamClock {
            period: Duration::from_secs_f64(1.0 / cfg.clicks_per_second),
            press: cfg.press,
            next_press: start,
            release_at: None,
        }
    }

    /// Presses or releases if due; returns when it next needs to run.
    fn tick(&mut self, now: Instant, input: &mut VirtualInput) -> std::io::Result<Instant> {
        if let Some(release_at) = self.release_at {
            if now < release_at {
                return Ok(release_at);
            }
            input.set_held(false)?;
            self.release_at = None;
        }
        if now >= self.next_press {
            input.set_held(true)?;
            self.release_at = Some(now + self.press);
            self.next_press += self.period;
            if self.next_press < now {
                // Fell behind (e.g. system stall): don't burst to catch up.
                self.next_press = now + self.period;
            }
            return Ok(now + self.press);
        }
        Ok(self.next_press)
    }

    /// Releases and restarts so the next click comes as soon as clicking resumes.
    fn pause(&mut self, now: Instant, input: &mut VirtualInput) -> std::io::Result<()> {
        input.set_held(false)?;
        self.release_at = None;
        self.next_press = now;
        Ok(())
    }
}

/// Hold/release controller for `track` mode.
///
/// Positions are fractions of the bar; holding pushes the zone toward 1.0.
/// It predicts the marker-zone offset `lookahead` ahead, holds if the marker
/// will be ahead of the zone, releases if behind, and inside the deadband
/// counters the zone's current motion so it settles under the marker.
pub struct Tracker {
    deadband: f32,
    lookahead: f32,
    last: Option<(Instant, f32, f32)>,
    error_velocity: f32,
    zone_velocity: f32,
    marker: Option<f32>,
    zone: Option<f32>,
}

/// Time constant (seconds) of the velocity smoothing. Time-based rather than
/// per-frame, so it behaves the same at 50 captures/s (grim) and at 165/s
/// (native capture on a 165 Hz monitor), where per-frame pixel jitter is
/// relatively larger.
const VELOCITY_TAU: f32 = 0.03;

impl Tracker {
    pub fn new(deadband: f32, lookahead: Duration) -> Tracker {
        Tracker {
            deadband,
            lookahead: lookahead.as_secs_f32(),
            last: None,
            error_velocity: 0.0,
            zone_velocity: 0.0,
            marker: None,
            zone: None,
        }
    }

    /// Returns `Some(true)` to hold, `Some(false)` to release, `None` if the
    /// marker or zone has not been seen yet.
    pub fn update(&mut self, at: Instant, analysis: &Analysis) -> Option<bool> {
        // If one of them is hidden for a frame (e.g. overlapping), reuse its
        // last known position.
        let marker = *self.marker.insert(analysis.marker.or(self.marker)?);
        let zone = *self.zone.insert(analysis.zone.map(|z| z.center()).or(self.zone)?);
        let error = marker - zone;

        match self.last {
            Some((t, last_error, last_zone)) => {
                let dt = at.saturating_duration_since(t).as_secs_f32();
                if dt > 0.001 {
                    let ev = (error - last_error) / dt;
                    let zv = (zone - last_zone) / dt;
                    let alpha = 1.0 - (-dt / VELOCITY_TAU).exp();
                    self.error_velocity += alpha * (ev - self.error_velocity);
                    self.zone_velocity += alpha * (zv - self.zone_velocity);
                    self.last = Some((at, error, zone));
                }
            }
            None => self.last = Some((at, error, zone)),
        }

        let predicted = error + self.error_velocity * self.lookahead;
        Some(if predicted > self.deadband {
            true
        } else if predicted < -self.deadband {
            false
        } else {
            self.zone_velocity < 0.0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::Span;

    fn analysis(marker: f32, zone_center: f32) -> Analysis {
        Analysis {
            marker: Some(marker),
            zone: Some(Span { lo: zone_center - 0.1, hi: zone_center + 0.1 }),
            minigame_visible: true,
            ..Analysis::default()
        }
    }

    #[test]
    fn holds_when_marker_is_ahead_and_releases_when_behind() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(0.03, Duration::ZERO);
        assert_eq!(tracker.update(t0, &analysis(0.8, 0.3)), Some(true));
        let mut tracker = Tracker::new(0.03, Duration::ZERO);
        assert_eq!(tracker.update(t0, &analysis(0.1, 0.6)), Some(false));
    }

    #[test]
    fn brakes_inside_deadband() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(0.05, Duration::ZERO);
        // Zone falling toward 0 while centred on the marker: hold to stop it.
        tracker.update(t0, &analysis(0.5, 0.52));
        assert_eq!(tracker.update(t0 + Duration::from_millis(20), &analysis(0.5, 0.50)), Some(true));
        // Zone rising: release.
        assert_eq!(tracker.update(t0 + Duration::from_millis(40), &analysis(0.5, 0.53)), Some(false));
    }

    #[test]
    fn lookahead_reacts_to_a_fast_marker() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(0.03, Duration::from_millis(200));
        tracker.update(t0, &analysis(0.50, 0.50));
        // Marker moving up 1.0/s: right now it's only 0.02 ahead (inside the
        // deadband), but in 200 ms it will be well ahead, so hold.
        assert_eq!(tracker.update(t0 + Duration::from_millis(20), &analysis(0.52, 0.50)), Some(true));
    }

    #[test]
    fn smoothing_does_not_depend_on_capture_rate() {
        // Marker moving at a constant 0.5/s: after the same 120 ms the velocity
        // estimate must be about the same whether sampled every 6 ms or 20 ms.
        let estimate = |step_ms: u64| {
            let t0 = Instant::now();
            let mut tracker = Tracker::new(0.03, Duration::ZERO);
            for i in 0..=(120 / step_ms) {
                let t = i as f32 * step_ms as f32 / 1000.0;
                tracker.update(t0 + Duration::from_millis(i * step_ms), &analysis(0.2 + 0.5 * t, 0.2));
            }
            tracker.error_velocity
        };
        let (fast, slow) = (estimate(6), estimate(20));
        assert!((fast - slow).abs() < 0.03, "{fast} vs {slow}");
        assert!(fast > 0.45 && fast < 0.51, "{fast}");
    }

    #[test]
    fn reuses_last_position_when_hidden() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(0.03, Duration::ZERO);
        assert_eq!(tracker.update(t0, &Analysis::default()), None);
        tracker.update(t0, &analysis(0.9, 0.2));
        let only_zone = Analysis { marker: None, ..analysis(0.0, 0.2) };
        assert_eq!(tracker.update(t0 + Duration::from_millis(20), &only_zone), Some(true));
    }
}
