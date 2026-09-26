//! Infrastructure slice for the Rust port: the real SDK kit, linked the way the
//! product is, driven through the generated bindings and prost types.
//!
//! Proves, before any command is ported: the kit links into a cargo-built
//! executable (CMake's link line via build.rs), bindgen's declarations match the
//! kit's ABI, and the kit accepts protobuf bytes built by prost and returns
//! bytes prost parses (the wire contract that replaced the kit's C++ headers).

use std::ffi::CString;

use wally::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use wally::sys;

#[test]
fn sdk_round_trip_through_bindings_and_prost() {
    let home = tempfile::tempdir().expect("temp home");
    let home_c = CString::new(home.path().to_str().unwrap()).unwrap();

    // The adapter is retained by rac_init until rac_shutdown: give it a stable
    // address for the life of the process, as bootstrap does.
    // SAFETY: plain-data C struct; rac_desktop_adapter_init fills every slot.
    let adapter: &'static mut sys::rac_platform_adapter_t =
        Box::leak(Box::new(unsafe { std::mem::zeroed() }));
    unsafe {
        assert_eq!(
            sys::rac_desktop_adapter_init(std::ptr::null(), adapter),
            sys::SUCCESS
        );
        assert_eq!(
            sys::rac_model_paths_set_base_dir(home_c.as_ptr()),
            sys::SUCCESS
        );
        sys::rac_logger_set_stderr_always(sys::FALSE);
        sys::rac_logger_set_min_level(sys::RAC_LOG_ERROR);

        let tag = c"wally";
        let mut config: sys::rac_config_t = std::mem::zeroed();
        config.platform_adapter = adapter;
        config.log_level = sys::RAC_LOG_ERROR;
        config.log_tag = tag.as_ptr();
        assert_eq!(sys::rac_init(&config), sys::SUCCESS);
        assert_eq!(sys::rac_desktop_http_transport_register(), sys::SUCCESS);
        #[cfg(wally_has_llamacpp)]
        assert_eq!(sys::rac_backend_llamacpp_register(), sys::SUCCESS);
    }

    // A request built by prost, accepted by the kit's C++ protobuf.
    let request = v1::RegisterModelFromUrlRequest {
        url: "https://huggingface.co/example/slice/resolve/main/slice-q4.gguf".into(),
        name: "Slice Test".into(),
        id: Some("wally-slice-test".into()),
        framework: Some(v1::InferenceFramework::LlamaCpp as i32),
        category: Some(v1::ModelCategory::Language as i32),
        download_size_bytes: Some(1234),
        context_length: Some(4096),
        ..Default::default()
    };
    let bytes = serialize(&request);
    let mut out = ProtoBuffer::new();
    let rc = unsafe {
        sys::rac_register_model_from_url_proto(bytes.as_ptr(), bytes.len(), out.as_mut_ptr())
    };
    assert_eq!(rc, sys::SUCCESS);
    let info: v1::ModelInfo = parse_proto_buffer(out).expect("kit returned ModelInfo bytes");
    assert_eq!(info.id, "wally-slice-test");
    assert_eq!(info.name, "Slice Test");

    // And bytes produced by the kit, parsed by prost.
    let mut list = ProtoBuffer::new();
    let rc = unsafe {
        sys::rac_model_registry_list_proto_buffer(sys::rac_get_model_registry(), list.as_mut_ptr())
    };
    assert_eq!(rc, sys::SUCCESS);
    let models: v1::ModelInfoList =
        parse_proto_buffer(list).expect("kit returned ModelInfoList bytes");
    assert!(
        models.models.iter().any(|m| m.id == "wally-slice-test"),
        "registered model is listed"
    );

    unsafe { sys::rac_shutdown() };
}

#[test]
fn error_messages_come_from_the_kit() {
    let text = wally::io::output::describe_result(sys::RAC_ERROR_INVALID_CONFIGURATION);
    assert!(
        text.ends_with(&format!("({})", sys::RAC_ERROR_INVALID_CONFIGURATION)),
        "{text}"
    );
}
