//! test_wally_unit.cpp cases owned by the dormant vision/text modalities port
//! (port of the `wally::image` and `--top-n` slices of
//! explore-main/tests/test_wally_unit.cpp).
//!
//! read_ppm/write_png helpers below (bytes_of/make_ppm/read_bytes/PngChunk/
//! parse_png/find_chunk/ZlibParse/parse_stored_zlib/test_crc32/test_adler32/
//! filtered_raw) are local, independent Rust reimplementations of the C++
//! file's own test-only helpers of the same purpose (deliberately separate
//! from src/io/image_io.rs's private encoder internals, so a regression there
//! cannot mask a regression here — same rationale as the C++ comment on
//! test_crc32).

#[allow(unused_imports)]
use super::common;

use wally::cli::{App, Validator};
use wally::io::image_io::{self, write_png};

// -----------------------------------------------------------------------------
// Local test-only helpers (ported from test_wally_unit.cpp's own helpers).
// -----------------------------------------------------------------------------

fn make_ppm(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    let mut v = format!("P6\n{w} {h}\n255\n").into_bytes();
    v.extend_from_slice(pixels);
    v
}

fn read_u32_be(b: &[u8], off: usize) -> u32 {
    (u32::from(b[off]) << 24)
        | (u32::from(b[off + 1]) << 16)
        | (u32::from(b[off + 2]) << 8)
        | u32::from(b[off + 3])
}

/// Independent CRC-32 (PNG polynomial) — deliberately separate from the
/// encoder's own implementation so a regression there cannot mask a
/// regression here.
fn test_crc32(data: &[u8]) -> u32 {
    fn table() -> [u32; 256] {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        table
    }
    let table = table();
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Independent Adler-32 (per-byte modulo form).
fn test_adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Reconstruct the PNG filtered scanlines the encoder feeds into DEFLATE: a
/// 0x00 filter byte per row followed by that row's RGBA bytes.
fn filtered_raw(rgba: &[u8], width: i32, height: i32) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut raw = Vec::with_capacity(height as usize * (1 + row_bytes));
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * row_bytes..y * row_bytes + row_bytes]);
    }
    raw
}

#[derive(Debug, Default)]
struct PngChunk {
    chunk_type: String,
    data: Vec<u8>,
    stored_crc: u32,
    computed_crc: u32,
}

/// Parse the 8-byte signature + length/type/data/CRC chunk stream. Records
/// both the stored CRC and an independently computed CRC over type+data per
/// chunk.
fn parse_png(png: &[u8]) -> Option<Vec<PngChunk>> {
    const SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if png.len() < 8 || png[0..8] != SIG {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= png.len() {
        let len = read_u32_be(png, pos) as usize;
        let data_off = pos + 8;
        if data_off + len + 4 > png.len() {
            return None;
        }
        let chunk_type = String::from_utf8_lossy(&png[pos + 4..pos + 8]).into_owned();
        let data = png[data_off..data_off + len].to_vec();
        let stored_crc = read_u32_be(png, data_off + len);
        let computed_crc = test_crc32(&png[pos + 4..data_off + len]);
        out.push(PngChunk {
            chunk_type,
            data,
            stored_crc,
            computed_crc,
        });
        pos = data_off + len + 4;
    }
    if pos == png.len() {
        Some(out)
    } else {
        None
    }
}

fn find_chunk<'a>(chunks: &'a [PngChunk], chunk_type: &str) -> Option<&'a PngChunk> {
    chunks.iter().find(|c| c.chunk_type == chunk_type)
}

/// Parse a zlib stream (0x78 0x01 + stored DEFLATE blocks + 4-byte Adler-32).
#[derive(Debug, Default)]
struct ZlibParse {
    ok: bool,
    payload: Vec<u8>,
    block_count: i32,
    bfinal_ok: bool,  // BFINAL set on exactly the last block, none earlier
    lennlen_ok: bool, // NLEN == ~LEN for every block
    adler: u32,
}

