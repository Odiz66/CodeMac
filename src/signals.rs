//! Signal handling.
//!
//! The hotkeys are Hyprland binds that run `pkill -USR1 -x fishmacro` (toggle)
//! and `pkill -USR2 -x fishmacro` (kill switch), so the macro never has to read
//! your keyboard. SIGINT (Ctrl+C), SIGTERM and SIGHUP request a clean exit.
//! The handlers only set atomics, which is async-signal-safe; the main loop
//! polls them and releases every held button on the way out.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

static TOGGLE: AtomicBool = AtomicBool::new(false);
static KILL: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    match sig {
        libc::SIGUSR1 => TOGGLE.store(true, Ordering::SeqCst),
        libc::SIGUSR2 => KILL.store(true, Ordering::SeqCst),
        _ => QUIT.store(true, Ordering::SeqCst),
    }
}

/// Installs the handlers. Call once at startup, before anything is pressed.
pub fn install() -> io::Result<()> {
    for sig in [libc::SIGUSR1, libc::SIGUSR2, libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: `sigaction` is zero-initialised and then filled in; the handler
        // only touches atomics.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// True once per toggle-key press.
pub fn take_toggle() -> bool {
    TOGGLE.swap(false, Ordering::SeqCst)
}

/// Kill-switch key pressed: stop everything and exit.
pub fn kill_requested() -> bool {
    KILL.load(Ordering::SeqCst)
}

/// Ctrl+C / SIGTERM / SIGHUP received.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

/// Either of the two above.
pub fn should_exit() -> bool {
    kill_requested() || quit_requested()
}
