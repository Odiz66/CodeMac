//! PNG helpers for debug / calibration snapshots.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::capture::Frame;

pub fn save_png(path: &Path, frame: &Frame) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut enc = png::Encoder::new(BufWriter::new(file), frame.width, frame.height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&frame.data)?;
    writer.finish()?;
    Ok(())
}

/// Loads any 8/16-bit PNG as RGB.
pub fn load_png(path: &Path) -> Result<Frame> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let mut buf = vec![0; reader.output_buffer_size().context("PNG too large")?];
    let info = reader.next_frame(&mut buf).context("decoding PNG")?;
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => bail!("indexed PNG was not expanded"),
    };
    let mut frame = Frame::new(info.width, info.height);
    for y in 0..info.height as usize {
        let line = &buf[y * info.line_size..];
        for x in 0..info.width as usize {
            let p = &line[x * channels..x * channels + channels];
            let rgb = if channels >= 3 {
                [p[0], p[1], p[2]]
            } else {
                [p[0], p[0], p[0]]
            };
            frame.set(x as u32, y as u32, rgb);
        }
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let mut f = Frame::new(3, 2);
        f.set(2, 1, [9, 8, 7]);
        f.set(0, 0, [255, 0, 128]);
        let dir = std::env::temp_dir().join(format!("fisch-macro-test-{}", std::process::id()));
        let path = dir.join("t.png");
        save_png(&path, &f).unwrap();
        let g = load_png(&path).unwrap();
        assert_eq!(g.data, f.data);
        let _ = std::fs::remove_dir_all(dir);
    }
}
