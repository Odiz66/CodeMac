//! `run`: the fishing loop.
//!
//! focus Sober -> cast -> UI navigation + Enter until the reel minigame shows
//! up -> play the minigame -> wait for it to disappear -> log -> repeat.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use evdev::KeyCode;

use crate::capture::{self, Capturer, Frame, Rect};
use crate::cast::{CastFinder, CastSession};
use crate::config::{CastMode, Config, NavOff};
use crate::detect::Detector;
use crate::hypr::Hypr;
use crate::input::{Input, parse_key};
use crate::log;
use crate::reel::{EndReason, Reel, ReelStats};
use crate::signals::{self, Flags};
use crate::util::paint;

/// Why the current session stopped without an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    Paused,
    Quit,
}

impl std::fmt::Display for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Interrupt::Paused => "paused",
            Interrupt::Quit => "quit",
        })
    }
}

impl std::error::Error for Interrupt {}

enum Shake {
    Minigame,
    Timeout,
}

#[derive(Default)]
struct Totals {
    casts: u32,
    minigames: u32,
    catches: u32,
    perfect: u32,
    lost: u32,
    shake_timeouts: u32,
    reel_timeouts: u32,
    /// Casts released by watching the power bar / after max_hold_ms.
    bar_casts: u32,
    fallback_casts: u32,
}

struct Keys {
    nav: KeyCode,
    shake: KeyCode,
    reequip: Option<KeyCode>,
}

struct Macro {
    cfg: Config,
    hypr: Hypr,
    input: Arc<Input>,
    flags: Arc<Flags>,
    cap: Box<dyn Capturer>,
    det: Detector,
    rect: Rect,
    cast_bar: Option<CastFinder>,
    keys: Keys,
    nav_on: bool,
    pending_key: Option<(KeyCode, Instant)>,
    totals: Totals,
    csv: Option<File>,
    verbose: bool,
    started: Instant,
}

pub fn run(cfg: Config, start: bool, verbose: bool, dry_run: bool) -> Result<()> {
    let det = Detector::new(&cfg)?;
    let rect = det.layout.capture;
    let keys = Keys {
        nav: parse_key(&cfg.keys.nav_toggle)?,
        shake: parse_key(&cfg.keys.shake)?,
        reequip: if cfg.keys.reequip.trim().is_empty() {
            None
        } else {
            Some(parse_key(&cfg.keys.reequip)?)
        },
    };

    let cast_bar = match cfg.cast.mode {
        CastMode::Timed => None,
        CastMode::Auto | CastMode::Perfect => Some(CastFinder::new(&cfg.cast)),
    };

    let hypr = Hypr::connect(cfg.window.dispatch_style)?;
    log!(
        "{} | dispatch syntax: {}",
        hypr.version().unwrap_or_default(),
        if hypr.uses_lua() { "Lua" } else { "legacy" }
    );
    match hypr.find_client(&cfg.window.class)? {
        Some(c) => log!(
            "Found {} ({}x{}{})",
            c.class,
            c.size[0],
            c.size[1],
            if c.is_fullscreen() {
                ", fullscreen"
            } else {
                ""
            }
        ),
        None => log!(
            "{}",
            paint(
                &format!(
                    "No window with class {:?} yet. Start Sober first.",
                    cfg.window.class
                ),
                "33"
            )
        ),
    }

    let flags = signals::install(start)?;
    let input = if dry_run {
        log!(
            "{}",
            paint("Dry run: no mouse/keyboard input will be sent.", "33")
        );
        Input::dry_run()
    } else {
        let input = Input::create(Duration::from_millis(cfg.timing.input_settle_ms))?;
        log!("Virtual mouse + keyboard ready.");
        input
    };
    log!("Capture region {rect}");
    let cap = capture::open(&cfg.capture, Some(rect))?;

    let csv = open_csv(&cfg.logging.catches_csv)?;
    let verbose = verbose || cfg.logging.verbose_reel;
    match &cast_bar {
        Some(b) => log!("Casting: perfect (looking for the power bar in {})", b.rect),
        None => log!("Casting: timed ({} ms hold)", cfg.timing.cast_hold_ms),
    }
    let mut m = Macro {
        cfg,
        hypr,
        input,
        flags,
        cap,
        det,
        rect,
        cast_bar,
        keys,
        nav_on: false,
        pending_key: None,
        totals: Totals::default(),
        csv,
        verbose,
        started: Instant::now(),
    };
    println!();
    println!("  Toggle:  F6 (bind) or  pkill -USR1 -x fisch-macro");
    println!("  Quit:    Ctrl+C       or  pkill -TERM -x fisch-macro");
    println!("  Make sure Roblox UI navigation is OFF before starting.");
    println!();
    m.main_loop()
}

