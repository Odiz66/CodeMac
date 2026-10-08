//! In-process wlr-screencopy client.
//!
//! Keeps one Wayland connection and one shared-memory buffer alive and asks the
//! compositor to copy only the bar region each frame. Compared with spawning
//! `grim` per frame this skips process start-up, the Wayland handshake and PPM
//! encoding, so a frame costs about one compositor refresh. SHM buffers are
//! filled by the compositor with a GPU read-back, which works on NVIDIA.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

use super::{Capturer, Frame, Rect};

#[derive(Default, Debug)]
struct OutputInfo {
    name: Option<String>,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    scale: i32,
}

struct Output {
    proxy: WlOutput,
    info: OutputInfo,
}

#[derive(Default)]
struct FrameState {
    shm: Vec<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    y_invert: bool,
    ready: Option<(u64, u32)>,
    failed: bool,
}

struct State {
    outputs: Vec<Output>,
    frame: FrameState,
}

struct ShmBuf {
    file: File,
    pool: WlShmPool,
    buffer: WlBuffer,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

pub struct Screencopy {
    _conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    manager: ZwlrScreencopyManagerV1,
    shm: WlShm,
    buf: Option<ShmBuf>,
    output_name: String,
    timeout: Duration,
    raw: Vec<u8>,
}

impl Screencopy {
    pub fn connect(output_name: &str, timeout: Duration) -> Result<Screencopy> {
        let conn = Connection::connect_to_env()
            .context("connecting to the Wayland display (is WAYLAND_DISPLAY set?)")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("reading Wayland globals")?;
        let qh = queue.handle();
        let shm: WlShm = globals
            .bind(&qh, 1..=1, ())
            .context("compositor has no wl_shm")?;
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .context("compositor does not offer zwlr_screencopy_manager_v1")?;

        let mut state = State {
            outputs: Vec::new(),
            frame: FrameState::default(),
        };
        globals.contents().with_list(|list| {
            for g in list
                .iter()
                .filter(|g| g.interface == WlOutput::interface().name)
            {
                let idx = state.outputs.len();
                let proxy =
                    globals
                        .registry()
                        .bind::<WlOutput, _, _>(g.name, g.version.min(4), &qh, idx);
                state.outputs.push(Output {
                    proxy,
                    info: OutputInfo::default(),
                });
            }
        });
        queue.roundtrip(&mut state).context("Wayland roundtrip")?;
        if state.outputs.is_empty() {
            bail!("compositor reported no outputs");
        }
        if !output_name.is_empty()
            && !state
                .outputs
                .iter()
                .any(|o| o.info.name.as_deref() == Some(output_name))
        {
            let names: Vec<_> = state
                .outputs
                .iter()
                .filter_map(|o| o.info.name.clone())
                .collect();
            bail!("output {output_name:?} not found (available: {names:?})");
        }

        Ok(Screencopy {
            _conn: conn,
            queue,
            qh,
            state,
            manager,
            shm,
            buf: None,
            output_name: output_name.to_string(),
            timeout,
            raw: Vec::new(),
        })
    }

    /// Returns the output index, the rectangle origin in output-local
    /// coordinates and the output's logical size (0 when unknown).
    fn pick_output(&self, r: Rect) -> (usize, i32, i32, i32, i32) {
        let outputs = &self.state.outputs;
        let local = |i: usize| {
            let o = &outputs[i].info;
            let s = o.scale.max(1);
            (i, r.x - o.x, r.y - o.y, o.width / s, o.height / s)
        };
        if !self.output_name.is_empty()
            && let Some(i) = outputs
                .iter()
                .position(|o| o.info.name.as_deref() == Some(&self.output_name))
        {
            return local(i);
        }
        for (i, o) in outputs.iter().enumerate() {
            let s = o.info.scale.max(1);
            let (w, h) = (o.info.width / s, o.info.height / s);
            if r.x >= o.info.x && r.y >= o.info.y && r.x < o.info.x + w && r.y < o.info.y + h {
                return local(i);
            }
        }
        local(0)
    }