fn parse_stored_zlib(z: &[u8]) -> ZlibParse {
    let mut r = ZlibParse {
        lennlen_ok: true,
        ..Default::default()
    };
    if z.len() < 6 || z[0] != 0x78 || z[1] != 0x01 {
        return r;
    }
    let adler_off = z.len() - 4;
    let mut pos = 2usize;
    let mut finals: Vec<bool> = Vec::new();
    while pos < adler_off {
        if adler_off - pos < 5 {
            return ZlibParse::default();
        }
        let hdr = z[pos];
        let btype = (hdr >> 1) & 0x03;
        if btype != 0 {
            return ZlibParse::default();
        }
        let len = u16::from(z[pos + 1]) | (u16::from(z[pos + 2]) << 8);
        let nlen = u16::from(z[pos + 3]) | (u16::from(z[pos + 4]) << 8);
        if !len != nlen {
            r.lennlen_ok = false;
        }
        let data_off = pos + 5;
        if data_off + len as usize > adler_off {
            return ZlibParse::default();
        }
        r.payload
            .extend_from_slice(&z[data_off..data_off + len as usize]);
        finals.push(hdr & 0x01 != 0);
        pos = data_off + len as usize;
        r.block_count += 1;
    }
    if pos != adler_off {
        return ZlibParse::default();
    }
    r.bfinal_ok = !finals.is_empty() && *finals.last().unwrap();
    for &f in &finals[..finals.len().saturating_sub(1)] {
        if f {
            r.bfinal_ok = false;
        }
    }
    r.adler = (u32::from(z[adler_off]) << 24)
        | (u32::from(z[adler_off + 1]) << 16)
        | (u32::from(z[adler_off + 2]) << 8)
        | u32::from(z[adler_off + 3]);
    r.ok = true;
    r
}

// -----------------------------------------------------------------------------
// read_ppm
// -----------------------------------------------------------------------------

#[test]
fn read_ppm_errors() {
    let dir = tempfile::tempdir().expect("temp dir");

    fn with_pixels(header: &str, n: usize) -> Vec<u8> {
        let mut v = header.as_bytes().to_vec();
        for i in 0..n {
            v.push(i as u8);
        }
        v
    }

    struct Case {
        label: &'static str,
        create: bool,
        bytes: Vec<u8>,
        expect_substr: &'static str,
    }
    let cases = [
        Case {
            label: "missing file",
            create: false,
            bytes: vec![],
            expect_substr: "cannot open",
        },
        Case {
            label: "ascii P3 magic",
            create: true,
            bytes: with_pixels("P3\n2 1\n255\n", 6),
            expect_substr: "is not a binary PPM (P6)",
        },
        Case {
            label: "one byte file",
            create: true,
            bytes: b"P".to_vec(),
            expect_substr: "is not a binary PPM (P6)",
        },
        Case {
            label: "non-numeric dimension",
            create: true,
            bytes: b"P6\nxx 1\n255\n".to_vec(),
            expect_substr: "malformed PPM header",
        },
        Case {
            label: "eof before maxval",
            create: true,
            bytes: b"P6\n2 1\n".to_vec(),
            expect_substr: "malformed PPM header",
        },
        Case {
            label: "zero width",
            create: true,
            bytes: b"P6\n0 1\n255\n".to_vec(),
            expect_substr: "unsupported PPM",
        },
        Case {
            label: "zero height",
            create: true,
            bytes: b"P6\n2 0\n255\n".to_vec(),
            expect_substr: "unsupported PPM",
        },
        Case {
            label: "maxval 254",
            create: true,
            bytes: with_pixels("P6\n2 1\n254\n", 6),
            expect_substr: "unsupported PPM",
        },
        Case {
            label: "maxval 65535",
            create: true,
            bytes: with_pixels("P6\n2 1\n65535\n", 6),
            expect_substr: "unsupported PPM",
        },
        Case {
            label: "truncated payload",
            create: true,
            bytes: with_pixels("P6\n2 2\n255\n", 6),
            expect_substr: "truncated PPM pixel data",
        },
        Case {
            label: "non-space byte after maxval",
            create: true,
            // "255X": next_ppm_uint stops at 'X' (not a digit), so maxval==255
            // but the byte right after it is not whitespace. Without the
            // bounds/whitespace check this byte is silently swallowed as the
            // header/payload separator, shifting every pixel by one.
            bytes: with_pixels("P6\n2 1\n255X", 6),
            expect_substr: "malformed PPM header",
        },
    ];

    // Note: the C++ case additionally seeds the *out* parameter with
    // sentinels and asserts it is untouched on failure. read_ppm() here
    // returns a fresh Result rather than writing through an out-param, so
    // there is nothing that can be mutated on the Err path; that assertion
    // has no Rust equivalent and is intentionally omitted.
    for (i, c) in cases.iter().enumerate() {
        let path = dir.path().join(format!("err-{i}.ppm"));
        if c.create {
            std::fs::write(&path, &c.bytes)
                .unwrap_or_else(|_| panic!("setup failed for case: {}", c.label));
        }
        let err = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect_err(&format!("expected failure for case: {}", c.label));
        assert!(
            err.contains(c.expect_substr),
            "wrong error for case: {}: expected substring \"{}\", got \"{err}\"",
            c.label,
            c.expect_substr
        );
    }
}

