//! Config file loading.
//!
//! The file is a small subset of TOML: `[sections]`, `key = value` lines,
//! double-quoted strings, integers, floats, booleans and `#` comments. That is
//! all this program needs, so it is parsed by hand instead of pulling in serde.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use evdev::KeyCode;

/// The commented example config, also written by `fishmacro init`.
pub const EXAMPLE: &str = include_str!("../config.example.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hold the button for the whole minigame.
    Hold,
    /// Click the button repeatedly.
    Spam,
    /// Hold/release to keep your zone on a moving marker.
    Track,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Positions run left to right; holding moves the zone right.
    Horizontal,
    /// Positions run bottom to top; holding moves the zone up.
    Vertical,
}

/// How the screen region is captured (`detect.capture_backend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Built-in wlr-screencopy client with a persistent connection.
    Screencopy,
    /// One `grim` process per frame.
    Grim,
}

/// Which Hyprland config parser is active; it decides the `hyprctl dispatch` syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyprConfigFormat {
    /// `hyprland.lua` (Hyprland 0.55+): dispatch arguments are Lua expressions.
    Lua,
    /// `hyprland.conf` (hyprlang): classic `dispatcher args` syntax.
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn parse(s: &str) -> Result<Rgb, String> {
        let hex = s.trim().trim_start_matches('#');
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("`{s}` is not a color; use \"#rrggbb\""));
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
        Ok(Rgb { r: channel(0), g: channel(2), b: channel(4) })
    }

    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// A color to look for, how close a pixel must be, and how many pixels count as "present".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorTarget {
    pub color: Rgb,
    /// Maximum per-channel difference (0-255) for a pixel to match.
    pub tolerance: u8,
    pub min_pixels: usize,
}

impl ColorTarget {
    #[inline]
    pub fn matches(&self, px: [u8; 3]) -> bool {
        let t = self.tolerance;
        px[0].abs_diff(self.color.r) <= t && px[1].abs_diff(self.color.g) <= t && px[2].abs_diff(self.color.b) <= t
    }
}

/// A screen rectangle in Hyprland layout coordinates (what `slurp` prints).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// The `-g` argument for grim: `"x,y wxh"`.
    pub fn geometry(&self) -> String {
        format!("{},{} {}x{}", self.x, self.y, self.width, self.height)
    }
}

#[derive(Debug, Clone)]
pub struct SpamConfig {
    pub clicks_per_second: f64,
    pub press: Duration,
}

#[derive(Debug, Clone)]
pub struct TrackConfig {
    pub axis: Axis,
    pub invert: bool,
    pub marker: Option<ColorTarget>,
    pub zone: Option<ColorTarget>,
    /// Fraction of the bar length where the controller only brakes.
    pub deadband: f32,
    pub lookahead: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub button: KeyCode,
    pub device_settle: Duration,
    pub start_timeout: Duration,
    pub end_grace: Duration,
    pub max_reel: Duration,
    /// hold/spam: start pressing right after the toggle instead of waiting
    /// for the minigame to appear.
    pub press_before_visible: bool,

    pub region: Region,

    pub capture_interval: Duration,
    pub capture_backend: CaptureBackend,
    pub active: Option<ColorTarget>,
    pub done: Option<ColorTarget>,

    pub spam: SpamConfig,
    pub track: TrackConfig,

    /// Where to warp the cursor before reeling, if anywhere.
    pub cursor: Option<(i32, i32)>,
    pub hyprland_config: HyprConfigFormat,

