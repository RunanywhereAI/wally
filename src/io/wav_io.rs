//! WAV read/write for the audio modality commands (port of src/io/wav_io.cpp).
//! Owner: the dormant audio modalities port.
//!
//! PCM conversion, linear resampling and RIFF/WAV container synthesis live in
//! commons (`rac_audio_*`); this module only does path-based read/write plus
//! thin wrappers that forward to those primitives. `read_wav` parses the RIFF
//! container itself (mirroring the C++, which does not route decoding through
//! the kit either).

use std::io::Write;

use crate::sys;

/// Decoded PCM: mono (channels collapsed by averaging) 16-bit samples.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WavData {
    pub samples: Vec<i16>,
    pub sample_rate: i32,
}

fn write_bytes(path: &str, data: &[u8]) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|_| format!("cannot create {path}"))?;
    if data.is_empty() || file.write_all(data).is_ok() {
        Ok(())
    } else {
        Err(format!("failed writing {path}"))
    }
}

/// Read a RIFF/WAVE file (16-bit PCM only). Mirrors the C++ chunk scanner:
/// an unsupported format sets a specific error; a truncated/missing chunk
/// falls back to the generic "missing fmt/data chunks" message.
pub fn read_wav(path: &str) -> Result<WavData, String> {
    let bytes = std::fs::read(path).map_err(|_| format!("cannot open {path}"))?;

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{path} is not a RIFF/WAVE file"));
    }

    let mut offset = 12usize;
    let mut have_fmt = false;
    let mut have_data = false;
    let mut channels: u16 = 0;
    let mut rate: u32 = 0;
    let mut data_start = 0usize;
    let mut data_end = 0usize;
    let mut error = String::new();

    while !have_fmt || !have_data {
        if offset + 8 > bytes.len() {
            break;
        }
        let chunk_id = &bytes[offset..offset + 4];
        let chunk_size =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap_or_default())
                as usize;
        offset += 8;

        if chunk_id == b"fmt " {
            if chunk_size < 16 || offset + chunk_size > bytes.len() {
                break;
            }
            let fmt = &bytes[offset..offset + chunk_size];
            let format = u16::from_le_bytes(fmt[0..2].try_into().unwrap_or_default());
            channels = u16::from_le_bytes(fmt[2..4].try_into().unwrap_or_default());
            rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap_or_default());
            let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap_or_default());
            offset += chunk_size;
            if format != 1 || bits != 16 || channels == 0 {
                error = "only 16-bit PCM WAV is supported".to_string();
                have_fmt = false;
                break;
            }
            have_fmt = true;
        } else if chunk_id == b"data" {
            if offset + chunk_size > bytes.len() {
                break;
            }
            data_start = offset;
            data_end = offset + chunk_size;
            offset += chunk_size;
            have_data = true;
        } else {
            let skip = chunk_size + (chunk_size & 1);
            if offset + skip > bytes.len() {
                break;
            }
            offset += skip;
        }
    }

    if !have_fmt || !have_data {
        if error.is_empty() {
            error = format!("{path} is missing fmt/data chunks");
        }
        return Err(error);
    }

    let data = &bytes[data_start..data_end];
    let channels = channels as usize;
    let frame_count = data.len() / (2 * channels);
    let mut samples = vec![0i16; frame_count];
    if channels == 1 {
        for (i, sample) in samples.iter_mut().enumerate() {
            let o = i * 2;
            *sample = i16::from_le_bytes([data[o], data[o + 1]]);
        }
    } else {
        for (i, sample) in samples.iter_mut().enumerate() {
            let mut acc: i32 = 0;
            for c in 0..channels {
                let o = (i * channels + c) * 2;
                acc += i16::from_le_bytes([data[o], data[o + 1]]) as i32;
            }
            *sample = (acc / channels as i32) as i16;
        }
    }

    Ok(WavData {
        samples,
        sample_rate: rate as i32,
    })
}

