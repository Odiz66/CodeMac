# fisch-macro

AFK fishing macro for the Roblox game **Fisch**, written in Rust for
**Sober on Hyprland (Wayland)**. Built for the **Ruinous Oath**: colours come
from calibration, detection tolerates the rod's effects, and the reel
controller aims for perfect catches (Mastered mutation).

Loop: focus Sober → cast (hold LMB, released at the top of the power bar) →
UI navigation `\` + Enter until the reel minigame appears → PD controller
with prediction keeps the fish in your bar → wait until the minigame
disappears → log the catch → recast.

| Piece | How |
|---|---|
| Input | uinput virtual mouse + keyboard (`evdev` crate), no root |
| Cursor | `hyprctl dispatch movecursor` / `hl.dsp.cursor.move` (Lua config) |
| Capture | built-in wlr-screencopy client (persistent connection, bar region only); `grim` as fallback |
| On/off | `SIGUSR1` from a Hyprland bind on F6 |

## 1. Setup

Check what is installed:

```fish
fish scripts/check-deps.fish
```

Install what it reports as missing:

```fish
sudo pacman -S --needed rustup grim flatpak
rustup default stable
flatpak install flathub org.vinegarhq.Sober   # if Sober is not installed yet
```

Allow your user to create virtual input devices (udev rule + `input` group):

```fish
fish scripts/setup-uinput.fish
```

Then **log out and back in** (group changes need a new session). Note that
the `input` group can also read your real keyboard and mouse.

Check the window class while Sober is running (should be `org.vinegarhq.Sober`;
otherwise put the real one in `config.toml` → `[window] class`):

```fish
hyprctl clients | grep class
```

### Hyprland binds

Hyprland 0.55+ prefers `~/.config/hypr/hyprland.lua` over `hyprland.conf`
(`check-deps.fish` tells you which one you use).

`hyprland.conf`:

```ini
bind = , F6, exec, pkill -USR1 -x fisch-macro
# Emergency stop: graceful first (releases the mouse), hard kill after 1 s
bind = SHIFT, F6, exec, pkill -TERM -x fisch-macro; sleep 1; pkill -KILL -x fisch-macro
```

`hyprland.lua` (if it only contains `require("config.…")` lines, as on CachyOS,
the binds belong in `~/.config/hypr/config/binds.lua`). These commands add
them:

```fish
echo 'hl.bind("F6", hl.dsp.exec_cmd("pkill -USR1 -x fisch-macro"))' >> ~/.config/hypr/config/binds.lua
echo 'hl.bind("SHIFT + F6", hl.dsp.exec_cmd("pkill -TERM -x fisch-macro; sleep 1; pkill -KILL -x fisch-macro"))' >> ~/.config/hypr/config/binds.lua
```

Reload with `hyprctl reload`. Even a hard kill cannot leave the button held:
the kernel removes the virtual mouse with the process, and its pressed button
goes with it.

### Monitor refresh rate

Every capture waits for the next screen refresh, so a faster monitor means
fresher frames and better tracking. Your MSI G272QPF offers up to 165 Hz
(`2560x1440@164.99`); in the simulation a hard fish goes from ~98% to ~100%
fish-in-bar when moving from 60 Hz to 170 Hz. Check the offered modes, set the
highest one and verify:

```fish
hyprctl monitors all | grep -i availablemodes
echo 'hl.monitor({ output = "DP-3", mode = "2560x1440@165", position = "0x0", scale = 1 })' >> ~/.config/hypr/config/monitors.lua
hyprctl reload
hyprctl monitors | grep @
```

The prediction scales with the measured capture rate by itself
(`lookahead_frames`), so no config change is needed when you switch.

### In-game settings

- **FPS cap 240** (what you use) is good: the game then reacts to a click
  within ~4 ms instead of ~17 ms at 60 FPS.
- **Higher Brightness** and **Higher Saturation**: turn both **off**. They
  push the minigame colours together. If you change any of these later, run
  calibration again.
- Roblox **Graphics quality**: set it to a fixed manual level. Automatic
  quality switches effects on and off while you play.
- **Bait**: calibrate with the bait you will actually use. If a bait tints or
  animates the minigame and detection struggles, use a plainer one.
- **UI Navigation Toggle** is `\` and must be **off** when you start the
  macro. The macro turns it on for the shake and off again afterwards. With the
  Polish (programmer's) layout `\` is on the same key as on a US keyboard, so
  the default `KEY_BACKSLASH` is right.
- The rod is expected in **hotbar slot 1** (`keys.reequip = "KEY_1"`): after a
  timeout it is unequipped and re-equipped to cancel a stuck cast.
- Keep Sober fullscreen at the same resolution. If you change the resolution,
  UI scale or window mode, the bar moves and you need to recalibrate.
- Keep screen shaders, hyprsunset/night light and HDR the same as during
  calibration (or off).

## 2. Calibration

```fish
cargo run --release -- calibrate
```

Each step counts down (5 s, `--countdown N`). **F6 reads the position
immediately**, which is easier than racing a countdown. You can play the
minigame with the mouse button while hovering, because moving the mouse does
not affect it.

1. Hover the **top-left corner** of the reel track (the long bar your bar slides in).
2. Hover its **bottom-right corner**.
3. Hover the **left end** of the progress bar under it.
4. Hover its **right end**.
5. Colours: start a Ruinous Oath minigame, **don't click** (your bar rests at
   the left end), hover **your bar** and wait. About 1.5 s of frames is
   recorded. The bar colour is taken under the cursor, the background is the
   dominant track colour, and the fish is the most common narrow full-height
   object that is neither. The progress colour comes from the left end of the
   progress bar.
6. Power bar (perfect cast): while charging a cast, hover the **empty end** of
   the power bar, then its **full end**.
7. Let go; when the countdown ends, **press and hold** to charge a cast for
   3 s. The fill colour is learned from that recording, and the fill over time
   is printed so you can see whether it reached 100%.

Everything is written to `config.toml`, and the frames go to `calibration/`.
`calibration/frame_000_ann.png` shows what was detected: green lines at the
bar edges, red at the fish, yellow track outline.

Redo a part with `--only region` / `--only colors` / `--only cast`, skip
progress with `--no-progress` or the power bar with `--no-cast`. Without a
calibrated power bar the macro falls back to holding for `timing.cast_hold_ms`.

**Manual colour picking** (e.g. the fish was missed): open a saved PNG in an
image viewer, note pixel coordinates, then run:

```fish
cargo run --release -- calibrate --png calibration/frame_012.png --fish 512,20 --fish 530,22 --append
```

`--bar X,Y`, `--progress X,Y` and `--cast-fill X,Y` (on a
`calibration/cast_*.png`) work the same way. Without `--append` the list is
replaced.

## 3. Check detection

```fish
cargo run --release -- debug            # live view, never touches the mouse
cargo run --release -- debug --bench    # screencopy vs grim speed
cargo run --release -- debug --full     # full screenshot with regions drawn on it
cargo run --release -- debug --cast     # power bar fill while you charge casts by hand
```

`debug --cast` prints the fill every frame and marks `<-- RELEASE` where the
macro would let go. Charge a few casts by hand: if the game shows a perfect
cast when you release at that moment, the timing is right. Otherwise adjust
`cast.release_at` (where to release) or `cast.lead_ms` (how early, to cover
latency).

`debug` prints a line per frame (interval, FPS, bar, fish, inside/outside +
margin, progress) and saves raw + annotated PNGs to `debug/` every 30 frames.
Play a minigame by hand: when it ends, debug says whether the macro would have
counted it as **PERFECT**. Aim for bar and fish detected in ~100% of minigame
frames before running the macro.

## 4. Running

**RustRover**: open the folder (Cargo.toml is at the root), pick a run
configuration from the toolbar and press Run:

- *fisch run*: the macro (waits for F6)
- *fisch run (verbose)*: also prints every reel frame
- *fisch run (dry run)*: the whole loop without sending any input
- *fisch calibrate*, *fisch debug*, *fisch bench capture*, *fisch tests*

Stopping from RustRover is safe. A graceful stop releases the mouse, and even
a forced kill can't leave the button held (see the binds section above).
RustRover must be started from your Hyprland session so it inherits
`WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE`.

**Terminal**:

```fish
cargo run --release -- run            # press F6 in game to start/stop
cargo run --release -- run --start    # start right away
cargo run --release -- run --verbose  # per-frame reel log
```

To stop: Ctrl+C or Shift+F6. A second Ctrl+C exits immediately. On any
stop, signal or panic the mouse button and keys are released, and UI
navigation is turned back off if the game is focused.

Every catch is logged:

```
[00:12:30] Cast #15 (power bar 97% after 1130 ms)
[00:12:31] Catch #14 PERFECT (7.9 s, fish in bar 100.0% of 1340 frames @ 170 fps, min margin +3.5 px, progress 100%) | perfect 12/14 (86%)
[00:13:02] Catch #15 not perfect: fish outside the bar in 4 frames (longest 3 in a row, worst 6.0 px) (...)
```

and appended to `catches.csv` for tuning.

## 5. Tuning for perfect catches

"Perfect" here means the fish centre never left your bar and the progress bar
never went down. The last ~0.1 s are ignored because that is the catch
animation.

Captures only arrive once per screen refresh (every 16.7 ms at 60 Hz, 5.9 ms
at 170 Hz), each one is already a little old, and a click takes a few more
milliseconds to act. The controller therefore **predicts** the fish (a
quadratic fit over the last frames, so speed + acceleration) and the bar
(speed) `lookahead_ms + lookahead_frames × frame interval` into the future.
It drives a PD law on that error, in track widths:

```
duty = neutral_duty + kp * error + kd * (fish speed - bar speed)
```

The duty cycle becomes hold/release frames through sigma-delta modulation, so
a duty of 0.6 means "hold 3 of every 5 frames". This keeps the tiny Ruinous
Oath bar steady instead of swinging it end to end. When the fish gets within
`edge_guard` of a bar edge and keeps moving outward, the controller pushes at
full strength.

Change **one** value in `[controller]` at a time, run 10+ catches, and compare
the perfect rate and `min_margin_px` in `catches.csv`. Use `run -v` to see
where it goes wrong.

| Symptom | Change |
|---|---|
| Bar trails behind the fish and loses it on the trailing side | `lookahead_frames` +0.25 (or `lookahead_ms` +5), or `kp` +3 |
| Bar overshoots and the fish leaves on alternating sides | `kd` +0.5, or `kp` -3 |
| Bar drifts while the fish is still (stun) | `neutral_duty`: raise it if the bar drifts left, lower it if it drifts right |
| Fish escapes on sudden direction changes | `edge_guard` 0.3 → 0.45, `fish_accel_weight` → 0.7, `fish_history` 5 → 4 |
| Jittery hold pattern from noisy detection | `fish_history` 5 → 6, check colours in `debug` |
| Capture slower than the refresh rate (`debug --bench`) | use the faster backend, raise `lookahead_ms` |

`mode = "bang_bang"` (hold whenever `kp*error + kd*speed > 0`) is there to
compare against. The defaults (`kp 20, kd 3, lookahead 1.75 frames`) were
chosen in a simulation of a 240 FPS game on 60 Hz and 170 Hz screens (`cargo
test sim`). Fisch's real physics may differ, so tune on the real game.

## 6. Troubleshooting

| Problem | Fix |
|---|---|
| `no permission to open /dev/uinput` | `fish scripts/setup-uinput.fish`, then log out/in. `id -nG` must list `input`. |
| Clicks or keys do nothing in game | Sober must be focused. Raise `timing.input_settle_ms`. `hyprctl devices` should list "fisch-macro virtual mouse/keyboard". |
| `\` does not toggle UI navigation | On a non-US layout set `keys.nav_toggle` to the key at the US `\` position (`KEY_BACKSLASH` or `KEY_102ND`). |
| `hyprctl dispatch failed` | Set `window.dispatch_style` to `"lua"` (hyprland.lua) or `"legacy"` (hyprland.conf). |
| Captures are black or frozen | Set Hyprland `render:direct_scanout` to 0. If `ecosystem:enforce_permissions` is on, allow screencopy for `target/release/fisch-macro` (and grim). Try `capture.backend = "grim"`. |
| Bar/fish not detected | Run `debug` during a minigame and look at `debug/*_ann.png`. Recalibrate, raise `colors.*_tolerance` by 10, or add colours with `calibrate --png ... --append`. |
| Slash effects cause false fish | Raise `detection.column_min_fraction` (0.4 → 0.6). |
| Fish and bar get mixed up | Their colours are too close: lower `bar_tolerance` / `fish_tolerance`. |
| Minigame "ends" too early or too late | `detection.end_ms` (no bar + no fish) and `detection.fish_lost_end_ms`. |
| Enter presses the wrong button (rod unequipped, menus) | Raise `timing.after_cast_ms` so navigation starts after the shake button exists. `keys.reequip` (rod slot, default `KEY_1`) recovers after timeouts. |
| Only the first shake button gets pressed | Set `keys.nav_refresh_ms = 1500`: UI navigation is switched off and on every 1.5 s so the newest button is selected. |
| Cast released too early / too late | Check with `debug --cast`, then adjust `cast.release_at` / `cast.lead_ms`. |
| "power bar not seen" in the cast log | Recalibrate with `calibrate --only cast`; `debug --full` shows where it looks (orange line). |
| Timed casts (no power bar) too weak or too strong | `timing.cast_hold_ms`. |
| Recasts while the catch popup is still shown | Raise `timing.after_catch_ms`. |
| Wrong screen coordinates | `debug --full` draws the configured regions on a screenshot. |

## Project layout

```
src/main.rs           CLI (run / calibrate / debug)
src/run.rs            fishing loop: focus, cast, shake, reel, recover, stats
src/reel.rs           per-minigame logic: tracking, gating, end detection, perfect stats
src/cast.rs           power bar reading + release timing for perfect casts
src/controller.rs     PD + sigma-delta hold/release
src/tracker.rs        least-squares motion estimation
src/detect.rs         bar/fish/progress detection
src/capture/          wlr-screencopy client + grim fallback
src/input.rs          uinput mouse/keyboard, release on drop/panic/signal
src/hypr.rs           hyprctl (legacy + Lua dispatch)
src/calibrate.rs      calibration wizard + colour clustering
src/debug.rs          debug view, PNGs, benchmark
src/sim.rs            controller simulation tests
scripts/              dependency check + uinput setup (fish)
.run/                 RustRover run configurations
```

Use at your own risk: automation may be against Roblox's or the game's rules.
