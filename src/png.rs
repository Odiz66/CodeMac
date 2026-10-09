//! Minimal PNG writer for calibration screenshots: 8-bit RGB, uncompressed
//! ("stored") deflate blocks. Files are larger than usual but every viewer
//! opens them, and it saves a dependency.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub fn write_rgb(path: &Path, width: usize, height: usize, rgb: &[u8]) -> io::Result<()> {
    assert_eq!(rgb.len(), width * height * 3, "pixel buffer size mismatch");
    let mut out = BufWriter::new(File::create(path)?);
    out.write_all(b"\x89PNG\r\n\x1a\n")?;

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    // bit depth 8, color type 2 (RGB), deflate, no filter method, no interlace
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr)?;

    // Each scanline is prefixed with filter type 0 (none).
    let row = width * 3;
    let mut raw = Vec::with_capacity((row + 1) * height);
    for line in rgb.chunks_exact(row) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    write_chunk(&mut out, b"IDAT", &zlib_stored(&raw))?;
    write_chunk(&mut out, b"IEND", &[])?;
    out.flush()
}

fn write_chunk(out: &mut impl Write, kind: &[u8; 4], data: &[u8]) -> io::Result<()> {
    out.write_all(&(data.len() as u32).to_be_bytes())?;
    out.write_all(kind)?;
    out.write_all(data)?;
    let crc = crc32(&[kind.as_slice(), data]);
    out.write_all(&crc.to_be_bytes())
}

/// A zlib stream made of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    const MAX_BLOCK: usize = 65_535;
    let mut out = Vec::with_capacity(data.len() + data.len() / MAX_BLOCK * 5 + 16);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut blocks = data.chunks(MAX_BLOCK).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = blocks.next() {
        let last = blocks.peek().is_none();
        let len = block.len() as u16;
        out.push(last as u8);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for part in parts {
        for &byte in *part {
            crc ^= byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += byte as u32;
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(&[b"IEND"]), 0xae42_6082);
        assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn stored_blocks_split_and_flag_last() {
        let data = vec![7u8; 70_000];
        let z = zlib_stored(&data);
        assert_eq!(z[2], 0); // first block not final
        assert_eq!(u16::from_le_bytes([z[3], z[4]]), 65_535);
        let second = 2 + 5 + 65_535;
        assert_eq!(z[second], 1); // second block final
        assert_eq!(u16::from_le_bytes([z[second + 1], z[second + 2]]), 4_465);
        assert_eq!(z.len(), 2 + 5 * 2 + 70_000 + 4);
    }
}
