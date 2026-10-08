//! Fallback capture: `grim -g "X,Y WxH" -t ppm -` per frame.

use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use super::{Capturer, Frame, Rect};

pub struct Grim;

impl Grim {
    pub fn new() -> Result<Grim> {
        // Only checks that grim can be started; its exit code for -h varies by version.
        Command::new("grim")
            .arg("-h")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("`grim` is not installed (sudo pacman -S grim)")?;
        Ok(Grim)
    }
}

impl Capturer for Grim {
    fn name(&self) -> &'static str {
        "grim"
    }

    fn grab(&mut self, rect: Rect) -> Result<Frame> {
        let out = Command::new("grim")
            .args(["-g", &rect.to_string(), "-t", "ppm", "-"])
            .stdin(Stdio::null())
            .output()
            .context("running grim")?;
        if !out.status.success() {
            bail!(
                "grim failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let mut frame = parse_ppm(&out.stdout)?;
        frame.received = Instant::now();
        Ok(frame)
    }
}

/// Parses a binary (P6) PPM.
pub fn parse_ppm(data: &[u8]) -> Result<Frame> {
    let mut pos = 0;
    let mut token = || -> Result<String> {
        // Skip whitespace and comments.
        loop {
            match data.get(pos) {
                Some(c) if c.is_ascii_whitespace() => pos += 1,
                Some(b'#') => {
                    while data.get(pos).is_some_and(|&c| c != b'\n') {
                        pos += 1;
                    }
                }
                Some(_) => break,
                None => bail!("truncated PPM header"),
            }
        }
        let start = pos;
        while data.get(pos).is_some_and(|c| !c.is_ascii_whitespace()) {
            pos += 1;
        }
        Ok(String::from_utf8_lossy(&data[start..pos]).into_owned())
    };
    if token()? != "P6" {
        bail!("not a binary PPM (P6)");
    }
    let width: u32 = token()?.parse().context("PPM width")?;
    let height: u32 = token()?.parse().context("PPM height")?;
    let maxval: u32 = token()?.parse().context("PPM maxval")?;
    if maxval == 0 || maxval > 65535 {
        bail!("bad PPM maxval {maxval}");
    }
    // Exactly one whitespace byte separates the header from the pixels.
    pos += 1;
    let pixels = width as usize * height as usize;
    let mut frame = Frame::new(width, height);
    if maxval < 256 {
        let body = data
            .get(pos..pos + pixels * 3)
            .context("truncated PPM data")?;
        if maxval == 255 {
            frame.data.copy_from_slice(body);
        } else {
            for (d, &s) in frame.data.iter_mut().zip(body) {
                *d = (s as u32 * 255 / maxval) as u8;
            }
        }
    } else {
        let body = data
            .get(pos..pos + pixels * 6)
            .context("truncated PPM data")?;
        for (d, s) in frame.data.iter_mut().zip(body.chunks_exact(2)) {
            *d = (u16::from_be_bytes([s[0], s[1]]) as u32 * 255 / maxval) as u8;
        }
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ppm() {
        let mut data = b"P6\n# grim\n2 1\n255\n".to_vec();
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let f = parse_ppm(&data).unwrap();
        assert_eq!((f.width, f.height), (2, 1));
        assert_eq!(f.px(1, 0), [4, 5, 6]);
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse_ppm(b"P6 2 2 255\n\x01\x02").is_err());
    }
}
