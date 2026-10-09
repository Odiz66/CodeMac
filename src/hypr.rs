//! Thin wrapper around `hyprctl`.
//!
//! Hyprland 0.55+ can be configured with `hyprland.lua`; with a Lua config
//! `hyprctl dispatch` evaluates its argument as Lua, so the classic
//! `dispatch movecursor X Y` no longer parses and the call has to be
//! `dispatch 'hl.dsp.cursor.move({ x = X, y = Y })'`. Which one is active is
//! read from `hyprctl -j status` (configProvider), and a failed dispatch is
//! retried in the other style.

use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::config::DispatchStyle;

#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub address: String,
    #[serde(default)]
    pub class: String,
    #[serde(default, rename = "initialClass")]
    pub initial_class: String,
    #[serde(default)]
    pub size: [i32; 2],
    #[serde(default)]
    pub fullscreen: serde_json::Value,
}

impl Client {
    pub fn matches(&self, class: &str) -> bool {
        self.class.eq_ignore_ascii_case(class) || self.initial_class.eq_ignore_ascii_case(class)
    }

    pub fn is_fullscreen(&self) -> bool {
        match &self.fullscreen {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) > 0,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    #[serde(default = "one")]
    pub scale: f64,
    #[serde(default, rename = "refreshRate")]
    pub refresh_rate: f64,
    #[serde(default)]
    pub focused: bool,
}

fn one() -> f64 {
    1.0
}

pub struct Hypr {
    lua: bool,
}

fn hyprctl(args: &[&str]) -> Result<String> {
    let out = Command::new("hyprctl").args(args).output().context(
        "running hyprctl (is Hyprland installed and are you inside a Hyprland session?)",
    )?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        bail!(
            "hyprctl {} failed: {} {}",
            args.join(" "),
            text,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(text)
}

impl Hypr {
    pub fn connect(style: DispatchStyle) -> Result<Hypr> {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            bail!(
                "HYPRLAND_INSTANCE_SIGNATURE is not set. Start this program from inside your Hyprland session \
                 (a terminal or RustRover launched from Hyprland)."
            );
        }
        let lua = match style {
            DispatchStyle::Lua => true,
            DispatchStyle::Legacy => false,
            DispatchStyle::Auto => Self::detect_lua(),
        };
        Ok(Hypr { lua })
    }

    fn detect_lua() -> bool {
        hyprctl(&["-j", "status"])
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("configProvider")
                    .and_then(|p| p.as_str())
                    .map(|p| p.eq_ignore_ascii_case("lua"))
            })
            .unwrap_or(false)
    }

    pub fn uses_lua(&self) -> bool {
        self.lua
    }

    pub fn version(&self) -> Result<String> {
        Ok(hyprctl(&["version"])?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string())
    }

    /// Shows a message on top of everything (including the fullscreen game)
    /// with `hyprctl notify`. Best effort: errors are ignored.
    pub fn notify(&self, text: &str, ms: u32) {
        let _ = hyprctl(&["notify", "1", &ms.to_string(), "rgb(33ccff)", text]);
    }

    pub fn cursor_pos(&self) -> Result<(i32, i32)> {
        let s = hyprctl(&["cursorpos"])?;
        parse_cursorpos(&s).ok_or_else(|| anyhow!("unexpected `hyprctl cursorpos` output: {s:?}"))
    }

    pub fn move_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        self.dispatch(
            &format!("movecursor {x} {y}"),
            &format!("hl.dsp.cursor.move({{ x = {x}, y = {y} }})"),
        )
    }

    pub fn focus(&mut self, client: &Client) -> Result<()> {
        let addr = &client.address;
        self.dispatch(
            &format!("focuswindow address:{addr}"),
            &format!("hl.dsp.focus({{ window = \"address:{addr}\" }})"),
        )
    }

    fn dispatch(&mut self, legacy: &str, lua: &str) -> Result<()> {
        let (first, second) = if self.lua {
            (lua, legacy)
        } else {
            (legacy, lua)
        };
        let r1 = hyprctl(&["dispatch", first]);
        if matches!(&r1, Ok(s) if s == "ok") {
            return Ok(());
        }
        let r2 = hyprctl(&["dispatch", second]);
        if matches!(&r2, Ok(s) if s == "ok") {
            self.lua = !self.lua;
            println!(
                "hyprctl: switched to {} dispatch syntax",
                if self.lua { "Lua" } else { "legacy" }
            );
            return Ok(());
        }
        let show = |r: Result<String>| match r {
            Ok(s) => s,
            Err(e) => format!("{e:#}"),
        };
        bail!(
            "hyprctl dispatch failed: `{first}` -> {}; `{second}` -> {}",
            show(r1),
            show(r2)
        )
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        let s = hyprctl(&["-j", "clients"])?;
        serde_json::from_str(&s).context("parsing `hyprctl -j clients`")
    }

    pub fn find_client(&self, class: &str) -> Result<Option<Client>> {
        Ok(self.clients()?.into_iter().find(|c| c.matches(class)))
    }

    /// Class of the focused window, if any.
    pub fn active_class(&self) -> Result<Option<String>> {
        let s = hyprctl(&["-j", "activewindow"])?;
        let v: serde_json::Value =
            serde_json::from_str(&s).context("parsing `hyprctl -j activewindow`")?;
        Ok(v.get("class").and_then(|c| c.as_str()).map(str::to_string))
    }

    pub fn is_active(&self, class: &str) -> Result<bool> {
        Ok(self
            .active_class()?
            .is_some_and(|c| c.eq_ignore_ascii_case(class)))
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        let s = hyprctl(&["-j", "monitors"])?;
        serde_json::from_str(&s).context("parsing `hyprctl -j monitors`")
    }
}

fn parse_cursorpos(s: &str) -> Option<(i32, i32)> {
    let (x, y) = s.trim().split_once(',')?;
    Some((
        x.trim().parse::<f64>().ok()?.round() as i32,
        y.trim().parse::<f64>().ok()?.round() as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cursorpos() {
        assert_eq!(parse_cursorpos("1280, 720"), Some((1280, 720)));
        assert_eq!(parse_cursorpos("12.6, 3.2\n"), Some((13, 3)));
        assert_eq!(parse_cursorpos("error"), None);
    }

    #[test]
    fn parses_clients() {
        let json = r#"[{"address":"0x55d8a1b0","mapped":true,"at":[0,0],"size":[2560,1440],
            "workspace":{"id":1,"name":"1"},"class":"org.vinegarhq.Sober","title":"Sober",
            "initialClass":"org.vinegarhq.Sober","pid":4242,"fullscreen":2}]"#;
        let clients: Vec<Client> = serde_json::from_str(json).unwrap();
        assert!(clients[0].matches("org.vinegarhq.sober"));
        assert!(clients[0].is_fullscreen());
    }
}
