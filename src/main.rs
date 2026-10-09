//! fishmacro: reels in a fish in a game on Hyprland/Wayland.
//!
//! You cast; when a fish bites you press the toggle key (a Hyprland bind that
//! sends SIGUSR1), the macro reels via a uinput virtual device while watching a
//! small screen region (built-in wlr-screencopy client, or grim if
//! configured), and stops by itself when the minigame is over. The kill-switch key (SIGUSR2) or Ctrl+C stops it at any time. Every
//! held button is released on every exit path.

mod calibrate;
mod capture;
mod config;
mod hypr;
mod input;
mod png;
mod reel;
mod screencopy;
mod signals;
mod vision;
mod wayland;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use capture::Capturer;
use config::{CaptureBackend, Config, Mode};
use input::VirtualInput;

const USAGE: &str = "\
Usage: fishmacro [--config PATH] [COMMAND]

Commands:
  run          Wait for the toggle key and reel (default)
               (--dry-run: print button presses instead of sending them)
  init         Write the example config (add --force to overwrite)
  calibrate    Capture the region once, save screenshots, print detected colors
               (--delay SECS, default 3: time to bring up the minigame)
  watch        Print live detection results without sending any input
  test-input   Click the reel button once (--delay SECS, default 3)
  help         Show this text

Hotkeys are Hyprland binds that signal this process:
  pkill -USR1 -x fishmacro   start/stop reeling
  pkill -USR2 -x fishmacro   kill switch: release everything and exit

Config: $XDG_CONFIG_HOME/fishmacro/config.toml unless --config is given.";

struct Args {
    command: String,
    config: PathBuf,
    delay: u64,
    force: bool,
    dry_run: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args =
        Args { command: "run".into(), config: config::default_path(), delay: 3, force: false, dry_run: false };
    let mut command = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" | "-c" => args.config = it.next().ok_or("--config needs a path")?.into(),
            "--delay" => {
                args.delay = it.next().and_then(|v| v.parse().ok()).ok_or("--delay needs a whole number of seconds")?
            }
            "--force" => args.force = true,
            "--dry-run" => args.dry_run = true,
            "-h" | "--help" => command = Some("help".to_string()),
            _ if arg.starts_with('-') => return Err(format!("unknown option `{arg}`")),
            _ if command.is_none() => command = Some(arg),
            _ => return Err(format!("unexpected argument `{arg}`")),
        }
    }
    if let Some(c) = command {
        args.command = c;
    }
    Ok(args)
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    let args = parse_args().map_err(|e| format!("{e}\n\n{USAGE}"))?;
    // Installed first so a stray toggle/kill signal never kills a fishmacro
    // process with the default signal action.
    signals::install().map_err(|e| format!("cannot install signal handlers: {e}"))?;

    match args.command.as_str() {
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        "init" => init(&args.config, args.force),
        "run" => run(&Config::load(&args.config)?, args.dry_run),
        "calibrate" => {
            let cfg = Config::load(&args.config)?;
            require_capture_tools(&cfg)?;
            println!("Bring up the reeling minigame in the game window.");
            if countdown(args.delay) {
                let out_dir = std::env::current_dir().map_err(|e| e.to_string())?;
                calibrate::calibrate(&cfg, &out_dir)?;
            }
            Ok(())
        }
        "watch" => {
            let cfg = Config::load(&args.config)?;
            require_capture_tools(&cfg)?;
            calibrate::watch(&cfg)
        }
        "test-input" => {
            let cfg = Config::load(&args.config)?;
            let mut input = VirtualInput::new(cfg.button, cfg.device_settle).map_err(|e| e.to_string())?;
            println!("Virtual device created. Focus the window that should receive the click.");
            if countdown(args.delay) {
                input.tap(Duration::from_millis(80)).map_err(|e| e.to_string())?;
                println!("Sent one {:?} press + release.", cfg.button);
            }
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

fn run(cfg: &Config, dry_run: bool) -> Result<(), String> {
    require_capture_tools(cfg)?;
    if cfg.cursor.is_some() {
        require_tool("hyprctl", "it ships with Hyprland; is this running inside Hyprland?")?;
    }

    let mode = match cfg.mode {
        Mode::Hold => "hold".to_string(),
        Mode::Spam => format!("spam ({} clicks/s)", cfg.spam.clicks_per_second),
        Mode::Track => "track".to_string(),
    };
    println!("fishmacro: mode {mode}, button {:?}, region {}", cfg.button, cfg.region.geometry());
    // One capturer (one compositor connection) for the whole run.
    let mut capturer = Capturer::new(cfg.capture_backend);
    println!("Capture: {}", capture::probe(&mut capturer, &cfg.region)?);
    let mut capturer = Some(capturer);

    // Creating the device includes the settle delay (300 ms by default) so
    // the compositor has picked it up before the first event.
    let mut input = if dry_run {
        println!("Dry run: no virtual device, button presses are only printed.");
        VirtualInput::dry_run(cfg.button)
    } else {
        VirtualInput::new(cfg.button, cfg.device_settle).map_err(|e| e.to_string())?
    };
    signals::take_toggle(); // ignore presses made while starting up
    println!("Ready. Press the toggle key when a fish bites; kill switch or Ctrl+C exits.");

    while !signals::should_exit() {
        if !signals::take_toggle() {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        println!("Reeling...");
        let started = Instant::now();
        match reel::reel(cfg, &mut input, &mut capturer) {
            Ok(outcome) => println!("  {} ({:.1} s)", outcome.describe(), started.elapsed().as_secs_f32()),
            Err(e) => eprintln!("  reeling aborted: {e}"),
        }
    }

    // Dropping the device sends a release for every key/button first.
    drop(input);
    if signals::kill_requested() {
        println!("Kill switch: all buttons released, exiting.");
    } else {
        println!("All buttons released, exiting.");
    }
    Ok(())
}

fn init(path: &Path, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!("{} already exists (use --force to overwrite)", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, config::EXAMPLE).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

/// Counts down `secs` seconds. Returns false if Ctrl+C / kill switch interrupted it.
fn countdown(secs: u64) -> bool {
    for left in (1..=secs).rev() {
        println!("  {left}...");
        for _ in 0..10 {
            if signals::should_exit() {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    !signals::should_exit()
}

/// grim is only needed with `capture_backend = "grim"`.
fn require_capture_tools(cfg: &Config) -> Result<(), String> {
    match cfg.capture_backend {
        CaptureBackend::Grim => require_tool("grim", "install it with `sudo pacman -S grim`"),
        CaptureBackend::Screencopy => Ok(()),
    }
}

fn require_tool(name: &str, hint: &str) -> Result<(), String> {
    let found = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false);
    if found { Ok(()) } else { Err(format!("`{name}` not found in PATH; {hint}")) }
}
