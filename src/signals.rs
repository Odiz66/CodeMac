//! Signal handling. Wayland has no global hotkeys for normal clients, so the
//! Hyprland F6 bind sends SIGUSR1 (`pkill -USR1 -x fisch-macro`) instead.
//!
//! - SIGUSR1: toggle on/off (in `calibrate`: "read the cursor now")
//! - SIGUSR2: pause
//! - SIGINT / SIGTERM / SIGHUP / SIGQUIT: quit (a second one exits immediately)
//!
//! The mouse button is released right here in the signal thread, before the
//! main loop even notices.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;

use anyhow::{Context, Result};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::Signals;

use crate::input::Input;

pub struct Flags {
    pub enabled: AtomicBool,
    pub quit: AtomicBool,
    /// Number of SIGUSR1 received so far.
    pub usr1: AtomicU32,
}

impl Flags {
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }

    pub fn usr1_count(&self) -> u32 {
        self.usr1.load(Ordering::SeqCst)
    }
}

fn release_input() {
    if let Some(input) = Input::global() {
        input.arm(false);
        input.release_all();
    }
}

pub fn install(start_enabled: bool) -> Result<Arc<Flags>> {
    let flags = Arc::new(Flags {
        enabled: AtomicBool::new(start_enabled),
        quit: AtomicBool::new(false),
        usr1: AtomicU32::new(0),
    });
    let mut signals = Signals::new([SIGUSR1, SIGUSR2, SIGINT, SIGTERM, SIGHUP, SIGQUIT])
        .context("installing signal handlers")?;
    let f = flags.clone();
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                match sig {
                    SIGUSR1 => {
                        f.usr1.fetch_add(1, Ordering::SeqCst);
                        let was_enabled = f.enabled.fetch_xor(true, Ordering::SeqCst);
                        if was_enabled {
                            release_input();
                        }
                    }
                    SIGUSR2 => {
                        f.enabled.store(false, Ordering::SeqCst);
                        release_input();
                    }
                    _ => {
                        let second = f.quit.swap(true, Ordering::SeqCst);
                        f.enabled.store(false, Ordering::SeqCst);
                        release_input();
                        if second {
                            eprintln!(
                                "\nSecond stop signal: exiting immediately (mouse button released)."
                            );
                            std::process::exit(130);
                        }
                    }
                }
            }
        })
        .context("spawning the signal thread")?;
    Ok(flags)
}