fn open_csv(path: &str) -> Result<Option<File>> {
    if path.trim().is_empty() {
        return Ok(None);
    }
    let new = !std::path::Path::new(path).exists();
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {path}"))?;
    if new {
        writeln!(
            f,
            "elapsed_s,outcome,perfect,duration_s,frames,measured,outside_frames,longest_outside_frames,\
             worst_outside_px,min_margin_px,progress_drops,final_progress,guard_frames,hold_ratio,frame_ms,\
             mode,kp,kd,lookahead_ms,lookahead_frames,edge_guard,end_reason"
        )?;
    }
    Ok(Some(f))
}

impl Macro {
    fn main_loop(&mut self) -> Result<()> {
        let mut announced = false;
        loop {
            if self.flags.quitting() {
                break;
            }
            if !self.flags.enabled() {
                if !announced {
                    log!("{}", paint("Paused. Press F6 to start.", "36"));
                    announced = true;
                }
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            announced = false;
            log!("{}", paint("Started.", "32"));
            self.input.arm(true);
            let result = self.session();
            self.input.arm(false);
            self.cleanup();
            match result {
                Ok(()) => {}
                Err(e) => match e.downcast_ref::<Interrupt>() {
                    Some(Interrupt::Quit) => break,
                    Some(Interrupt::Paused) => {}
                    None => {
                        log!("{} {e:#}", paint("Error:", "31"));
                        log!("Pausing. Fix the problem and press F6 to try again.");
                        self.flags
                            .enabled
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                },
            }
        }
        self.cleanup();
        self.print_totals();
        Ok(())
    }

    fn session(&mut self) -> Result<()> {
        self.focus_game(true)?;
        loop {
            self.cycle()?;
        }
    }

    fn check(&self) -> Result<()> {
        if self.flags.quitting() {
            return Err(Interrupt::Quit.into());
        }
        if !self.flags.enabled() {
            return Err(Interrupt::Paused.into());
        }
        Ok(())
    }

    /// Sleeps while staying responsive to F6 / Ctrl+C.
    fn sleep(&mut self, ms: u64) -> Result<()> {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            self.check()?;
            self.service_keys()?;
            let now = Instant::now();
            if now >= end {
                return Ok(());
            }
            thread::sleep((end - now).min(Duration::from_millis(5)));
        }
    }

    /// Releases a key pressed with `press_async` once its hold time is over.
    fn service_keys(&mut self) -> Result<()> {
        if let Some((key, until)) = self.pending_key
            && Instant::now() >= until
        {
            self.input.key_up(key)?;
            self.pending_key = None;
        }
        Ok(())
    }

    fn release_pending_key(&mut self) -> Result<()> {
        if let Some((key, _)) = self.pending_key.take() {
            self.input.key_up(key)?;
        }
        Ok(())
    }

    /// Presses a key and lets the capture loop release it later, so frames keep coming.
    fn press_async(&mut self, key: KeyCode) -> Result<()> {
        self.release_pending_key()?;
        self.input.key_down(key)?;
        self.pending_key = Some((
            key,
            Instant::now() + Duration::from_millis(self.cfg.timing.key_press_ms),
        ));
        Ok(())
    }

    fn tap(&mut self, key: KeyCode) -> Result<()> {
        self.release_pending_key()?;
        self.input
            .tap(key, Duration::from_millis(self.cfg.timing.key_press_ms))
    }

    fn focus_game(&mut self, force: bool) -> Result<()> {
        let class = self.cfg.window.class.clone();
        if !self.hypr.is_active(&class).unwrap_or(false) {
            if !force && !self.cfg.window.refocus {
                log!("Sober lost focus and refocus = false; pausing.");
                self.flags
                    .enabled
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                return Err(Interrupt::Paused.into());
            }
            let client = self.hypr.find_client(&class)?.ok_or_else(|| {
                anyhow!(
                    "no window with class {class:?}; is Sober running? (check `hyprctl clients`)"
                )
            })?;
            self.hypr.focus(&client)?;
            self.sleep(self.cfg.timing.focus_delay_ms)?;
            if !self.hypr.is_active(&class)? {
                return Err(anyhow!("could not focus {class}"));
            }
            log!("Focused {class}.");
        }
        self.hypr
            .move_cursor(self.cfg.window.cursor_x, self.cfg.window.cursor_y)?;
        Ok(())
    }

    fn grab(&mut self) -> Result<Frame> {
        self.grab_rect(self.rect)
    }

    fn grab_rect(&mut self, rect: Rect) -> Result<Frame> {
        let mut failures = 0;
        loop {
            match self.cap.grab(rect) {
                Ok(f) => return Ok(f),
                Err(e) => {
                    failures += 1;
                    if failures >= 5 {
                        return Err(e.context("screen capture keeps failing"));
                    }
                    self.check()?;
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn cycle(&mut self) -> Result<()> {
        self.focus_game(false)?;
        // Started in the middle of a minigame? Play it instead of casting.
        let d = {
            let f = self.grab()?;
            self.det.detect(&f)
        };
        if d.bar.is_some() && !d.fish.is_empty() {
            return self.reel();
        }
        self.cast()?;
        match self.shake()? {
            Shake::Minigame => self.reel(),
            Shake::Timeout => {
                self.totals.shake_timeouts += 1;
                log!(
                    "{}",
                    paint("No minigame before the shake timeout; recasting.", "33")
                );
                self.recover()
            }
        }
    }

    fn cast(&mut self) -> Result<()> {
        self.totals.casts += 1;
        self.input.set_lmb(false)?;
        self.input.wiggle()?;
        self.input.set_lmb(true)?;
        let how = match self.cast_bar.take() {
            Some(bar) => {
                let result = self.charge(&bar);
                self.cast_bar = Some(bar);
                result?
            }
            None => {
                self.sleep(self.cfg.timing.cast_hold_ms)?;
                "timed".to_string()
            }
        };
        self.input.set_lmb(false)?;
        log!("Cast #{} ({how})", self.totals.casts);
        self.sleep(self.cfg.timing.after_cast_ms)
    }

    /// Holds the button while watching the power bar; returns how it ended.
    fn charge(&mut self, bar: &CastFinder) -> Result<String> {
        let mut session = CastSession::new(&self.cfg.cast);
        let start = Instant::now();
        let max = Duration::from_millis(self.cfg.cast.max_hold_ms);
        let timed = Duration::from_millis(self.cfg.timing.cast_hold_ms);
        loop {
            self.check()?;
            let frame = self.grab_rect(bar.rect)?;
            let t = frame
                .received
                .saturating_duration_since(start)
                .as_secs_f64();
            let step = session.update(bar, &frame, t);
            if step.release {
                self.totals.bar_casts += 1;
                let fill = step.reading.map_or(0.0, |r| r.fill);
                return Ok(format!(
                    "power bar {:.0}% after {} ms",
                    fill * 100.0,
                    start.elapsed().as_millis()
                ));
            }
            if !session.seen() && self.cfg.cast.mode == CastMode::Auto && start.elapsed() >= timed {
                self.totals.fallback_casts += 1;
                return Ok(paint(
                    "power bar not found - timed cast; check `debug --cast`",
                    "33",
                ));
            }
            if start.elapsed() >= max {
                self.totals.fallback_casts += 1;
                return Ok(if !session.seen() {
                    paint(
                        "power bar not seen - released after max_hold_ms; check `debug --cast`",
                        "33",
                    )
                } else {
                    paint(
                        &format!(
                            "power bar peaked at {:.0}% (< release_at) - released after max_hold_ms",
                            session.peak() * 100.0
                        ),
                        "33",
                    )
                });
            }
        }
    }

    fn shake(&mut self) -> Result<Shake> {
        if self.cfg.keys.toggle_nav_each_cast && !self.nav_on {
            self.tap(self.keys.nav)?;
            self.nav_on = true;
            self.sleep(60)?;
        }
        let start = Instant::now();
        let interval = Duration::from_millis(self.cfg.timing.enter_interval_ms);
        let timeout = Duration::from_millis(self.cfg.timing.shake_timeout_ms);
        let mut last_enter: Option<Instant> = None;
        let mut last_focus_check = Instant::now();
        let refresh = Duration::from_millis(self.cfg.keys.nav_refresh_ms);
        let mut last_refresh = Instant::now();
        // Set while UI navigation is briefly off during a refresh.
        let mut nav_back_on: Option<Instant> = None;
        let mut streak = 0;
        loop {
            self.check()?;
            self.service_keys()?;
            let frame = self.grab()?;
            let d = self.det.detect(&frame);
            streak = if d.bar.is_some() && !d.fish.is_empty() {
                streak + 1
            } else {
                0
            };
            if streak >= self.cfg.detection.start_frames {
                self.release_pending_key()?;
                if self.nav_on
                    && self.cfg.keys.toggle_nav_each_cast
                    && self.cfg.keys.nav_off == NavOff::ReelStart
                {
                    self.press_async(self.keys.nav)?;
                    self.nav_on = false;
                }
                return Ok(Shake::Minigame);
            }
            if self.pending_key.is_none() && last_enter.is_none_or(|t| t.elapsed() >= interval) {
                self.press_async(self.keys.shake)?;
                last_enter = Some(Instant::now());
            }
            // Optional: switch UI navigation off and on again so the newest
            // shake button gets selected.
            if !refresh.is_zero()
                && self.cfg.keys.toggle_nav_each_cast
                && self.pending_key.is_none()
            {
                match nav_back_on {
                    Some(at) if Instant::now() >= at => {
                        self.press_async(self.keys.nav)?;
                        self.nav_on = true;
                        nav_back_on = None;
                        last_refresh = Instant::now();
                    }
                    None if self.nav_on && last_refresh.elapsed() >= refresh => {
                        self.press_async(self.keys.nav)?;
                        self.nav_on = false;
                        nav_back_on = Some(Instant::now() + Duration::from_millis(100));
                    }
                    _ => {}
                }
            }
            if start.elapsed() > timeout {
                return Ok(Shake::Timeout);
            }
            // Never spam Enter into another window.
            if last_focus_check.elapsed() > Duration::from_secs(2) {
                last_focus_check = Instant::now();
                if !self.hypr.is_active(&self.cfg.window.class).unwrap_or(false) {
                    self.release_pending_key()?;
                    log!("Sober lost focus during the shake phase.");
                    self.focus_game(false)?;
                }
            }
        }
    }

    fn reel(&mut self) -> Result<()> {
        self.totals.minigames += 1;
        log!("Reeling...");
        let mut reel = Reel::new(&self.cfg, self.det.layout.track_width());
        let t0 = Instant::now();
        let mut last_stamp = None;
        let reason = loop {
            if let Err(e) = self.check() {
                self.input.set_lmb(false)?;
                return Err(e);
            }
            self.service_keys()?;
            let frame = self.grab()?;
            let duplicate = frame.stamp.is_some() && frame.stamp == last_stamp;
            last_stamp = frame.stamp;
            let t = frame.received.saturating_duration_since(t0).as_secs_f64();
            let d = self.det.detect(&frame);
            let s = reel.step(t, &d, duplicate);
            self.input.set_lmb(s.hold)?;
            if self.verbose {
                println!(
                    "  t{:7.3} bar {:>13} fish {:>6} pred {:>6}/{:>6} e{:+.4} duty {:.2} {}{} prog {}",
                    t,
                    s.bar
                        .map_or("-".into(), |b| format!("{:.0}..{:.0}", b.left, b.right)),
                    s.fish.map_or("-".into(), |f| format!("{f:.1}")),
                    s.fish_pred.map_or("-".into(), |f| format!("{f:.1}")),
                    s.bar_pred.map_or("-".into(), |f| format!("{f:.1}")),
                    s.ctl.error,
                    s.ctl.duty,
                    if s.hold { "HOLD" } else { "rel " },
                    match s.inside {
                        Some(true) => " in ",
                        Some(false) => " OUT",
                        None => "    ",
                    },
                    d.progress
                        .map_or("-".into(), |p| format!("{:.0}%", p * 100.0)),
                );
            }
            if let Some(r) = s.ended {
                break r;
            }
        };
        self.input.set_lmb(false)?;
        self.release_pending_key()?;
        if self.nav_on && self.cfg.keys.toggle_nav_each_cast {
            self.tap(self.keys.nav)?;
            self.nav_on = false;
        }
        let stats = reel.finish();
        self.report(reason, &stats);
        if reason == EndReason::Timeout {
            self.totals.reel_timeouts += 1;
            return self.recover();
        }
        self.sleep(self.cfg.timing.after_catch_ms)
    }

    fn recover(&mut self) -> Result<()> {
        self.input.set_lmb(false)?;
        if self.nav_on {
            self.tap(self.keys.nav)?;
            self.nav_on = false;
            self.sleep(100)?;
        }
        if let Some(k) = self.keys.reequip {
            log!("Re-equipping the rod.");
            self.tap(k)?;
            self.sleep(700)?;
            self.tap(k)?;
            self.sleep(900)?;
        }
        self.sleep(self.cfg.timing.recover_delay_ms)
    }

    fn report(&mut self, reason: EndReason, s: &ReelStats) {
        let outcome = match (reason, s.caught()) {
            (EndReason::Timeout, _) => "timeout",
            (_, Some(false)) => "lost",
            (_, Some(true)) => "caught",
            (_, None) => "finished",
        };
        let counts_as_catch = matches!(outcome, "caught" | "finished");
        if counts_as_catch {
            self.totals.catches += 1;
            if s.perfect() {
                self.totals.perfect += 1;
            }
        } else if outcome == "lost" {
            self.totals.lost += 1;
        }

        let progress = s
            .last_progress
            .map_or("n/a".into(), |p| format!("{:.0}%", p * 100.0));
        let summary = format!(
            "{:.1} s, fish in bar {:.1}% of {} frames @ {:.0} fps, min margin {:+.1} px, progress {}",
            s.duration_s,
            s.inside_ratio() * 100.0,
            s.measured,
            1000.0 / s.frame_ms.max(0.1),
            s.min_margin_px,
            progress
        );
        let rate = if self.totals.catches > 0 {
            format!(
                " | perfect {}/{} ({:.0}%)",
                self.totals.perfect,
                self.totals.catches,
                100.0 * self.totals.perfect as f32 / self.totals.catches as f32
            )
        } else {
            String::new()
        };
        match outcome {
            "lost" => log!("{} ({summary}){rate}", paint("Fish escaped", "31")),
            "timeout" => log!("{} ({summary})", paint("Reel timed out", "31")),
            _ if s.perfect() => log!(
                "Catch #{} {} ({summary}){rate}",
                self.totals.catches,
                paint("PERFECT", "1;32")
            ),
            _ => {
                let mut why = Vec::new();
                if s.measured == 0 {
                    why.push(
                        "fish/bar never detected together - check calibration with `debug`"
                            .to_string(),
                    );
                }
                if s.outside > 0 {
                    why.push(format!(
                        "fish outside the bar in {} frames (longest {} in a row, worst {:.1} px)",
                        s.outside, s.longest_outside_frames, s.worst_outside_px
                    ));
                }
                if s.progress_drops > 0 {
                    why.push(format!("{} progress drop(s)", s.progress_drops));
                }
                log!(
                    "Catch #{} {}: {} ({summary}){rate}",
                    self.totals.catches,
                    paint("not perfect", "33"),
                    why.join(", ")
                );
            }
        }
        if reason == EndReason::FishLost {
            log!(
                "  (ended because the fish was not visible for {} ms)",
                self.cfg.detection.fish_lost_end_ms
            );
        }

        if let Some(f) = self.csv.as_mut() {
            let c = &self.cfg.controller;
            let _ = writeln!(
                f,
                "{:.1},{outcome},{},{:.2},{},{},{},{},{:.1},{:.1},{},{},{},{:.3},{:.2},{:?},{},{},{},{},{},{:?}",
                self.started.elapsed().as_secs_f64(),
                s.perfect(),
                s.duration_s,
                s.frames,
                s.measured,
                s.outside,
                s.longest_outside_frames,
                s.worst_outside_px,
                s.min_margin_px,
                s.progress_drops,
                s.last_progress.map_or(String::new(), |p| format!("{p:.3}")),
                s.guard_frames,
                s.hold_frames as f32 / s.frames.max(1) as f32,
                s.frame_ms,
                c.mode,
                c.kp,
                c.kd,
                c.lookahead_ms,
                c.lookahead_frames,
                c.edge_guard,
                reason,
            );
        }
    }

    /// Leaves the game in a clean state: mouse up, keys up, UI navigation off.
    fn cleanup(&mut self) {
        let _ = self.release_pending_key();
        self.input.release_all();
        if self.nav_on {
            if self.hypr.is_active(&self.cfg.window.class).unwrap_or(false) {
                let _ = self.input.force_tap(
                    self.keys.nav,
                    Duration::from_millis(self.cfg.timing.key_press_ms),
                );
            } else {
                log!(
                    "Sober is not focused; UI navigation may still be on (press {} in-game).",
                    self.cfg.keys.nav_toggle
                );
            }
            self.nav_on = false;
        }
    }

    fn print_totals(&self) {
        let t = &self.totals;
        println!();
        log!(
            "Session: {} casts ({} by power bar, {} fallback), {} minigames, {} catches ({} perfect), {} lost, \
             {} shake timeouts, {} reel timeouts.",
            t.casts,
            t.bar_casts,
            t.fallback_casts,
            t.minigames,
            t.catches,
            t.perfect,
            t.lost,
            t.shake_timeouts,
            t.reel_timeouts
        );
    }
}
