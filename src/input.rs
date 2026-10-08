//! Virtual mouse + keyboard through /dev/uinput.
//!
//! Hyprland sees them as ordinary input devices, so this works on Wayland
//! where X11 tools (xdotool, ...) do not. The mouse button and any held key
//! are released when the program stops, on Drop, from the panic hook and from
//! the signal thread.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode};

static GLOBAL: OnceLock<Arc<Input>> = OnceLock::new();

/// Accepts Linux key names ("KEY_BACKSLASH"), short names ("enter", "1") or "\".
pub fn parse_key(name: &str) -> Result<KeyCode> {
    let n = name.trim();
    if n == "\\" {
        return Ok(KeyCode::KEY_BACKSLASH);
    }
    let upper = n.to_ascii_uppercase();
    let full = if upper.starts_with("KEY_") || upper.starts_with("BTN_") {
        upper
    } else {
        format!("KEY_{upper}")
    };
    KeyCode::from_str(&full).map_err(|_| {
        anyhow!("unknown key {name:?}; use Linux key names like KEY_BACKSLASH, KEY_ENTER or KEY_1")
    })
}

pub struct Input {
    /// `None` in dry-run mode: nothing is sent, key presses are only logged.
    mouse: Mutex<Option<VirtualDevice>>,
    keyboard: Mutex<Option<VirtualDevice>>,
    lmb: AtomicBool,
    /// When false, presses are dropped (releases always go through). Cleared
    /// by the signal thread the moment F6 / Ctrl+C arrives.
    armed: AtomicBool,
    keys_down: Mutex<Vec<KeyCode>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn uinput_error(e: std::io::Error) -> anyhow::Error {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => anyhow!(
            "no permission to open /dev/uinput. Run `fish scripts/setup-uinput.fish` once, then log out and back in \
             (check with: ls -l /dev/uinput; groups)"
        ),
        std::io::ErrorKind::NotFound => {
            anyhow!(
                "/dev/uinput does not exist; load the module with `sudo modprobe uinput` (setup-uinput.fish makes it permanent)"
            )
        }
        _ => anyhow!("creating a uinput device failed: {e}"),
    }
}

