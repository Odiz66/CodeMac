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
use crate::cast::CastBar;
use crate::config::{ColorsCfg, Config, Rgb};
use crate::detect::{Detector, Layout, Palette, dist_sq, runs};
use crate::hypr::Hypr;
use crate::img::{load_png, save_png};
use crate::signals::{self, Flags};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Part {
    Region,
    Colors,
    Cast,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Seconds before each cursor position is read (F6 reads it immediately).
    #[arg(long, default_value_t = 5)]
    countdown: u64,
    /// Only redo one part.
    #[arg(long, value_enum)]
    only: Option<Part>,
    /// Do not set up progress-bar reading.
    #[arg(long)]
    no_progress: bool,
    /// Skip the power bar (perfect cast) step.
    #[arg(long)]
    no_cast: bool,
    /// Frames captured for the colour analysis.
    #[arg(long, default_value_t = 90)]
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
    /// With --png: X,Y of a pixel on the filled cast power bar (repeatable).
    #[arg(long, value_name = "X,Y", value_parser = parse_point)]
    cast_fill: Vec<(u32, u32)>,
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
    println!(
        "Calibration ({} capture). Ctrl+C cancels; F6 skips a countdown.",
        cap.name()
    );
    if args.only != Some(Part::Cast) {
        println!("Have Fisch open with the Ruinous Oath equipped. Cast manually and get the reel");
        println!("minigame on screen for each step (you can play it with the mouse button while");
        println!("hovering - moving the mouse does not affect the minigame).");
    }
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
    if (all && !args.no_cast) || args.only == Some(Part::Cast) {
        calibrate_cast(&ctx, cap.as_mut(), &mut cfg)?;
        cfg.save(path)?;
        println!("\nSaved the power bar to {}.", path.display());
    }
    println!(
        "\nNext: `cargo run --release -- debug` while a minigame is on screen to check detection,"
    );
    println!("then `cargo run --release -- run`.");
    Ok(())
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
            "the cursor ({cx},{cy}) is not on the reel track; redo positions with `calibrate --only region`"
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

fn calibrate_cast(ctx: &Ctx, cap: &mut dyn Capturer, cfg: &mut Config) -> Result<()> {
    println!("\n== Power bar (perfect cast) ==");
    println!("For the next two steps hold the mouse button to charge a cast while you hover");
    println!("(release and charge again whenever you need to).");
    let empty = ctx.point("Hover the EMPTY end of the power bar - where the fill starts.")?;
    let full =
        ctx.point("Hover the FULL end of the power bar - where the fill ends when it is full.")?;
    let len = (full.0 - empty.0).abs().max((full.1 - empty.1).abs());
    if len < 10 {
        bail!("the two ends are only {len} px apart; hover both ends of the power bar");
    }
    let probe = CastBar::from_points(empty, full, Palette::new(&[], 0.0));
    ctx.countdown(
        "Let go of the mouse button. When the countdown ends, press and HOLD it to charge a \
         cast and keep holding for 3 seconds - I record the bar while it fills.",
        "recording starts",
    )?;
    let start = Instant::now();
    let mut frames = Vec::new();
    while start.elapsed() < Duration::from_millis(3000) {
        frames.push(cap.grab(probe.rect)?);
    }
    println!(
        "\r  captured {} frames of the power bar                    ",
        frames.len()
    );

    // By the second half of the recording the empty end has been filled for a
    // while, so its dominant colours are the fill.
    let px: Vec<[u8; 3]> = frames[frames.len() / 2..]
        .iter()
        .flat_map(|f| probe.empty_end_pixels(f))
        .collect();
    let fill: Vec<Rgb> = clusters(&px, 18.0, 4)
        .into_iter()
        .filter(|(_, s)| *s >= 0.2)
        .take(3)
        .map(|(c, _)| Rgb(c))
        .collect();
    if fill.is_empty() {
        bail!("could not find a fill colour at the empty end of the bar");
    }
    let bar = CastBar::from_points(empty, full, Palette::new(&fill, cfg.cast.tolerance));
    let fills: Vec<f32> = frames.iter().map(|f| bar.fill(f)).collect();
    let peak = fills.iter().cloned().fold(0.0, f32::max);
    let first_full = fills.iter().position(|&f| f >= 0.95);
    println!("  fill colour(s): {}", list(&fill));
    let step = (fills.len() / 15).max(1);
    let timeline: Vec<String> = fills
        .iter()
        .step_by(step)
        .map(|f| format!("{:.0}%", f * 100.0))
        .collect();
    println!("  fill over time: {}", timeline.join(" "));
    match first_full {
        Some(i) => println!("  full after {:.2} s", i as f64 * 3.0 / fills.len() as f64),
        None => {
            println!(
                "  WARNING: the bar never looked more than {:.0}% full.",
                peak * 100.0
            );
            println!("  If the top is drawn in another colour, add it by hand:");
            println!("  calibrate --png calibration/cast_NNN.png --cast-fill X,Y --append");
        }
    }
    if let Some(i) = first_full
        && fills[i..].iter().any(|&f| f < 0.7)
    {
        println!("  The bar goes back down after filling; the macro releases on the way up.");
    }

    let dir = Path::new("calibration");
    for (i, f) in frames.iter().enumerate().step_by(4) {
        save_png(&dir.join(format!("cast_{i:03}.png")), f)?;
    }
    println!("  frames saved to {}/cast_*.png", dir.display());

    let c = &mut cfg.cast;
    (c.empty_x, c.empty_y) = empty;
    (c.full_x, c.full_y) = full;
    c.fill = fill;
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
    if args.bar.is_empty()
        && args.fish.is_empty()
        && args.progress.is_empty()
        && args.cast_fill.is_empty()
    {
        bail!(
            "--png needs at least one of --bar X,Y / --fish X,Y / --progress X,Y / --cast-fill X,Y"
        );
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
    apply(&mut cfg.cast.fill, &args.cast_fill, "cast fill")?;
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
}