#[test]
fn read_ppm_happy_path() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Minimal 2x1 image: exact tight RGB8 packing (what cmd_segment feeds as
    // stride = width*3, RAC_SEGMENTATION_PIXEL_FORMAT_RGB8).
    {
        let pixels: [u8; 6] = [10, 20, 30, 200, 210, 220];
        let path = dir.path().join("2x1.ppm");
        std::fs::write(&path, make_ppm(2, 1, &pixels)).expect("setup: cannot write 2x1 ppm");
        let out = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect("read_ppm failed on valid 2x1");
        assert_eq!(out.width, 2, "wrong width");
        assert_eq!(out.height, 1, "wrong height");
        assert_eq!(
            out.rgb,
            pixels.to_vec(),
            "pixel payload mismatch (tight RGB8 packing)"
        );
    }

    // Larger buffer with a trailing byte beyond the declared payload: exactly
    // width*height*3 bytes are captured and the extra byte is ignored (no
    // off-by-one at the payload boundary).
    {
        let (w, h) = (4u32, 3u32);
        let mut pixels2 = vec![0u8; (w * h * 3) as usize];
        for (i, byte) in pixels2.iter_mut().enumerate() {
            *byte = (i * 7 + 1) as u8;
        }
        let mut file = make_ppm(w, h, &pixels2);
        file.push(0xAB); // trailing byte past the payload
        let path = dir.path().join("4x3.ppm");
        std::fs::write(&path, &file).expect("setup: cannot write 4x3 ppm");
        let out = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect("read_ppm failed on valid 4x3");
        assert_eq!(out.width, w, "wrong width");
        assert_eq!(out.height, h, "wrong height");
        assert_eq!(out.rgb, pixels2, "4x3 payload/boundary mismatch");
    }
}

#[test]
fn read_ppm_header_lexing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pixels: [u8; 6] = [1, 2, 3, 4, 5, 6];

    // (a) '#'-to-EOL comments are skipped and (b) arbitrary/mixed whitespace
    // (spaces, tabs, newlines) between the magic and the three integers is
    // tolerated.
    {
        let header = "P6\n# a comment line\n\t 2 \t 1\n# another comment\n255\n";
        let mut file = header.as_bytes().to_vec();
        file.extend_from_slice(&pixels);
        let path = dir.path().join("comments.ppm");
        std::fs::write(&path, &file).expect("setup: cannot write commented ppm");
        let out = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect("comments/whitespace not tolerated");
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 1);
        assert_eq!(
            out.rgb,
            pixels.to_vec(),
            "commented header parsed to the wrong image"
        );
    }

    // (c) exactly ONE whitespace byte is consumed between maxval and the
    // pixel payload (the `pos += 1` contract); a single space separator must
    // work.
    {
        let header = "P6\n2 1\n255 "; // one space, then pixels
        let mut file = header.as_bytes().to_vec();
        file.extend_from_slice(&pixels);
        let path = dir.path().join("space-sep.ppm");
        std::fs::write(&path, &file).expect("setup: cannot write space-separator ppm");
        let out = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect("single-space separator not accepted");
        assert_eq!(out.width, 2);
        assert_eq!(out.height, 1);
        assert_eq!(
            out.rgb,
            pixels.to_vec(),
            "space-separated header parsed to the wrong image"
        );
    }

    // (d) uint overflow guard: a dimension token > 0xFFFFFFFF is malformed.
    {
        let header = "P6\n4294967296 1\n255\n"; // 2^32 width
        let mut file = header.as_bytes().to_vec();
        file.extend_from_slice(&pixels);
        let path = dir.path().join("overflow.ppm");
        std::fs::write(&path, &file).expect("setup: cannot write overflow ppm");
        let err = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect_err("overflowing dimension should be rejected");
        assert!(err.contains("malformed PPM header"), "got: {err}");
    }
}

// -----------------------------------------------------------------------------
// write_png
// -----------------------------------------------------------------------------