/// Write mono 16-bit PCM samples as a WAV file via rac_audio_int16_to_wav.
pub fn write_wav(path: &str, samples: &[i16], sample_rate: i32) -> Result<(), String> {
    if samples.is_empty() || sample_rate <= 0 {
        return Err(format!("invalid PCM16 input for {path}"));
    }

    let mut wav_data: *mut std::os::raw::c_void = std::ptr::null_mut();
    let mut wav_size: usize = 0;
    // SAFETY: `samples` is a valid non-empty slice kept alive for the call;
    // wav_data/wav_size are valid out-params on the stack.
    let rc = unsafe {
        sys::rac_audio_int16_to_wav(
            samples.as_ptr() as *const std::os::raw::c_void,
            samples.len() * std::mem::size_of::<i16>(),
            sample_rate,
            &mut wav_data,
            &mut wav_size,
        )
    };
    if rc != sys::SUCCESS || wav_data.is_null() || wav_size == 0 {
        return Err(format!("rac_audio_int16_to_wav failed for {path}"));
    }
    // SAFETY: the kit just filled wav_data/wav_size with an owned buffer of
    // wav_size bytes on success, checked above.
    let bytes = unsafe { std::slice::from_raw_parts(wav_data as *const u8, wav_size) };
    let result = write_bytes(path, bytes);
    // SAFETY: wav_data was allocated by the kit; rac_free is its matching
    // deallocator and accepts the pointer exactly once.
    unsafe { sys::rac_free(wav_data) };
    result
}

/// Write mono float32 PCM samples as a WAV file via rac_audio_float32_to_wav.
pub fn write_wav_f32(path: &str, samples: &[f32], sample_rate: i32) -> Result<(), String> {
    if samples.is_empty() || sample_rate <= 0 {
        return Err(format!("invalid float32 PCM input for {path}"));
    }

    let mut wav_data: *mut std::os::raw::c_void = std::ptr::null_mut();
    let mut wav_size: usize = 0;
    // SAFETY: `samples` is a valid non-empty slice kept alive for the call;
    // wav_data/wav_size are valid out-params on the stack.
    let rc = unsafe {
        sys::rac_audio_float32_to_wav(
            samples.as_ptr() as *const std::os::raw::c_void,
            samples.len() * std::mem::size_of::<f32>(),
            sample_rate,
            &mut wav_data,
            &mut wav_size,
        )
    };
    if rc != sys::SUCCESS || wav_data.is_null() || wav_size == 0 {
        return Err(format!("rac_audio_float32_to_wav failed for {path}"));
    }
    // SAFETY: the kit just filled wav_data/wav_size with an owned buffer of
    // wav_size bytes on success, checked above.
    let bytes = unsafe { std::slice::from_raw_parts(wav_data as *const u8, wav_size) };
    let result = write_bytes(path, bytes);
    // SAFETY: wav_data was allocated by the kit; rac_free is its matching
    // deallocator and accepts the pointer exactly once.
    unsafe { sys::rac_free(wav_data) };
    result
}

/// Linear resample via rac_audio_resample_f32 (PCM16 <-> float around the call).
pub fn resample(samples: &[i16], from_rate: i32, to_rate: i32) -> Vec<i16> {
    if samples.is_empty() || from_rate == to_rate {
        return samples.to_vec();
    }

    let mut in_f = vec![0f32; samples.len()];
    // SAFETY: samples/in_f are valid buffers of the same length; the kit
    // writes exactly samples.len() floats into in_f.
    let rc = unsafe {
        sys::rac_audio_pcm16_to_float32(samples.as_ptr(), samples.len(), in_f.as_mut_ptr())
    };
    if rc != sys::SUCCESS {
        return Vec::new();
    }

    let mut out_f: *mut f32 = std::ptr::null_mut();
    let mut out_frames: usize = 0;
    // SAFETY: in_f is a valid buffer of in_f.len() floats; out_f/out_frames
    // are valid out-params on the stack.
    let rc = unsafe {
        sys::rac_audio_resample_f32(
            in_f.as_ptr(),
            in_f.len(),
            from_rate,
            to_rate,
            &mut out_f,
            &mut out_frames,
        )
    };
    if rc != sys::SUCCESS {
        return Vec::new();
    }
    if out_frames == 0 {
        // SAFETY: rac_free accepts a NULL or kit-owned pointer.
        unsafe { sys::rac_free(out_f as *mut std::os::raw::c_void) };
        return Vec::new();
    }

    let mut out = vec![0i16; out_frames];
    // SAFETY: out_f holds out_frames valid floats, allocated by the kit above.
    let qrc = unsafe { sys::rac_audio_float32_to_pcm16(out_f, out_frames, out.as_mut_ptr()) };
    // SAFETY: out_f was allocated by rac_audio_resample_f32; rac_free is its
    // matching deallocator, called exactly once.
    unsafe { sys::rac_free(out_f as *mut std::os::raw::c_void) };
    if qrc != sys::SUCCESS {
        return Vec::new();
    }
    out
}

