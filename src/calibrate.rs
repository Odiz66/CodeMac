//! `calibrate`: records where the reel bar is and which colours it uses.
//!
//! Positions are read with `hyprctl cursorpos` after a countdown (or right
//! away when F6 / SIGUSR1 arrives). Colours are learned from a burst of real
//! Ruinous Oath minigame frames:
//! - bar:  the pixels under the cursor (the user hovers their resting bar),
//! - background: the dominant remaining colours of the track,
//! - fish: the most common colour of narrow, full-height objects that are
//!   neither bar nor background,
//! - progress: the left end of the progress bar, which is filled as long as
//!   the minigame is running.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

use crate::capture::{self, Capturer, Frame};
use crate::config::{ColorsCfg, Config, Rgb};
use crate::detect::{Detector, Layout, Palette, dist_sq, runs};
use crate::hypr::Hypr;
use crate::img::{load_png, save_png};
use crate::signals::{self, Flags};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Part {
    Region,
    Colors,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Old hover-and-F6 wizard instead of the automatic calibration.
    #[arg(long)]
    manual: bool,
    /// --manual: seconds before each cursor position is read (0 = wait for F6).
    #[arg(long, default_value_t = 0, requires = "manual")]
    countdown: u64,
    /// --manual: only redo one part.
    #[arg(long, value_enum, requires = "manual")]
    only: Option<Part>,
    /// --manual: do not set up progress-bar reading.
    #[arg(long, requires = "manual")]
    no_progress: bool,
    /// --manual: frames captured for the colour analysis.
    #[arg(long, default_value_t = 90, requires = "manual")]
    frames: usize,
    /// Pick colours from a saved PNG (from debug/ or calibration/) instead of live.
    #[arg(long, value_name = "FILE")]
    png: Option<PathBuf>,
    /// With --png: X,Y of a pixel on your bar (repeatable).
    #[arg(long, value_name = "X,Y", value_parser = parse_point)]
    bar: Vec<(u32, u32)>,
    /// With --png: X,Y of a pixel on the fish (repeatable).
    #[arg(long, value_name = "X,Y", value_parser = parse_point)]
    fish: Vec<(u32, u32)>,
    /// With --png: X,Y of a pixel on the filled progress bar (repeatable).
    #[arg(long, value_name = "X,Y", value_parser = parse_point)]
    progress: Vec<(u32, u32)>,
    /// With --png: add to the existing colours instead of replacing them.
    #[arg(long)]
    append: bool,
}

fn parse_point(s: &str) -> std::result::Result<(u32, u32), String> {
    let (x, y) = s.split_once(',').ok_or("expected X,Y")?;
    Ok((
        x.trim().parse().map_err(|_| "bad X")?,
        y.trim().parse().map_err(|_| "bad Y")?,
    ))
}

struct Ctx {
    hypr: Hypr,
    flags: Arc<Flags>,
    countdown: u64,
}

impl Ctx {
    /// Countdown, then read the cursor position.
    fn point(&self, what: &str) -> Result<(i32, i32)> {
        self.countdown(what, "reading the cursor")?;
        let (x, y) = self.hypr.cursor_pos()?;
        println!("\r  -> cursor at {x}, {y}                          ");
        Ok((x, y))
    }

