//! `config.toml`: everything that can be tuned without recompiling.
//!
//! The file is written by hand-rolled code (see [`Config::to_toml`]) so the
//! comments survive every `calibrate` run.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};

/// An sRGB colour, written as `"#rrggbb"` in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub [u8; 3]);

impl Rgb {
    pub fn parse(s: &str) -> Result<Rgb> {
        let hex = s.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid colour {s:?}, expected \"#rrggbb\"");
        }
        let v = u32::from_str_radix(hex, 16)?;
        Ok(Rgb([(v >> 16) as u8, (v >> 8) as u8, v as u8]))
    }

    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0[0], self.0[1], self.0[2])
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Rgb::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Benchmark both at startup and keep the faster one.
    Auto,
    /// wlr-screencopy over a persistent Wayland connection.
    Screencopy,
    /// Spawn `grim` for every frame.
    Grim,
}

impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Screencopy => "screencopy",
            Backend::Grim => "grim",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DispatchStyle {
    /// Ask `hyprctl -j status` which config provider is active.
    Auto,
    /// `hyprctl dispatch movecursor X Y` (hyprland.conf / hyprlang).
    Legacy,
    /// `hyprctl dispatch 'hl.dsp.cursor.move({ x = X, y = Y })'` (hyprland.lua, 0.56+).
    Lua,
}

