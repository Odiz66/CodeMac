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
    let frame = capturer.grab(&region, &signals::should_exit)?;

    let region_path = out_dir.join(REGION_FILE);
    let zoom = (1200 / frame.width.max(frame.height)).clamp(1, 8);
    save(&region_path, &upscale(&frame, zoom))?;

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

    // Context: the region plus a margin around it (kept on the same monitor),
    // with the region outlined. Optional: a failure here only skips the image.
    let context = context_box(region, cfg.calibrate_margin as i32, capturer.monitor_rect(&region));
    match capturer.grab(&context, &signals::should_exit) {
        Ok(mut context_frame) => {
            let scale = context_frame.width as f32 / context.width as f32;
            outline(
                &mut context_frame,
                ((region.x - context.x) as f32 * scale).round() as isize,
                ((region.y - context.y) as f32 * scale).round() as isize,
                (region.width as f32 * scale).round() as isize,
                (region.height as f32 * scale).round() as isize,
            );
            let context_path = out_dir.join(CONTEXT_FILE);
            save(&context_path, &context_frame)?;
            println!("  {}  (region outlined in magenta, with its surroundings)", context_path.display());
        }
        Err(e) => println!("  (no context screenshot: {e})"),
    }
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

/// `region` grown by `margin` on every side, clipped to `monitor` (x, y, w, h)
/// when known, otherwise to non-negative coordinates.
fn context_box(region: Region, margin: i32, monitor: Option<(i32, i32, i32, i32)>) -> Region {
    let (mx, my, mw, mh) = monitor.unwrap_or((0, 0, i32::MAX, i32::MAX));
    let x0 = (region.x - margin).max(mx);
    let y0 = (region.y - margin).max(my);
    let x1 = (region.x + region.width as i32 + margin).min(mx.saturating_add(mw));
    let y1 = (region.y + region.height as i32 + margin).min(my.saturating_add(mh));
    Region { x: x0, y: y0, width: (x1 - x0).max(1) as u32, height: (y1 - y0).max(1) as u32 }
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
    let mut capturer = Capturer::new(cfg.capture_backend);
    println!("Capture: {}", capture::probe(&mut capturer, &cfg.region)?);
    let capture = CaptureThread::start(capturer, cfg.region, Detector::from_config(cfg), cfg.capture_interval);
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
    fn context_box_stays_on_the_monitor() {
        let region = Region { x: 1080, y: 1350, width: 400, height: 30 };
        // Bottom edge of a 2560x1440 monitor: clipped at y = 1440.
        let c = context_box(region, 150, Some((0, 0, 2560, 1440)));
        assert_eq!(c, Region { x: 930, y: 1200, width: 700, height: 240 });
        // Second monitor to the right: never crosses into the first one.
        let region = Region { x: 2600, y: 100, width: 50, height: 20 };
        let c = context_box(region, 150, Some((2560, 0, 1920, 1080)));
        assert_eq!((c.x, c.y), (2560, 0));
        // Unknown monitor (grim backend): only clamped at 0.
        let c = context_box(Region { x: 10, y: 10, width: 5, height: 5 }, 150, None);
        assert_eq!(c, Region { x: 0, y: 0, width: 165, height: 165 });
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
