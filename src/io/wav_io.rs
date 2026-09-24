//! WAV read/write for the audio modality commands (port of src/io/wav_io.cpp).
//! Owner: the dormant audio modalities port.

/// Decoded PCM: mono (channels collapsed by averaging) 16-bit samples.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WavData {
    pub samples: Vec<i16>,
    pub sample_rate: i32,
}

pub fn read_wav(path: &str) -> Result<WavData, String> {
    todo!("port src/io/wav_io.cpp read_wav ({path})")
}

pub fn write_wav(path: &str, samples: &[i16], sample_rate: i32) -> Result<(), String> {
    todo!(
        "port src/io/wav_io.cpp write_wav ({path}, {}, {sample_rate})",
        samples.len()
    )
}

pub fn write_wav_f32(path: &str, samples: &[f32], sample_rate: i32) -> Result<(), String> {
    todo!(
        "port src/io/wav_io.cpp write_wav_f32 ({path}, {}, {sample_rate})",
        samples.len()
    )
}

pub fn resample(samples: &[i16], from_rate: i32, to_rate: i32) -> Vec<i16> {
    todo!(
        "port src/io/wav_io.cpp resample ({}, {from_rate}, {to_rate})",
        samples.len()
    )
}

pub fn to_float(samples: &[i16]) -> Vec<f32> {
    todo!("port src/io/wav_io.cpp to_float ({})", samples.len())
}
