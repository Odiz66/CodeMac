# fishmacro

A reeling macro for fishing minigames on **Hyprland / Wayland**, written in Rust.

You cast the line yourself. When a fish bites you press the **toggle key**. The
macro reels by itself while it watches a small part of the screen, and stops on
its own when the minigame is gone. The **kill-switch key** (or Ctrl+C) stops
everything at any time, and every held button is released on the way out.

Three reeling styles are supported (`mode` in the config):

| mode    | what it does                                                                 |
|---------|------------------------------------------------------------------------------|
| `hold`  | holds the button while the minigame is on screen                             |
| `spam`  | clicks the button rapidly (configurable clicks/s) while the minigame is on screen |
| `track` | holds/releases to keep your zone on a moving marker (fish) inside a bar     |

## How it works

- **Input**: a virtual mouse/keyboard created through the kernel's `/dev/uinput`
  (the [`evdev`](https://crates.io/crates/evdev) crate). It works below the
  compositor, so no X11 tools are involved. uinput sends *keycodes*: `KEY_E`
  means the key in the "E" position of a US layout. The macro waits 300 ms after
  creating the device before sending anything, so Hyprland has picked it up.
- **Screen**: each frame is `grim -t ppm -g "x,y wxh" -`. grim uses the
  wlr-screencopy protocol, which Hyprland supports, and copies only the
  configured region. That gives roughly 30–60 captures per second, and capture
  runs on its own thread.
- **Catch done**: the catch counts as done when the minigame's color has been
  gone from the region for `end_grace_ms`. It also counts as done right away if
  an optional "done" color shows up (a "Caught!" banner, for example).
- **Hotkeys**: Hyprland binds that run `pkill -USR1 -x fishmacro` (toggle) and
  `pkill -USR2 -x fishmacro` (kill switch). The macro never reads your keyboard,
  so it needs no `input` group. Hyprland consumes the bound keys, so the game
  never sees them.
- **Cursor** (optional, off by default): if your game needs the pointer at an
  exact spot, the macro warps it with Hyprland IPC instead of relative uinput
  motion, which goes through pointer acceleration. With a Lua config (0.55+) it
  runs `hyprctl dispatch 'hl.dsp.cursor.move({ x = 1280, y = 720 })'`, per the
  0.56 wiki's Dispatchers page (`hl.dsp.cursor.move({ x, y })`). With a legacy
  `hyprland.conf` it runs `hyprctl dispatch movecursor 1280 720`.
- **Safety**: buttons are only pressed while the minigame is visible, so the
  macro never clicks after the catch, when a click would cast again in many
  games.

Dependencies: `evdev` and `libc`. The config parser and the PNG writer used for
calibration screenshots are built in.

## 1. Install tools and build

All commands are for the **fish** shell.

```fish
sudo pacman -S --needed rust grim slurp
cd ~/path/to/CodeMac
cargo build --release
```

The binary is `target/release/fishmacro`. To have `fishmacro` on your PATH:

```fish
cargo install --path .
fish_add_path ~/.cargo/bin
```

**RustRover:** *File → Open* the project folder; it is a normal Cargo project.
To run from the IDE, create *Cargo* run configurations with commands such as
`run --release -- calibrate`, `run --release -- watch`,
`run --release -- run --dry-run` and `run --release`. The process is still
called `fishmacro`, so the hotkeys work while it runs from the IDE.

## 2. Allow uinput without root (one-time)

```fish
# Load the uinput module now and at every boot
sudo modprobe uinput
sudo install -Dm644 setup/uinput.conf /etc/modules-load.d/uinput.conf

# A `uinput` group that may open /dev/uinput, with you in it
getent group uinput >/dev/null; or sudo groupadd --system uinput
sudo usermod -aG uinput $USER

# udev rule: /dev/uinput owned by group uinput, mode 0660
sudo install -Dm644 setup/60-fishmacro-uinput.rules /etc/udev/rules.d/60-fishmacro-uinput.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --sysname-match=uinput
```

**Log out and back in (or reboot)** so the new group applies, then check:

```fish
ls -l /dev/uinput                                   # crw-rw---- 1 root uinput ...
contains uinput (id -nG | string split ' '); and echo "ok: in uinput group"
fishmacro init                                      # writes ~/.config/fishmacro/config.toml
fishmacro test-input                                # clicks once after 3 s; click somewhere harmless
```

Being in the `uinput` group lets any program you run inject input. That's the
same trust level as tools like ydotool or kanata.

## 3. Hotkeys in Hyprland

Pick two keys the game doesn't use. F8 and F9 are used here.

**Lua config** (`~/.config/hypr/hyprland.lua`, Hyprland 0.55+):

```lua
-- fishmacro: F8 = start/stop reeling, F9 = kill switch.
-- dont_inhibit keeps them working if the game inhibits compositor shortcuts.
hl.bind("F8", hl.dsp.exec_cmd("pkill -USR1 -x fishmacro"), { dont_inhibit = true })
hl.bind("F9", hl.dsp.exec_cmd("pkill -USR2 -x fishmacro"), { dont_inhibit = true })
```

**Legacy config** (`~/.config/hypr/hyprland.conf`); the `p` flag is the same
as `dont_inhibit`:

```ini
bindp = , F8, exec, pkill -USR1 -x fishmacro
bindp = , F9, exec, pkill -USR2 -x fishmacro
```

Reload with `hyprctl reload`, and check the binds are registered with
`hyprctl binds`. If you use the legacy config and enable the cursor feature,
also set `config_format = "legacy"` under `[hyprland]` in the fishmacro config.

## 4. Configure and calibrate

Edit `~/.config/fishmacro/config.toml`. Every setting is explained in it; see
[`config.example.toml`](config.example.toml).

1. **Mode and button.** Set `mode` (`hold` / `spam` / `track`) and `button`
   (`BTN_LEFT`, `BTN_RIGHT`, `KEY_SPACE`, `KEY_E`, ...).
2. **Region.** Get the reeling minigame on screen and drag a tight box around
   the bar with `slurp`. It prints `x,y wxh`; copy the four numbers into
   `[region]`. The minigame can vanish before you get to a terminal, so a
   temporary bind is easiest: press it while the minigame is up, then
   `cat /tmp/fishmacro-region.txt`.
   ```lua
   hl.bind("F10", hl.dsp.exec_cmd("slurp > /tmp/fishmacro-region.txt"))
   ```
   With a legacy config: `bind = , F10, exec, slurp > /tmp/fishmacro-region.txt`.
   On a 2560x1440 monitor at scale 1, layout pixels are screen pixels. Keep the
   region small: just the bar.
3. **Colors.** Run `fishmacro calibrate`. You have 3 seconds (`--delay N` to
   change) to bring up the minigame. It then:
   - saves `fishmacro-region.png` (the region, zoomed) and
     `fishmacro-context.png` (the region outlined in magenta with its
     surroundings) in the current directory. Open them to check the box is in
     the right place.
   - prints the most common colors in the region and what the detector finds
     with your current settings.

   Copy colors from that list into the config:
   - `detect.active_color`: something in the region **only while the minigame
     is up** (bar frame or background). Required for `hold`/`spam`. In `track`
     mode you can leave it `""`, and then the marker/zone colors are used.
   - `track.marker_color`: the fish or marker you follow. `track.zone_color`:
     your controllable bar.
   - `detect.done_color` (optional): a color that appears when the catch
     succeeds.

   Run `calibrate` **with the minigame open** (it must say `Minigame visible:
   YES`) **and with it closed** (it must say `no`). If it still says YES with
   the minigame closed, the active color is too generic or the tolerance too
   high, and the macro would never notice the catch ending.
4. **Live check.** `fishmacro watch` prints the detector's view about 4 times
   a second without sending input: fps, visible/done, marker and zone
   positions, and in track mode whether it would HOLD or release.
5. **Dry run.** `fishmacro run --dry-run` runs the real loop with your hotkeys
   but only prints the presses it would send.

`track` mode assumes holding moves your zone **right** (`axis = "horizontal"`)
or **up** (`axis = "vertical"`). Set `invert = true` if holding moves it the
other way. Tuning: if the zone jitters around the marker, raise `deadband`. If
it lags behind a fast marker, raise `lookahead_ms`. If it overshoots, lower
`lookahead_ms`.

## 5. Use

```fish
fishmacro            # same as `fishmacro run`
```

Cast, wait for the bite, press **F8**. The macro reels and prints how the
session ended:

- `catch done`: the minigame closed, or the done color appeared.
- `minigame never appeared`: nothing was seen within `start_timeout_ms` (3 s).
- `stopped by toggle key`: you pressed F8 again.
- `gave up after max_reel_ms`: hit the hard time limit.

It then waits for the next F8. **F9** (kill switch) or Ctrl+C releases
everything and exits.

## Releasing buttons on exit

The button state is owned by one object whose `Drop` sends a release for every
key and button the virtual device has. It then waits 50 ms so Hyprland reads
the release before the device is destroyed. That runs when:

- a session ends for any reason (catch done, toggle, timeout, error);
- the kill switch, Ctrl+C, `SIGTERM` or `SIGHUP` arrive (signal handlers only
  set flags; the main loop then unwinds normally);
- any error is returned, or a panic unwinds (`panic = "unwind"` is kept in the
  release profile).

`SIGKILL` (`kill -9`) cannot be caught by any program. The kernel removes the
virtual device when the process dies. If a button ever looks stuck after that,
click it once on your real mouse.

## Troubleshooting

- **`no permission to open /dev/uinput`**: do step 2 and log in again. Check
  with `ls -l /dev/uinput` and `id -nG`.
- **`/dev/uinput does not exist`**: run `sudo modprobe uinput`, and check
  `/etc/modules-load.d/uinput.conf`.
- **F8 does nothing**: check the macro is running (`pgrep -x fishmacro`) and the
  bind exists (`hyprctl binds`). Test without the key: `pkill -USR1 -x fishmacro`.
- **The game ignores the clicks**: run `fishmacro test-input` over the game
  window. Some anti-cheat systems ignore virtual devices; nothing in this
  program can change that.
- **`minigame never appeared`**: the region or `active_color` is wrong. Check
  with `fishmacro calibrate` / `watch` while the minigame is up.
- **The macro never stops**: `active_color` also matches when the minigame is
  closed. Lower `active_tolerance` or pick a more specific color.
- **Low fps in `watch`**: make the region smaller (a thin strip through the bar
  is enough for `track`).
- **Cursor move fails**: `hyprctl` must run inside the Hyprland session
  (`HYPRLAND_INSTANCE_SIGNATURE` set), and `[hyprland] config_format` must match
  your config.

Automating input may be against a game's rules. Check before using this online.
