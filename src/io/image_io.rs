//! PNG writing and PPM reading for the image/segment commands (port of
//! src/io/image_io.cpp). Owner: the dormant vision/text modalities port.

/// Write RGBA pixels as a PNG.
pub fn write_png(path: &str, rgba: &[u8], width: i32, height: i32) -> Result<(), String> {
    todo!(
        "port src/io/image_io.cpp write_png ({path}, {}, {width}x{height})",
        rgba.len()
    )
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RgbImage {
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub fn read_ppm(path: &str) -> Result<RgbImage, String> {
    todo!("port src/io/image_io.cpp read_ppm ({path})")
}