/// int16 -> float [-1, 1] via rac_audio_pcm16_to_float32.
pub fn to_float(samples: &[i16]) -> Vec<f32> {
    let mut out = vec![0f32; samples.len()];
    if !samples.is_empty() {
        // SAFETY: samples/out are valid buffers of the same length.
        let _rc = unsafe {
            sys::rac_audio_pcm16_to_float32(samples.as_ptr(), samples.len(), out.as_mut_ptr())
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> String {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "wally-wav-io-test-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn write_then_read_wav_round_trips_samples_and_rate() {
        let path = temp_path("roundtrip.wav");
        let samples: Vec<i16> = vec![0, 1000, -1000, 32767, -32768, 42];
        write_wav(&path, &samples, 16000).expect("write_wav should succeed");

        let decoded = read_wav(&path).expect("read_wav should succeed");
        assert_eq!(decoded.sample_rate, 16000);
        assert_eq!(decoded.samples, samples);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_wav_f32_then_read_wav_round_trips_within_quantization() {
        let path = temp_path("f32-roundtrip.wav");
        let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 0.999, -0.999];
        write_wav_f32(&path, &samples, 22050).expect("write_wav_f32 should succeed");

        let decoded = read_wav(&path).expect("read_wav should succeed");
        assert_eq!(decoded.sample_rate, 22050);
        assert_eq!(decoded.samples.len(), samples.len());
        for (original, got) in samples.iter().zip(decoded.samples.iter()) {
            let expected = (*original * 32768.0).round().clamp(-32768.0, 32767.0) as i32;
            assert!((expected - *got as i32).abs() <= 1);
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resample_same_rate_is_a_no_op() {
        let samples: Vec<i16> = vec![1, 2, 3, 4, 5];
        assert_eq!(resample(&samples, 16000, 16000), samples);
    }

    #[test]
    fn resample_empty_input_returns_empty() {
        let samples: Vec<i16> = Vec::new();
        assert_eq!(resample(&samples, 16000, 8000), Vec::<i16>::new());
    }

    #[test]
    fn resample_changes_frame_count_with_rate() {
        let samples: Vec<i16> = (0..1600).map(|i| (i % 100) as i16).collect();
        let up = resample(&samples, 8000, 16000);
        assert!(!up.is_empty());
        // Upsampling 2x should roughly double the frame count.
        let ratio = up.len() as f64 / samples.len() as f64;
        assert!((ratio - 2.0).abs() < 0.05, "ratio was {ratio}");
    }

    #[test]
    fn to_float_converts_full_scale_values() {
        let samples: Vec<i16> = vec![0, i16::MAX, i16::MIN];
        let floats = to_float(&samples);
        assert_eq!(floats.len(), 3);
        assert!((floats[0] - 0.0).abs() < 1e-6);
        assert!((floats[1] - 1.0).abs() < 1e-3);
        assert!((floats[2] - (-1.0)).abs() < 1e-3);
    }

    #[test]
    fn to_float_empty_input_returns_empty() {
        assert_eq!(to_float(&[]), Vec::<f32>::new());
    }

    #[test]
    fn read_wav_missing_file_reports_cannot_open() {
        let err = read_wav("/nonexistent/path/does-not-exist.wav").unwrap_err();
        assert!(err.contains("cannot open"));
    }

    #[test]
    fn read_wav_rejects_non_riff_file() {
        let path = temp_path("not-a-wav.wav");
        std::fs::write(&path, b"not a riff file at all, just junk bytes").unwrap();
        let err = read_wav(&path).unwrap_err();
        assert!(err.contains("is not a RIFF/WAVE file"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_wav_rejects_empty_samples() {
        let err = write_wav("/tmp/should-not-be-created.wav", &[], 16000).unwrap_err();
        assert!(err.contains("invalid PCM16 input"));
    }

    #[test]
    fn write_wav_f32_rejects_invalid_sample_rate() {
        let err = write_wav_f32("/tmp/should-not-be-created.wav", &[0.1, 0.2], 0).unwrap_err();
        assert!(err.contains("invalid float32 PCM input"));
    }
}