    pub calibrate_margin: u32,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "cannot read config {}: {e}\n  create one with: fishmacro init --config {}",
                path.display(),
                path.display()
            )
        })?;
        Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let mut r = Reader::new(parse_document(text)?);

        let mode = match r.string("general.mode", "track")?.as_str() {
            "hold" => Mode::Hold,
            "spam" => Mode::Spam,
            "track" => Mode::Track,
            other => return Err(format!("general.mode: unknown mode `{other}` (use hold, spam or track)")),
        };
        let button_name = r.string("general.button", "BTN_LEFT")?;
        let button = KeyCode::from_str(&button_name).map_err(|_| {
            format!(
                "general.button: `{button_name}` is not an evdev key name (e.g. BTN_LEFT, BTN_RIGHT, KEY_SPACE, KEY_E)"
            )
        })?;
        let device_settle = r.millis("general.device_settle_ms", 300)?;
        let start_timeout = r.millis("general.start_timeout_ms", 3000)?;
        let end_grace = r.millis("general.end_grace_ms", 700)?;
        let max_reel = r.millis("general.max_reel_ms", 60_000)?;
        let press_before_visible = r.bool("general.press_before_visible", false)?;

        let region = Region {
            x: r.int("region.x", 1080)? as i32,
            y: r.int("region.y", 1150)? as i32,
            width: r.positive("region.width", 400)?,
            height: r.positive("region.height", 30)?,
        };

        let capture_interval = r.millis("detect.capture_interval_ms", 0)?;
        let capture_backend = match r.string("detect.capture_backend", "screencopy")?.as_str() {
            "screencopy" => CaptureBackend::Screencopy,
            "grim" => CaptureBackend::Grim,
            other => {
                return Err(format!("detect.capture_backend: unknown backend `{other}` (use screencopy or grim)"));
            }
        };
        let active = r.color_target("detect.active", 20)?;
        let done = r.color_target("detect.done", 20)?;

        let clicks_per_second = r.float("spam.clicks_per_second", 12.0)?;
        if !(clicks_per_second > 0.0 && clicks_per_second <= 50.0) {
            return Err("spam.clicks_per_second must be between 0 and 50".into());
        }
        let press = r.millis("spam.press_ms", 25)?;
        if press.as_secs_f64() >= 1.0 / clicks_per_second {
            return Err("spam.press_ms must be shorter than one click period (1000 / clicks_per_second)".into());
        }

        let axis = match r.string("track.axis", "horizontal")?.as_str() {
            "horizontal" => Axis::Horizontal,
            "vertical" => Axis::Vertical,
            other => return Err(format!("track.axis: unknown axis `{other}` (use horizontal or vertical)")),
        };
        let invert = r.bool("track.invert", false)?;
        let marker = r.color_target("track.marker", 4)?;
        let zone = r.color_target("track.zone", 10)?;
        let deadband = r.float("track.deadband", 0.03)? as f32;
        if !(0.0..0.5).contains(&deadband) {
            return Err("track.deadband must be between 0.0 and 0.5".into());
        }
        let lookahead = r.millis("track.lookahead_ms", 50)?;

        let cursor_enabled = r.bool("cursor.enabled", false)?;
        let cursor_x = r.int("cursor.x", 1280)? as i32;
        let cursor_y = r.int("cursor.y", 720)? as i32;

        let hyprland_config = match r.string("hyprland.config_format", "lua")?.as_str() {
            "lua" => HyprConfigFormat::Lua,
            "legacy" => HyprConfigFormat::Legacy,
            other => return Err(format!("hyprland.config_format: unknown format `{other}` (use lua or legacy)")),
        };

        let calibrate_margin = r.int("calibrate.context_margin", 150)?.max(0) as u32;

        r.finish()?;

        match mode {
            Mode::Hold | Mode::Spam if active.is_none() => {
                return Err(format!(
                    "detect.active_color must be set for mode `{}`: the macro decides the catch is done when \
                     that color disappears from the region",
                    if mode == Mode::Hold { "hold" } else { "spam" }
                ));
            }
            Mode::Track if marker.is_none() || zone.is_none() => {
                return Err("mode `track` needs both track.marker_color and track.zone_color".into());
            }
            _ => {}
        }

        Ok(Config {
            mode,
            button,
            device_settle,
            start_timeout,
            end_grace,
            max_reel,
            press_before_visible,
            region,
            capture_interval,
            capture_backend,
            active,
            done,
            spam: SpamConfig { clicks_per_second, press },
            track: TrackConfig { axis, invert, marker, zone, deadband, lookahead },
            cursor: cursor_enabled.then_some((cursor_x, cursor_y)),
            hyprland_config,
            calibrate_margin,
        })
    }
}

/// `$XDG_CONFIG_HOME/fishmacro/config.toml`, falling back to `~/.config/...`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("fishmacro").join("config.toml")
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Str(_) => "a string",
            Value::Int(_) => "an integer",
            Value::Float(_) => "a number",
            Value::Bool(_) => "true/false",
        }
    }
}

