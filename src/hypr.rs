//! Cursor warping through Hyprland IPC (`hyprctl`).
//!
//! uinput mouse motion is relative and goes through pointer acceleration, so
//! exact positioning is done by the compositor instead. In Hyprland 0.55+
//! with a Lua config, `hyprctl dispatch` takes a Lua dispatcher expression
//! (`hl.dsp.cursor.move({ x = .., y = .. })`); with a legacy hyprland.conf it
//! takes the old `movecursor x y`. Coordinates are global layout coordinates.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::config::HyprConfigFormat;

pub fn move_cursor(x: i32, y: i32, format: HyprConfigFormat) -> Result<(), String> {
    let output = Command::new("hyprctl")
        .arg("dispatch")
        .arg(dispatch_arg(x, y, format))
        .stdin(Stdio::null())
        .process_group(0)
        .output()
        .map_err(|e| format!("cannot run hyprctl: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reply = stdout.trim();
    if output.status.success() && reply.starts_with("ok") {
        Ok(())
    } else {
        Err(format!(
            "hyprctl cursor move failed: {}{}\n  (is hyprland.config_format = {:?} right for your setup?)",
            reply,
            String::from_utf8_lossy(&output.stderr).trim(),
            format
        ))
    }
}

fn dispatch_arg(x: i32, y: i32, format: HyprConfigFormat) -> String {
    match format {
        HyprConfigFormat::Lua => format!("hl.dsp.cursor.move({{ x = {x}, y = {y} }})"),
        HyprConfigFormat::Legacy => format!("movecursor {x} {y}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_syntax() {
        assert_eq!(dispatch_arg(1280, 720, HyprConfigFormat::Lua), "hl.dsp.cursor.move({ x = 1280, y = 720 })");
        assert_eq!(dispatch_arg(10, 20, HyprConfigFormat::Legacy), "movecursor 10 20");
    }
}
