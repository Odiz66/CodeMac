//! `fishmacro calibrate` and `fishmacro watch`: check the capture region and
//! colors without sending any input.

use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use crate::capture::{self, CaptureThread, Capturer, Frame};
use crate::config::{ColorTarget, Config, Mode, Region};
use crate::png;
use crate::reel::Tracker;
use crate::signals;
use crate::vision::{self, Analysis, Detector};

const REGION_FILE: &str = "fishmacro-region.png";
const CONTEXT_FILE: &str = "fishmacro-context.png";
const OUTLINE: [u8; 3] = [255, 0, 255];

/// Captures the region once, saves screenshots and prints what the detector sees.
pub fn calibrate(cfg: &Config, out_dir: &Path) -> Result<(), String> {
    let region = cfg.region;
    let mut capturer = Capturer::new(cfg.capture_backend);
    let frame = capturer.grab(&region)?;

    // Context: the region plus a margin around it, with the region outlined.
    let m = cfg.calibrate_margin as i32;
    let cx = (region.x - m).max(0);
    let cy = (region.y - m).max(0);
    let context = Region {
        x: cx,
        y: cy,
        width: (region.x + region.width as i32 + m - cx) as u32,
        height: (region.y + region.height as i32 + m - cy) as u32,
    };
    let mut context_frame = capturer.grab(&context)?;
    let scale = context_frame.width as f32 / context.width as f32;
    outline(
        &mut context_frame,
        ((region.x - cx) as f32 * scale).round() as isize,
        ((region.y - cy) as f32 * scale).round() as isize,
        (region.width as f32 * scale).round() as isize,
        (region.height as f32 * scale).round() as isize,
    );

    let region_path = out_dir.join(REGION_FILE);
    let context_path = out_dir.join(CONTEXT_FILE);
    let zoom = (1200 / frame.width.max(frame.height)).clamp(1, 8);
    let zoomed = upscale(&frame, zoom);
    save(&region_path, &zoomed)?;
    save(&context_path, &context_frame)?;

    println!(
        "Region {} (layout px) -> captured {}x{} px with {}",
        region.geometry(),
        frame.width,
        frame.height,
        capturer.describe()
    );
    if frame.width != region.width as usize || frame.height != region.height as usize {
        println!("  note: monitor scale is not 1; detection works in captured pixels, that's fine");
    }
    println!();
    println!("Saved:");
    println!("  {}  (the region, zoomed {zoom}x)", region_path.display());
    println!("  {}  (region outlined in magenta with {m} px around it)", context_path.display());
    println!();

    let total = frame.width * frame.height;
    println!("Most common colors in the region:");
    for (color, count) in vision::dominant_colors(&frame, 8) {
        println!("  {}  {:5.1}%", color.hex(), 100.0 * count as f32 / total as f32);
    }
    println!();

    let analysis = Detector::from_config(cfg).analyze(&frame);
    print_report(cfg, &analysis);
    Ok(())
}

fn print_report(cfg: &Config, a: &Analysis) {
    println!("Detection with the current config:");
    let line = |name: &str, target: Option<ColorTarget>, count: usize| match target {
        Some(t) => println!(
            "  {name:<8} {} ±{:<3} {count:6} px matching (need {}) -> {}",
            t.color.hex(),
            t.tolerance,
            t.min_pixels,
            if count >= t.min_pixels { "FOUND" } else { "not found" }
        ),
        None => println!("  {name:<8} (not set)"),
    };
    line("active", cfg.active, a.active_pixels);
    line("done", cfg.done, a.done_pixels);
    if cfg.mode == Mode::Track {
        line("marker", cfg.track.marker, a.marker_pixels);
        line("zone", cfg.track.zone, a.zone_pixels);
        println!("  marker at {}, zone {}", fmt_pos(a.marker), fmt_zone(a));
    }
    println!();
    println!(
        "Minigame visible: {}   catch done: {}",
        if a.minigame_visible { "YES" } else { "no" },
        if a.catch_done { "YES" } else { "no" }
    );
    println!("  Check both cases: with the minigame on screen this must say YES, and with it");
    println!("  closed it must say no (otherwise the macro never sees the catch end; use a");
    println!("  more specific color or a lower tolerance).");
}

