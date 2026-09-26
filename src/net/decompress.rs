//! Upstream response body decompression, mirroring cpp-httplib's
//! `create_decompressor` (httplib.h `create_decompressor` /
//! `gzip_decompressor` / `brotli_decompressor` / `zstd_decompressor`):
//! `http1::Client` advertises `Accept-Encoding: br, gzip, deflate, zstd`
//! (`Client::DEFAULT_ACCEPT_ENCODING`), so a real server is free to answer
//! with any of the four, and previously nothing decoded them -- a compressed
//! reply just failed downstream (JSON parse error -> 502) instead of being
//! read.
//!
//! httplib resolves `Content-Encoding: gzip` and `Content-Encoding: deflate`
//! to the *same* decompressor: a zlib `inflateInit2` call with
//! `windowBits = 32 + 15`, which auto-detects a gzip or zlib header from the
//! bytes themselves rather than trusting which of the two names the
//! `Content-Encoding` header used (httplib.h:6972 `create_decompressor`,
//! httplib.h:6772 `gzip_decompressor::gzip_decompressor`'s comment on the
//! `32 +` offset). `Decoder::GzipOrZlibPending` below does the same --
//! decided from the first two bytes of the body, not the header's spelling.
//!
//! A `Content-Encoding` httplib does not recognise (including none at all)
//! is passed through unchanged: on the *client*, an unrecognised encoding
//! never becomes an error, it just is not decompressed
//! (`ClientImpl::StreamHandle::read()`, httplib.h:12978 -- a null
//! `decompressor_` reads the raw body). That is different from the
//! *server*-side `prepare_content_receiver` (httplib.h:7271), which answers
//! 415 for an unrecognised `Content-Encoding` on a *request* body; this
//! module is only ever used on the client side reading a response, so that
//! codepath does not apply here.

use std::io::{self, Write};

use brotli_decompressor::writer::DecompressorWriter as BrotliWriter;
use flate2::write::{GzDecoder, ZlibDecoder};
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::FrameDecoder as ZstdFrameDecoder;

/// Matches `CPPHTTPLIB_COMPRESSION_BUFSIZ` (httplib.h:158): the scratch
/// buffer size brotli decodes into per call, and the size of the output
/// buffer `Zstd::push` drains its decoder through. Not load-bearing for
/// correctness (both loop until drained either way), just parity.
const COMPRESSION_BUFSIZ: usize = 16384;

/// Bytes decoded out of a `Decoder` land here. Returns `false` to cancel the
/// transfer, the same contract `http1::read_chunked` / `read_exact_len` /
/// `read_until_close`'s own sink closures already use.
pub type Sink<'a> = dyn FnMut(&[u8]) -> bool + 'a;

/// One response body's decompression state, built once from its
/// `Content-Encoding` header and fed the raw, already de-chunked /
/// length-delimited wire bytes as they arrive. Streams: decoded bytes reach
/// `sink` as soon as a `push` call produces them, not buffered until the
/// body ends, so a compressed SSE reply keeps streaming through decoding.
pub enum Decoder {
    /// No `Content-Encoding`, or one this module does not decode.
    Identity,
    /// `gzip` or `deflate`, buffered until there are enough bytes (2) to
    /// tell a gzip stream from a zlib-wrapped one by its magic bytes.
    GzipOrZlibPending(Vec<u8>),
    Gzip(Box<GzDecoder<Vec<u8>>>),
    Zlib(Box<ZlibDecoder<Vec<u8>>>),
    Brotli(Box<BrotliWriter<Vec<u8>>>),
    Zstd(Box<Zstd>),
}

impl Decoder {
    /// Picks a decoder the way httplib's `create_decompressor` does: `gzip`
    /// or `deflate` by exact value, `br` and `zstd` merely as substrings
    /// (matching e.g. a `q`-weighted `"br;q=1"` a literal server might
    /// send), anything else -- including no header at all -- left
    /// undecoded.
    pub fn for_content_encoding(value: Option<&str>) -> Decoder {
        let value = match value {
            Some(v) if !v.is_empty() => v,
            _ => return Decoder::Identity,
        };
        if value == "gzip" || value == "deflate" {
            Decoder::GzipOrZlibPending(Vec::new())
        } else if value.contains("br") {
            Decoder::Brotli(Box::new(BrotliWriter::new(Vec::new(), COMPRESSION_BUFSIZ)))
        } else if value == "zstd" || value.contains("zstd") {
            Decoder::Zstd(Box::new(Zstd::new()))
        } else {
            Decoder::Identity
        }
    }