/// Parses the document into `"section.key" -> (value, line number)`.
fn parse_document(text: &str) -> Result<BTreeMap<String, (Value, usize)>, String> {
    let mut out = BTreeMap::new();
    let mut section = String::new();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| format!("line {line_no}: expected `[section]`"))?.trim();
            if !is_ident(name) {
                return Err(format!("line {line_no}: invalid section name `{name}`"));
            }
            section = name.to_string();
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| format!("line {line_no}: expected `key = value`"))?;
        let key = key.trim();
        if !is_ident(key) {
            return Err(format!("line {line_no}: invalid key `{key}`"));
        }
        let value = parse_value(value.trim()).map_err(|e| format!("line {line_no}: {e}"))?;
        let full = if section.is_empty() { key.to_string() } else { format!("{section}.{key}") };
        if out.insert(full.clone(), (value, line_no)).is_some() {
            return Err(format!("line {line_no}: `{full}` is set twice"));
        }
    }
    Ok(out)
}

fn is_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Removes a trailing `# comment`, leaving `#` inside strings alone.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_value(s: &str) -> Result<Value, String> {
    if let Some(rest) = s.strip_prefix('"') {
        let inner = rest.strip_suffix('"').ok_or("unterminated string")?;
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                if c == '"' {
                    return Err("unexpected `\"` inside string".into());
                }
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                _ => return Err("unsupported escape in string".into()),
            }
        }
        return Ok(Value::Str(out));
    }
    match s {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        _ => {}
    }
    let number = s.replace('_', "");
    if let Ok(i) = number.parse::<i64>() {
        return Ok(Value::Int(i));
    }
    if number.chars().any(|c| c.is_ascii_digit()) {
        if let Ok(f) = number.parse::<f64>() {
            return Ok(Value::Float(f));
        }
    }
    Err(format!("cannot read value `{s}` (strings need double quotes)"))
}

/// Typed access to parsed keys; anything left unread at the end is reported as unknown.
struct Reader {
    map: BTreeMap<String, (Value, usize)>,
    used: BTreeSet<String>,
}

impl Reader {
    fn new(map: BTreeMap<String, (Value, usize)>) -> Self {
        Reader { map, used: BTreeSet::new() }
    }

    fn get(&mut self, key: &str) -> Option<(Value, usize)> {
        self.used.insert(key.to_string());
        self.map.get(key).cloned()
    }

    fn wrong_type(key: &str, line: usize, want: &str, got: &Value) -> String {
        format!("line {line}: `{key}` must be {want}, not {}", got.kind())
    }

    fn string(&mut self, key: &str, default: &str) -> Result<String, String> {
        match self.get(key) {
            None => Ok(default.to_string()),
            Some((Value::Str(s), _)) => Ok(s),
            Some((v, line)) => Err(Self::wrong_type(key, line, "a string", &v)),
        }
    }

    fn int(&mut self, key: &str, default: i64) -> Result<i64, String> {
        match self.get(key) {
            None => Ok(default),
            Some((Value::Int(i), _)) => Ok(i),
            Some((v, line)) => Err(Self::wrong_type(key, line, "an integer", &v)),
        }
    }

    fn positive(&mut self, key: &str, default: i64) -> Result<u32, String> {
        let v = self.int(key, default)?;
        u32::try_from(v).ok().filter(|v| *v > 0).ok_or_else(|| format!("`{key}` must be a positive integer"))
    }

    fn millis(&mut self, key: &str, default: i64) -> Result<Duration, String> {
        let v = self.int(key, default)?;
        u64::try_from(v).map(Duration::from_millis).map_err(|_| format!("`{key}` must not be negative"))
    }

    fn float(&mut self, key: &str, default: f64) -> Result<f64, String> {
        match self.get(key) {
            None => Ok(default),
            Some((Value::Float(f), _)) => Ok(f),
            Some((Value::Int(i), _)) => Ok(i as f64),
            Some((v, line)) => Err(Self::wrong_type(key, line, "a number", &v)),
        }
    }