#[test]
fn write_png_invalid_args() {
    let dir = tempfile::tempdir().expect("temp dir");
    let px = vec![0x33u8; 2 * 2 * 4];

    // "null data" has no direct Rust equivalent for a safe &[u8] (see
    // io/image_io.rs's own doc comment: an empty slice is the mirror of the
    // C++ null-pointer guard).
    struct Case<'a> {
        label: &'static str,
        data: &'a [u8],
        width: i32,
        height: i32,
    }
    let cases = [
        Case {
            label: "null data",
            data: &[],
            width: 2,
            height: 2,
        },
        Case {
            label: "zero width",
            data: &px,
            width: 0,
            height: 2,
        },
        Case {
            label: "negative width",
            data: &px,
            width: -1,
            height: 2,
        },
        Case {
            label: "zero height",
            data: &px,
            width: 2,
            height: 0,
        },
        Case {
            label: "negative height",
            data: &px,
            width: 2,
            height: -3,
        },
    ];

    for (i, c) in cases.iter().enumerate() {
        let path = dir.path().join(format!("badarg-{i}.png"));
        let err = write_png(path.to_str().expect("utf8 path"), c.data, c.width, c.height)
            .expect_err(&format!("expected failure for case: {}", c.label));
        assert_eq!(
            err, "invalid image dimensions or data",
            "wrong error for case: {}",
            c.label
        );
        assert!(
            !path.exists(),
            "no file should be created for case: {}",
            c.label
        );
    }
}

#[test]
fn write_png_container() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (w, h) = (2i32, 2i32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (i, byte) in rgba.iter_mut().enumerate() {
        *byte = (i * 11 + 3) as u8;
    }

    let path = dir.path().join("container.png");
    write_png(path.to_str().expect("utf8 path"), &rgba, w, h).expect("write_png failed");
    let png = std::fs::read(&path).expect("cannot read back written png");

    let sig: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    assert!(
        png.len() >= 8 && png[0..8] == sig,
        "missing/incorrect 8-byte PNG signature"
    );

    let chunks = parse_png(&png).expect("PNG chunk structure did not parse");
    assert!(chunks.len() >= 3, "expected at least 3 chunks");
    assert_eq!(
        chunks.first().unwrap().chunk_type,
        "IHDR",
        "first chunk must be IHDR"
    );
    let last = chunks.last().unwrap();
    assert!(
        last.chunk_type == "IEND" && last.data.is_empty(),
        "final chunk must be a zero-length IEND"
    );

    let ihdr = chunks.first().unwrap();
    assert_eq!(ihdr.data.len(), 13, "IHDR must be 13 bytes");
    assert_eq!(read_u32_be(&ihdr.data, 0), w as u32, "IHDR width mismatch");
    assert_eq!(read_u32_be(&ihdr.data, 4), h as u32, "IHDR height mismatch");
    assert_eq!(
        (ihdr.data[8], ihdr.data[9]),
        (8, 6),
        "IHDR bit-depth/color-type must be 8/6 (RGBA)"
    );

    let idat = find_chunk(&chunks, "IDAT").expect("no IDAT chunk");
    assert!(
        idat.data.len() >= 2 && idat.data[0] == 0x78 && idat.data[1] == 0x01,
        "IDAT zlib header must be 0x78 0x01"
    );
}

#[test]
fn write_png_byte_exact() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (w, h) = (3i32, 2i32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (i, byte) in rgba.iter_mut().enumerate() {
        *byte = (i * 13 + 5) as u8;
    }

    let path = dir.path().join("exact.png");
    write_png(path.to_str().expect("utf8 path"), &rgba, w, h).expect("write_png failed");
    let png = std::fs::read(&path).expect("cannot read back written png");
    let chunks = parse_png(&png).expect("PNG did not parse");

    // (c) every chunk's stored CRC-32 matches an independent computation.
    for c in &chunks {
        assert_eq!(
            c.stored_crc, c.computed_crc,
            "CRC-32 mismatch on chunk {}",
            c.chunk_type
        );
    }

    let idat = find_chunk(&chunks, "IDAT").expect("no IDAT chunk");
    let z = parse_stored_zlib(&idat.data);
    assert!(z.ok, "IDAT zlib stored-block stream did not parse");

    // (a) the stored block payload equals the independently reconstructed
    // filtered scanlines.
    let raw = filtered_raw(&rgba, w, h);
    assert_eq!(
        z.payload, raw,
        "stored DEFLATE payload != filtered scanlines"
    );
    assert_eq!(
        z.block_count, 1,
        "small image should be a single stored block"
    );
    assert!(z.bfinal_ok, "single block must have BFINAL=1");
    assert!(
        z.lennlen_ok,
        "stored block LEN/NLEN are not one's-complement"
    );

    // (b) trailing big-endian Adler-32 matches adler32(raw).
    assert_eq!(z.adler, test_adler32(&raw), "Adler-32 checksum mismatch");
}

