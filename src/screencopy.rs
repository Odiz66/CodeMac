//! Native region capture with wlr-screencopy over one persistent Wayland
//! connection.
//!
//! Spawning grim per frame costs a process start plus a fresh connection and
//! roundtrips every time (~20 ms). Keeping the connection and a shared-memory
//! buffer around, each capture is just: `capture_output_region` -> `copy` ->
//! `ready`, which can keep up with the monitor refresh (e.g. 165 Hz).
//!
//! Keep one `Screencopy` alive for as long as possible: Hyprland 0.56 creates
//! a capture session per client and region and only frees it when the monitor
//! goes away, so a new connection per capture would pile sessions up.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::capture::Frame;
use crate::config::Region;
use crate::wayland::{Connection, DISPLAY_ID, Event, Message};

/// How long one capture may take before giving up.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
/// Rate limit when the whole monitor has to be copied (scaled monitors).
const FULL_OUTPUT_INTERVAL: Duration = Duration::from_millis(33);
const SETUP_TIMEOUT: Duration = Duration::from_secs(2);

// Opcodes (requests and events are numbered separately, in XML order).
const WL_DISPLAY_SYNC: u16 = 0;
const WL_DISPLAY_GET_REGISTRY: u16 = 1;
const WL_DISPLAY_EV_ERROR: u16 = 0;
const WL_DISPLAY_EV_DELETE_ID: u16 = 1;
const WL_REGISTRY_BIND: u16 = 0;
const WL_REGISTRY_EV_GLOBAL: u16 = 0;
const WL_CALLBACK_EV_DONE: u16 = 0;
const WL_SHM_CREATE_POOL: u16 = 0;
const WL_SHM_POOL_CREATE_BUFFER: u16 = 0;
const WL_SHM_POOL_DESTROY: u16 = 1;
const WL_BUFFER_DESTROY: u16 = 0;
const WL_OUTPUT_EV_GEOMETRY: u16 = 0;
const WL_OUTPUT_EV_MODE: u16 = 1;
const WL_OUTPUT_EV_SCALE: u16 = 3;
const WL_OUTPUT_EV_NAME: u16 = 4;
const XDG_OUTPUT_MANAGER_GET_XDG_OUTPUT: u16 = 1;
const XDG_OUTPUT_EV_LOGICAL_POSITION: u16 = 0;
const XDG_OUTPUT_EV_LOGICAL_SIZE: u16 = 1;
const WL_SHM_EV_FORMAT: u16 = 0;
const SCREENCOPY_MANAGER_CAPTURE_OUTPUT: u16 = 0;
const SCREENCOPY_MANAGER_CAPTURE_OUTPUT_REGION: u16 = 1;
const FRAME_COPY: u16 = 0;
const FRAME_DESTROY: u16 = 1;
const FRAME_EV_BUFFER: u16 = 0;
const FRAME_EV_FLAGS: u16 = 1;
const FRAME_EV_READY: u16 = 2;
const FRAME_EV_FAILED: u16 = 3;
const FRAME_EV_BUFFER_DONE: u16 = 6;
const FRAME_FLAG_Y_INVERT: u32 = 1;
const WL_OUTPUT_MODE_CURRENT: u32 = 1;

// wl_shm formats: 0 and 1 are special, the rest are DRM fourcc codes.
const SHM_ARGB8888: u32 = 0;
const SHM_XRGB8888: u32 = 1;
const fn fourcc(s: &[u8; 4]) -> u32 {
    s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}
const SHM_ABGR8888: u32 = fourcc(b"AB24");
const SHM_XBGR8888: u32 = fourcc(b"XB24");
const SHM_ARGB2101010: u32 = fourcc(b"AR30");
const SHM_XRGB2101010: u32 = fourcc(b"XR30");
const SHM_ABGR2101010: u32 = fourcc(b"AB30");
const SHM_XBGR2101010: u32 = fourcc(b"XB30");
const SHM_RGB888: u32 = fourcc(b"RG24");
const SHM_BGR888: u32 = fourcc(b"BG24");