impl Input {
    pub fn create(settle: Duration) -> Result<Arc<Input>> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE] {
            buttons.insert(b);
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for a in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
        ] {
            axes.insert(a);
        }
        let mouse = VirtualDevice::builder()
            .map_err(uinput_error)?
            .name("fisch-macro virtual mouse")
            .with_keys(&buttons)
            .map_err(uinput_error)?
            .with_relative_axes(&axes)
            .map_err(uinput_error)?
            .build()
            .map_err(uinput_error)?;

        // KEY_ESC..KEY_F12 so libinput/udev classify it as a full keyboard.
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=88u16 {
            keys.insert(KeyCode::new(code));
        }
        let keyboard = VirtualDevice::builder()
            .map_err(uinput_error)?
            .name("fisch-macro virtual keyboard")
            .with_keys(&keys)
            .map_err(uinput_error)?
            .build()
            .map_err(uinput_error)?;

        // Hyprland needs a moment to pick up new devices; events sent before
        // that are silently lost.
        thread::sleep(settle);

        Ok(Self::install(Some(mouse), Some(keyboard)))
    }

    /// No virtual devices: every action is a no-op and key presses are logged.
    pub fn dry_run() -> Arc<Input> {
        Self::install(None, None)
    }

    fn install(mouse: Option<VirtualDevice>, keyboard: Option<VirtualDevice>) -> Arc<Input> {
        let input = Arc::new(Input {
            mouse: Mutex::new(mouse),
            keyboard: Mutex::new(keyboard),
            lmb: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            keys_down: Mutex::new(Vec::new()),
        });
        let _ = GLOBAL.set(input.clone());
        input
    }

    /// The instance used by the panic hook and the signal thread.
    pub fn global() -> Option<&'static Arc<Input>> {
        GLOBAL.get()
    }

    pub fn arm(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    fn emit(dev: &Mutex<Option<VirtualDevice>>, events: &[InputEvent]) -> Result<()> {
        match lock(dev).as_mut() {
            Some(d) => d
                .emit(events)
                .map_err(|e| anyhow!("writing to uinput failed: {e}")),
            None => Ok(()),
        }
    }

    fn key_event(code: KeyCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code.code(), value)
    }

    /// Presses or releases the left mouse button (no-op when already in that state).
    pub fn set_lmb(&self, down: bool) -> Result<()> {
        if down && !self.armed.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.lmb.load(Ordering::SeqCst) == down {
            return Ok(());
        }
        Self::emit(
            &self.mouse,
            &[Self::key_event(KeyCode::BTN_LEFT, down as i32)],
        )?;
        self.lmb.store(down, Ordering::SeqCst);
        Ok(())
    }

    /// Moves the pointer one pixel right and back; Roblox sometimes ignores a
    /// click on a pointer that has not moved since it was warped.
    pub fn wiggle(&self) -> Result<()> {
        for dx in [1, -1] {
            Self::emit(
                &self.mouse,
                &[InputEvent::new(
                    EventType::RELATIVE.0,
                    RelativeAxisCode::REL_X.0,
                    dx,
                )],
            )?;
            thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    }

    pub fn key_down(&self, key: KeyCode) -> Result<()> {
        self.key_down_inner(key, false)
    }

    fn key_down_inner(&self, key: KeyCode, force: bool) -> Result<()> {
        if !force && !self.armed.load(Ordering::SeqCst) {
            return Ok(());
        }
        Self::emit(&self.keyboard, &[Self::key_event(key, 1)])?;
        if lock(&self.keyboard).is_none() {
            crate::log!("[dry-run] key {key:?}");
        }
        let mut down = lock(&self.keys_down);
        if !down.contains(&key) {
            down.push(key);
        }
        Ok(())
    }

    pub fn key_up(&self, key: KeyCode) -> Result<()> {
        Self::emit(&self.keyboard, &[Self::key_event(key, 0)])?;
        lock(&self.keys_down).retain(|k| *k != key);
        Ok(())
    }

    /// Press + release, blocking for `hold`.
    pub fn tap(&self, key: KeyCode, hold: Duration) -> Result<()> {
        self.key_down(key)?;
        thread::sleep(hold);
        self.key_up(key)
    }

    /// Like `tap`, but also works while disarmed (used for cleanup).
    pub fn force_tap(&self, key: KeyCode, hold: Duration) -> Result<()> {
        self.key_down_inner(key, true)?;
        thread::sleep(hold);
        self.key_up(key)
    }

    /// Releases the mouse button and every key we pressed. Never fails.
    pub fn release_all(&self) {
        let _ = Self::emit(&self.mouse, &[Self::key_event(KeyCode::BTN_LEFT, 0)]);
        self.lmb.store(false, Ordering::SeqCst);
        let keys: Vec<KeyCode> = lock(&self.keys_down).drain(..).collect();
        for k in keys {
            let _ = Self::emit(&self.keyboard, &[Self::key_event(k, 0)]);
        }
    }

    /// Panic-hook variant: never blocks on a mutex the panicking thread may hold.
    pub fn release_all_nonblocking(&self) {
        self.armed.store(false, Ordering::SeqCst);
        for _ in 0..10 {
            if let Ok(mut m) = self.mouse.try_lock() {
                if let Some(m) = m.as_mut() {
                    let _ = m.emit(&[Self::key_event(KeyCode::BTN_LEFT, 0)]);
                }
                self.lmb.store(false, Ordering::SeqCst);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        if let (Ok(mut kb), Ok(mut down)) = (self.keyboard.try_lock(), self.keys_down.try_lock())
            && let Some(kb) = kb.as_mut()
        {
            for k in down.drain(..) {
                let _ = kb.emit(&[Self::key_event(k, 0)]);
            }
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_names() {
        assert_eq!(parse_key("KEY_BACKSLASH").unwrap(), KeyCode::KEY_BACKSLASH);
        assert_eq!(parse_key("\\").unwrap(), KeyCode::KEY_BACKSLASH);
        assert_eq!(parse_key("enter").unwrap(), KeyCode::KEY_ENTER);
        assert_eq!(parse_key("1").unwrap(), KeyCode::KEY_1);
        assert!(parse_key("KEY_NOPE").is_err());
    }
}
