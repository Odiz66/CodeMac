//! Screen capture of a small region with grim (wlr-screencopy under the hood).
//!
//! grim is asked for a binary PPM on stdout, which is trivial to parse, so no
//! image crate is needed. Only the configured region is copied.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::config::Region;
use crate::vision::{Analysis, Detector};

/// An RGB image, row-major, 3 bytes per pixel.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
}

impl Frame {
    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

/// Captures `region` once.
pub fn grab(region: &Region) -> Result<Frame, String> {
    let output = Command::new("grim")
        .args(["-t", "ppm", "-g", &region.geometry(), "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group: Ctrl+C in the terminal goes to fishmacro only,
        // which then shuts down cleanly instead of seeing grim die mid-capture.
        .process_group(0)
        .output()
        .map_err(|e| format!("cannot run grim ({e}); install it with `sudo pacman -S grim`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("grim failed for region {}: {}", region.geometry(), stderr.trim()));
    }
    parse_ppm(&output.stdout)
}

/// Parses a binary (P6) PPM with maxval 255, as written by grim.
pub fn parse_ppm(data: &[u8]) -> Result<Frame, String> {
    let mut pos = 0;
    let mut header = [0usize; 3];

    if data.get(..2) != Some(b"P6") {
        return Err("grim output is not a P6 PPM image".into());
    }
    pos += 2;
    for field in header.iter_mut() {
        // Skip whitespace and `#` comments between header fields.
        loop {
            match data.get(pos) {
                Some(c) if c.is_ascii_whitespace() => pos += 1,
                Some(b'#') => {
                    while data.get(pos).is_some_and(|c| *c != b'\n') {
                        pos += 1;
                    }
                }
                _ => break,
            }
        }
        let start = pos;
        while data.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        *field =
            std::str::from_utf8(&data[start..pos]).ok().and_then(|s| s.parse().ok()).ok_or("malformed PPM header")?;
    }
    let [width, height, maxval] = header;
    if maxval != 255 {
        return Err(format!("unsupported PPM maxval {maxval}"));
    }
    // Exactly one whitespace byte separates the header from the pixels.
    if !data.get(pos).is_some_and(u8::is_ascii_whitespace) {
        return Err("malformed PPM header".into());
    }
    pos += 1;
    let len = width * height * 3;
    let pixels = data.get(pos..pos + len).ok_or("truncated PPM image")?;
    if width == 0 || height == 0 {
        return Err("empty PPM image".into());
    }
    Ok(Frame { width, height, rgb: pixels.to_vec() })
}

/// One analysed frame from the capture thread.
#[derive(Debug, Clone)]
pub struct Sample {
    /// When the capture finished.
    pub at: Instant,
    /// How long grim took.
    pub latency: Duration,
    pub analysis: Analysis,
}

/// Captures and analyses frames on a background thread so the control loop
/// never blocks on grim.
pub struct CaptureThread {
    stop: Arc<AtomicBool>,
    pub rx: Receiver<Result<Sample, String>>,
    handle: Option<JoinHandle<()>>,
}

/// Consecutive grim failures tolerated before giving up.
const MAX_FAILURES: u32 = 3;

impl CaptureThread {
    pub fn start(region: Region, detector: Detector, min_interval: Duration) -> CaptureThread {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop_flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut failures = 0;
            while !stop_flag.load(Ordering::SeqCst) {
                let started = Instant::now();
                let message = match grab(&region) {
                    Ok(frame) => {
                        failures = 0;
                        let at = Instant::now();
                        Some(Ok(Sample { at, latency: at - started, analysis: detector.analyze(&frame) }))
                    }
                    Err(e) => {
                        failures += 1;
                        (failures >= MAX_FAILURES).then_some(Err(e))
                    }
                };
                if let Some(message) = message {
                    let fatal = message.is_err();
                    if tx.send(message).is_err() || fatal {
                        break;
                    }
                }
                let elapsed = started.elapsed();
                if elapsed < min_interval {
                    thread::sleep(min_interval - elapsed);
                }
            }
        });
        CaptureThread { stop, rx, handle: Some(handle) }
    }
}

impl Drop for CaptureThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grim_style_ppm() {
        let mut data = b"P6\n2 1\n255\n".to_vec();
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let frame = parse_ppm(&data).unwrap();
        assert_eq!((frame.width, frame.height), (2, 1));
        assert_eq!(frame.pixel(1, 0), [4, 5, 6]);
    }

    #[test]
    fn parses_ppm_with_comments() {
        let mut data = b"P6 # made by hand\n1\t1 255 ".to_vec();
        data.extend_from_slice(&[9, 8, 7]);
        assert_eq!(parse_ppm(&data).unwrap().pixel(0, 0), [9, 8, 7]);
    }

    #[test]
    fn rejects_bad_ppm() {
        assert!(parse_ppm(b"P3\n1 1\n255\n").is_err());
        assert!(parse_ppm(b"P6\n2 2\n255\n\x00\x00\x00").is_err());
        assert!(parse_ppm(b"P6\n1 1\n65535\n\x00\x00\x00\x00\x00\x00").is_err());
    }
}