    fn dispatch_until(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> Result<()> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .context("Wayland dispatch")?;
            if done(&self.state) {
                return Ok(());
            }
            self.queue.flush().context("Wayland flush")?;
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out waiting for the compositor to deliver a frame");
            }
            let left = deadline - now;
            let ts = Timespec {
                tv_sec: left.as_secs() as _,
                tv_nsec: left.subsec_nanos() as _,
            };
            let readable = {
                let mut fds = [PollFd::from_borrowed_fd(
                    guard.connection_fd(),
                    PollFlags::IN | PollFlags::ERR,
                )];
                match poll(&mut fds, Some(&ts)) {
                    Ok(n) => n > 0,
                    Err(rustix::io::Errno::INTR) => false,
                    Err(e) => return Err(e).context("poll on the Wayland socket"),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e).context("reading from the Wayland socket"),
                }
            }
        }
    }

    fn ensure_buffer(
        &mut self,
        format: wl_shm::Format,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<()> {
        if let Some(b) = &self.buf
            && b.format == format
            && b.width == width
            && b.height == height
            && b.stride == stride
        {
            return Ok(());
        }
        if let Some(old) = self.buf.take() {
            old.buffer.destroy();
            old.pool.destroy();
        }
        let size = stride as usize * height as usize;
        let fd = rustix::fs::memfd_create("fisch-macro-capture", rustix::fs::MemfdFlags::CLOEXEC)
            .context("memfd_create")?;
        let file = File::from(fd);
        file.set_len(size as u64)?;
        let pool = self
            .shm
            .create_pool(file.as_fd(), size as i32, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            &self.qh,
            (),
        );
        self.buf = Some(ShmBuf {
            file,
            pool,
            buffer,
            format,
            width,
            height,
            stride,
        });
        Ok(())
    }

    fn capture(&mut self, frame: &ZwlrScreencopyFrameV1, deadline: Instant) -> Result<Frame> {
        let v3 = self.manager.version() >= 3;
        self.dispatch_until(deadline, |s| {
            s.frame.failed
                || if v3 {
                    s.frame.buffer_done
                } else {
                    !s.frame.shm.is_empty()
                }
        })?;
        if self.state.frame.failed {
            bail!("compositor refused the capture (screencopy 'failed')");
        }
        let (format, width, height, stride) = self
            .state
            .frame
            .shm
            .iter()
            .copied()
            .find(|(f, ..)| supported(*f))
            .ok_or_else(|| {
                anyhow!(
                    "no supported SHM format offered: {:?}",
                    self.state.frame.shm
                )
            })?;
        self.ensure_buffer(format, width, height, stride)?;
        let buf = self.buf.as_ref().expect("buffer was just created");
        frame.copy(&buf.buffer);
        self.dispatch_until(deadline, |s| s.frame.failed || s.frame.ready.is_some())?;
        if self.state.frame.failed {
            bail!("compositor failed to copy the frame");
        }

        let buf = self.buf.as_ref().expect("buffer exists");
        self.raw.resize(stride as usize * height as usize, 0);
        buf.file
            .read_exact_at(&mut self.raw, 0)
            .context("reading the SHM buffer")?;
        let mut out = Frame::new(width, height);
        out.received = Instant::now();
        out.stamp = self.state.frame.ready;
        convert(
            format,
            &self.raw,
            width,
            height,
            stride,
            self.state.frame.y_invert,
            &mut out.data,
        );
        Ok(out)
    }
}

impl Capturer for Screencopy {
    fn name(&self) -> &'static str {
        "screencopy"
    }

    fn grab(&mut self, rect: Rect) -> Result<Frame> {
        if rect.w <= 0 || rect.h <= 0 {
            bail!("empty capture rectangle {rect}");
        }
        let (idx, lx, ly, ow, oh) = self.pick_output(rect);
        // The compositor rejects regions that stick out of the output, so
        // capture the visible part and paste it into a frame of the full size.
        let (x0, y0) = (lx.max(0), ly.max(0));
        let (x1, y1) = if ow > 0 && oh > 0 {
            ((lx + rect.w).min(ow), (ly + rect.h).min(oh))
        } else {
            (lx + rect.w, ly + rect.h)
        };
        if x1 <= x0 || y1 <= y0 {
            bail!("capture rectangle {rect} is outside the screen");
        }
        let output = self.state.outputs[idx].proxy.clone();
        self.state.frame = FrameState::default();
        // overlay_cursor = 0: never capture the mouse pointer.
        let frame =
            self.manager
                .capture_output_region(0, &output, x0, y0, x1 - x0, y1 - y0, &self.qh, ());
        let result = self.capture(&frame, Instant::now() + self.timeout);
        frame.destroy();
        let part = result?;
        if (x0, y0, x1, y1) == (lx, ly, lx + rect.w, ly + rect.h) {
            return Ok(part);
        }
        let mut full = Frame::new(rect.w as u32, rect.h as u32);
        full.received = part.received;
        full.stamp = part.stamp;
        let (dx, dy) = ((x0 - lx) as u32, (y0 - ly) as u32);
        for y in 0..part.height {
            for x in 0..part.width {
                full.set(x + dx, y + dy, part.px(x, y));
            }
        }
        Ok(full)
    }
}