    fn bool(&mut self, key: &str, default: bool) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(default),
            Some((Value::Bool(b), _)) => Ok(b),
            Some((v, line)) => Err(Self::wrong_type(key, line, "true or false", &v)),
        }
    }

    /// Reads `<prefix>_color`, `<prefix>_tolerance` and `<prefix>_min_pixels`.
    /// An empty or missing color disables the target.
    fn color_target(&mut self, prefix: &str, default_min_pixels: i64) -> Result<Option<ColorTarget>, String> {
        let color = self.string(&format!("{prefix}_color"), "")?;
        let tolerance = self.int(&format!("{prefix}_tolerance"), 30)?;
        let min_pixels = self.int(&format!("{prefix}_min_pixels"), default_min_pixels)?;
        if color.trim().is_empty() {
            return Ok(None);
        }
        let color = Rgb::parse(&color).map_err(|e| format!("{prefix}_color: {e}"))?;
        let tolerance = u8::try_from(tolerance).map_err(|_| format!("{prefix}_tolerance must be 0-255"))?;
        let min_pixels = usize::try_from(min_pixels)
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| format!("{prefix}_min_pixels must be at least 1"))?;
        Ok(Some(ColorTarget { color, tolerance, min_pixels }))
    }

    fn finish(self) -> Result<(), String> {
        let unknown: Vec<String> = self
            .map
            .iter()
            .filter(|(k, _)| !self.used.contains(*k))
            .map(|(k, (_, line))| format!("`{k}` (line {line})"))
            .collect();
        if unknown.is_empty() { Ok(()) } else { Err(format!("unknown setting(s): {}", unknown.join(", "))) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_is_valid() {
        let cfg = Config::parse(EXAMPLE).expect("example config must parse");
        assert_eq!(cfg.button, KeyCode::BTN_LEFT);
        assert_eq!(cfg.device_settle, Duration::from_millis(300));
        assert_eq!(cfg.hyprland_config, HyprConfigFormat::Lua);
    }

    #[test]
    fn parses_values_and_comments() {
        let doc = parse_document(
            "top = 1\n[a]\ns = \"x # not a comment\" # comment\nf = 0.5\nb = true\nn = -3\nbig = 60_000\n",
        )
        .unwrap();
        assert_eq!(doc["top"].0, Value::Int(1));
        assert_eq!(doc["a.s"].0, Value::Str("x # not a comment".into()));
        assert_eq!(doc["a.f"].0, Value::Float(0.5));
        assert_eq!(doc["a.b"].0, Value::Bool(true));
        assert_eq!(doc["a.n"].0, Value::Int(-3));
        assert_eq!(doc["a.big"].0, Value::Int(60_000));
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        let base = "[detect]\nactive_color = \"#ffffff\"\n[general]\nmode = \"hold\"\n";
        assert!(Config::parse(base).is_ok());
        let err = Config::parse(&format!("{base}typo_key = 1\n")).unwrap_err();
        assert!(err.contains("general.typo_key"), "{err}");
        assert!(Config::parse("[general]\nmode = hold\n").is_err());
        assert!(Config::parse("[general]\nmode = \"hold\"\nmode = \"spam\"\n").is_err());
        assert!(Config::parse("[general]\nbutton = \"BTN_NOPE\"\n").is_err());
    }

    #[test]
    fn mode_requirements() {
        // hold/spam need an active color to know when the catch ends.
        assert!(Config::parse("[general]\nmode = \"spam\"\n").is_err());
        // track needs marker and zone colors.
        let err = Config::parse("[general]\nmode = \"track\"\n[track]\nmarker_color = \"#ff0000\"\n").unwrap_err();
        assert!(err.contains("zone_color"), "{err}");
    }

    #[test]
    fn color_parsing_and_matching() {
        let c = Rgb::parse("#1A2b3C").unwrap();
        assert_eq!(c, Rgb { r: 0x1a, g: 0x2b, b: 0x3c });
        assert_eq!(c.hex(), "#1a2b3c");
        assert!(Rgb::parse("#12345").is_err());
        let t = ColorTarget { color: c, tolerance: 5, min_pixels: 1 };
        assert!(t.matches([0x1a + 5, 0x2b - 5, 0x3c]));
        assert!(!t.matches([0x1a + 6, 0x2b, 0x3c]));
    }
}
