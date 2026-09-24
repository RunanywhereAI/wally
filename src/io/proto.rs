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

    /// Like `error_message()`, but only null-checks the pointer, returning
    /// `Some("")` for a non-null empty C string instead of falling through to
    /// `None`. Some C++ call sites (e.g. `register_url`'s early-return path,
    /// which never reaches `parse_proto_buffer`) reimplement the null check
    /// inline without the empty-string guard `parse_proto_buffer` applies;
    /// this mirrors that narrower check for callers that need to match it.
    pub fn error_message_nullable(&self) -> Option<String> {
        if self.0.error_message.is_null() {
            return None;
        }
        // SAFETY: NUL-terminated string owned by the buffer.
        let text = unsafe { std::ffi::CStr::from_ptr(self.0.error_message) }.to_string_lossy();
        Some(text.into_owned())
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

#[cfg(test)]
mod fix_run_tests {
    use super::*;
    use std::ffi::CString;

    /// `error_message()` treats a non-null empty C string the same as a null
    /// pointer (None) -- the shared `parse_proto_buffer` convention. Some
    /// call sites (e.g. `register_url`'s early-return path in
    /// catalog/model_ref.rs, which the C++ original reimplements inline
    /// without the empty-string guard) need the narrower null-only check
    /// instead; `error_message_nullable` is that.
    #[test]
    fn error_message_nullable_keeps_empty_string_error_message_distinguishes_none() {
        let mut buffer = ProtoBuffer::new();
        let empty = CString::new("").unwrap();
        // SAFETY: buffer is freshly initialised; empty is a valid, live
        // NUL-terminated string for the duration of this call.
        unsafe {
            sys::rac_proto_buffer_set_error(
                buffer.as_mut_ptr(),
                sys::RAC_ERROR_INVALID_ARGUMENT,
                empty.as_ptr(),
            );
        }
        assert_eq!(buffer.error_message(), None);
        assert_eq!(buffer.error_message_nullable(), Some(String::new()));
    }

    #[test]
    fn error_message_nullable_is_none_for_null_pointer() {
        // A freshly-initialised buffer with no error set has a null
        // error_message pointer; both accessors agree it is None.
        let buffer = ProtoBuffer::new();
        assert_eq!(buffer.error_message(), None);
        assert_eq!(buffer.error_message_nullable(), None);
    }

    #[test]
    fn error_message_nullable_returns_text_when_present() {
        let mut buffer = ProtoBuffer::new();
        let text = CString::new("boom").unwrap();
        // SAFETY: buffer is freshly initialised; text is a valid, live
        // NUL-terminated string for the duration of this call.
        unsafe {
            sys::rac_proto_buffer_set_error(
                buffer.as_mut_ptr(),
                sys::RAC_ERROR_INVALID_ARGUMENT,
                text.as_ptr(),
            );
        }
        assert_eq!(buffer.error_message(), Some("boom".to_string()));
        assert_eq!(buffer.error_message_nullable(), Some("boom".to_string()));
    }
}