#[test]
fn write_png_multi_block() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Filtered raw = height*(1 + width*4) must exceed 0xFFFF to force >1
    // stored DEFLATE block. 200 * (1 + 400) = 80200 bytes => two blocks.
    let (w, h) = (100i32, 200i32);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for (i, byte) in rgba.iter_mut().enumerate() {
        *byte = ((i * 31 + 17) & 0xFF) as u8;
    }

    let path = dir.path().join("multiblock.png");
    write_png(path.to_str().expect("utf8 path"), &rgba, w, h).expect("write_png failed");
    let png = std::fs::read(&path).expect("cannot read back written png");
    let chunks = parse_png(&png).expect("PNG did not parse");
    let idat = find_chunk(&chunks, "IDAT").expect("no IDAT chunk");
    let z = parse_stored_zlib(&idat.data);
    assert!(z.ok, "IDAT zlib stored-block stream did not parse");
    assert!(
        z.block_count > 1,
        "expected more than one stored block, got {}",
        z.block_count
    );
    assert!(z.bfinal_ok, "only the final stored block may set BFINAL=1");
    assert!(
        z.lennlen_ok,
        "each block's LEN/NLEN must be one's-complement"
    );

    let raw = filtered_raw(&rgba, w, h);
    assert_eq!(
        z.payload, raw,
        "reassembled multi-block payload != filtered scanlines"
    );
    assert_eq!(
        z.adler,
        test_adler32(&raw),
        "Adler-32 checksum mismatch across blocks"
    );
}

#[test]
fn write_png_unwritable_path() {
    let base = tempfile::tempdir().expect("temp dir");
    // A path under a directory that was never created -> the file open fails.
    let path = base.path().join("no_such_subdir").join("x.png");
    let rgba = vec![0x40u8; 2 * 2 * 4];

    let err = write_png(path.to_str().expect("utf8 path"), &rgba, 2, 2)
        .expect_err("write_png should fail into a non-existent directory");
    assert!(
        err.contains("cannot open") && err.contains("for writing"),
        "expected \"cannot open <path> for writing\", got \"{err}\""
    );
    assert!(!path.exists(), "no file should be produced on open failure");
}

// -----------------------------------------------------------------------------
// `rerank --top-n` usage-error exit-code contract.
//
// In explore-main/tests/test_wally_unit.cpp both cases are wrapped in
// `#if !WALLY_LLM_ONLY_CUT` with the comment: "register_rerank() is also
// commented out in src/app.cpp, so `wally rerank ...` is an unrecognized
// subcommand that fails with ExtrasError -> exit 2 before `--top-n`'s own
// validation ever runs... Excluded from the suite rather than left to pass
// for the wrong reason."
//
// The same trap applies here: `register_rerank` is not wired into the shared
// app (LLM-only release, see src/app.rs's configure_app), and driving it via
// common::run_in_process would exercise nothing but that same ExtrasError
// path once the CLI parser lands — a spurious pass, not a test of `--top-n`.
// Rather than reproduce that, these register `rerank` directly on a
// standalone App (the same technique segment_option_spec in
// tests/test_wally_segment.rs uses) and assert against the actual `--top-n`
// Range validator cmd_rerank.rs wires up (`Validator::Range(1, i32::MAX)`),
// checking that the specific value this test names falls outside it — real
// coverage of the mechanism that produces exit 2, without depending on the
// not-yet-implemented parser/run path.
// -----------------------------------------------------------------------------

fn assert_top_n_value_rejected(value: i64) {
    let mut app = App::new(
        "RunAnywhere on-device AI CLI — run, manage and serve local models",
        "wally",
    );
    wally::commands::register_rerank(&mut app);
    let rerank = app
        .get_subcommand("rerank")
        .expect("rerank subcommand not registered by register_rerank");
    let top_n = rerank
        .get_option("--top-n")
        .expect("'--top-n' option missing");
    let (min, max) = top_n
        .validators
        .iter()
        .find_map(|v| match v {
            Validator::Range(min, max) => Some((*min, *max)),
            _ => None,
        })
        .expect("'--top-n' must carry a Range validator");
    assert!(
        value < min || value > max,
        "value {value} should be rejected by the --top-n range [{min}, {max}]"
    );
}

#[test]
fn rerank_top_n_zero_exit2() {
    // `rerank --top-n 0` should be a usage error, not every document back.
    assert_top_n_value_rejected(0);
}

#[test]
fn rerank_top_n_negative_exit2() {
    // `rerank --top-n -1` should be a usage error.
    assert_top_n_value_rejected(-1);
}
