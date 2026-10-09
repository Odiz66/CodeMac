//! Virtual mouse/keyboard through /dev/uinput.
//!
//! uinput works below the compositor, so it behaves like a real device on
//! Wayland. It sends keycodes, not characters: `KEY_E` is the key in the "E"
//! position of a US layout, whatever your layout prints there.

use std::io;
use std::thread;
use std::time::Duration;

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, BusType, InputId, KeyCode, KeyEvent, RelativeAxisCode};

const DEVICE_NAME: &str = "fishmacro virtual input";

/// Time given to the compositor to read the final release events before the
/// device is destroyed (events still queued on a removed device are dropped).
const RELEASE_FLUSH: Duration = Duration::from_millis(50);

pub struct VirtualInput {
    /// `None` in dry-run mode: button changes are printed instead of sent.
    device: Option<VirtualDevice>,
    button: KeyCode,
    /// Every key/button the device can send; all of them are released on exit.
    keys: Vec<KeyCode>,
    held: bool,
}

impl VirtualInput {
    /// Creates the device and waits `settle` so the compositor has picked it up
    /// before the first event (events sent earlier are silently lost).
    pub fn new(button: KeyCode, settle: Duration) -> io::Result<VirtualInput> {
        // Mouse buttons plus relative axes make libinput classify the device
        // as a pointer; without REL_X/REL_Y button events may be ignored.
        let mut keys = vec![KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE];
        if !keys.contains(&button) {
            keys.push(button);
        }
        let mut key_set = AttributeSet::<KeyCode>::new();
        for key in &keys {
            key_set.insert(*key);
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        axes.insert(RelativeAxisCode::REL_X);
        axes.insert(RelativeAxisCode::REL_Y);
        axes.insert(RelativeAxisCode::REL_WHEEL);

        let device = VirtualDevice::builder()
            .map_err(explain_open_error)?
            .name(DEVICE_NAME)
            .input_id(InputId::new(BusType::BUS_USB, 0x1234, 0x5678, 1))
            .with_keys(&key_set)?
            .with_relative_axes(&axes)?
            .build()?;

        thread::sleep(settle);
        Ok(VirtualInput { device: Some(device), button, keys, held: false })
    }

    /// A stand-in that sends nothing and prints each press/release, for
    /// `fishmacro run --dry-run`.
    pub fn dry_run(button: KeyCode) -> VirtualInput {
        VirtualInput { device: None, button, keys: vec![button], held: false }
    }

    /// Presses or releases the reel button; does nothing if already in that state.
    pub fn set_held(&mut self, held: bool) -> io::Result<()> {
        if held != self.held {
            match &mut self.device {
                Some(device) => device.emit(&[*KeyEvent::new(self.button, held as i32)])?,
                None => println!("  [dry-run] {} {:?}", if held { "press" } else { "release" }, self.button),
            }
            self.held = held;
        }
        Ok(())
    }

    /// Single press and release, for `fishmacro test-input`.
    pub fn tap(&mut self, hold_for: Duration) -> io::Result<()> {
        self.set_held(true)?;
        thread::sleep(hold_for);
        self.set_held(false)
    }

    /// Sends a release for every key/button the device has. The kernel drops
    /// releases for keys that are not down, so this is always safe to call.
    pub fn release_all(&mut self) -> io::Result<()> {
        let events: Vec<_> = self.keys.iter().map(|k| *KeyEvent::new(*k, 0)).collect();
        match &mut self.device {
            Some(device) => device.emit(&events)?,
            None if self.held => println!("  [dry-run] release {:?}", self.button),
            None => {}
        }
        self.held = false;
        Ok(())
    }
}

impl Drop for VirtualInput {
    fn drop(&mut self) {
        // Runs on normal exit, on `?` errors and while unwinding from a panic.
        if let Err(e) = self.release_all() {
            eprintln!("warning: failed to release buttons on exit: {e}");
        }
        if self.device.is_some() {
            thread::sleep(RELEASE_FLUSH);
        }
    }
}

fn explain_open_error(e: io::Error) -> io::Error {
    let hint = match e.kind() {
        io::ErrorKind::PermissionDenied => {
            "no permission to open /dev/uinput; install the udev rule, add yourself to the `uinput` group and log in again (see README)"
        }
        io::ErrorKind::NotFound => {
            "/dev/uinput does not exist; load the module with `sudo modprobe uinput` (see README)"
        }
        _ => "cannot open /dev/uinput",
    };
    io::Error::new(e.kind(), format!("{hint}: {e}"))
}