#[derive(Debug, Default)]
struct Output {
    id: u32,
    name: String,
    /// From zxdg_output_v1 (preferred): layout position and logical size.
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    /// From wl_output, used when xdg-output is unavailable.
    position: (i32, i32),
    mode: (i32, i32),
    scale: i32,
    transform: i32,
}

impl Output {
    /// (x, y, width, height) in layout coordinates.
    fn logical_rect(&self) -> (i32, i32, i32, i32) {
        let (x, y) = self.logical_position.unwrap_or(self.position);
        let (w, h) = self.logical_size.unwrap_or_else(|| {
            let scale = self.scale.max(1);
            let (w, h) = (self.mode.0 / scale, self.mode.1 / scale);
            // Odd transforms are rotated by 90/270 degrees.
            if self.transform % 2 == 1 { (h, w) } else { (w, h) }
        });
        (x, y, w, h)
    }

    /// Physical pixels per logical pixel (1.0 when unscaled).
    fn pixel_scale(&self) -> f64 {
        match self.logical_size {
            Some((lw, _)) if lw > 0 && self.mode.0 > 0 => self.mode.0 as f64 / lw as f64,
            _ => self.scale.max(1) as f64,
        }
    }
}

/// A rectangle in buffer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// What to ask the compositor for, and which part of the result to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Plan {
    output: u32,
    /// Output-local logical region, or `None` for the whole output.
    region: Option<(i32, i32, i32, i32)>,
    /// Part of the returned buffer to keep (whole-output captures only).
    crop: Option<Rect>,
}

/// Buffer parameters the compositor asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BufferSpec {
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

/// A wl_buffer backed by a memfd mapping we read the pixels from.
struct ShmBuffer {
    id: u32,
    spec: BufferSpec,
    ptr: *mut u8,
    len: usize,
}