impl DispatchStyle {
    fn as_str(self) -> &'static str {
        match self {
            DispatchStyle::Auto => "auto",
            DispatchStyle::Legacy => "legacy",
            DispatchStyle::Lua => "lua",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    /// PD controller -> duty cycle -> sigma-delta hold/release.
    Pd,
    /// Hold whenever (error + kd/kp * error_rate) > 0.
    BangBang,
}

impl ControlMode {
    fn as_str(self) -> &'static str {
        match self {
            ControlMode::Pd => "pd",
            ControlMode::BangBang => "bang_bang",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavOff {
    /// Turn UI navigation off as soon as the reel minigame shows up.
    ReelStart,
    /// Leave it on during the reel and turn it off afterwards.
    ReelEnd,
}

impl NavOff {
    fn as_str(self) -> &'static str {
        match self {
            NavOff::ReelStart => "reel_start",
            NavOff::ReelEnd => "reel_end",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CastMode {
    /// "perfect" when the power bar is calibrated, otherwise "timed".
    Auto,
    /// Watch the power bar and release at the top.
    Perfect,
    /// Hold for `timing.cast_hold_ms`.
    Timed,
}

impl CastMode {
    fn as_str(self) -> &'static str {
        match self {
            CastMode::Auto => "auto",
            CastMode::Perfect => "perfect",
            CastMode::Timed => "timed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowCfg {
    pub class: String,
    pub refocus: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub dispatch_style: DispatchStyle,
}

impl Default for WindowCfg {
    fn default() -> Self {
        Self {
            class: "org.vinegarhq.Sober".into(),
            refocus: true,
            cursor_x: 1280,
            cursor_y: 500,
            dispatch_style: DispatchStyle::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureCfg {
    pub backend: Backend,
    pub output: String,
    pub frame_timeout_ms: u64,
}

impl Default for CaptureCfg {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            output: String::new(),
            frame_timeout_ms: 500,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutCfg {
    pub track_left: i32,
    pub track_top: i32,
    pub track_right: i32,
    pub track_bottom: i32,
    pub progress_y: i32,
    pub progress_left: i32,
    pub progress_right: i32,
    pub padding: i32,
}

impl Default for LayoutCfg {
    fn default() -> Self {
        // Fisch's reel UI on a 2560x1440 screen (measured from screenshots);
        // `calibrate` refines the progress row.
        Self {
            track_left: 764,
            track_top: 1219,
            track_right: 1796,
            track_bottom: 1258,
            progress_y: 1315,
            progress_left: 1004,
            progress_right: 1555,
            padding: 6,
        }
    }
}

impl LayoutCfg {
    pub fn is_calibrated(&self) -> bool {
        self.track_right - self.track_left >= 20
            && self.track_bottom >= self.track_top
            && self.track_top > 0
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorsCfg {
    pub bar: Vec<Rgb>,
    pub fish: Vec<Rgb>,
    pub progress: Vec<Rgb>,
    pub bar_tolerance: f32,
    pub fish_tolerance: f32,
    pub progress_tolerance: f32,
}

impl Default for ColorsCfg {
    fn default() -> Self {
        Self {
            bar: Vec::new(),
            fish: Vec::new(),
            progress: Vec::new(),
            bar_tolerance: 45.0,
            fish_tolerance: 40.0,
            progress_tolerance: 45.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CastCfg {
    pub mode: CastMode,
    pub search_x: i32,
    pub search_y: i32,
    pub search_w: i32,
    pub search_h: i32,
    pub release_at: f32,
    pub lead_ms: f32,
    pub max_hold_ms: u64,
}

impl Default for CastCfg {
    fn default() -> Self {
        // The power bar floats next to the character, right of the screen
        // centre on a 2560x1440 screen.
        Self {
            mode: CastMode::Auto,
            search_x: 1300,
            search_y: 500,
            search_w: 420,
            search_h: 660,
            release_at: 0.96,
            lead_ms: 20.0,
            max_hold_ms: 3000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectionCfg {
    pub row_samples: u32,
    pub row_band: f32,
    pub column_min_fraction: f32,
    pub bar_min_width: u32,
    pub bar_max_width: f32,
    pub bar_merge_gap: u32,
    pub fish_min_width: u32,
    pub fish_max_width: u32,
    pub fish_max_jump: f32,
    pub start_frames: u32,
    pub end_ms: u64,
    pub fish_lost_end_ms: u64,
    pub progress_drop: f32,
}

impl Default for DetectionCfg {
    fn default() -> Self {
        Self {
            row_samples: 5,
            row_band: 0.6,
            column_min_fraction: 0.4,
            bar_min_width: 3,
            bar_max_width: 0.7,
            bar_merge_gap: 40,
            fish_min_width: 2,
            fish_max_width: 70,
            fish_max_jump: 0.2,
            start_frames: 3,
            end_ms: 500,
            fish_lost_end_ms: 2000,
            progress_drop: 0.03,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControllerCfg {
    pub mode: ControlMode,
    pub kp: f32,
    pub kd: f32,
    pub neutral_duty: f32,
    pub lookahead_ms: f32,
    pub lookahead_frames: f32,
    pub fish_history: usize,
    pub fish_accel_weight: f32,
    pub bar_history: usize,
    pub edge_guard: f32,
    pub coast_ms: f32,
}

impl Default for ControllerCfg {
    fn default() -> Self {
        Self {
            mode: ControlMode::Pd,
            kp: 20.0,
            kd: 3.0,
            neutral_duty: 0.5,
            lookahead_ms: 0.0,
            lookahead_frames: 1.75,
            fish_history: 5,
            fish_accel_weight: 0.5,
            bar_history: 3,
            edge_guard: 0.3,
            coast_ms: 150.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimingCfg {
    pub focus_delay_ms: u64,
    pub cast_hold_ms: u64,
    pub after_cast_ms: u64,
    pub enter_interval_ms: u64,
    pub key_press_ms: u64,
    pub shake_timeout_ms: u64,
    pub reel_timeout_ms: u64,
    pub after_catch_ms: u64,
    pub recover_delay_ms: u64,
    pub input_settle_ms: u64,
}

impl Default for TimingCfg {
    fn default() -> Self {
        Self {
            focus_delay_ms: 300,
            cast_hold_ms: 1000,
            after_cast_ms: 1200,
            enter_interval_ms: 90,
            key_press_ms: 25,
            shake_timeout_ms: 30_000,
            reel_timeout_ms: 120_000,
            after_catch_ms: 2200,
            recover_delay_ms: 1500,
            input_settle_ms: 700,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeysCfg {
    pub nav_toggle: String,
    pub shake: String,
    pub toggle_nav_each_cast: bool,
    pub nav_off: NavOff,
    pub nav_refresh_ms: u64,
    pub reequip: String,
}

impl Default for KeysCfg {
    fn default() -> Self {
        Self {
            nav_toggle: "KEY_BACKSLASH".into(),
            shake: "KEY_ENTER".into(),
            toggle_nav_each_cast: true,
            nav_off: NavOff::ReelStart,
            nav_refresh_ms: 0,
            reequip: "KEY_1".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogCfg {
    pub catches_csv: String,
    pub verbose_reel: bool,
}

impl Default for LogCfg {
    fn default() -> Self {
        Self {
            catches_csv: "catches.csv".into(),
            verbose_reel: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub window: WindowCfg,
    pub capture: CaptureCfg,
    pub layout: LayoutCfg,
    pub colors: ColorsCfg,
    pub detection: DetectionCfg,
    pub controller: ControllerCfg,
    pub cast: CastCfg,
    pub timing: TimingCfg,
    pub keys: KeysCfg,
    pub logging: LogCfg,
}

impl Config {
    /// Loads `path`, creating it with defaults when it does not exist yet.
    pub fn load_or_create(path: &Path) -> Result<Config> {
        if !path.exists() {
            let cfg = Config::default();
            cfg.save(path)?;
            println!("Created {} with default settings.", path.display());
            return Ok(cfg);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut table: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let migrated = migrate(&mut table);
        let cfg: Config = toml::Value::Table(table)
            .try_into()
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        if migrated {
            cfg.save(path)?;
            println!("Updated {} to the current format.", path.display());
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let c = &self.controller;
        if c.fish_history < 2 || c.bar_history < 2 {
            bail!("controller.fish_history and controller.bar_history must be at least 2");
        }
        if !(0.0..=1.0).contains(&c.neutral_duty) {
            bail!("controller.neutral_duty must be between 0 and 1");
        }
        if c.kp <= 0.0 {
            bail!("controller.kp must be > 0");
        }
        if !(0.1..=1.0).contains(&self.cast.release_at) {
            bail!("cast.release_at must be between 0.1 and 1.0");
        }
        let d = &self.detection;
        if d.row_samples == 0 || !(0.0..=1.0).contains(&d.column_min_fraction) {
            bail!("detection.row_samples must be > 0 and detection.column_min_fraction in 0..1");
        }
        if !(0.05..=1.0).contains(&d.row_band) {
            bail!("detection.row_band must be between 0.05 and 1.0");
        }
        crate::input::parse_key(&self.keys.nav_toggle)?;
        crate::input::parse_key(&self.keys.shake)?;
        if !self.keys.reequip.trim().is_empty() {
            crate::input::parse_key(&self.keys.reequip)?;
        }
        Ok(())
    }

    /// Serialises the config with explanatory comments.
    pub fn to_toml(&self) -> String {
        let mut s = String::new();
        let w = &self.window;
        let cap = &self.capture;
        let l = &self.layout;
        let c = &self.colors;
        let d = &self.detection;
        let k = &self.controller;
        let ca = &self.cast;
        let t = &self.timing;
        let keys = &self.keys;
        let lg = &self.logging;

        let _ = write!(
            s,
            r#"# fisch-macro configuration.
# `cargo run --release -- calibrate` rewrites [colors] (and the progress row in
# [layout]); everything else is yours to tune. Coordinates are screen pixels as
# printed by `hyprctl cursorpos`.

[window]
# Window class of Roblox (check with `hyprctl clients`).
class = {class}
# Re-focus the game before every cast if something else has focus.
refocus = {refocus}
# Where the cursor is parked while fishing (over open water, away from UI).
cursor_x = {cursor_x}
cursor_y = {cursor_y}
# How to talk to `hyprctl dispatch`: "auto", "legacy" (hyprland.conf) or "lua" (hyprland.lua).
dispatch_style = "{dispatch_style}"

[capture]
# "auto" benchmarks both backends at startup and keeps the faster one,
# "screencopy" (wlr-screencopy, in-process) or "grim" (spawns grim per frame).
backend = "{backend}"
# Output name for screencopy (e.g. "DP-3"); empty = the output that contains the bar.
output = {output}
# Give up on a frame after this long.
frame_timeout_ms = {frame_timeout_ms}

[layout]
# Reel track (the moving-bar area), inclusive screen coordinates. The defaults
# are Fisch's reel UI on a 2560x1440 screen; `calibrate --manual` re-measures.
track_left = {track_left}
track_top = {track_top}
track_right = {track_right}
track_bottom = {track_bottom}
# Progress bar row under the track. progress_y = 0 disables progress reading.
# progress_left/right = 0 means "same as the track".
progress_y = {progress_y}
progress_left = {progress_left}
progress_right = {progress_right}
# Extra pixels captured around everything above.
padding = {padding}

[colors]
# Reference colours sampled by `calibrate` during a real Ruinous Oath minigame.
# A pixel matches when its RGB distance to ANY listed colour is <= the tolerance.
bar = {bar}
fish = {fish}
progress = {progress}
bar_tolerance = {bar_tolerance}
fish_tolerance = {fish_tolerance}
progress_tolerance = {progress_tolerance}

[detection]
# Rows sampled across the middle `row_band` fraction of the track height.
row_samples = {row_samples}
row_band = {row_band}
# A column counts as bar/fish when this fraction of the sampled rows match
# (makes slash effects and particles that only cover part of the height harmless).
column_min_fraction = {column_min_fraction}
# Bar width limits: pixels for the minimum, fraction of the track for the maximum.
bar_min_width = {bar_min_width}
bar_max_width = {bar_max_width}
# Bar segments separated by at most this many pixels (the fish drawn over the bar)
# are merged into one bar.
bar_merge_gap = {bar_merge_gap}
fish_min_width = {fish_min_width}
fish_max_width = {fish_max_width}
# A fish detection further than this (fraction of the track width) from the
# predicted position must be seen twice in a row before it is believed.
fish_max_jump = {fish_max_jump}
# Frames in a row with bar + fish visible before the minigame counts as started.
start_frames = {start_frames}
# The minigame is over once neither bar nor fish has been seen for end_ms,
# or the fish alone has been missing for fish_lost_end_ms.
end_ms = {end_ms}
fish_lost_end_ms = {fish_lost_end_ms}
# A progress decrease larger than this (fraction of the bar) counts as a "drop".
progress_drop = {progress_drop}

[controller]
# "pd": PD controller -> duty cycle -> sigma-delta hold/release (recommended).
# "bang_bang": hold whenever error + (kd/kp) * error_rate > 0.
mode = "{mode}"
# Gains act on errors measured in track widths (resolution independent).
# kp: how hard to chase the fish. kd: damping, counters the bar's momentum.
kp = {kp}
kd = {kd}
# Hold duty that keeps the bar still (0.5 = holding and releasing are equally strong).
neutral_duty = {neutral_duty}
# How far past each frame to predict the fish and the bar:
#   lookahead_ms + lookahead_frames * (measured time between captured frames)
# It covers capture latency, the wait for the next frame and input latency, and
# scales by itself with the monitor refresh rate (60 Hz vs 170 Hz).
# Raise it if the bar lags behind the fish.
lookahead_ms = {lookahead_ms}
lookahead_frames = {lookahead_frames}
# Frames used to estimate fish speed/acceleration (quadratic fit).
fish_history = {fish_history}
# 0 = ignore fish acceleration, 1 = trust it fully.
fish_accel_weight = {fish_accel_weight}
# Frames used to estimate the bar's speed (linear fit).
bar_history = {bar_history}
# Safety margin as a fraction of the bar half-width: when the predicted fish gets
# this close to a bar edge and is moving outwards, the controller saturates.
edge_guard = {edge_guard}
# Keep predicting from the last estimate for this long when the fish or bar is
# hidden by an effect.
coast_ms = {coast_ms}

[cast]
# While charging, the power bar (green cap on top, white fill growing from the
# bottom) is searched for next to the character and LMB is released at the top.
# "auto": like perfect, but a cast whose bar is not found within
#         timing.cast_hold_ms is released then (a plain timed cast).
# "perfect": always wait for the bar (up to max_hold_ms).
# "timed": never look, just hold for timing.cast_hold_ms.
mode = "{cast_mode}"
# Screen area searched for the power bar (it moves a little with the camera).
search_x = {search_x}
search_y = {search_y}
search_w = {search_w}
search_h = {search_h}
# Release once the predicted fill reaches this fraction of the bar.
release_at = {release_at}
# Release this much earlier to cover capture + input latency.
lead_ms = {lead_ms}
# Release anyway after this long (bar not visible / never reached release_at).
max_hold_ms = {max_hold_ms}

[timing]
focus_delay_ms = {focus_delay_ms}
# How long LMB is held to charge a "timed" cast.
cast_hold_ms = {cast_hold_ms}
# Wait after releasing the cast before UI navigation + Enter spam starts.
after_cast_ms = {after_cast_ms}
enter_interval_ms = {enter_interval_ms}
key_press_ms = {key_press_ms}
# Recast when no minigame appeared within this time.
shake_timeout_ms = {shake_timeout_ms}
reel_timeout_ms = {reel_timeout_ms}
# Pause after a minigame ends (catch popup / animation).
after_catch_ms = {after_catch_ms}
recover_delay_ms = {recover_delay_ms}
# Time for Hyprland to pick up the virtual mouse/keyboard after they are created.
input_settle_ms = {input_settle_ms}

[keys]
# Linux key names (KEY_BACKSLASH, KEY_ENTER, KEY_1, ...). On a non-US layout use the
# key that sits where \ is on a US keyboard, e.g. "KEY_102ND" or "KEY_BACKSLASH".
nav_toggle = {nav_toggle}
shake = {shake}
# Press nav_toggle after every cast (and again once the shake phase ends).
# Set false if you keep UI navigation on all the time.
toggle_nav_each_cast = {toggle_nav_each_cast}
# When to turn UI navigation back off: "reel_start" or "reel_end".
nav_off = "{nav_off}"
# If Enter stops pressing new shake buttons, set this to e.g. 1500: UI navigation
# is then switched off and on again every 1.5 s to re-select the button. 0 = off.
nav_refresh_ms = {nav_refresh_ms}
# Hotbar key of the rod. After a timeout the rod is unequipped and re-equipped
# to cancel a stuck cast. Empty = disabled.
reequip = {reequip}

[logging]
# Per-catch stats for tuning (empty = disabled).
catches_csv = {catches_csv}
# Print one line per frame during the reel.
verbose_reel = {verbose_reel}
"#,
            class = toml_str(&w.class),
            refocus = w.refocus,
            cursor_x = w.cursor_x,
            cursor_y = w.cursor_y,
            dispatch_style = w.dispatch_style.as_str(),
            backend = cap.backend.as_str(),
            output = toml_str(&cap.output),
            frame_timeout_ms = cap.frame_timeout_ms,
            track_left = l.track_left,
            track_top = l.track_top,
            track_right = l.track_right,
            track_bottom = l.track_bottom,
            progress_y = l.progress_y,
            progress_left = l.progress_left,
            progress_right = l.progress_right,
            padding = l.padding,
            bar = colors_str(&c.bar),
            fish = colors_str(&c.fish),
            progress = colors_str(&c.progress),
            bar_tolerance = f(c.bar_tolerance),
            fish_tolerance = f(c.fish_tolerance),
            progress_tolerance = f(c.progress_tolerance),
            row_samples = d.row_samples,
            row_band = f(d.row_band),
            column_min_fraction = f(d.column_min_fraction),
            bar_min_width = d.bar_min_width,
            bar_max_width = f(d.bar_max_width),
            bar_merge_gap = d.bar_merge_gap,
            fish_min_width = d.fish_min_width,
            fish_max_width = d.fish_max_width,
            fish_max_jump = f(d.fish_max_jump),
            start_frames = d.start_frames,
            end_ms = d.end_ms,
            fish_lost_end_ms = d.fish_lost_end_ms,
            progress_drop = f(d.progress_drop),
            mode = k.mode.as_str(),
            kp = f(k.kp),
            kd = f(k.kd),
            neutral_duty = f(k.neutral_duty),
            lookahead_ms = f(k.lookahead_ms),
            lookahead_frames = f(k.lookahead_frames),
            fish_history = k.fish_history,
            fish_accel_weight = f(k.fish_accel_weight),
            bar_history = k.bar_history,
            edge_guard = f(k.edge_guard),
            coast_ms = f(k.coast_ms),
            cast_mode = ca.mode.as_str(),
            search_x = ca.search_x,
            search_y = ca.search_y,
            search_w = ca.search_w,
            search_h = ca.search_h,
            release_at = f(ca.release_at),
            lead_ms = f(ca.lead_ms),
            max_hold_ms = ca.max_hold_ms,
            focus_delay_ms = t.focus_delay_ms,
            cast_hold_ms = t.cast_hold_ms,
            after_cast_ms = t.after_cast_ms,
            enter_interval_ms = t.enter_interval_ms,
            key_press_ms = t.key_press_ms,
            shake_timeout_ms = t.shake_timeout_ms,
            reel_timeout_ms = t.reel_timeout_ms,
            after_catch_ms = t.after_catch_ms,
            recover_delay_ms = t.recover_delay_ms,
            input_settle_ms = t.input_settle_ms,
            nav_toggle = toml_str(&keys.nav_toggle),
            shake = toml_str(&keys.shake),
            toggle_nav_each_cast = keys.toggle_nav_each_cast,
            nav_off = keys.nav_off.as_str(),
            nav_refresh_ms = keys.nav_refresh_ms,
            reequip = toml_str(&keys.reequip),
            catches_csv = toml_str(&lg.catches_csv),
            verbose_reel = lg.verbose_reel,
        );
        s
    }
}

/// Upgrades configs written by older versions. Returns true if anything changed.
fn migrate(table: &mut toml::Table) -> bool {
    let mut changed = false;
    // The power bar used to be two calibrated points; it is now searched for.
    if let Some(toml::Value::Table(cast)) = table.get_mut("cast") {
        for key in [
            "empty_x",
            "empty_y",
            "full_x",
            "full_y",
            "fill",
            "tolerance",
        ] {
            changed |= cast.remove(key).is_some();
        }
    }
    // An uncalibrated (all zero) layout now gets the built-in Fisch defaults.
    if let Some(toml::Value::Table(layout)) = table.get("layout") {
        let get = |k: &str| layout.get(k).and_then(|v| v.as_integer()).unwrap_or(0);
        if get("track_right") - get("track_left") < 20 {
            table.remove("layout");
            changed = true;
        }
    }
    changed
}

/// Float formatting that always round-trips as a TOML float.
fn f(v: f32) -> String {
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn colors_str(colors: &[Rgb]) -> String {
    let items: Vec<String> = colors.iter().map(|c| format!("\"{}\"", c.hex())).collect();
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips() {
        let cfg = Config::default();
        let parsed: Config = toml::from_str(&cfg.to_toml()).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn custom_round_trips() {
        let mut cfg = Config::default();
        cfg.colors.bar = vec![Rgb([1, 2, 3]), Rgb([250, 128, 0])];
        cfg.colors.fish = vec![Rgb([0x43, 0x4b, 0x5b])];
        cfg.layout.track_left = 760;
        cfg.controller.kp = 12.5;
        cfg.controller.mode = ControlMode::BangBang;
        cfg.window.class = "weird\"class\\".into();
        cfg.keys.nav_off = NavOff::ReelEnd;
        cfg.keys.nav_refresh_ms = 1500;
        cfg.cast.mode = CastMode::Perfect;
        cfg.cast.search_x = 1400;
        let parsed: Config = toml::from_str(&cfg.to_toml()).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn example_file_matches_defaults() {
        let example = include_str!("../config.example.toml");
        assert_eq!(
            example,
            Config::default().to_toml(),
            "regenerate config.example.toml"
        );
    }

    #[test]
    fn migrates_old_configs() {
        let mut old = Config::default().to_toml();
        old = old.replace("search_x = 1300", "empty_x = 1276\nempty_y = 708\nfull_x = 1037\nfull_y = 708\nfill = [\"#0000ff\"]\ntolerance = 45.0\nsearch_x = 1300");
        old = old
            .replace("track_left = 764", "track_left = 0")
            .replace("track_right = 1796", "track_right = 0");
        let mut table: toml::Table = toml::from_str(&old).unwrap();
        assert!(migrate(&mut table));
        let cfg: Config = toml::Value::Table(table).try_into().unwrap();
        assert_eq!(cfg.layout, LayoutCfg::default());
        assert_eq!(cfg.cast, CastCfg::default());
        let mut current: toml::Table = toml::from_str(&Config::default().to_toml()).unwrap();
        assert!(!migrate(&mut current));
    }

    #[test]
    fn rgb_parse() {
        assert_eq!(Rgb::parse("#434b5b").unwrap(), Rgb([0x43, 0x4b, 0x5b]));
        assert_eq!(Rgb::parse("FFFFFF").unwrap(), Rgb([255, 255, 255]));
        assert!(Rgb::parse("#12345").is_err());
    }
}
