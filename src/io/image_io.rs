//! PNG writing and PPM reading for the image/segment commands (port of
//! src/io/image_io.cpp). Owner: the dormant vision/text modalities port.
//!
//! Self-contained (no libpng / zlib dependency): emits a PNG whose IDAT is a
//! zlib stream of *stored* (uncompressed) DEFLATE blocks, which every decoder
//! accepts. The commons diffusion engines return raw RGBA pixels
//! (image_media_type "image/raw-rgba"); wally renders them to a real PNG so
//! `--out foo.png` is a valid image.

use std::sync::OnceLock;

fn put_u32_be(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn crc32_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        table
    })
}

fn crc32(data: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    let mut i = 0usize;
    while i < data.len() {
        let block = std::cmp::min(data.len() - i, 5552);
        for &byte in &data[i..i + block] {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
        i += block;
    }
    (b << 16) | a
}

fn write_chunk(png: &mut Vec<u8>, chunk_type: &[u8; 4], data: &[u8]) {
    put_u32_be(png, data.len() as u32);
    let crc_start = png.len();
    png.extend_from_slice(chunk_type);
    png.extend_from_slice(data);
    let crc = crc32(&png[crc_start..]);
    put_u32_be(png, crc);
}

/// Write 8-bit RGBA pixels (row-major, width*height*4 bytes) as a PNG file.
/// An empty `rgba` mirrors the C++ null-pointer guard.
pub fn write_png(path: &str, rgba: &[u8], width: i32, height: i32) -> Result<(), String> {
    if rgba.is_empty() || width <= 0 || height <= 0 {
        return Err("invalid image dimensions or data".to_string());
    }

    // Filtered scanlines: PNG requires a per-row filter byte (0 = None).
    let width = width as usize;
    let height = height as usize;
    let row_bytes = width * 4;
    let mut raw = Vec::with_capacity(height * (1 + row_bytes));
    for y in 0..height {
        raw.push(0); // filter type: none
        let start = y * row_bytes;
        raw.extend_from_slice(&rgba[start..start + row_bytes]);
    }

    // zlib stream: 2-byte header, stored DEFLATE blocks, 4-byte Adler-32.
    let mut zlib = Vec::new();
    zlib.push(0x78); // CMF: deflate, 32K window
    zlib.push(0x01); // FLG: no dict, fastest
    let mut pos = 0usize;
    loop {
        let block = std::cmp::min(raw.len() - pos, 0xFFFF);
        let final_block = pos + block >= raw.len();
        zlib.push(if final_block { 1 } else { 0 }); // BFINAL + BTYPE=00 (stored)
        let len = block as u16;
        let nlen = !len;
        zlib.push((len & 0xFF) as u8);
        zlib.push(((len >> 8) & 0xFF) as u8);
        zlib.push((nlen & 0xFF) as u8);
        zlib.push(((nlen >> 8) & 0xFF) as u8);
        zlib.extend_from_slice(&raw[pos..pos + block]);
        pos += block;
        if pos >= raw.len() {
            break;
        }
    }
    let adler = adler32(&raw);
    put_u32_be(&mut zlib, adler);

    let mut png = Vec::new();
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);

    let mut ihdr = Vec::new();
    put_u32_be(&mut ihdr, width as u32);
    put_u32_be(&mut ihdr, height as u32);
    ihdr.push(8); // bit depth
    ihdr.push(6); // color type: RGBA
    ihdr.push(0); // compression: deflate
    ihdr.push(0); // filter: adaptive
    ihdr.push(0); // interlace: none
    write_chunk(&mut png, b"IHDR", &ihdr);
    write_chunk(&mut png, b"IDAT", &zlib);
    write_chunk(&mut png, b"IEND", &[]);

    std::fs::write(path, &png).map_err(|_| format!("cannot open {path} for writing"))?;
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RgbImage {
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// C `isspace` on a raw byte (space, \t, \n, \v, \f, \r).
fn is_ppm_space(b: u8) -> bool {
    matches!(b, b' ' | 0x09..=0x0D)
}

fn is_ppm_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

/// Parse the next whitespace-delimited unsigned integer from a PPM header,
/// skipping '#'-to-EOL comments. Returns `None` at EOF / on non-numeric input.
fn next_ppm_uint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut i = *pos;
    loop {
        while i < bytes.len() && is_ppm_space(bytes[i]) {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'#' {
            // comment to end of line
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        break;
    }
    if i >= bytes.len() || !is_ppm_digit(bytes[i]) {
        return None;
    }
    let mut value: u64 = 0;
    while i < bytes.len() && is_ppm_digit(bytes[i]) {
        value = value * 10 + u64::from(bytes[i] - b'0');
        if value > 0xFFFF_FFFF {
            return None;
        }
        i += 1;
    }
    *pos = i;
    Some(value as u32)
}

/// Read a binary PPM (P6, maxval 255) image into tightly-packed RGB8. PPM is
/// the dependency-free counterpart to write_png: `magick in.png out.ppm` (or
/// `ffmpeg -i in.png out.ppm`) produces one.
pub fn read_ppm(path: &str) -> Result<RgbImage, String> {
    let bytes = std::fs::read(path).map_err(|_| format!("cannot open {path}"))?;

    if bytes.len() < 2 || bytes[0] != b'P' || bytes[1] != b'6' {
        return Err(format!(
            "{path} is not a binary PPM (P6); convert with e.g. `magick in.png out.ppm`"
        ));
    }
    let mut pos = 2usize;
    let width = next_ppm_uint(&bytes, &mut pos)
        .ok_or_else(|| format!("malformed PPM header in {path}"))?;
    let height = next_ppm_uint(&bytes, &mut pos)
        .ok_or_else(|| format!("malformed PPM header in {path}"))?;
    let maxval = next_ppm_uint(&bytes, &mut pos)
        .ok_or_else(|| format!("malformed PPM header in {path}"))?;
    if width == 0 || height == 0 || maxval != 255 {
        return Err("unsupported PPM (need non-empty dimensions and maxval 255)".to_string());
    }
    // Bound dimensions (4096 mirrors the SDK's segmentation source cap) so the
    // `width * height * 3` below cannot overflow and a pathological header
    // cannot mint a huge RgbImage backed by a tiny buffer.
    const MAX_PPM_DIMENSION: u32 = 4096;
    if width > MAX_PPM_DIMENSION || height > MAX_PPM_DIMENSION {
        return Err(format!(
            "PPM dimensions exceed the supported maximum (4096x4096) in {path}"
        ));
    }
    // Exactly one whitespace byte separates the header from the pixel payload.
    pos += 1;
    let expected = width as usize * height as usize * 3;
    if pos + expected > bytes.len() {
        return Err(format!("truncated PPM pixel data in {path}"));
    }
    Ok(RgbImage {
        rgb: bytes[pos..pos + expected].to_vec(),
        width,
        height,
    })
}