impl ShmBuffer {
    fn pixels(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` bytes for the lifetime of self; the
        // compositor only writes to it between `copy` and `ready`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

// SAFETY: the mapping is owned exclusively by this struct (no aliases), so
// moving it to another thread is fine; it is never shared between threads.
unsafe impl Send for ShmBuffer {}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        // SAFETY: unmapping the region mapped in `create_buffer`.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

pub struct Screencopy {
    conn: Connection,
    shm: u32,
    /// Formats wl_shm accepts; creating a buffer in any other is a fatal error.
    shm_formats: Vec<u32>,
    manager: u32,
    manager_version: u32,
    outputs: Vec<Output>,
    xdg_outputs: Vec<(u32, usize)>,
    buffer: Option<ShmBuffer>,
    /// The last capture had to copy the whole monitor (scaled monitor).
    full_output: bool,
    warned_full_output: bool,
}

impl Screencopy {
    pub fn connect() -> Result<Screencopy, String> {
        Self::connect_inner().map_err(|e| format!("screencopy: {e}"))
    }

    fn connect_inner() -> io::Result<Screencopy> {
        let mut conn = Connection::connect()?;
        let registry = conn.new_id()?;
        conn.send(&Message::new(DISPLAY_ID, WL_DISPLAY_GET_REGISTRY).object(registry), None)?;

        // Collect the globals.
        let mut globals = Vec::new();
        let done = Instant::now() + SETUP_TIMEOUT;
        let callback = conn.new_id()?;
        conn.send(&Message::new(DISPLAY_ID, WL_DISPLAY_SYNC).object(callback), None)?;
        loop {
            let ev = conn.next_event(done)?;
            if ev.object == callback && ev.opcode == WL_CALLBACK_EV_DONE {
                break;
            }
            if ev.object == registry && ev.opcode == WL_REGISTRY_EV_GLOBAL {
                let mut r = ev.reader();
                globals.push((r.uint()?, r.string()?, r.uint()?));
            } else {
                handle_display_event(&mut conn, &ev)?;
            }
        }

        let find = |name: &str| globals.iter().find(|g| g.1 == name).map(|g| (g.0, g.2));
        let (shm_name, _) = find("wl_shm").ok_or_else(|| io::Error::other("compositor has no wl_shm"))?;
        let (manager_name, manager_advertised) = find("zwlr_screencopy_manager_v1")
            .ok_or_else(|| io::Error::other("compositor does not support wlr-screencopy"))?;

        let shm = bind(&mut conn, registry, shm_name, "wl_shm", 1)?;
        let manager_version = manager_advertised.min(3);
        let manager = bind(&mut conn, registry, manager_name, "zwlr_screencopy_manager_v1", manager_version)?;

        let mut outputs = Vec::new();
        for (name, _, version) in globals.iter().filter(|g| g.1 == "wl_output") {
            let id = bind(&mut conn, registry, *name, "wl_output", (*version).min(4))?;
            outputs.push(Output { id, scale: 1, ..Output::default() });
        }
        let mut xdg_outputs = Vec::new();
        if let Some((name, version)) = find("zxdg_output_manager_v1") {
            let xdg_manager = bind(&mut conn, registry, name, "zxdg_output_manager_v1", version.min(3))?;
            for (index, output) in outputs.iter().enumerate() {
                let xdg = conn.new_id()?;
                conn.send(
                    &Message::new(xdg_manager, XDG_OUTPUT_MANAGER_GET_XDG_OUTPUT).object(xdg).object(output.id),
                    None,
                )?;
                xdg_outputs.push((xdg, index));
            }
        }

        let mut sc = Screencopy {
            conn,
            shm,
            shm_formats: Vec::new(),
            manager,
            manager_version,
            outputs,
            xdg_outputs,
            buffer: None,
            full_output: false,
            warned_full_output: false,
        };
        // Receive the output descriptions and the wl_shm formats.
        sc.roundtrip()?;
        if sc.outputs.is_empty() {
            return Err(io::Error::other("compositor reports no outputs"));
        }
        Ok(sc)
    }

    /// Short description for status lines.
    pub fn describe(&self) -> String {
        let mode = if self.full_output { ", whole-monitor copy (scaled monitor)" } else { "" };
        format!("wlr-screencopy v{}{mode}", self.manager_version)
    }

    /// Minimum time between captures this client should keep.
    pub fn min_interval(&self) -> Duration {
        if self.full_output { FULL_OUTPUT_INTERVAL } else { Duration::ZERO }
    }

    /// Captures `region` (layout coordinates) from the monitor that contains it.
    pub fn grab(&mut self, region: &Region) -> Result<Frame, String> {
        self.grab_inner(region).map_err(|e| format!("screencopy: {e}"))
    }

    fn grab_inner(&mut self, region: &Region) -> io::Result<Frame> {
        let plan = self.plan(region)?;
        self.full_output = plan.region.is_none();
        if self.full_output && !self.warned_full_output {
            self.warned_full_output = true;
            eprintln!(
                "note: the monitor scale is not 1, so the whole monitor is copied and cropped (max ~30 captures/s). \
                 Hyprland 0.56 keeps a new capture session for every region capture on scaled monitors. \
                 Use monitor scale 1 for full-rate capture."
            );
        }
        let frame = self.conn.new_id()?;
        let request = match plan.region {
            Some((x, y, w, h)) => Message::new(self.manager, SCREENCOPY_MANAGER_CAPTURE_OUTPUT_REGION)
                .object(frame)
                .int(0) // no cursor
                .object(plan.output)
                .int(x)
                .int(y)
                .int(w)
                .int(h),
            None => {
                Message::new(self.manager, SCREENCOPY_MANAGER_CAPTURE_OUTPUT).object(frame).int(0).object(plan.output)
            }
        };
        self.conn.send(&request, None)?;
        let result = self.copy_frame(frame, plan.crop);
        // Always destroy the frame object, even after `failed`.
        let destroyed = self.conn.send(&Message::new(frame, FRAME_DESTROY), None);
        let frame = result?;
        destroyed?;
        Ok(frame)
    }

    fn copy_frame(&mut self, frame: u32, crop: Option<Rect>) -> io::Result<Frame> {
        let deadline = Instant::now() + FRAME_TIMEOUT;

        // 1. Buffer requirements. With v3 the list ends with buffer_done;
        //    before v3 the single `buffer` event is all there is.
        let mut spec = None;
        loop {
            let ev = self.next_event(deadline)?;
            if ev.object != frame {
                continue;
            }
            match ev.opcode {
                FRAME_EV_BUFFER => {
                    let mut r = ev.reader();
                    spec =
                        Some(BufferSpec { format: r.uint()?, width: r.uint()?, height: r.uint()?, stride: r.uint()? });
                    if self.manager_version < 3 {
                        break;
                    }
                }
                FRAME_EV_BUFFER_DONE => break,
                FRAME_EV_FAILED => return Err(io::Error::other("compositor refused the capture")),
                _ => {}
            }
        }
        let mut spec = spec.ok_or_else(|| io::Error::other("compositor offered no shared-memory buffer"))?;
        spec.format = self.pick_format(spec.format)?;

        // 2. Copy into our (reused) buffer.
        let buffer = self.ensure_buffer(spec)?;
        self.conn.send(&Message::new(frame, FRAME_COPY).object(buffer), None)?;

        // 3. Wait until the compositor has written the pixels.
        let mut y_invert = false;
        loop {
            let ev = self.next_event(deadline)?;
            if ev.object != frame {
                continue;
            }
            match ev.opcode {
                FRAME_EV_FLAGS => y_invert = ev.reader().uint()? & FRAME_FLAG_Y_INVERT != 0,
                FRAME_EV_READY => break,
                FRAME_EV_FAILED => {
                    return Err(io::Error::other("capture failed (output changed or permission denied?)"));
                }
                _ => {}
            }
        }

        let full = Rect { x: 0, y: 0, width: spec.width, height: spec.height };
        let crop = match crop {
            Some(c) => c.clamp_to(full).ok_or_else(|| io::Error::other("region lies outside the captured monitor"))?,
            None => full,
        };
        let buffer = self.buffer.as_ref().expect("buffer was just ensured");
        let rgb = to_rgb(buffer.pixels(), spec, crop, y_invert)?;
        Ok(Frame { width: crop.width as usize, height: crop.height as usize, rgb })
    }

    /// The offered format if wl_shm lists it, else its opaque twin (Hyprland
    /// accepts both); creating a buffer in an unlisted format is fatal.
    fn pick_format(&self, offered: u32) -> io::Result<u32> {
        check_format(offered)?;
        if self.shm_formats.is_empty() || self.shm_formats.contains(&offered) {
            return Ok(offered);
        }
        match opaque_variant(offered) {
            Some(alt) if self.shm_formats.contains(&alt) => Ok(alt),
            _ => Err(io::Error::other(format!(
                "compositor offered pixel format 0x{offered:08x} that wl_shm doesn't list"
            ))),
        }
    }

    /// Finds the output containing `region` and decides how to capture it.
    fn plan(&self, region: &Region) -> io::Result<Plan> {
        let (rx, ry) = (region.x, region.y);
        let (rw, rh) = (region.width as i32, region.height as i32);
        for output in &self.outputs {
            let (ox, oy, ow, oh) = output.logical_rect();
            if !(rx >= ox && ry >= oy && rx + rw <= ox + ow && ry + rh <= oy + oh) {
                continue;
            }
            let (lx, ly) = (rx - ox, ry - oy);
            if output.transform != 0 {
                return Err(io::Error::other(
                    "rotated/flipped monitors are not supported by the built-in capture; set capture_backend = \"grim\"",
                ));
            }
            let scale = output.pixel_scale();
            if (scale - 1.0).abs() < 1e-6 {
                return Ok(Plan { output: output.id, region: Some((lx, ly, rw, rh)), crop: None });
            }
            // Scaled monitor: copy it whole (Hyprland reuses that session) and
            // crop the region in physical pixels.
            let px = |v: i32| (v as f64 * scale).round().max(0.0) as u32;
            let crop = Rect { x: px(lx), y: px(ly), width: px(rw).max(1), height: px(rh).max(1) };
            return Ok(Plan { output: output.id, region: None, crop: Some(crop) });
        }
        let monitors: Vec<String> = self
            .outputs
            .iter()
            .map(|o| {
                let (x, y, w, h) = o.logical_rect();
                format!("{} at {x},{y} {w}x{h}", if o.name.is_empty() { "?" } else { &o.name })
            })
            .collect();
        Err(io::Error::other(format!(
            "region {} is not inside a single monitor ({})",
            region.geometry(),
            monitors.join(", ")
        )))
    }

    /// Reuses the current buffer if it matches, otherwise creates a new one.
    fn ensure_buffer(&mut self, spec: BufferSpec) -> io::Result<u32> {
        if let Some(buffer) = &self.buffer {
            if buffer.spec == spec {
                return Ok(buffer.id);
            }
        }
        if let Some(old) = self.buffer.take() {
            self.conn.send(&Message::new(old.id, WL_BUFFER_DESTROY), None)?;
        }
        let len = spec.stride as usize * spec.height as usize;
        if len == 0 || spec.stride < spec.width * bytes_per_pixel(spec.format) {
            return Err(io::Error::other(format!("bad buffer parameters {spec:?}")));
        }
        let fd = memfd(len)?;
        // SAFETY: mapping a fresh memfd of exactly `len` bytes.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Owned from here on, so it gets unmapped on any error below.
        let mut buffer = ShmBuffer { id: 0, spec, ptr: ptr as *mut u8, len };

        let pool = self.conn.new_id()?;
        self.conn
            .send(&Message::new(self.shm, WL_SHM_CREATE_POOL).object(pool).int(len as i32), Some(fd.as_raw_fd()))?;
        let id = self.conn.new_id()?;
        self.conn.send(
            &Message::new(pool, WL_SHM_POOL_CREATE_BUFFER)
                .object(id)
                .int(0)
                .int(spec.width as i32)
                .int(spec.height as i32)
                .int(spec.stride as i32)
                .uint(spec.format),
            None,
        )?;
        // The buffer keeps the pool's memory alive; the pool and fd can go.
        self.conn.send(&Message::new(pool, WL_SHM_POOL_DESTROY), None)?;
        drop(fd);
        buffer.id = id;
        self.buffer = Some(buffer);
        Ok(id)
    }

    /// Next event, after handling the ones every object can get
    /// (display errors, id deletion, output descriptions).
    fn next_event(&mut self, deadline: Instant) -> io::Result<Event> {
        loop {
            let ev = self.conn.next_event(deadline)?;
            if ev.object == DISPLAY_ID {
                handle_display_event(&mut self.conn, &ev)?;
                continue;
            }
            if self.handle_output_event(&ev)? {
                continue;
            }
            return Ok(ev);
        }
    }

    fn handle_output_event(&mut self, ev: &Event) -> io::Result<bool> {
        if ev.object == self.shm {
            if ev.opcode == WL_SHM_EV_FORMAT {
                self.shm_formats.push(ev.reader().uint()?);
            }
            return Ok(true);
        }
        if let Some(output) = self.outputs.iter_mut().find(|o| o.id == ev.object) {
            let mut r = ev.reader();
            match ev.opcode {
                WL_OUTPUT_EV_GEOMETRY => {
                    output.position = (r.int()?, r.int()?);
                    let _physical_size = (r.int()?, r.int()?);
                    let _subpixel = r.int()?;
                    let _make = r.string()?;
                    let _model = r.string()?;
                    output.transform = r.int()?;
                }
                WL_OUTPUT_EV_MODE => {
                    let flags = r.uint()?;
                    let size = (r.int()?, r.int()?);
                    if flags & WL_OUTPUT_MODE_CURRENT != 0 {
                        output.mode = size;
                    }
                }
                WL_OUTPUT_EV_SCALE => output.scale = r.int()?,
                WL_OUTPUT_EV_NAME => output.name = r.string()?,
                _ => {}
            }
            return Ok(true);
        }
        if let Some(&(_, index)) = self.xdg_outputs.iter().find(|(id, _)| *id == ev.object) {
            let output = &mut self.outputs[index];
            let mut r = ev.reader();
            match ev.opcode {
                XDG_OUTPUT_EV_LOGICAL_POSITION => output.logical_position = Some((r.int()?, r.int()?)),
                XDG_OUTPUT_EV_LOGICAL_SIZE => output.logical_size = Some((r.int()?, r.int()?)),
                _ => {}
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Waits until the compositor has processed everything sent so far.
    fn roundtrip(&mut self) -> io::Result<()> {
        let callback = self.conn.new_id()?;
        self.conn.send(&Message::new(DISPLAY_ID, WL_DISPLAY_SYNC).object(callback), None)?;
        let deadline = Instant::now() + SETUP_TIMEOUT;
        loop {
            let ev = self.next_event(deadline)?;
            if ev.object == callback && ev.opcode == WL_CALLBACK_EV_DONE {
                return Ok(());
            }
        }
    }
}

/// Handles wl_display events; a protocol error is fatal.
fn handle_display_event(conn: &mut Connection, ev: &Event) -> io::Result<()> {
    if ev.object != DISPLAY_ID {
        return Ok(());
    }
    let mut r = ev.reader();
    match ev.opcode {
        WL_DISPLAY_EV_ERROR => {
            let (object, code, message) = (r.uint()?, r.uint()?, r.string()?);
            Err(io::Error::other(format!("Wayland protocol error on object {object} (code {code}): {message}")))
        }
        WL_DISPLAY_EV_DELETE_ID => {
            conn.release_id(r.uint()?);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn bind(conn: &mut Connection, registry: u32, name: u32, interface: &str, version: u32) -> io::Result<u32> {
    let id = conn.new_id()?;
    conn.send(&Message::new(registry, WL_REGISTRY_BIND).uint(name).string(interface).uint(version).object(id), None)?;
    Ok(id)
}

fn memfd(len: usize) -> io::Result<OwnedFd> {
    // SAFETY: plain syscalls on a fd we own.
    unsafe {
        let fd = libc::memfd_create(c"fishmacro-capture".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn bytes_per_pixel(format: u32) -> u32 {
    match format {
        SHM_RGB888 | SHM_BGR888 => 3,
        _ => 4,
    }
}

/// The same layout without alpha.
fn opaque_variant(format: u32) -> Option<u32> {
    match format {
        SHM_ARGB8888 => Some(SHM_XRGB8888),
        SHM_ABGR8888 => Some(SHM_XBGR8888),
        SHM_ARGB2101010 => Some(SHM_XRGB2101010),
        SHM_ABGR2101010 => Some(SHM_XBGR2101010),
        _ => None,
    }
}

impl Rect {
    /// The part of `self` inside `bounds`, if any.
    fn clamp_to(self, bounds: Rect) -> Option<Rect> {
        let x0 = self.x.max(bounds.x);
        let y0 = self.y.max(bounds.y);
        let x1 = (self.x + self.width).min(bounds.x + bounds.width);
        let y1 = (self.y + self.height).min(bounds.y + bounds.height);
        (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
    }
}

fn check_format(format: u32) -> io::Result<()> {
    match format {
        SHM_ARGB8888 | SHM_XRGB8888 | SHM_ABGR8888 | SHM_XBGR8888 | SHM_ARGB2101010 | SHM_XRGB2101010
        | SHM_ABGR2101010 | SHM_XBGR2101010 | SHM_RGB888 | SHM_BGR888 => Ok(()),
        other => Err(io::Error::other(format!("unsupported pixel format 0x{other:08x}"))),
    }
}

/// Converts `crop` of the compositor's buffer to tightly packed RGB, top row first.
fn to_rgb(data: &[u8], spec: BufferSpec, crop: Rect, y_invert: bool) -> io::Result<Vec<u8>> {
    let (w, h, stride) = (spec.width as usize, spec.height as usize, spec.stride as usize);
    let bpp = bytes_per_pixel(spec.format) as usize;
    if data.len() < stride * h || stride < w * bpp {
        return Err(io::Error::other("capture buffer smaller than announced"));
    }
    let (cx, cy, cw, ch) = (crop.x as usize, crop.y as usize, crop.width as usize, crop.height as usize);
    if cx + cw > w || cy + ch > h {
        return Err(io::Error::other("crop outside the capture buffer"));
    }
    let mut out = Vec::with_capacity(cw * ch * 3);
    for row in cy..cy + ch {
        // With y_invert the buffer's first row is the bottom of the image.
        let src_row = if y_invert { h - 1 - row } else { row };
        let start = src_row * stride + cx * bpp;
        let line = &data[start..start + cw * bpp];
        for px in line.chunks_exact(bpp) {
            let rgb = match spec.format {
                // Little-endian 0xAARRGGBB: bytes B, G, R, A.
                SHM_ARGB8888 | SHM_XRGB8888 => [px[2], px[1], px[0]],
                // 0xAABBGGRR: bytes R, G, B, A.
                SHM_ABGR8888 | SHM_XBGR8888 => [px[0], px[1], px[2]],
                SHM_ARGB2101010 | SHM_XRGB2101010 | SHM_ABGR2101010 | SHM_XBGR2101010 => {
                    let v = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                    // Top 8 of each 10-bit channel.
                    let (hi, mid, lo) = ((v >> 22) as u8, (v >> 12) as u8, (v >> 2) as u8);
                    if matches!(spec.format, SHM_ARGB2101010 | SHM_XRGB2101010) { [hi, mid, lo] } else { [lo, mid, hi] }
                }
                // RGB888 is little-endian 0xRRGGBB: bytes B, G, R.
                SHM_RGB888 => [px[2], px[1], px[0]],
                SHM_BGR888 => [px[0], px[1], px[2]],
                _ => unreachable!("format checked before copying"),
            };
            out.extend_from_slice(&rgb);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(format: u32, width: u32, height: u32, stride: u32) -> BufferSpec {
        BufferSpec { format, width, height, stride }
    }

    #[test]
    fn fourcc_values() {
        assert_eq!(SHM_ABGR8888, 0x3432_4241);
        assert_eq!(SHM_XBGR8888, 0x3432_4258);
        assert_eq!(SHM_XRGB2101010, 0x3033_5258);
        assert_eq!(SHM_XBGR2101010, 0x3033_4258);
    }

    fn all(s: BufferSpec) -> Rect {
        Rect { x: 0, y: 0, width: s.width, height: s.height }
    }

    #[test]
    fn converts_8bit_formats_with_stride_padding() {
        // 1x2 image, stride 8 (4 bytes of padding per row).
        let data = [10, 20, 30, 255, 0, 0, 0, 0, 40, 50, 60, 255, 0, 0, 0, 0];
        let s = spec(SHM_XRGB8888, 1, 2, 8);
        assert_eq!(to_rgb(&data, s, all(s), false).unwrap(), [30, 20, 10, 60, 50, 40]);
        let s = spec(SHM_XBGR8888, 1, 2, 8);
        assert_eq!(to_rgb(&data, s, all(s), false).unwrap(), [10, 20, 30, 40, 50, 60]);
        assert_eq!(to_rgb(&data, s, all(s), true).unwrap(), [40, 50, 60, 10, 20, 30]);
    }

    #[test]
    fn converts_10bit_formats() {
        // R = 1023, G = 512, B = 0 in XRGB2101010.
        let v: u32 = (1023 << 20) | (512 << 10);
        let s = spec(SHM_XRGB2101010, 1, 1, 4);
        assert_eq!(to_rgb(&v.to_le_bytes(), s, all(s), false).unwrap(), [255, 128, 0]);
        // Same bits in XBGR2101010 mean B = 1023, R = 0.
        let s = spec(SHM_XBGR2101010, 1, 1, 4);
        assert_eq!(to_rgb(&v.to_le_bytes(), s, all(s), false).unwrap(), [0, 128, 255]);
    }

    #[test]
    fn crops_with_and_without_y_invert() {
        // 3x3 XBGR image where pixel (x, y) = [x, y, 0].
        let mut data = Vec::new();
        for y in 0..3u8 {
            for x in 0..3u8 {
                data.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        let s = spec(SHM_XBGR8888, 3, 3, 12);
        let crop = Rect { x: 1, y: 0, width: 2, height: 2 };
        assert_eq!(to_rgb(&data, s, crop, false).unwrap(), [1, 0, 0, 2, 0, 0, 1, 1, 0, 2, 1, 0]);
        // y_invert: buffer row 2 is the image's top row.
        assert_eq!(to_rgb(&data, s, crop, true).unwrap(), [1, 2, 0, 2, 2, 0, 1, 1, 0, 2, 1, 0]);
        assert!(to_rgb(&data, s, Rect { x: 2, y: 0, width: 2, height: 1 }, false).is_err());
    }

    #[test]
    fn rect_clamping() {
        let bounds = Rect { x: 0, y: 0, width: 100, height: 50 };
        assert_eq!(
            Rect { x: 90, y: 40, width: 20, height: 20 }.clamp_to(bounds),
            Some(Rect { x: 90, y: 40, width: 10, height: 10 })
        );
        assert_eq!(Rect { x: 100, y: 0, width: 5, height: 5 }.clamp_to(bounds), None);
    }

    #[test]
    fn rejects_unknown_format_and_short_buffers() {
        assert!(check_format(fourcc(b"NV12")).is_err());
        let s = spec(SHM_XRGB8888, 2, 1, 8);
        assert!(to_rgb(&[0; 4], s, all(s), false).is_err());
        assert_eq!(opaque_variant(SHM_ARGB8888), Some(SHM_XRGB8888));
        assert_eq!(opaque_variant(SHM_ABGR8888), Some(SHM_XBGR8888));
    }

    #[test]
    fn pixel_scale_from_xdg_output() {
        let o = Output { mode: (2560, 1440), logical_size: Some((2560, 1440)), ..Output::default() };
        assert_eq!(o.pixel_scale(), 1.0);
        let o = Output { mode: (2560, 1440), logical_size: Some((2048, 1152)), ..o };
        assert_eq!(o.pixel_scale(), 1.25);
    }

    #[test]
    fn logical_rect_falls_back_to_mode_and_scale() {
        let o = Output { position: (2560, 0), mode: (3840, 2160), scale: 2, transform: 1, ..Output::default() };
        assert_eq!(o.logical_rect(), (2560, 0, 1080, 1920));
        let o = Output { logical_position: Some((0, 0)), logical_size: Some((2560, 1440)), ..o };
        assert_eq!(o.logical_rect(), (0, 0, 2560, 1440));
    }
}