impl Drop for Screencopy {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            b.buffer.destroy();
            b.pool.destroy();
        }
        let _ = self.queue.flush();
    }
}

fn supported(f: wl_shm::Format) -> bool {
    use wl_shm::Format::*;
    matches!(
        f,
        Argb8888
            | Xrgb8888
            | Abgr8888
            | Xbgr8888
            | Rgb888
            | Bgr888
            | Xrgb2101010
            | Argb2101010
            | Xbgr2101010
            | Abgr2101010
    )
}

/// Converts a DRM/wl_shm little-endian pixel buffer to packed RGB.
fn convert(
    format: wl_shm::Format,
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
    out: &mut [u8],
) {
    use wl_shm::Format::*;
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    for row in 0..h {
        let src_row = if y_invert { h - 1 - row } else { row };
        let line = &raw[src_row * stride..];
        let dst = &mut out[row * w * 3..(row + 1) * w * 3];
        for x in 0..w {
            let rgb = match format {
                // [31:0] x:R:G:B -> bytes B, G, R, x
                Argb8888 | Xrgb8888 => {
                    let p = &line[x * 4..x * 4 + 4];
                    [p[2], p[1], p[0]]
                }
                // [31:0] x:B:G:R -> bytes R, G, B, x
                Abgr8888 | Xbgr8888 => {
                    let p = &line[x * 4..x * 4 + 4];
                    [p[0], p[1], p[2]]
                }
                // [23:0] R:G:B -> bytes B, G, R
                Rgb888 => {
                    let p = &line[x * 3..x * 3 + 3];
                    [p[2], p[1], p[0]]
                }
                // [23:0] B:G:R -> bytes R, G, B
                Bgr888 => {
                    let p = &line[x * 3..x * 3 + 3];
                    [p[0], p[1], p[2]]
                }
                Xrgb2101010 | Argb2101010 => {
                    let v = u32::from_le_bytes(line[x * 4..x * 4 + 4].try_into().unwrap());
                    [
                        ((v >> 22) & 0xff) as u8,
                        ((v >> 12) & 0xff) as u8,
                        ((v >> 2) & 0xff) as u8,
                    ]
                }
                Xbgr2101010 | Abgr2101010 => {
                    let v = u32::from_le_bytes(line[x * 4..x * 4 + 4].try_into().unwrap());
                    [
                        ((v >> 2) & 0xff) as u8,
                        ((v >> 12) & 0xff) as u8,
                        ((v >> 22) & 0xff) as u8,
                    ]
                }
                _ => [0, 0, 0],
            };
            dst[x * 3..x * 3 + 3].copy_from_slice(&rgb);
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Output hot-plug is not handled; restart the macro after changing monitors.
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(*idx) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                o.info.x = x;
                o.info.y = y;
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                o.info.width = width;
                o.info.height = height;
            }
            wl_output::Event::Scale { factor } => o.info.scale = factor,
            wl_output::Event::Name { name } => o.info.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let f = &mut state.frame;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => f.shm.push((format, width, height, stride)),
            Event::Flags {
                flags: WEnum::Value(flags),
            } => f.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => f.ready = Some((((tv_sec_hi as u64) << 32) | tv_sec_lo as u64, tv_nsec)),
            Event::Failed => f.failed = true,
            Event::BufferDone => f.buffer_done = true,
            _ => {}
        }
    }
}

delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore ZwlrScreencopyManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_xrgb8888_and_y_invert() {
        // 2x2 image, stride 8: row0 = red, green; row1 = blue, white
        let raw = [0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
        let mut out = [0u8; 12];
        convert(wl_shm::Format::Xrgb8888, &raw, 2, 2, 8, false, &mut out);
        assert_eq!(out, [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        convert(wl_shm::Format::Xrgb8888, &raw, 2, 2, 8, true, &mut out);
        assert_eq!(out, [0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn converts_xbgr8888_and_10bit() {
        let raw = [10, 20, 30, 0];
        let mut out = [0u8; 3];
        convert(wl_shm::Format::Xbgr8888, &raw, 1, 1, 4, false, &mut out);
        assert_eq!(out, [10, 20, 30]);
        let v: u32 = (1023 << 20) | (512 << 10) | 4; // R=max, G=half, B=~0
        convert(
            wl_shm::Format::Xrgb2101010,
            &v.to_le_bytes(),
            1,
            1,
            4,
            false,
            &mut out,
        );
        assert_eq!(out, [255, 128, 1]);
    }
}
