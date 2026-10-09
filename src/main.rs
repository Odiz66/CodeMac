//! AFK fishing macro for Roblox "Fisch" (Sober) on Hyprland / Wayland.
//!
//! See README.md for setup, calibration and tuning.

mod calibrate;
mod capture;
mod cast;
mod config;
mod controller;
mod debug;
mod detect;
mod hypr;
mod img;
mod input;
mod reel;
mod run;
mod signals;
mod sim;
mod tracker;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::input::Input;

#[derive(Parser)]
#[command(
    name = "fisch-macro",
    version,
    about = "AFK fishing macro for Roblox Fisch (Sober) on Hyprland"
)]
struct Cli {
    /// Path to the config file (created with defaults if missing).
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the macro. F6 (SIGUSR1) toggles it, Ctrl+C / SIGTERM quits.
    Run {
        /// Start fishing immediately instead of waiting for F6.
        #[arg(long)]
        start: bool,
        /// Print one line per frame while reeling.
        #[arg(short, long)]
        verbose: bool,
        /// Do everything except sending mouse/keyboard input (no /dev/uinput needed).
        #[arg(long)]
        dry_run: bool,
    },
    /// Record the reel bar position and the Ruinous Oath colours.
    Calibrate(calibrate::Args),
    /// Show detections and capture speed, save PNGs. Never sends input.
    Debug(debug::Args),
    /// Print the config with all defaults filled in.
    #[command(hide = true)]
    PrintConfig,
}

fn main() -> ExitCode {
    util::init_clock();
    // Whatever happens, never leave the mouse button held down.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(input) = Input::global() {
            input.release_all_nonblocking();
        }
        default_hook(info);
    }));

    let cli = Cli::parse();
    let result = (|| {
        let cfg = Config::load_or_create(&cli.config)?;
        match cli.command {
            Command::Run {
                start,
                verbose,
                dry_run,
            } => run::run(cfg, start, verbose, dry_run),
            Command::Calibrate(args) => calibrate::calibrate(&cli.config, cfg, args),
            Command::Debug(args) => debug::debug(cfg, args),
            Command::PrintConfig => {
                print!("{}", cfg.to_toml());
                Ok(())
            }
        }
    })();

    if let Some(input) = Input::global() {
        input.release_all();
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