    /// Feeds `data` through this decoder. Returns `Ok(false)` if `sink`
    /// canceled (never call `push` again on this decoder after that); `Err`
    /// if the compressed bytes themselves are invalid -- the same failure
    /// httplib's `decompressor->decompress()` returning `false` produces,
    /// which `http1::Client::send` maps to `Error::Read` exactly like any
    /// other body-read failure.
    pub fn push(&mut self, data: &[u8], sink: &mut Sink<'_>) -> io::Result<bool> {
        if data.is_empty() {
            return Ok(true);
        }
        match self {
            Decoder::Identity => Ok(sink(data)),
            Decoder::GzipOrZlibPending(buf) => {
                buf.extend_from_slice(data);
                if buf.len() < 2 {
                    return Ok(true);
                }
                let buffered = std::mem::take(buf);
                let mut resolved = if buffered[0] == 0x1f && buffered[1] == 0x8b {
                    Decoder::Gzip(Box::new(GzDecoder::new(Vec::new())))
                } else {
                    Decoder::Zlib(Box::new(ZlibDecoder::new(Vec::new())))
                };
                let ok = resolved.push(&buffered, sink)?;
                *self = resolved;
                Ok(ok)
            }
            Decoder::Gzip(d) => write_and_drain(d.as_mut(), data, sink),
            Decoder::Zlib(d) => write_and_drain(d.as_mut(), data, sink),
            Decoder::Brotli(d) => write_and_drain(d.as_mut(), data, sink),
            Decoder::Zstd(z) => z.push(data, sink),
        }
    }

    /// Finalizes decoding once the last `push()` for a body has happened,
    /// rejecting a stream that stopped short of a real end-of-body marker.
    /// `push()` alone never notices this: it only reports an error for bytes
    /// that are structurally invalid, and a body that simply stops -- a
    /// dropped connection mid-transfer on a `read_until_close` reply, for
    /// example -- looks identical to one that ended cleanly until something
    /// checks for the trailer each format ends with.
    pub fn finish(&mut self, sink: &mut Sink<'_>) -> io::Result<()> {
        match self {
            Decoder::Identity => Ok(()),
            Decoder::GzipOrZlibPending(buf) => {
                if buf.is_empty() {
                    Ok(())
                } else {
                    // Never received the 2 bytes needed to even tell gzip
                    // and zlib framing apart, so this is definitely
                    // truncated, not just ambiguous.
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated gzip/deflate body (fewer than 2 bytes received)",
                    ))
                }
            }
            Decoder::Gzip(d) => {
                let result = d.try_finish();
                drain_finished_output(d.as_mut(), sink);
                result
            }
            // flate2's ZlibDecoder has no CRC/ISIZE trailer the way
            // GzDecoder does, so try_finish() cannot reliably distinguish a
            // truncated deflate/zlib stream from a complete one; leave this
            // variant's truncation undetected here rather than fail closed
            // on legitimate streams. See the module docs on the gzip/zlib
            // auto-detect.
            Decoder::Zlib(_) => Ok(()),
            Decoder::Brotli(d) => {
                let result = d.close();
                drain_finished_output(d.as_mut(), sink);
                result
            }
            Decoder::Zstd(z) => {
                if z.decoder.is_finished() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated zstd stream",
                    ))
                }
            }
        }
    }
}

/// Drains whatever a finalizing call (`try_finish()` / `close()`) flushed
/// into `decoder`'s owned output buffer, same as `write_and_drain` does for
/// an ordinary `push()`. Called regardless of whether the finalize call
/// succeeded: a truncated stream can still have legitimate trailing bytes
/// worth delivering before the caller sees the error.
fn drain_finished_output<D: DecodingWriter>(decoder: &mut D, sink: &mut Sink<'_>) {
    let out = std::mem::take(decoder.buffered_output());
    if !out.is_empty() {
        sink(&out);
    }
}

/// A decoding `Write` sink that buffers its output in an owned `Vec<u8>`
/// instead of a caller-supplied stream: `flate2`'s and
/// `brotli_decompressor`'s streaming decoders are push-based (`Write`
/// implementations, not `Read`), which is what makes incremental decode
/// possible at all here, but they need to *own* their output writer for the
/// decoder's whole lifetime -- incompatible with `Sink<'_>`'s per-call
/// borrow. Routing through an owned `Vec<u8>` that gets drained after every
/// `push` bridges the two without unsafe code or a second thread.
trait DecodingWriter: Write {
    fn buffered_output(&mut self) -> &mut Vec<u8>;
}