/// Live detector output without sending input. Ends on Ctrl+C or the kill switch.
pub fn watch(cfg: &Config) -> Result<(), String> {
    println!("Capture: {}", capture::probe(cfg.capture_backend, &cfg.region)?);
    let capture =
        CaptureThread::start(cfg.region, Detector::from_config(cfg), cfg.capture_interval, cfg.capture_backend);
    let mut tracker = Tracker::new(cfg.track.deadband, cfg.track.lookahead);
    let mut frames = 0u32;
    let mut latency = Duration::ZERO;
    let mut window_start = Instant::now();
    let mut latest: Option<(Analysis, Option<bool>)> = None;

    println!("Watching region {} (Ctrl+C to stop). No input is sent.", cfg.region.geometry());
    while !signals::should_exit() {
        signals::take_toggle();
        match capture.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(sample) => {
                let sample = sample?;
                frames += 1;
                latency += sample.latency;
                let decision = (cfg.mode == Mode::Track && sample.analysis.minigame_visible)
                    .then(|| tracker.update(sample.at, &sample.analysis))
                    .flatten();
                latest = Some((sample.analysis, decision));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("capture thread stopped".into()),
        }

        let elapsed = window_start.elapsed();
        if elapsed >= Duration::from_millis(250) {
            if let Some((a, decision)) = &latest {
                let fps = frames as f32 / elapsed.as_secs_f32();
                let grim_ms = latency.as_secs_f32() * 1000.0 / frames.max(1) as f32;
                let mut line = format!(
                    "{fps:5.1} fps  capture {grim_ms:4.1} ms  visible {:3}  done {:3}",
                    yes_no(a.minigame_visible),
                    yes_no(a.catch_done)
                );
                if cfg.mode == Mode::Track {
                    line += &format!(
                        "  marker {}  zone {}  -> {}",
                        fmt_pos(a.marker),
                        fmt_zone(a),
                        match decision {
                            Some(true) => "HOLD",
                            Some(false) => "release",
                            None => "-",
                        }
                    );
                }
                println!("{line}");
            }
            frames = 0;
            latency = Duration::ZERO;
            window_start = Instant::now();
        }
    }
    Ok(())
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn fmt_pos(p: Option<f32>) -> String {
    p.map_or_else(|| "  -  ".into(), |p| format!("{p:.3}"))
}

fn fmt_zone(a: &Analysis) -> String {
    a.zone.map_or_else(|| "     -     ".into(), |z| format!("{:.3}-{:.3}", z.lo, z.hi))
}

fn save(path: &Path, frame: &Frame) -> Result<(), String> {
    png::write_rgb(path, frame.width, frame.height, &frame.rgb)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Draws a 2 px rectangle just outside (x, y, w, h), clipped to the frame.
fn outline(frame: &mut Frame, x: isize, y: isize, w: isize, h: isize) {
    let (fw, fh) = (frame.width as isize, frame.height as isize);
    let mut put = |px: isize, py: isize| {
        if (0..fw).contains(&px) && (0..fh).contains(&py) {
            let i = ((py * fw + px) * 3) as usize;
            frame.rgb[i..i + 3].copy_from_slice(&OUTLINE);
        }
    };
    for t in 1..=2 {
        for px in (x - t)..(x + w + t) {
            put(px, y - t);
            put(px, y + h - 1 + t);
        }
        for py in (y - t)..(y + h + t) {
            put(x - t, py);
            put(x + w - 1 + t, py);
        }
    }
}

/// Nearest-neighbour enlargement so a thin bar is easy to inspect.
fn upscale(frame: &Frame, k: usize) -> Frame {
    let (width, height) = (frame.width * k, frame.height * k);
    let mut rgb = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        for x in 0..width {
            rgb.extend_from_slice(&frame.pixel(x / k, y / k));
        }
    }
    Frame { width, height, rgb }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_surrounds_region_without_covering_it() {
        let mut f = Frame { width: 10, height: 10, rgb: vec![0; 300] };
        outline(&mut f, 3, 3, 4, 4);
        assert_eq!(f.pixel(3, 3), [0, 0, 0]); // inside untouched
        assert_eq!(f.pixel(6, 6), [0, 0, 0]);
        assert_eq!(f.pixel(2, 2), OUTLINE);
        assert_eq!(f.pixel(1, 5), OUTLINE);
        assert_eq!(f.pixel(8, 5), OUTLINE);
        assert_eq!(f.pixel(0, 5), [0, 0, 0]);
    }

    #[test]
    fn upscale_repeats_pixels() {
        let f = Frame { width: 2, height: 1, rgb: vec![1, 1, 1, 2, 2, 2] };
        let u = upscale(&f, 3);
        assert_eq!((u.width, u.height), (6, 3));
        assert_eq!(u.pixel(2, 2), [1, 1, 1]);
        assert_eq!(u.pixel(3, 0), [2, 2, 2]);
    }
}
