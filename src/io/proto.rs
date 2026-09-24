//! rac_proto_buffer_t ⇄ protobuf message glue.
//!
//! Every lifecycle C ABI call returns serialized proto bytes in a
//! rac_proto_buffer_t with the canonical {data, size, status} convention.
//! `parse_proto_buffer()` checks the status envelope, parses, and ALWAYS frees
//! the buffer.
//!
//! The message types (`v1::*`) are generated at build time from the kit's own
//! share/runanywhere/idl/*.proto, after build.rs has checked their SCHEMA_LOCK
//! hash against versions.toml. They are the proto SOT, as the kit's C++ headers
//! were for the C++ build.

use prost::Message;

use crate::io::output::describe_result;
use crate::sys;

/// `runanywhere.v1` message and enum types, generated from the kit IDL.
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/runanywhere.v1.rs"));
}

/// An out-buffer for a `*_proto` C ABI call, freed on drop.
pub struct ProtoBuffer(pub sys::rac_proto_buffer_t);

impl ProtoBuffer {
    pub fn new() -> Self {
        // SAFETY: rac_proto_buffer_t is plain data; init sets the canonical empty state.
        let mut buffer: sys::rac_proto_buffer_t = unsafe { std::mem::zeroed() };
        unsafe { sys::rac_proto_buffer_init(&mut buffer) };
        ProtoBuffer(buffer)
    }

    pub fn as_mut_ptr(&mut self) -> *mut sys::rac_proto_buffer_t {
        &mut self.0
    }

    pub fn status(&self) -> sys::rac_result_t {
        self.0.status
    }

    /// The payload bytes (empty when there are none).
    pub fn bytes(&self) -> &[u8] {
        if self.0.data.is_null() || self.0.size == 0 {
            return &[];
        }
        // SAFETY: the kit guarantees `data` holds `size` bytes until freed.
        unsafe { std::slice::from_raw_parts(self.0.data, self.0.size) }
    }

    /// The error envelope's message, when the kit set one.
    pub fn error_message(&self) -> Option<String> {
        if self.0.error_message.is_null() {
            return None;
        }
        // SAFETY: NUL-terminated string owned by the buffer.
        let text = unsafe { std::ffi::CStr::from_ptr(self.0.error_message) }.to_string_lossy();
        (!text.is_empty()).then(|| text.into_owned())
    }
}

impl Default for ProtoBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProtoBuffer {
    fn drop(&mut self) {
        // SAFETY: freeing an initialised (possibly empty) buffer is always valid.
        unsafe { sys::rac_proto_buffer_free(&mut self.0) };
    }
}

/// Parse an out-buffer into `M`, freeing the buffer in all paths. On failure the
/// error is the buffer's error envelope, or `failed to parse <Name> bytes`
/// (the C++ printed `Message::descriptor()->name()`; prost's `Name::NAME` is
/// the same short name).
pub fn parse_proto_buffer<M: Message + Default + prost::Name>(
    buffer: ProtoBuffer,
) -> Result<M, String> {
    if buffer.status() != sys::SUCCESS {
        return Err(buffer
            .error_message()
            .unwrap_or_else(|| describe_result(buffer.status())));
    }
    M::decode(buffer.bytes()).map_err(|_| format!("failed to parse {} bytes", M::NAME))
}

/// Serialize a request message into bytes (proto3 encoding does not fail).
pub fn serialize<M: Message>(message: &M) -> Vec<u8> {
    message.encode_to_vec()
}