impl DecodingWriter for GzDecoder<Vec<u8>> {
    fn buffered_output(&mut self) -> &mut Vec<u8> {
        self.get_mut()
    }
}

impl DecodingWriter for ZlibDecoder<Vec<u8>> {
    fn buffered_output(&mut self) -> &mut Vec<u8> {
        self.get_mut()
    }
}

impl DecodingWriter for BrotliWriter<Vec<u8>> {
    fn buffered_output(&mut self) -> &mut Vec<u8> {
        self.get_mut()
    }
}

fn write_and_drain<D: DecodingWriter>(
    decoder: &mut D,
    data: &[u8],
    sink: &mut Sink<'_>,
) -> io::Result<bool> {
    decoder.write_all(data)?;
    // flate2's write decoders (`GzDecoder`/`ZlibDecoder`, the two concrete
    // `D`s this is ever called with) decompress into their own internal
    // scratch buffer during `write()`/`write_all()`, and only move that into
    // the writer given to `new()` -- our `Vec<u8>`, reachable through
    // `get_mut()` -- on the *next* write call's own internal flush, or on an
    // explicit `flush()`/`finish()` (flate2's `zio::Writer::write_with_status`
    // dumps at the *start* of a write, not after one; see `zio::Writer::dump`
    // and `::flush`). Without calling `flush()` here, a body delivered in
    // one `push` -- the common case for anything but a byte-at-a-time SSE
    // stream -- would decompress correctly internally but never reach
    // `sink` at all.
    decoder.flush()?;
    let out = std::mem::take(decoder.buffered_output());
    if out.is_empty() {
        return Ok(true);
    }
    Ok(sink(&out))
}

/// zstd decompression state: `ruzstd`'s `FrameDecoder` is pull-based
/// (`decode_from_to(source, target)`, "if it returns 0 bytes read, the
/// source did not contain a full block; call again with more input"), so
/// `pending` carries whatever tail of the current push did not yet make up
/// a full block, to be retried once the next push extends it.
pub struct Zstd {
    decoder: ZstdFrameDecoder,
    pending: Vec<u8>,
}

impl Zstd {
    fn new() -> Zstd {
        Zstd {
            decoder: ZstdFrameDecoder::new(),
            pending: Vec::new(),
        }
    }

    fn push(&mut self, data: &[u8], sink: &mut Sink<'_>) -> io::Result<bool> {
        self.pending.extend_from_slice(data);
        let mut out = [0u8; COMPRESSION_BUFSIZ];
        loop {
            let (consumed, written) = match self.decoder.decode_from_to(&self.pending, &mut out) {
                Ok(v) => v,
                Err(e) if frame_header_is_incomplete(&e) => {
                    // `decode_from_to` only reads the frame header (magic
                    // number, descriptor, window descriptor, ...) the very
                    // first time it is called, via a plain `Read` over
                    // `self.pending`; a slice that ends mid-header is
                    // reported the same way a truly corrupt one would be
                    // (this crate has no "not enough bytes yet" return),
                    // except through these specific
                    // `ReadFrameHeaderError::*ReadError` variants, which --
                    // unlike `BadMagicNumber` / `InvalidFrameDescriptor` --
                    // only ever fire on a short read. Wait for the next push
                    // to extend `pending` rather than failing a stream that
                    // has simply not delivered its header yet (a push-based
                    // sink, unlike a byte-by-byte SSE body, can easily hand
                    // this decoder a single byte at a time).
                    break;
                }
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
            };
            if written > 0 && !sink(&out[..written]) {
                return Ok(false);
            }
            if consumed > self.pending.len() {
                // ruzstd 0.9's trailing-content-checksum shortcut reports a
                // fixed `consumed = 4` as soon as the frame's last block has
                // been decoded, even when fewer than 4 bytes of that
                // checksum have actually been pushed yet (it only *reads*
                // the 4 bytes, and only updates its own state, when
                // `pending.len() >= 4`; the reported `consumed` does not
                // depend on that check passing, and on the next call it
                // looks for all 4 bytes at the *start* of whatever slice
                // `pending` is then, not a byte offset it remembers). A
                // push-based caller can easily be mid-checksum here (a
                // byte-at-a-time SSE body in particular); leave `pending`
                // untouched -- draining the bytes that are there now would
                // permanently lose them, so a checksum split across pushes
                // would never be complete enough to read, let alone verify --
                // and wait for the next push to extend it to the full 4
                // bytes instead.
                break;
            }
            if consumed > 0 {
                self.pending.drain(..consumed);
            }
            if let (Some(from_trailer), Some(calculated)) = (
                self.decoder.get_checksum_from_data(),
                self.decoder.get_calculated_checksum(),
            ) {
                // `decode_from_to` only records the trailer's checksum and
                // the one it calculated while decoding; it never compares
                // them itself (see ruzstd's own `decode_corpus` test, which
                // does exactly this comparison). httplib's
                // `ZSTD_decompressStream` does this check internally and
                // fails the read on a mismatch -- match that once the
                // trailer is complete enough for both values to exist.
                if from_trailer != calculated {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "zstd content checksum mismatch",
                    ));
                }
            }
            if consumed == 0 && written == 0 {
                // No new block decoded and nothing left buffered inside the
                // decoder to drain -- wait for the next push to bring more
                // input; calling again with the same `pending` would not
                // make progress (see the doc comment on `decode_from_to`).
                break;
            }
        }
        Ok(true)
    }
}

