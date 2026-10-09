//! `debug`: shows what the detector sees, without touching the mouse.
//!
//! Prints one line per frame (capture interval, FPS, bar, fish, progress),
//! saves raw + annotated PNGs of the bar region, and runs the reel logic in
//! "dry-run" mode so you can play a minigame by hand and see whether the
//! macro would have counted it as perfect.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::{self, Capturer, Frame, Grim, Rect, Screencopy};
use crate::cast::{CastFinder, CastWatch};
use crate::config::Config;
use crate::detect::{Detector, Layout};
use crate::hypr::Hypr;
use crate::img::save_png;
use crate::reel::Reel;
use crate::signals::{self, Flags};
use crate::util::paint;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Stop after this many frames (0 = until Ctrl+C).
    #[arg(long, default_value_t = 0)]
    frames: u64,
    /// Save raw + annotated PNGs every N frames (0 = never).
    #[arg(long, default_value_t = 30)]
    save_every: u64,
    /// Directory for the PNGs.
    #[arg(long, default_value = "debug")]
    out: PathBuf,
    /// Print only every Nth frame line.
    #[arg(long, default_value_t = 1)]
    print_every: u64,
    /// Benchmark screencopy against grim on the bar region, then exit.
    #[arg(long)]
    bench: bool,
    /// Frames per backend for --bench.
    #[arg(long, default_value_t = 180)]
    bench_frames: usize,
    /// Save a full-screen screenshot with the configured regions drawn on it, then exit.
    #[arg(long)]
    full: bool,
    /// Watch the cast power bar instead: charge casts by hand and see the fill
    /// and when the macro would release.
    #[arg(long)]
    cast: bool,
}