    /// Prints `what`, then counts down (F6 ends it early).
    fn countdown(&self, what: &str, action: &str) -> Result<()> {
        println!("\n> {what}");
        let start = self.flags.usr1_count();
        let mut tenths = self.countdown * 10;
        loop {
            if self.flags.quitting() {
                bail!("calibration cancelled");
            }
            if self.flags.usr1_count() != start {
                break;
            }
            if self.countdown == 0 {
                if tenths == 0 {
                    print!("\r  {action}: press F6 when ready ");
                    let _ = std::io::stdout().flush();
                    tenths = 1;
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            if tenths == 0 {
                break;
            }
            if tenths.is_multiple_of(10) {
                print!("\r  {action} in {} s (F6 = now) ", tenths / 10);
                let _ = std::io::stdout().flush();
            }
            thread::sleep(Duration::from_millis(100));
            tenths -= 1;
        }
        Ok(())
    }
}

pub fn calibrate(path: &Path, mut cfg: Config, args: Args) -> Result<()> {
    if let Some(png) = &args.png {
        return pick_from_png(path, cfg, &args, png);
    }
    let mut hypr = Hypr::connect(cfg.window.dispatch_style)?;
    let flags = signals::install(false)?;
    let mut cap = capture::open(&cfg.capture, None)?;
    println!("Calibration ({} capture). Ctrl+C cancels.", cap.name());
    match hypr.find_client(&cfg.window.class)? {
        Some(c) => {
            println!("Switching to {} in 2 s...", c.class);
            thread::sleep(Duration::from_secs(2));
            if let Err(e) = hypr.focus(&c) {
                println!("  could not focus it ({e:#}); switch to the game yourself.");
            }
        }
        None => println!(
            "Warning: no {:?} window found; switch to the game yourself.",
            cfg.window.class
        ),
    }
    let ctx = Ctx {
        hypr,
        flags,
        countdown: args.countdown,
    };

    if !args.manual {
        calibrate_auto(&ctx, cap.as_mut(), &mut cfg)?;
        cfg.save(path)?;
        println!("\nSaved to {}.", path.display());
        println!("Next: `cargo run --release -- run`, switch to the game and press F6.");
        return Ok(());
    }

    println!("Hover each spot and press F6 (cast and keep a reel minigame on screen;");
    println!("you can play it with the mouse button while hovering).");
    let all = args.only.is_none();
    if all || args.only == Some(Part::Region) {
        calibrate_region(&ctx, &mut cfg, &args)?;
        cfg.save(path)?;
        println!("\nSaved positions to {}.", path.display());
    }
    if all || args.only == Some(Part::Colors) {
        calibrate_colors(&ctx, cap.as_mut(), &mut cfg, &args)?;
        cfg.save(path)?;
        println!("\nSaved colours to {}.", path.display());
    }
    println!(
        "\nNext: `cargo run --release -- debug` while a minigame is on screen to check detection,"
    );
    println!("then `cargo run --release -- run`.");
    Ok(())
}

/// Brightness of a pixel (0..255).
fn luma(p: [u8; 3]) -> u32 {
    (p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000
}

/// True when the frame shows the reel minigame: the track is mostly dark and
/// a wide bright block (your bar, white or brown) sits on it.
pub fn looks_like_minigame(frame: &Frame, layout: &Layout) -> bool {
    let rows = layout.sample_rows(9, 0.6);
    let need = (rows.len() * 6).div_ceil(10);
    let n = layout.track_width() as usize;
    let mut dark = 0;
    let mut bright = vec![false; n];
    for (i, x) in (layout.track_x0..=layout.track_x1).enumerate() {
        if x >= frame.width {
            break;
        }
        let lum: Vec<u32> = rows
            .iter()
            .filter(|&&y| y < frame.height)
            .map(|&y| luma(frame.px(x, y)))
            .collect();
        if lum.iter().filter(|&&l| l < 60).count() >= need {
            dark += 1;
        }
        bright[i] = lum.iter().filter(|&&l| l >= 80).count() >= need;
    }
    let wide = runs(&bright, 3).iter().any(|r| {
        let w = r.end - r.start + 1;
        w * 100 >= n * 6 && w * 100 <= n * 75
    });
    dark * 100 >= n * 30 && wide && progress_visible(frame, layout)
}

/// The progress bar under the track: some row near the expected one is
/// white at its left end (the fill always starts there). True when no
/// progress bar is configured.
fn progress_visible(frame: &Frame, layout: &Layout) -> bool {
    let Some(p) = layout.progress else {
        return true;
    };
    let k = ((p.x1 - p.x0 + 1) / 50).max(4);
    let lo = p.y.saturating_sub(8);
    let hi = (p.y + 8).min(frame.height.saturating_sub(1));
    (lo..=hi).any(|y| {
        let lit = (p.x0 + 3..p.x0 + 3 + k)
            .filter(|&x| x < frame.width && luma(frame.px(x, y)) >= 170)
            .count();
        lit * 10 >= k as usize * 8
    })
}

/// Automatic calibration: wait for a minigame the user plays by hand, record
/// it and learn the colours. The positions are the built-in defaults.
fn calibrate_auto(ctx: &Ctx, cap: &mut dyn Capturer, cfg: &mut Config) -> Result<()> {
    // Capture a little more around the progress bar so its row can be refined.
    // If progress reading was switched off before, search at the default row.
    let defaults = crate::config::LayoutCfg::default();
    let mut wide_cfg = crate::config::LayoutCfg {
        padding: 16,
        ..cfg.layout.clone()
    };
    if wide_cfg.progress_y == 0 {
        wide_cfg.progress_y = defaults.progress_y;
        wide_cfg.progress_left = defaults.progress_left;
        wide_cfg.progress_right = defaults.progress_right;
    }
    let layout = Layout::from_config(&wide_cfg)?;
    let msg = "Calibration: catch a fish by hand. I record the reel minigame by myself.";
    println!("\n{msg}");
    println!("Cast, shake and play the reel minigame normally; let the fish leave your bar");
    println!("at least once so I also see the bar's other colour. F6 = record right now.");
    ctx.hypr.notify(msg, 15_000);

    let start_usr1 = ctx.flags.usr1_count();
    let mut streak = 0;
    loop {
        if ctx.flags.quitting() {
            bail!("calibration cancelled");
        }
        let f = cap.grab(layout.capture)?;
        streak = if looks_like_minigame(&f, &layout) {
            streak + 1
        } else {
            0
        };
        if streak >= 4 || ctx.flags.usr1_count() != start_usr1 {
            break;
        }
    }
    println!("Minigame found - recording 5 s, keep playing...");
    ctx.hypr
        .notify("Minigame found - keep playing (recording 5 s)", 5_000);
    // Keep only frames that show the minigame; stop when it is over.
    let t = Instant::now();
    let mut frames: Vec<Frame> = Vec::new();
    let mut last: Option<Instant> = None;
    let mut gone = 0;
    while t.elapsed() < Duration::from_secs(5) && gone < 40 {
        let f = cap.grab(layout.capture)?;
        if !looks_like_minigame(&f, &layout) {
            gone += 1;
            continue;
        }
        gone = 0;
        if last.is_none_or(|l| f.received.duration_since(l) >= Duration::from_millis(20)) {
            last = Some(f.received);
            frames.push(f);
        }
    }
    println!("  recorded {} minigame frames", frames.len());
    if frames.len() < 40 {
        ctx.hypr.notify(
            "Calibration: the minigame was too short - run it again",
            8_000,
        );
        bail!(
            "only {} frames of the minigame were recorded (need ~1 s). Nothing was saved; \
             run calibrate again and keep the fish on the line a little longer",
            frames.len()
        );
    }

    let dir = Path::new("calibration");
    for (i, f) in frames.iter().enumerate().step_by(10) {
        save_png(&dir.join(format!("frame_{i:03}.png")), f)?;
    }
    let a = analyze_auto(&frames, &layout, &cfg.colors, cfg.detection.fish_max_width)?;
    for note in &a.notes {
        println!("  note: {note}");
    }
    println!("  background: {}", shares(&a.background));
    println!("  your bar:   {}", shares(&a.bar));
    println!("  fish:       {}", shares(&a.fish));
    println!("  progress:   {}", list(&a.progress));
    cfg.colors.bar = a.bar.iter().map(|(c, _)| *c).collect();
    cfg.colors.fish = a.fish.iter().map(|(c, _)| *c).collect();
    if let Some(y) = a.progress_row {
        cfg.colors.progress = a.progress.clone();
        cfg.layout.progress_y = layout.capture.y + y as i32;
        cfg.layout.progress_left = wide_cfg.progress_left;
        cfg.layout.progress_right = wide_cfg.progress_right;
    } else {
        println!("  note: kept the previous progress bar settings");
    }

    let mut det = Detector::with_layout(layout.clone(), &cfg.colors, &cfg.detection)?;
    let (mut bar_hits, mut fish_hits) = (0, 0);
    for f in &frames {
        let d = det.detect(f);
        bar_hits += d.bar.is_some() as usize;
        fish_hits += (!d.fish.is_empty()) as usize;
    }
    let mid = &frames[frames.len() / 2];
    let d = det.detect(mid);
    save_png(&dir.join("check.png"), &det.annotate(mid, &d))?;
    let pct = |h: usize| 100.0 * h as f64 / frames.len() as f64;
    println!(
        "  check: your bar found in {:.0}% of frames, fish in {:.0}% (see calibration/check.png)",
        pct(bar_hits),
        pct(fish_hits)
    );
    if pct(bar_hits) >= 80.0 && pct(fish_hits) >= 80.0 {
        ctx.hypr
            .notify("Calibration done! Go back to the terminal.", 6_000);
    } else if pct(bar_hits) >= 50.0 && pct(fish_hits) >= 50.0 {
        println!("  Detection is only fair. Saved anyway; if the macro loses the fish, run");
        println!("  calibrate again during a longer minigame.");
        ctx.hypr.notify(
            "Calibration saved, but detection is only fair - see the terminal",
            8_000,
        );
    } else {
        ctx.hypr.notify(
            "Calibration failed - nothing saved, see the terminal",
            8_000,
        );
        bail!(
            "detection with the learned colours is too weak (bar {:.0}%, fish {:.0}%). Nothing \
             was saved. Send this output and calibration/check.png for help.",
            pct(bar_hits),
            pct(fish_hits)
        );
    }
    Ok(())
}

pub struct AutoAnalysis {
    pub background: Vec<(Rgb, f32)>,
    pub bar: Vec<(Rgb, f32)>,
    pub fish: Vec<(Rgb, f32)>,
    pub progress: Vec<Rgb>,
    /// Refined progress row, relative to the capture.
    pub progress_row: Option<u32>,
    pub notes: Vec<String>,
}

/// Learns the minigame colours from frames of a minigame played by hand.
/// Unlike `analyze` it needs no cursor position: your bar is the wide bright
/// block on the dark track, the fish is the thin full-height line.
pub fn analyze_auto(
    frames: &[Frame],
    layout: &Layout,
    colors: &ColorsCfg,
    fish_max_width: u32,
) -> Result<AutoAnalysis> {
    let mut notes = Vec::new();
    let rows = layout.sample_rows(9, 0.6);
    let n = layout.track_width() as usize;
    let xs = || layout.track_x0..=layout.track_x1;

    // 1. Background: the dark colours of the track.
    let mut dark = Vec::new();
    for f in frames.iter().step_by(2) {
        for x in xs() {
            for &y in &rows {
                let p = f.px(x, y);
                if luma(p) < 70 {
                    dark.push(p);
                }
            }
        }
    }
    let mut background = Vec::new();
    let mut cum = 0.0;
    for (c, s) in clusters(&dark, 20.0, 8) {
        if cum >= 0.9 || background.len() >= 4 {
            break;
        }
        cum += s;
        background.push((Rgb(c), s));
    }
    if background.is_empty() {
        bail!("the reel track did not look dark - was the minigame on screen?");
    }
    let bg = Palette::new(
        &background.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        28.0,
    );

    // 2. Your bar: wide runs of bright columns. The colour is decided per
    //    frame (the bar is one colour at a time: white with the fish inside,
    //    brown with it outside), so a colour seen only briefly still counts.
    let half = rows.len().div_ceil(2);
    let mut cols = vec![false; n];
    let mut per_frame = Vec::new();
    for f in frames {
        for (i, x) in xs().enumerate() {
            cols[i] = rows.iter().filter(|&&y| luma(f.px(x, y)) >= 80).count() >= half;
        }
        let mut px = Vec::new();
        for r in runs(&cols, 3).into_iter().filter(|r| {
            let w = r.end - r.start + 1;
            w * 100 >= n * 5 && w * 100 <= n * 75
        }) {
            for i in r.start..=r.end {
                for &y in &rows {
                    let p = f.px(layout.track_x0 + i as u32, y);
                    if luma(p) >= 80 {
                        px.push(p);
                    }
                }
            }
        }
        if let Some(&(c, _)) = clusters(&px, 24.0, 1).first() {
            per_frame.push(c);
        }
    }
    let bar: Vec<(Rgb, f32)> = clusters(&per_frame, 24.0, 4)
        .into_iter()
        .filter(|(_, s)| *s * per_frame.len() as f32 >= 3.0)
        .take(3)
        .map(|(c, s)| (Rgb(c), s))
        .collect();
    if bar.is_empty() {
        bail!("could not find your bar on the track - was the minigame on screen?");
    }
    if bar.len() == 1 {
        notes.push(
            "only one bar colour seen; if the bar changes colour when the fish leaves it, \
             run calibrate again and let the fish leave the bar once"
                .into(),
        );
    }
    let bar_pal = Palette::new(
        &bar.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        colors.bar_tolerance,
    );

    // 3. Fish: thin runs of columns that are neither background nor bar over
    //    most of the height.
    let tall = (rows.len() * 7).div_ceil(10);
    let max_w = (n * 3 / 100).max(fish_max_width as usize);
    let mut fish_px = Vec::new();
    let mut frames_with = 0;
    for f in frames {
        for (i, x) in xs().enumerate() {
            cols[i] = rows
                .iter()
                .filter(|&&y| {
                    let p = f.px(x, y);
                    !bg.matches(p) && !bar_pal.matches(p)
                })
                .count()
                >= tall;
        }
        let mut any = false;
        for r in runs(&cols, 1)
            .into_iter()
            .filter(|r| r.end - r.start < max_w)
        {
            any = true;
            for i in r.start..=r.end {
                for &y in &rows {
                    let p = f.px(layout.track_x0 + i as u32, y);
                    if !bg.matches(p) && !bar_pal.matches(p) {
                        fish_px.push(p);
                    }
                }
            }
        }
        frames_with += any as usize;
    }
    let fish_cl = clusters(&fish_px, 22.0, 6);
    let top = fish_cl.first().map_or(0.0, |c| c.1);
    let fish: Vec<(Rgb, f32)> = fish_cl
        .into_iter()
        .filter(|(_, s)| *s >= top * 0.25)
        .take(3)
        .map(|(c, s)| (Rgb(c), s))
        .collect();
    if fish.is_empty() {
        bail!("could not find the fish line on the track");
    }
    if frames_with * 2 < frames.len() {
        notes.push(format!(
            "the fish was only found in {frames_with} of {} frames",
            frames.len()
        ));
    }

    // 4. Progress bar: the row whose left end is filled (white) while its
    //    right end is still empty, in most frames. The white outline rows are
    //    white at both ends and are skipped that way.
    let mut progress = Vec::new();
    let mut progress_row = None;
    if let Some(p) = layout.progress {
        let width = (p.x1 - p.x0 + 1) as usize;
        let k = (width / 50).max(4) as u32;
        let left = p.x0 + 3..p.x0 + 3 + k;
        let right = p.x1.saturating_sub(3 + k)..p.x1.saturating_sub(3);
        let y_lo = p.y.saturating_sub(12);
        let y_hi = (p.y + 12).min(frames[0].height - 1);
        let mut good: Vec<u32> = Vec::new();
        for y in y_lo..=y_hi {
            let hits = frames
                .iter()
                .filter(|f| {
                    let l = left.clone().filter(|&x| luma(f.px(x, y)) >= 170).count();
                    let r = right.clone().filter(|&x| luma(f.px(x, y)) < 120).count();
                    l * 10 >= k as usize * 8 && r * 10 >= k as usize * 8
                })
                .count();
            if hits * 2 >= frames.len() {
                good.push(y);
            }
        }
        if let Some(&y) = good.get(good.len() / 2) {
            progress_row = Some(y);
            let px: Vec<[u8; 3]> = frames
                .iter()
                .flat_map(|f| left.clone().map(move |x| f.px(x, y)))
                .collect();
            progress = clusters(&px, 20.0, 2)
                .into_iter()
                .filter(|(_, s)| *s >= 0.3)
                .map(|(c, _)| Rgb(c))
                .collect();
        } else {
            notes.push("progress bar not found; progress reading is switched off".into());
        }
    }

    Ok(AutoAnalysis {
        background,
        bar,
        fish,
        progress,
        progress_row,
        notes,
    })
}

fn calibrate_region(ctx: &Ctx, cfg: &mut Config, args: &Args) -> Result<()> {
    println!("\n== Positions ==");
    let (x1, y1) = ctx
        .point("Hover the TOP-LEFT corner of the reel track (the long bar your bar slides in).")?;
    let (x2, y2) = ctx.point("Hover the BOTTOM-RIGHT corner of the reel track.")?;
    let l = &mut cfg.layout;
    l.track_left = x1.min(x2);
    l.track_right = x1.max(x2);
    l.track_top = y1.min(y2);
    l.track_bottom = y1.max(y2);
    if l.track_right - l.track_left < 40 {
        bail!(
            "the track is only {} px wide; hover the two opposite corners",
            l.track_right - l.track_left
        );
    }
    if l.track_bottom - l.track_top < 4 {
        println!(
            "  note: the track is only {} px tall; hover the corners, not the middle line.",
            l.track_bottom - l.track_top
        );
    }
    if args.no_progress {
        l.progress_y = 0;
        return Ok(());
    }
    let (px1, py1) = ctx.point("Hover the LEFT end of the PROGRESS bar (under the track).")?;
    let (px2, py2) = ctx.point("Hover the RIGHT end of the PROGRESS bar.")?;
    let l = &mut cfg.layout;
    l.progress_y = (py1 + py2) / 2;
    l.progress_left = px1.min(px2);
    l.progress_right = px1.max(px2);
    if (l.track_top..=l.track_bottom).contains(&l.progress_y) {
        println!(
            "  warning: that row is inside the reel track; progress reading will be unreliable."
        );
    }
    println!(
        "  track {},{} -> {},{}   progress row y={} x {}..{}",
        l.track_left,
        l.track_top,
        l.track_right,
        l.track_bottom,
        l.progress_y,
        l.progress_left,
        l.progress_right
    );
    Ok(())
}

fn calibrate_colors(
    ctx: &Ctx,
    cap: &mut dyn Capturer,
    cfg: &mut Config,
    args: &Args,
) -> Result<()> {
    let layout = Layout::from_config(&cfg.layout)?;
    println!("\n== Colours ==");
    let (cx, cy) = ctx.point(
        "Get a Ruinous Oath reel minigame on screen and DON'T click, so your bar rests at the LEFT \
         end of the track. Hover over YOUR BAR. I will then record about 1.5 s of frames.",
    )?;
    let rx = cx - layout.capture.x;
    let ry = cy - layout.capture.y;
    if rx < layout.track_x0 as i32
        || rx > layout.track_x1 as i32
        || ry < layout.track_y0 as i32 - 3
        || ry > layout.track_y1 as i32 + 3
    {
        bail!(
            "the cursor ({cx},{cy}) is not on the reel track; redo positions with `calibrate --manual --only region`"
        );
    }

    let n = args.frames.max(10);
    let mut frames = Vec::with_capacity(n);
    let t = Instant::now();
    for _ in 0..n {
        frames.push(cap.grab(layout.capture)?);
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "  captured {n} frames in {secs:.2} s ({:.0} fps)",
        n as f64 / secs
    );

    let a = analyze(
        &frames,
        &layout,
        rx as u32,
        &cfg.colors,
        cfg.detection.fish_max_width,
    )?;
    for note in &a.notes {
        println!("  note: {note}");
    }
    println!("  bar:        {}", list(&a.bar));
    println!("  background: {}", shares(&a.background));
    println!("  fish:       {}", shares(&a.fish));
    if layout.progress.is_some() {
        println!("  progress:   {}", list(&a.progress));
    }

    cfg.colors.bar = a.bar.clone();
    if a.fish.is_empty() {
        println!("  WARNING: no fish colour found. Keep the minigame running during the capture,");
        println!("  or pick it by hand: calibrate --png calibration/frame_000.png --fish X,Y");
    } else {
        cfg.colors.fish = a.fish.iter().map(|(c, _)| *c).collect();
    }
    if !a.progress.is_empty() {
        cfg.colors.progress = a.progress.clone();
    }

    // Save the burst for inspection / manual picking.
    let dir = Path::new("calibration");
    for (i, f) in frames.iter().enumerate().step_by(3) {
        save_png(&dir.join(format!("frame_{i:03}.png")), f)?;
    }
    if !cfg.colors.fish.is_empty() {
        let mut det = Detector::with_layout(layout, &cfg.colors, &cfg.detection)?;
        let mut bar_hits = 0;
        let mut fish_hits = 0;
        for f in &frames {
            let d = det.detect(f);
            bar_hits += d.bar.is_some() as usize;
            fish_hits += (!d.fish.is_empty()) as usize;
        }
        let d0 = det.detect(&frames[0]);
        save_png(
            &dir.join("frame_000_ann.png"),
            &det.annotate(&frames[0], &d0),
        )?;
        let pct = |h: usize| 100.0 * h as f64 / frames.len() as f64;
        println!(
            "  check: bar found in {:.0}% of frames, fish in {:.0}%",
            pct(bar_hits),
            pct(fish_hits)
        );
        if pct(bar_hits) < 85.0 || pct(fish_hits) < 85.0 {
            println!(
                "  Detection is weak. Look at calibration/*.png, raise the tolerances in [colors]"
            );
            println!("  or add colours with `calibrate --png ... --bar X,Y --fish X,Y --append`.");
        }
    }
    println!(
        "  frames saved to {}/ (frame_000_ann.png shows what was detected)",
        dir.display()
    );
    Ok(())
}

pub struct Analysis {
    pub bar: Vec<Rgb>,
    pub background: Vec<(Rgb, f32)>,
    pub fish: Vec<(Rgb, f32)>,
    pub progress: Vec<Rgb>,
    pub notes: Vec<String>,
}

pub fn analyze(
    frames: &[Frame],
    layout: &Layout,
    bar_x: u32,
    colors: &ColorsCfg,
    fish_max_width: u32,
) -> Result<Analysis> {
    let mut notes = Vec::new();
    let rows = layout.sample_rows(9, 0.6);
    let first = &frames[0];

    // 1. Bar: pixels under the cursor in the first frame.
    let x0 = bar_x.saturating_sub(2).max(layout.track_x0);
    let x1 = (bar_x + 2).min(layout.track_x1);
    let px: Vec<[u8; 3]> = (x0..=x1)
        .flat_map(|x| rows.iter().map(move |&y| first.px(x, y)))
        .collect();
    let bar: Vec<Rgb> = clusters(&px, 18.0, 3)
        .into_iter()
        .filter(|(_, s)| *s >= 0.25)
        .take(2)
        .map(|(c, _)| Rgb(c))
        .collect();
    let bar_pal = Palette::new(&bar, colors.bar_tolerance);

    // 2. Background: what remains of the track most of the time.
    let mut bg_px = Vec::new();
    for f in frames.iter().step_by(3) {
        for x in layout.track_x0..=layout.track_x1 {
            for &y in &rows {
                let p = f.px(x, y);
                if !bar_pal.matches(p) {
                    bg_px.push(p);
                }
            }
        }
    }
    let total_track = (frames.len().div_ceil(3)) * layout.track_width() as usize * rows.len();
    let bar_share = 1.0 - bg_px.len() as f32 / total_track.max(1) as f32;
    if bar_share > 0.6 {
        bail!(
            "{:.0}% of the track matches the colour under the cursor, so that is probably the track \
             background, not your bar. Hover exactly over your bar and try again.",
            bar_share * 100.0
        );
    }
    let mut background = Vec::new();
    let mut cum = 0.0;
    for (c, s) in clusters(&bg_px, 20.0, 8) {
        if cum >= 0.85 || background.len() >= 4 {
            break;
        }
        cum += s;
        background.push((Rgb(c), s));
    }
    let bg_colors: Vec<Rgb> = background.iter().map(|(c, _)| *c).collect();
    let bg_pal = Palette::new(&bg_colors, 28.0);

    // 3. Fish: narrow objects covering most of the sampled rows that are neither bar nor background.
    let max_w = (layout.track_width() * 0.08).max(fish_max_width as f32) as usize;
    let half = rows.len().div_ceil(2);
    let mut fish_px = Vec::new();
    let mut frames_with = 0;
    let mut foreign = vec![false; layout.track_width() as usize];
    for f in frames {
        for (i, x) in (layout.track_x0..=layout.track_x1).enumerate() {
            let n = rows.iter().filter(|&&y| {
                let p = f.px(x, y);
                !bar_pal.matches(p) && !bg_pal.matches(p)
            });
            foreign[i] = n.count() >= half;
        }
        let mut any = false;
        for r in runs(&foreign, 1)
            .into_iter()
            .filter(|r| r.end - r.start < max_w)
        {
            any = true;
            for x in r.start..=r.end {
                for &y in &rows {
                    let p = f.px(layout.track_x0 + x as u32, y);
                    if !bar_pal.matches(p) && !bg_pal.matches(p) {
                        fish_px.push(p);
                    }
                }
            }
        }
        frames_with += any as usize;
    }
    let fish_clusters = clusters(&fish_px, 22.0, 6);
    let top = fish_clusters.first().map_or(0.0, |c| c.1);
    let fish: Vec<(Rgb, f32)> = fish_clusters
        .into_iter()
        .filter(|(_, s)| *s >= top * 0.25)
        .take(3)
        .map(|(c, s)| (Rgb(c), s))
        .collect();
    if frames_with * 2 < frames.len() {
        notes.push(format!(
            "a fish-like object was only visible in {frames_with} of {} frames",
            frames.len()
        ));
    }
    for (c, _) in &fish {
        if bar.iter().any(|b| {
            dist_sq(c.0, [b.0[0] as i32, b.0[1] as i32, b.0[2] as i32]) as f32
                <= colors.bar_tolerance.powi(2)
        }) {
            notes.push(format!("fish colour {} is close to the bar colour; lower bar_tolerance if detection mixes them up", c.hex()));
        }
    }

    // 4. Progress fill: the left end of the progress bar.
    let mut progress = Vec::new();
    if let Some(p) = layout.progress {
        let sample = |x0: u32, x1: u32| -> Vec<[u8; 3]> {
            let mut v = Vec::new();
            for f in frames {
                for x in x0..=x1 {
                    for y in p.y.saturating_sub(1)..=p.y + 1 {
                        if x < f.width && y < f.height {
                            v.push(f.px(x, y));
                        }
                    }
                }
            }
            v
        };
        let left = clusters(&sample(p.x0 + 3, p.x0 + 5), 16.0, 3);
        if let Some(&(c, share)) = left.first() {
            if share >= 0.5 {
                progress.push(Rgb(c));
            }
            let right = clusters(
                &sample(p.x1.saturating_sub(5), p.x1.saturating_sub(3)),
                16.0,
                1,
            );
            if right.first().is_some_and(|(r, s)| {
                *s > 0.9 && dist_sq(*r, [c[0] as i32, c[1] as i32, c[2] as i32]) < 400
            }) {
                notes.push("both ends of the progress bar have the same colour; progress reading may not work".into());
            }
        }
        if progress.is_empty() {
            notes.push("could not find a stable progress colour".into());
        }
    }

    Ok(Analysis {
        bar,
        background,
        fish,
        progress,
        notes,
    })
}

/// Greedy colour clustering: 5-bit histogram bins, most common first, merged
/// into the first cluster within `radius`. Returns (mean colour, share).
pub fn clusters(px: &[[u8; 3]], radius: f32, max: usize) -> Vec<([u8; 3], f32)> {
    if px.is_empty() {
        return Vec::new();
    }
    let mut bins: HashMap<[u8; 3], (u64, [u64; 3])> = HashMap::new();
    for p in px {
        let e = bins.entry([p[0] >> 3, p[1] >> 3, p[2] >> 3]).or_default();
        e.0 += 1;
        for (acc, &v) in e.1.iter_mut().zip(p) {
            *acc += v as u64;
        }
    }
    let mut bins: Vec<(u64, [u64; 3])> = bins.into_values().collect();
    bins.sort_by_key(|b| std::cmp::Reverse(b.0));
    let r2 = (radius * radius) as i32;
    let mut cl: Vec<(u64, [u64; 3])> = Vec::new();
    for (n, sum) in bins {
        let mean = |n: u64, s: [u64; 3]| [(s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8];
        let m = mean(n, sum);
        match cl.iter_mut().find(|(cn, cs)| {
            let c = mean(*cn, *cs);
            dist_sq(m, [c[0] as i32, c[1] as i32, c[2] as i32]) <= r2
        }) {
            Some(c) => {
                c.0 += n;
                for (acc, v) in c.1.iter_mut().zip(sum) {
                    *acc += v;
                }
            }
            None => cl.push((n, sum)),
        }
    }
    cl.sort_by_key(|c| std::cmp::Reverse(c.0));
    let total = px.len() as f32;
    cl.into_iter()
        .take(max)
        .map(|(n, s)| {
            (
                [(s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8],
                n as f32 / total,
            )
        })
        .collect()
}

fn list(c: &[Rgb]) -> String {
    if c.is_empty() {
        "-".into()
    } else {
        c.iter().map(Rgb::hex).collect::<Vec<_>>().join(" ")
    }
}

fn shares(c: &[(Rgb, f32)]) -> String {
    if c.is_empty() {
        return "-".into();
    }
    c.iter()
        .map(|(c, s)| format!("{} ({:.0}%)", c.hex(), s * 100.0))
        .collect::<Vec<_>>()
        .join(" ")
}

fn median3x3(img: &Frame, x: u32, y: u32) -> Result<Rgb> {
    if x >= img.width || y >= img.height {
        bail!(
            "point {x},{y} is outside the {}x{} image",
            img.width,
            img.height
        );
    }
    let mut ch: [Vec<u8>; 3] = Default::default();
    for yy in y.saturating_sub(1)..=(y + 1).min(img.height - 1) {
        for xx in x.saturating_sub(1)..=(x + 1).min(img.width - 1) {
            let p = img.px(xx, yy);
            for k in 0..3 {
                ch[k].push(p[k]);
            }
        }
    }
    let mut out = [0u8; 3];
    for k in 0..3 {
        ch[k].sort_unstable();
        out[k] = ch[k][ch[k].len() / 2];
    }
    Ok(Rgb(out))
}

fn pick_from_png(path: &Path, mut cfg: Config, args: &Args, png: &Path) -> Result<()> {
    if args.bar.is_empty() && args.fish.is_empty() && args.progress.is_empty() {
        bail!("--png needs at least one of --bar X,Y / --fish X,Y / --progress X,Y");
    }
    let img = load_png(png).with_context(|| format!("loading {}", png.display()))?;
    let apply = |target: &mut Vec<Rgb>, points: &[(u32, u32)], name: &str| -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let picked: Vec<Rgb> = points
            .iter()
            .map(|&(x, y)| median3x3(&img, x, y))
            .collect::<Result<_>>()?;
        if !args.append {
            target.clear();
        }
        for c in picked {
            if !target
                .iter()
                .any(|t| dist_sq(c.0, [t.0[0] as i32, t.0[1] as i32, t.0[2] as i32]) < 36)
            {
                target.push(c);
            }
        }
        println!("{name}: {}", list(target));
        Ok(())
    };
    apply(&mut cfg.colors.bar, &args.bar, "bar")?;
    apply(&mut cfg.colors.fish, &args.fish, "fish")?;
    apply(&mut cfg.colors.progress, &args.progress, "progress")?;
    cfg.save(path)?;
    println!("Saved to {}.", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::tests::{BAR, BG, FISH, PROG, render, test_config};

    #[test]
    fn clusters_find_dominant_colours() {
        let mut px = vec![[10, 10, 10]; 70];
        px.extend(vec![[12, 9, 11]; 10]);
        px.extend(vec![[200, 30, 30]; 20]);
        let c = clusters(&px, 10.0, 4);
        assert_eq!(c.len(), 2);
        assert!((c[0].1 - 0.8).abs() < 1e-6);
        assert_eq!(c[1].0, [200, 30, 30]);
    }

    #[test]
    fn analysis_recovers_synthetic_colours() {
        let cfg = test_config();
        let layout = Layout::from_config(&cfg.layout).unwrap();
        // Bar resting at the left, fish wandering.
        let frames: Vec<Frame> = (0..30)
            .map(|i| render(&layout, Some((0, 24)), Some((60 + i * 3, 66 + i * 3)), 0.4))
            .collect();
        let a = analyze(
            &frames,
            &layout,
            layout.track_x0 + 10,
            &cfg.colors,
            cfg.detection.fish_max_width,
        )
        .unwrap();
        assert_eq!(a.bar, vec![Rgb(BAR)]);
        assert_eq!(a.background[0].0, Rgb(BG));
        assert_eq!(a.fish[0].0, Rgb(FISH));
        assert_eq!(a.progress, vec![Rgb(PROG)]);
    }

    #[test]
    fn hovering_the_background_is_rejected() {
        let cfg = test_config();
        let layout = Layout::from_config(&cfg.layout).unwrap();
        let frames: Vec<Frame> = (0..10)
            .map(|_| render(&layout, Some((0, 24)), Some((100, 106)), 0.4))
            .collect();
        assert!(analyze(&frames, &layout, layout.track_x0 + 150, &cfg.colors, 70).is_err());
    }

    /// A frame that looks like Fisch's reel UI in the user's screenshots.
    fn fisch_frame(layout: &Layout, bar: (u32, u32), fish: u32, progress: f32, seed: u32) -> Frame {
        let mut f = Frame::new(layout.capture.w as u32, layout.capture.h as u32);
        // Scene behind the UI: speckled rock.
        for y in 0..f.height {
            for x in 0..f.width {
                let n = (x
                    .wrapping_mul(2654435761)
                    .wrapping_add(y * 40503 + seed * 7))
                    >> 24;
                let v = if n % 3 == 0 { 200 } else { 60 } as u8;
                f.set(x, y, [v, v, v.saturating_add(30)]);
            }
        }
        let inside = fish >= bar.0 && fish <= bar.1;
        for y in layout.track_y0..=layout.track_y1 {
            for i in 0..layout.track_width() as u32 {
                let x = layout.track_x0 + i;
                let n = ((i * 31 + y * 17 + seed) % 9) as u8;
                let mut c = [16 + n, 16 + n, 18 + n];
                if i >= bar.0 && i <= bar.1 {
                    c = if inside {
                        [239, 239, 239]
                    } else {
                        [138, 90, 58]
                    };
                    // Arrow strokes: a horizontal shaft one row high.
                    let mid = (layout.track_y0 + layout.track_y1) / 2;
                    if y == mid && (i - bar.0) % 100 < 30 {
                        c = [70, 70, 70];
                    }
                }
                f.set(x, y, c);
            }
            for x in fish - 2..=fish + 2 {
                f.set(layout.track_x0 + x, y, [120, 120, 125]);
            }
        }
        if let Some(p) = layout.progress {
            let split = p.x0 + ((p.x1 - p.x0) as f32 * progress) as u32;
            for x in p.x0..=p.x1 {
                f.set(x, p.y - 4, [245, 245, 245]);
                f.set(x, p.y + 4, [245, 245, 245]);
                for y in p.y - 3..=p.y + 3 {
                    f.set(
                        x,
                        y,
                        if x <= split {
                            [245, 245, 245]
                        } else {
                            [20, 20, 20]
                        },
                    );
                }
            }
        }
        f
    }

    fn fisch_layout() -> Layout {
        let cfg = crate::config::LayoutCfg {
            padding: 16,
            ..Default::default()
        };
        Layout::from_config(&cfg).unwrap()
    }

    #[test]
    fn auto_calibration_learns_fisch_colours() {
        let layout = fisch_layout();
        let n = layout.track_width() as u32;
        let frames: Vec<Frame> = (0..60)
            .map(|i| {
                let bar_left = 100 + i * 6;
                // The fish leaves the bar for the last third of the frames.
                let fish = if i < 40 {
                    bar_left + 150
                } else {
                    (bar_left + 500).min(n - 10)
                };
                fisch_frame(
                    &layout,
                    (bar_left, bar_left + 390),
                    fish,
                    0.2 + i as f32 * 0.005,
                    i,
                )
            })
            .collect();
        assert!(looks_like_minigame(&frames[0], &layout));
        let cfg = Config::default();
        let a = analyze_auto(&frames, &layout, &cfg.colors, cfg.detection.fish_max_width).unwrap();
        let has = |v: &[(Rgb, f32)], c: [u8; 3]| {
            v.iter()
                .any(|(x, _)| dist_sq(x.0, [c[0] as i32, c[1] as i32, c[2] as i32]) < 200)
        };
        assert!(has(&a.bar, [239, 239, 239]), "{:?}", a.bar);
        assert!(has(&a.bar, [138, 90, 58]), "{:?}", a.bar);
        assert!(has(&a.fish, [120, 120, 125]), "{:?}", a.fish);
        let p = layout.progress.unwrap();
        let row = a.progress_row.unwrap();
        assert!(row.abs_diff(p.y) <= 3, "progress row {row} vs {}", p.y);
        assert_eq!(a.progress.len(), 1);

        // The learned colours detect bar and fish in every frame.
        let mut colors = cfg.colors.clone();
        colors.bar = a.bar.iter().map(|(c, _)| *c).collect();
        colors.fish = a.fish.iter().map(|(c, _)| *c).collect();
        colors.progress = a.progress.clone();
        let mut det = Detector::with_layout(layout.clone(), &colors, &cfg.detection).unwrap();
        for (i, f) in frames.iter().enumerate() {
            let d = det.detect(f);
            let bar = d.bar.expect("bar");
            assert!(
                (bar.left - (100.0 + i as f32 * 6.0)).abs() <= 1.0,
                "frame {i}: {bar:?}"
            );
            assert_eq!(d.fish.len(), 1, "frame {i}");
        }
    }

    #[test]
    fn no_minigame_in_the_plain_scene() {
        let layout = fisch_layout();
        let mut f = fisch_frame(&layout, (100, 490), 200, 0.3, 1);
        // Wipe the UI: rock everywhere.
        let rock = fisch_frame(&layout, (0, 0), 2, 0.0, 3);
        for y in layout.track_y0 - 10..=layout.track_y1 + 10 {
            for x in 0..f.width {
                let n = (x.wrapping_mul(2654435761).wrapping_add(y * 40503)) >> 24;
                let v = if n % 3 == 0 { 200 } else { 60 } as u8;
                f.set(x, y, [v, v, v.saturating_add(30)]);
            }
        }
        assert!(!looks_like_minigame(&f, &layout));
        let dark = Frame::new(rock.width, rock.height);
        assert!(!looks_like_minigame(&dark, &layout));
    }

    #[test]
    fn a_briefly_brown_bar_is_still_learned() {
        let layout = fisch_layout();
        let n = layout.track_width() as u32;
        // The fish is outside the bar (brown) in only 4 of 120 frames.
        let frames: Vec<Frame> = (0..120)
            .map(|i| {
                let bar_left = 100 + (i % 60) * 6;
                let fish = if (50..54).contains(&i) {
                    (bar_left + 500).min(n - 10)
                } else {
                    bar_left + 150
                };
                fisch_frame(&layout, (bar_left, bar_left + 390), fish, 0.3, i)
            })
            .collect();
        let cfg = Config::default();
        let a = analyze_auto(&frames, &layout, &cfg.colors, cfg.detection.fish_max_width).unwrap();
        let has = |c: [u8; 3]| {
            a.bar
                .iter()
                .any(|(x, _)| dist_sq(x.0, [c[0] as i32, c[1] as i32, c[2] as i32]) < 200)
        };
        assert!(has([239, 239, 239]) && has([138, 90, 58]), "{:?}", a.bar);
    }

    #[test]
    fn bright_object_in_a_dark_scene_is_not_a_minigame() {
        let layout = fisch_layout();
        let mut f = Frame::new(layout.capture.w as u32, layout.capture.h as u32);
        for y in 0..f.height {
            for x in 0..f.width {
                f.set(x, y, [15, 20, 30]);
            }
        }
        for y in layout.track_y0..=layout.track_y1 {
            for x in layout.track_x0 + 300..layout.track_x0 + 450 {
                f.set(x, y, [200, 200, 200]);
            }
        }
        assert!(!looks_like_minigame(&f, &layout));
    }
}