/// Whether `err` came from `decode_from_to`'s frame-header read simply
/// running out of bytes (`std::io::Read` on a slice returns short/`Ok(0)`
/// once the slice is exhausted, which these variants wrap) rather than the
/// header bytes present being structurally wrong. See the call site in
/// `Zstd::push`.
fn frame_header_is_incomplete(err: &FrameDecoderError) -> bool {
    matches!(
        err,
        FrameDecoderError::ReadFrameHeaderError(
            ReadFrameHeaderError::MagicNumberReadError(_)
                | ReadFrameHeaderError::FrameDescriptorReadError(_)
                | ReadFrameHeaderError::WindowDescriptorReadError(_)
                | ReadFrameHeaderError::DictionaryIdReadError(_)
                | ReadFrameHeaderError::FrameContentSizeReadError(_)
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(mut decoder: Decoder, chunks: &[&[u8]]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut sink: Box<Sink<'_>> = Box::new(|data: &[u8]| -> bool {
                out.extend_from_slice(data);
                true
            });
            for chunk in chunks {
                decoder.push(chunk, &mut *sink)?;
            }
        }
        Ok(out)
    }

    const PLAINTEXT: &[u8] = b"hello from the shim decompression test\n";

    // Real gzip bytes (`gzip -c`), not this crate's own encoder, so a
    // roundtrip bug in this module can't hide behind a symmetric bug in the
    // same code producing the fixture.
    const GZIP_FIXTURE: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x08, 0x4b, 0x42, 0xb5, 0x6a, 0x00, 0x03, 0x64, 0x65, 0x63, 0x6f, 0x6d,
        0x70, 0x2d, 0x66, 0x69, 0x78, 0x74, 0x75, 0x72, 0x65, 0x2e, 0x74, 0x78, 0x74, 0x00, 0xcb,
        0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x48, 0x2b, 0xca, 0xcf, 0x55, 0x28, 0xc9, 0x48, 0x55, 0x28,
        0xce, 0xc8, 0xcc, 0x55, 0x48, 0x49, 0x4d, 0xce, 0xcf, 0x2d, 0x28, 0x4a, 0x2d, 0x2e, 0xce,
        0xcc, 0xcf, 0x53, 0x28, 0x49, 0x2d, 0x2e, 0xe1, 0x02, 0x00, 0xb9, 0x92, 0x41, 0x58, 0x27,
        0x00, 0x00, 0x00,
    ];

    // Same plaintext, zlib-wrapped (`python3 -c "import zlib;
    // ...zlib.compress(...)"`) instead of gzip's own framing -- proves the
    // gzip/zlib auto-detect, not just gzip.
    const ZLIB_FIXTURE: &[u8] = &[
        0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x48, 0x2b, 0xca, 0xcf, 0x55, 0x28, 0xc9,
        0x48, 0x55, 0x28, 0xce, 0xc8, 0xcc, 0x55, 0x48, 0x49, 0x4d, 0xce, 0xcf, 0x2d, 0x28, 0x4a,
        0x2d, 0x2e, 0xce, 0xcc, 0xcf, 0x53, 0x28, 0x49, 0x2d, 0x2e, 0xe1, 0x02, 0x00, 0x25, 0xb2,
        0x0e, 0xa0,
    ];

    // Real brotli bytes (`brotli -c`).
    const BROTLI_FIXTURE: &[u8] = &[
        0xa1, 0x30, 0x01, 0xc0, 0xef, 0x48, 0x9d, 0xfa, 0xe4, 0xe1, 0x92, 0xac, 0x6d, 0xae, 0xca,
        0xd0, 0x12, 0x44, 0x21, 0xad, 0x07, 0x39, 0x44, 0x11, 0x78, 0xf4, 0x28, 0xb7, 0xf0, 0x72,
        0x0e, 0x76, 0xe4, 0xff, 0x21, 0x89, 0x2b,
    ];

    // Real zstd bytes (`zstd -c`).
    const ZSTD_FIXTURE: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x27, 0x39, 0x01, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20,
        0x66, 0x72, 0x6f, 0x6d, 0x20, 0x74, 0x68, 0x65, 0x20, 0x73, 0x68, 0x69, 0x6d, 0x20, 0x64,
        0x65, 0x63, 0x6f, 0x6d, 0x70, 0x72, 0x65, 0x73, 0x73, 0x69, 0x6f, 0x6e, 0x20, 0x74, 0x65,
        0x73, 0x74, 0x0a, 0x94, 0xe3, 0x2b, 0xe5,
    ];

    #[test]
    fn no_content_encoding_passes_bytes_through_unchanged() {
        let decoder = Decoder::for_content_encoding(None);
        assert!(matches!(decoder, Decoder::Identity));
        let out = decode_all(decoder, &[b"raw, not compressed"]).unwrap();
        assert_eq!(out, b"raw, not compressed");
    }

    #[test]
    fn an_encoding_httplib_does_not_recognise_is_not_an_error_and_is_not_decoded() {
        // Matches httplib's client-side behaviour: an unrecognised
        // Content-Encoding leaves `decompressor_` null and the raw body is
        // read as-is (httplib.h:12970-12981) -- unlike the server-side
        // request-body path, which 415s.
        let decoder = Decoder::for_content_encoding(Some("compress"));
        assert!(matches!(decoder, Decoder::Identity));
    }

    #[test]
    fn gzip_content_encoding_decodes_real_gzip_bytes() {
        let decoder = Decoder::for_content_encoding(Some("gzip"));
        let out = decode_all(decoder, &[GZIP_FIXTURE]).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn deflate_content_encoding_decodes_a_zlib_wrapped_stream_by_its_magic_bytes() {
        // httplib's `create_decompressor` sends both "gzip" and "deflate"
        // through the *same* auto-detecting decompressor, so a server that
        // calls its zlib-format output "deflate" (common in practice) must
        // still decode, keyed off the bytes, not the header's spelling.
        let decoder = Decoder::for_content_encoding(Some("deflate"));
        let out = decode_all(decoder, &[ZLIB_FIXTURE]).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn gzip_content_encoding_also_decodes_a_zlib_wrapped_stream() {
        // And the reverse: "gzip" plus zlib-shaped bytes, since the
        // decompressor is chosen from the bytes, not the label.
        let decoder = Decoder::for_content_encoding(Some("gzip"));
        let out = decode_all(decoder, &[ZLIB_FIXTURE]).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn br_content_encoding_decodes_real_brotli_bytes() {
        let decoder = Decoder::for_content_encoding(Some("br"));
        let out = decode_all(decoder, &[BROTLI_FIXTURE]).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn zstd_content_encoding_decodes_real_zstd_bytes() {
        let decoder = Decoder::for_content_encoding(Some("zstd"));
        let out = decode_all(decoder, &[ZSTD_FIXTURE]).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn a_gzip_body_split_across_many_small_pushes_still_decodes() {
        // The wire delivers a response body as whatever chunks the socket
        // read produced, not necessarily one push per call -- streaming
        // (SSE) framing in particular can hand this decoder a handful of
        // bytes at a time. Feed the fixture one byte at a time.
        let decoder = Decoder::for_content_encoding(Some("gzip"));
        let byte_chunks: Vec<&[u8]> = GZIP_FIXTURE.iter().map(std::slice::from_ref).collect();
        let out = decode_all(decoder, &byte_chunks).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn a_zstd_body_split_across_many_small_pushes_still_decodes() {
        let decoder = Decoder::for_content_encoding(Some("zstd"));
        let byte_chunks: Vec<&[u8]> = ZSTD_FIXTURE.iter().map(std::slice::from_ref).collect();
        let out = decode_all(decoder, &byte_chunks).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn a_brotli_body_split_across_many_small_pushes_still_decodes() {
        let decoder = Decoder::for_content_encoding(Some("br"));
        let byte_chunks: Vec<&[u8]> = BROTLI_FIXTURE.iter().map(std::slice::from_ref).collect();
        let out = decode_all(decoder, &byte_chunks).unwrap();
        assert_eq!(out, PLAINTEXT);
    }

    #[test]
    fn corrupt_gzip_body_fails_instead_of_silently_returning_garbage() {
        let mut decoder = Decoder::for_content_encoding(Some("gzip"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        // Real gzip magic bytes followed by nonsense: is_valid() but the
        // deflate stream itself is not decodable, matching httplib's
        // Z_DATA_ERROR -> `decompress()` returns false ->
        // `http1::Error::Read` path (not a 415 or a silently-empty body).
        let corrupt = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff,
        ];
        let result = decoder.push(&corrupt, &mut *sink);
        assert!(
            result.is_err(),
            "expected corrupt gzip-labeled bytes to fail decompression, got {result:?}"
        );
    }

    #[test]
    fn corrupt_brotli_body_fails_instead_of_silently_returning_garbage() {
        let mut decoder = Decoder::for_content_encoding(Some("br"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        let corrupt = [0xffu8; 16];
        let result = decoder.push(&corrupt, &mut *sink);
        assert!(
            result.is_err(),
            "expected garbage labeled as brotli to fail decompression, got {result:?}"
        );
    }

    #[test]
    fn corrupt_zstd_body_fails_instead_of_silently_returning_garbage() {
        let mut decoder = Decoder::for_content_encoding(Some("zstd"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        // A real zstd frame header (so the decoder gets past "not enough
        // bytes yet to parse the header", which is legitimately ambiguous
        // with a truncated-but-valid stream and is therefore tolerated, not
        // an error) followed by a block header whose 2-bit block_type field
        // is 3 -- "Reserved", not a valid value per the zstd spec -- which
        // is unambiguous corruption.
        let corrupt = [
            0x28, 0xb5, 0x2f, 0xfd, 0x24, 0x27, 0x3f, 0x01, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            0x20, 0x66, 0x72, 0x6f, 0x6d, 0x20, 0x74, 0x68, 0x65, 0x20, 0x73, 0x68, 0x69, 0x6d,
            0x20, 0x64, 0x65, 0x63, 0x6f, 0x6d, 0x70, 0x72, 0x65, 0x73, 0x73, 0x69, 0x6f, 0x6e,
            0x20, 0x74, 0x65, 0x73, 0x74, 0x0a, 0x94, 0xe3, 0x2b, 0xe5,
        ];
        let result = decoder.push(&corrupt, &mut *sink);
        assert!(
            result.is_err(),
            "expected garbage labeled as zstd to fail decompression, got {result:?}"
        );
    }

    #[test]
    fn a_zstd_trailer_split_byte_by_byte_still_decodes_and_verifies() {
        // The frame's last 4 bytes are its content checksum. httplib hands
        // ZSTD_decompressStream every byte as it arrives and it still
        // verifies a checksum split across many small pushes; prove the same
        // here by delivering the body in one push and the trailer one byte
        // at a time.
        let mut decoder = Decoder::for_content_encoding(Some("zstd"));
        let mut out = Vec::new();
        {
            let mut sink: Box<Sink<'_>> = Box::new(|data: &[u8]| -> bool {
                out.extend_from_slice(data);
                true
            });
            let (body, trailer) = ZSTD_FIXTURE.split_at(ZSTD_FIXTURE.len() - 4);
            decoder.push(body, &mut *sink).unwrap();
            for byte in trailer {
                decoder
                    .push(std::slice::from_ref(byte), &mut *sink)
                    .unwrap();
            }
        }
        assert_eq!(out, PLAINTEXT);
    }

    fn zstd_with_corrupted_checksum() -> Vec<u8> {
        let mut corrupt = ZSTD_FIXTURE.to_vec();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xff;
        corrupt
    }

    #[test]
    fn a_corrupted_zstd_checksum_fails_the_read_when_delivered_whole() {
        let mut decoder = Decoder::for_content_encoding(Some("zstd"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        let corrupt = zstd_with_corrupted_checksum();
        let result = decoder.push(&corrupt, &mut *sink);
        assert!(
            result.is_err(),
            "expected a corrupted zstd checksum to fail decompression, got {result:?}"
        );
    }

    #[test]
    fn a_corrupted_zstd_checksum_fails_the_read_when_split_across_pushes() {
        let mut decoder = Decoder::for_content_encoding(Some("zstd"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        let corrupt = zstd_with_corrupted_checksum();
        let (body, trailer) = corrupt.split_at(corrupt.len() - 4);
        decoder.push(body, &mut *sink).unwrap();
        let mut result = Ok(true);
        for byte in trailer {
            result = decoder.push(std::slice::from_ref(byte), &mut *sink);
            if result.is_err() {
                break;
            }
        }
        assert!(
            result.is_err(),
            "expected a corrupted zstd checksum split across pushes to fail decompression, got {result:?}"
        );
    }

    #[test]
    fn a_canceling_sink_stops_gzip_decoding() {
        let mut decoder = Decoder::for_content_encoding(Some("gzip"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { false });
        let ok = decoder.push(GZIP_FIXTURE, &mut *sink).unwrap();
        assert!(!ok, "a canceling sink must be reported back to the caller");
    }

    #[test]
    fn truncated_gzip_body_fails_finish_instead_of_silently_succeeding() {
        // The deflate stream itself decodes fine without its trailer, so
        // push() alone never notices a body a server (or a dropped
        // connection) cut short right before the CRC32/ISIZE trailer.
        let mut decoder = Decoder::for_content_encoding(Some("gzip"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        let truncated = &GZIP_FIXTURE[..GZIP_FIXTURE.len() - 8];
        decoder.push(truncated, &mut *sink).unwrap();
        let result = decoder.finish(&mut *sink);
        assert!(
            result.is_err(),
            "expected a gzip body missing its trailer to fail finish(), got {result:?}"
        );
    }

    #[test]
    fn truncated_brotli_body_fails_finish_instead_of_silently_succeeding() {
        let mut decoder = Decoder::for_content_encoding(Some("br"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        let truncated = &BROTLI_FIXTURE[..BROTLI_FIXTURE.len() - 3];
        decoder.push(truncated, &mut *sink).unwrap();
        let result = decoder.finish(&mut *sink);
        assert!(
            result.is_err(),
            "expected a brotli body missing its final bytes to fail finish(), got {result:?}"
        );
    }

    #[test]
    fn truncated_zstd_body_fails_finish_instead_of_silently_succeeding() {
        // Drops the trailing 4-byte content checksum entirely, unlike
        // `a_zstd_trailer_split_byte_by_byte_still_decodes_and_verifies`
        // above, which always eventually delivers it.
        let mut decoder = Decoder::for_content_encoding(Some("zstd"));
        let mut sink: Box<Sink<'_>> = Box::new(|_: &[u8]| -> bool { true });
        let truncated = &ZSTD_FIXTURE[..ZSTD_FIXTURE.len() - 4];
        decoder.push(truncated, &mut *sink).unwrap();
        let result = decoder.finish(&mut *sink);
        assert!(
            result.is_err(),
            "expected a zstd body missing its checksum trailer to fail finish(), got {result:?}"
        );
    }

    #[test]
    fn a_fully_delivered_body_still_finishes_ok() {
        // Control case: finish() must not spuriously fail a complete
        // stream for any of the three formats it actually validates.
        for (label, fixture) in [
            ("gzip", GZIP_FIXTURE),
            ("br", BROTLI_FIXTURE),
            ("zstd", ZSTD_FIXTURE),
        ] {
            let mut decoder = Decoder::for_content_encoding(Some(label));
            let mut out = Vec::new();
            {
                let mut sink: Box<Sink<'_>> = Box::new(|data: &[u8]| -> bool {
                    out.extend_from_slice(data);
                    true
                });
                decoder.push(fixture, &mut *sink).unwrap();
                decoder.finish(&mut *sink).unwrap_or_else(|e| {
                    panic!("finish() on a complete {label} body failed: {e}")
                });
            }
            assert_eq!(out, PLAINTEXT, "{label} payload mismatch");
        }
    }
}