pub fn debug(cfg: Config, args: Args) -> Result<()> {
    let flags = signals::install(true)?;
    if args.full {
        return full_screenshot(&cfg, &args.out);
    }
    if args.cast {
        return watch_cast(&cfg, &flags, args.frames, &args.out);
    }
    let layout = Layout::from_config(&cfg.layout)?;
    let rect = layout.capture;
    if args.bench {
        return bench(&cfg, rect, args.bench_frames);
    }

    let mut cap = capture::open(&cfg.capture, Some(rect))?;
    let mut det = match Detector::with_layout(layout.clone(), &cfg.colors, &cfg.detection) {
        Ok(d) => Some(d),
        Err(e) => {
            println!("{e:#}\nCapturing and saving PNGs only.");
            None
        }
    };
    println!(
        "Capturing {rect} with {}. Ctrl+C to stop. The mouse is never touched.",
        cap.name()
    );
    if args.save_every > 0 {
        println!(
            "PNGs every {} frames -> {}/",
            args.save_every,
            args.out.display()
        );
    }

    let start = Instant::now();
    let mut last: Option<Instant> = None;
    let mut fps = 0.0f64;
    let (mut n, mut bar_hits, mut fish_hits, mut dups) = (0u64, 0u64, 0u64, 0u64);
    let (mut min_dt, mut max_dt) = (f64::MAX, 0.0f64);
    let mut last_stamp = None;
    let mut streak = 0;
    let mut reel: Option<(Reel, Instant)> = None;

    while !flags.quitting() && (args.frames == 0 || n < args.frames) {
        let frame = match cap.grab(rect) {
            Ok(f) => f,
            Err(e) => {
                println!("capture failed: {e:#}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        n += 1;
        let dt = last.map(|l| frame.received.duration_since(l).as_secs_f64() * 1000.0);
        last = Some(frame.received);
        if let Some(dt) = dt {
            min_dt = min_dt.min(dt);
            max_dt = max_dt.max(dt);
            let inst = 1000.0 / dt.max(0.001);
            fps = if fps == 0.0 {
                inst
            } else {
                fps * 0.9 + inst * 0.1
            };
        }
        let duplicate = frame.stamp.is_some() && frame.stamp == last_stamp;
        last_stamp = frame.stamp;
        dups += duplicate as u64;

        let mut line = format!(
            "#{n:<6} {:>6} {fps:5.1} fps",
            dt.map_or("-".into(), |d| format!("{d:.1}ms"))
        );
        if duplicate {
            line.push_str(" dup");
        }
        if let Some(det) = det.as_mut() {
            let d = det.detect(&frame);
            bar_hits += d.bar.is_some() as u64;
            fish_hits += (!d.fish.is_empty()) as u64;
            match d.bar {
                Some(b) => line.push_str(&format!(
                    " | bar {:6.1}..{:6.1} w{:<4.0}",
                    b.left,
                    b.right,
                    b.width()
                )),
                None => line.push_str(" | bar        -          "),
            }
            match d.fish.first() {
                Some(f) => line.push_str(&format!(" | fish {:6.1}", f.center())),
                None => line.push_str(" | fish      -"),
            }
            if d.fish.len() > 1 {
                line.push_str(&format!(" (+{})", d.fish.len() - 1));
            }
            if let (Some(b), Some(f)) = (d.bar, d.fish.first()) {
                let m = (f.center() - b.left).min(b.right - f.center());
                line.push_str(&if m >= 0.0 {
                    format!(" | in  m{m:+5.1}")
                } else {
                    paint(&format!(" | OUT m{m:+5.1}"), "31")
                });
            }
            if let Some(p) = d.progress {
                line.push_str(&format!(" | prog {:3.0}%", p * 100.0));
            }

            // Dry-run of the reel logic.
            streak = if d.bar.is_some() && !d.fish.is_empty() {
                streak + 1
            } else {
                0
            };
            if reel.is_none() && streak >= cfg.detection.start_frames {
                println!("{}", paint("--- minigame started ---", "36"));
                reel = Some((Reel::new(&cfg, det.layout.track_width()), frame.received));
            }
            if let Some((r, t0)) = reel.as_mut() {
                let s = r.step(
                    frame.received.duration_since(*t0).as_secs_f64(),
                    &d,
                    duplicate,
                );
                line.push_str(&format!(
                    " | would {} d{:.2}",
                    if s.hold { "HOLD" } else { "rel " },
                    s.ctl.duty
                ));
                if let Some(reason) = s.ended {
                    let st = r.finish();
                    let verdict = if st.perfect() {
                        paint("PERFECT", "1;32")
                    } else {
                        paint("not perfect", "33")
                    };
                    println!(
                        "{} ({reason:?}): {verdict} - {:.1} s, in bar {:.1}% of {} frames, {} outside (longest {}), \
                         min margin {:+.1} px, {} progress drops",
                        paint("--- minigame ended", "36"),
                        st.duration_s,
                        st.inside_ratio() * 100.0,
                        st.measured,
                        st.outside,
                        st.longest_outside_frames,
                        st.min_margin_px,
                        st.progress_drops
                    );
                    reel = None;
                }
            }

            if args.save_every > 0 && n % args.save_every == 0 {
                save_pair(&args.out, n, &frame, Some(&det.annotate(&frame, &d)))?;
            }
        } else if args.save_every > 0 && n % args.save_every == 0 {
            save_pair(&args.out, n, &frame, None)?;
        }
        if n % args.print_every.max(1) == 0 {
            println!("{line}");
        }
    }

    let secs = start.elapsed().as_secs_f64();
    println!();
    println!(
        "{n} frames in {secs:.1} s = {:.1} fps average",
        n as f64 / secs.max(1e-9)
    );
    if n > 1 {
        println!("frame interval min {min_dt:.1} ms, max {max_dt:.1} ms; duplicate frames: {dups}");
    }
    if det.is_some() && n > 0 {
        println!(
            "bar detected in {:.0}% of frames, fish in {:.0}% (only meaningful while a minigame was on screen)",
            100.0 * bar_hits as f64 / n as f64,
            100.0 * fish_hits as f64 / n as f64
        );
    }
    Ok(())
}

fn save_pair(dir: &Path, n: u64, raw: &Frame, ann: Option<&Frame>) -> Result<()> {
    save_png(&dir.join(format!("frame_{n:06}.png")), raw)?;
    if let Some(a) = ann {
        save_png(&dir.join(format!("frame_{n:06}_ann.png")), a)?;
    }
    Ok(())
}

fn bench(cfg: &Config, rect: Rect, frames: usize) -> Result<()> {
    println!("Benchmarking capture of {rect} ({frames} frames each). Keep the game running.\n");
    let timeout = Duration::from_millis(cfg.capture.frame_timeout_ms.max(50));
    let mut best: Option<(&str, f64)> = None;
    for name in ["screencopy", "grim"] {
        let c: Result<Box<dyn Capturer>> = match name {
            "screencopy" => Screencopy::connect(&cfg.capture.output, timeout)
                .map(|c| Box::new(c) as Box<dyn Capturer>),
            _ => Grim::new().map(|c| Box::new(c) as Box<dyn Capturer>),
        };
        let mut c = match c {
            Ok(c) => c,
            Err(e) => {
                println!("{name:<10} unavailable: {e:#}");
                continue;
            }
        };
        match capture::bench(c.as_mut(), rect, 5, frames) {
            Ok(s) => {
                println!(
                    "{name:<10} {:6.1} fps | per frame: mean {:5.1} ms, median {:5.1}, p95 {:5.1}, max {:5.1} | duplicates {}",
                    s.fps, s.mean_ms, s.p50_ms, s.p95_ms, s.max_ms, s.duplicates
                );
                if best.is_none_or(|(_, f)| s.fps > f) {
                    best = Some((name, s.fps));
                }
            }
            Err(e) => println!("{name:<10} failed: {e:#}"),
        }
    }
    if let Some((name, fps)) = best {
        println!("\nFastest: {name} ({fps:.0} fps). backend = \"auto\" picks it automatically,");
        println!("or pin it in config.toml: [capture] backend = \"{name}\"");
        if fps < 50.0 {
            println!("Below the 60 Hz refresh rate: raise controller.lookahead_ms to compensate.");
        }
    }
    Ok(())
}

fn full_screenshot(cfg: &Config, out: &Path) -> Result<()> {
    let hypr = Hypr::connect(cfg.window.dispatch_style)?;
    let monitors = hypr.monitors()?;
    let l = &cfg.layout;
    let mon = monitors
        .iter()
        .find(|m| {
            l.is_calibrated()
                && l.track_left >= m.x
                && l.track_left < m.x + m.width
                && l.track_top >= m.y
                && l.track_top < m.y + m.height
        })
        .or_else(|| monitors.iter().find(|m| m.focused))
        .or(monitors.first())
        .ok_or_else(|| anyhow::anyhow!("hyprctl reported no monitors"))?;
    let rect = Rect {
        x: mon.x,
        y: mon.y,
        w: (mon.width as f64 / mon.scale).round() as i32,
        h: (mon.height as f64 / mon.scale).round() as i32,
    };
    println!(
        "Monitor {} {}x{} @ {:.0} Hz, scale {}",
        mon.name, mon.width, mon.height, mon.refresh_rate, mon.scale
    );
    let mut cap = capture::open(&cfg.capture, None)?;
    let frame = cap.grab(rect)?;
    let raw_path = out.join("fullscreen.png");
    save_png(&raw_path, &frame)?;

    let mut ann = frame.clone();
    let to_local = |x: i32, y: i32| (x - rect.x, y - rect.y);
    let outline = |f: &mut Frame, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 3]| {
        for x in x0..=x1 {
            for y in [y0, y1] {
                if x >= 0 && y >= 0 {
                    f.set(x as u32, y as u32, c);
                }
            }
        }
        for y in y0..=y1 {
            for x in [x0, x1] {
                if x >= 0 && y >= 0 {
                    f.set(x as u32, y as u32, c);
                }
            }
        }
    };
    if let Ok(layout) = Layout::from_config(l) {
        let c = layout.capture;
        let (x, y) = to_local(c.x, c.y);
        outline(&mut ann, x - 1, y - 1, x + c.w, y + c.h, [255, 0, 255]);
        let (tx0, ty0) = to_local(l.track_left, l.track_top);
        let (tx1, ty1) = to_local(l.track_right, l.track_bottom);
        outline(&mut ann, tx0, ty0, tx1, ty1, [255, 220, 0]);
        if let Some(p) = layout.progress {
            let py = c.y + p.y as i32 - rect.y;
            outline(
                &mut ann,
                c.x + p.x0 as i32 - rect.x,
                py,
                c.x + p.x1 as i32 - rect.x,
                py,
                [0, 255, 255],
            );
        }
    } else {
        println!("The reel bar is not calibrated yet; only the cursor park point is drawn.");
    }
    {
        let r = CastFinder::new(&cfg.cast).rect;
        let (x, y) = to_local(r.x, r.y);
        outline(&mut ann, x, y, x + r.w - 1, y + r.h - 1, [255, 128, 0]);
    }
    let (cx, cy) = to_local(cfg.window.cursor_x, cfg.window.cursor_y);
    for d in -12..=12 {
        for w in -1..=1 {
            for (x, y) in [(cx + d, cy + w), (cx + w, cy + d)] {
                if x >= 0 && y >= 0 {
                    ann.set(x as u32, y as u32, [0, 255, 0]);
                }
            }
        }
    }
    let ann_path = out.join("fullscreen_ann.png");
    save_png(&ann_path, &ann)?;
    println!("Saved {} and {}", raw_path.display(), ann_path.display());
    println!(
        "magenta = capture region, yellow = reel track, cyan = progress row, orange = power bar search area, \
         green cross = cursor park point"
    );
    Ok(())
}

/// `debug --cast`: prints what the power bar finder sees while you charge casts by hand.
fn watch_cast(cfg: &Config, flags: &Flags, frames: u64, out: &Path) -> Result<()> {
    let finder = CastFinder::new(&cfg.cast);
    let mut cap = capture::open(&cfg.capture, Some(finder.rect))?;
    println!(
        "Looking for the power bar in {}. Charge casts by hand; Ctrl+C to stop.",
        finder.rect
    );
    println!("\"RELEASE\" marks the frame where the macro would let go.");
    let mut watch: Option<(CastWatch, Instant, bool, u32)> = None;
    let mut n = 0u64;
    let mut saved = false;
    while !flags.quitting() && (frames == 0 || n < frames) {
        let frame = cap.grab(finder.rect)?;
        n += 1;
        let hint = watch.as_ref().map(|w| w.3);
        let Some(r) = finder.read(&frame, hint) else {
            if let Some((w, ..)) = watch.take() {
                println!("--- bar gone (peak {:.0}%) ---", w.peak() * 100.0);
            }
            continue;
        };
        if watch.is_none() {
            watch = Some((CastWatch::new(&cfg.cast), frame.received, false, r.bottom));
            println!(
                "--- power bar found at {},{} (cap) - {} px tall ---",
                finder.rect.x + r.cap.0 as i32,
                finder.rect.y + r.cap.1 as i32,
                r.bottom - r.cap.1
            );
            if !saved {
                save_png(&out.join("cast_found.png"), &frame)?;
                saved = true;
            }
        }
        let (w, t0, released, bottom) = watch.as_mut().expect("just set");
        *bottom = (*bottom).max(r.bottom);
        let t = frame.received.duration_since(*t0).as_secs_f64();
        let mut line = format!(
            "{:6.1}% {}",
            r.fill * 100.0,
            "#".repeat((r.fill * 40.0).round() as usize)
        );
        if w.update(t, r.fill) && !*released {
            *released = true;
            line.push_str(&format!("   <-- RELEASE ({} ms)", (t * 1000.0).round()));
        }
        println!("{line}");
    }
    Ok(())
}
