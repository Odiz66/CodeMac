//! Screen capture of a small screen rectangle.

mod grim;
mod screencopy;

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::config::{Backend, CaptureCfg};

pub use grim::Grim;
pub use screencopy::Screencopy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl std::fmt::Display for Rect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{} {}x{}", self.x, self.y, self.w, self.h)
    }
}

/// A captured RGB image.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGB, row-major.
    pub data: Vec<u8>,
    /// When the frame arrived in this process.
    pub received: Instant,
    /// Compositor timestamp, if the backend provides one. Equal stamps mean the
    /// compositor handed us the same frame twice.
    pub stamp: Option<(u64, u32)>,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            data: vec![0; width as usize * height as usize * 3],
            received: Instant::now(),
            stamp: None,
        }
    }

    #[inline]
    pub fn px(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    #[inline]
    pub fn set(&mut self, x: u32, y: u32, c: [u8; 3]) {
        if x < self.width && y < self.height {
            let i = (y as usize * self.width as usize + x as usize) * 3;
            self.data[i..i + 3].copy_from_slice(&c);
        }
    }
}

pub trait Capturer {
    fn name(&self) -> &'static str;
    fn grab(&mut self, rect: Rect) -> Result<Frame>;
}

/// Opens the configured backend. With `backend = "auto"` both backends are
/// benchmarked on `bench_rect` and the faster one is kept; without a rectangle
/// the first one that works (screencopy, then grim) is used.
pub fn open(cfg: &CaptureCfg, bench_rect: Option<Rect>) -> Result<Box<dyn Capturer>> {
    let timeout = Duration::from_millis(cfg.frame_timeout_ms.max(50));
    match cfg.backend {
        Backend::Screencopy => Ok(Box::new(Screencopy::connect(&cfg.output, timeout)?)),
        Backend::Grim => Ok(Box::new(Grim::new()?)),
        Backend::Auto => {
            let mut candidates: Vec<Box<dyn Capturer>> = Vec::new();
            match Screencopy::connect(&cfg.output, timeout) {
                Ok(s) => candidates.push(Box::new(s)),
                Err(e) => println!("capture: screencopy unavailable: {e:#}"),
            }
            match Grim::new() {
                Ok(g) => candidates.push(Box::new(g)),
                Err(e) => println!("capture: grim unavailable: {e:#}"),
            }
            if candidates.is_empty() {
                bail!("no capture backend works (see messages above)");
            }
            let Some(rect) = bench_rect else {
                return Ok(candidates.remove(0));
            };
            let mut best: Option<(f64, Box<dyn Capturer>)> = None;
            for mut c in candidates {
                match bench(c.as_mut(), rect, 3, 20) {
                    Ok(s) => {
                        println!(
                            "capture: {:<10} {:6.1} fps  (avg {:5.1} ms)",
                            c.name(),
                            s.fps,
                            s.mean_ms
                        );
                        // Screencopy is tried first and kept unless grim is clearly
                        // faster: it has less latency per frame, uses less CPU and
                        // reports frame timestamps (duplicate detection).
                        if best.as_ref().is_none_or(|(fps, _)| s.fps > *fps * 1.1) {
                            best = Some((s.fps, c));
                        }
                    }
                    Err(e) => println!("capture: {} failed: {e:#}", c.name()),
                }
            }
            let (_, c) = best.ok_or_else(|| anyhow::anyhow!("every capture backend failed"))?;
            println!("capture: using {}", c.name());
            Ok(c)
        }
    }
}

pub struct BenchStats {
    pub fps: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    pub duplicates: usize,
}

pub fn bench(c: &mut dyn Capturer, rect: Rect, warmup: usize, frames: usize) -> Result<BenchStats> {
    for _ in 0..warmup {
        c.grab(rect)?;
    }
    let mut times = Vec::with_capacity(frames);
    let mut duplicates = 0;
    let mut last_stamp = None;
    let start = Instant::now();
    for _ in 0..frames {
        let t = Instant::now();
        let f = c.grab(rect)?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        if f.stamp.is_some() && f.stamp == last_stamp {
            duplicates += 1;
        }
        last_stamp = f.stamp;
    }
    let total = start.elapsed().as_secs_f64();
    times.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    Ok(BenchStats {
        fps: frames as f64 / total,
        mean_ms: times.iter().sum::<f64>() / times.len() as f64,
        p50_ms: pct(0.5),
        p95_ms: pct(0.95),
        max_ms: *times.last().unwrap(),
        duplicates,
    })
}
