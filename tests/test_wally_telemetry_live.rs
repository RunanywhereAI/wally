//! Live telemetry integration test — sends real, authenticated per-modality
//! telemetry to the configured backend and asserts each is accepted (2xx).
//! Port of tests/test_wally_telemetry_live.cpp.
//!
//! This complements the hermetic commons unit test (test_telemetry_extraction),
//! which validates JSON shape offline against a mock sink. Here we exercise the
//! full wire path: wally::bootstrap() registers the desktop adapter + HTTP
//! transport and authenticates (API key -> device register -> JWT); we then
//! override the process telemetry manager's HTTP callback with a
//! status-recording POST (the same recipe as bootstrap's private
//! wally_telemetry_http_callback) so we can assert the backend's response. A
//! strict-schema rejection (422 extra_forbidden) fails the test — catching
//! field drift against the real V2 endpoints.
//!
//! Opt-in: runs ONLY with WALLY_LIVE_TELEMETRY=1 AND the creds in the
//! environment (RUNANYWHERE_BASE_URL + RUNANYWHERE_API_KEY, optional
//! RUNANYWHERE_ENVIRONMENT). The C++ original gates on an explicit `--live`
//! argv flag; `cargo test`'s libtest harness has no equivalent custom-flag
//! passthrough for a plain `#[test]`, so WALLY_LIVE_TELEMETRY=1 is the direct
//! substitute — same double-gate property (creds alone are not enough, a
//! human must opt in explicitly), safe to leave registered for `cargo test`
//! with no extra arguments (default: skip, exit success).
//!
//!   WALLY_LIVE_TELEMETRY=1 RUNANYWHERE_BASE_URL=... RUNANYWHERE_API_KEY=... \
//!     cargo test --test test_wally_telemetry_live -- --ignored --nocapture
//!
//! (Not marked #[ignore] — like the C++ binary registered with ctest and no
//! `--live`, the env-var gate alone is what keeps a normal `cargo test` run
//! skipping this instead of hitting the network.)

use std::ffi::{c_void, CStr, CString};
use std::os::raw::c_char;

use prost::Message;

use wally::bootstrap::{self, GlobalOptions};
use wally::io::proto::v1;
use wally::sys;

/// Records the backend's response for the most recent flushed batch, then
/// hands the result back to the manager. POST recipe mirrors bootstrap's
/// private wally_telemetry_http_callback (all-public rac_http_* / rac_auth_*
/// APIs) — duplicated here exactly as the C++ original duplicates it, since
/// that function is private to bootstrap.rs.
struct LiveState {
    manager: *mut sys::rac_telemetry_manager_t,
    called: bool,
    status: i32,
    ok: bool,
    endpoint: String,
    body: String,
}

/// # Safety
/// `user_data` must be a valid, live `*mut LiveState` for the duration of this
/// call; the SDK invokes this callback synchronously from
/// `rac_telemetry_manager_track_proto`, so no state outlives the call it was
/// installed for.
unsafe extern "C" fn live_post_cb(
    user_data: *mut c_void,
    endpoint: *const c_char,
    json_body: *const c_char,
    json_length: usize,
    requires_auth: sys::rac_bool_t,
) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: caller guarantees `user_data` is a valid, live `*mut
        // LiveState` for the duration of this call.
        let state = unsafe { &mut *(user_data as *mut LiveState) };
        state.called = true;
        // SAFETY: `endpoint` is the caller-supplied NUL-terminated endpoint
        // per this callback's documented contract, or NULL.
        state.endpoint = if endpoint.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(endpoint) }
                .to_string_lossy()
                .into_owned()
        };
        state.status = 0;
        state.ok = false;
        state.body.clear();

        // SAFETY: takes no arguments; returns a commons-owned NUL-terminated
        // string or NULL, valid for this call.
        let base_url_ptr = unsafe { sys::rac_state_get_base_url() };
        // SAFETY: `base_url_ptr` may be NULL, which is checked immediately.
        let base_url_empty = base_url_ptr.is_null()
            || unsafe { CStr::from_ptr(base_url_ptr) }
                .to_bytes()
                .is_empty();
        // SAFETY: takes no arguments; always safe to call.
        let transport_registered = unsafe { sys::rac_http_transport_is_registered() };
        if base_url_empty || transport_registered != sys::TRUE {
            complete(
                state.manager,
                false,
                std::ptr::null(),
                c"transport unavailable".as_ptr(),
            );
            return;
        }

        let mut url_buf = [0 as c_char; 2048];
        // SAFETY: `base_url_ptr` was just checked non-null/non-empty above;
        // `endpoint` is the caller-supplied NUL-terminated endpoint;
        // `url_buf` is a correctly-sized out buffer.
        let written = unsafe {
            sys::rac_build_url(base_url_ptr, endpoint, url_buf.as_mut_ptr(), url_buf.len())
        };
        if written < 0 {
            complete(
                state.manager,
                false,
                std::ptr::null(),
                c"url build failed".as_ptr(),
            );
            return;
        }

        let mut headers: Vec<sys::rac_http_header_kv_t> = Vec::new();
        let mut defaults_ptr: *const sys::rac_http_header_kv_t = std::ptr::null();
        let mut default_count: usize = 0;
        // SAFETY: out-params are valid stack locals.
        if unsafe { sys::rac_http_default_headers(&mut defaults_ptr, &mut default_count) }
            == sys::SUCCESS
            && !defaults_ptr.is_null()
        {
            // SAFETY: commons guarantees `defaults_ptr` describes
            // `default_count` valid, static-lifetime entries.
            let defaults = unsafe { std::slice::from_raw_parts(defaults_ptr, default_count) };
            headers.extend_from_slice(defaults);
        }
        let mut auth_value = CString::default();
        if requires_auth == sys::TRUE {
            // SAFETY: takes no arguments; returns a commons-owned
            // NUL-terminated string or NULL, valid for this call.
            let token_ptr = unsafe { sys::rac_auth_get_access_token() };
            if !token_ptr.is_null() {
                // SAFETY: non-null, NUL-terminated per the function's
                // contract.
                let token = unsafe { CStr::from_ptr(token_ptr) }.to_string_lossy();
                if !token.is_empty() {
                    auth_value = CString::new(format!("Bearer {token}")).unwrap_or_default();
                }
            }
        }
        if !auth_value.as_bytes().is_empty() {
            headers.push(sys::rac_http_header_kv_t {
                name: c"Authorization".as_ptr(),
                value: auth_value.as_ptr(),
            });
        }

        let mut client: *mut sys::rac_http_client_t = std::ptr::null_mut();
        // SAFETY: `&mut client` is a valid out-param.
        let rc = unsafe { sys::rac_http_client_create(&mut client) };
        if rc != sys::SUCCESS {
            complete(
                state.manager,
                false,
                std::ptr::null(),
                c"client create failed".as_ptr(),
            );
            return;
        }

        // SAFETY: takes no arguments; always safe to call.
        let environment = unsafe { sys::rac_state_get_environment() };
        let request = sys::rac_http_request_t {
            method: c"POST".as_ptr(),
            url: url_buf.as_ptr(),
            headers: if headers.is_empty() {
                std::ptr::null()
            } else {
                headers.as_ptr()
            },
            header_count: headers.len(),
            body_bytes: json_body as *const u8,
            body_len: json_length,
            // SAFETY: takes an environment enum; always safe to call.
            timeout_ms: unsafe { sys::rac_env_default_http_timeout_ms(environment) },
            follow_redirects: sys::FALSE,
            expected_checksum_hex: std::ptr::null(),
        };

        let mut response: sys::rac_http_response_t = unsafe { std::mem::zeroed() };
        // SAFETY: `client` is a freshly created, valid handle; `request` is
        // built above with every pointer alive for this call (including
        // `json_body`, which the callback contract guarantees is valid for
        // `json_length` bytes for the duration of this call); `response` is a
        // correctly-sized out-param.
        let rc = unsafe { sys::rac_http_request_send(client, &request, &mut response) };
        // SAFETY: `client` was created by rac_http_client_create above and is
        // not used again after this.
        unsafe { sys::rac_http_client_destroy(client) };

        state.status = response.status;
        state.ok = rc == sys::SUCCESS && (200..300).contains(&response.status);
        if !response.body_bytes.is_null() && response.body_len > 0 {
            // SAFETY: commons guarantees `body_bytes` holds `body_len` valid
            // bytes until `rac_http_response_free`.
            let body_slice =
                unsafe { std::slice::from_raw_parts(response.body_bytes, response.body_len) };
            state.body = String::from_utf8_lossy(body_slice).into_owned();
        }
        let body_c =
            (!state.body.is_empty()).then(|| CString::new(state.body.clone()).unwrap_or_default());
        complete(
            state.manager,
            state.ok,
            body_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            if state.ok {
                std::ptr::null()
            } else {
                c"POST failed".as_ptr()
            },
        );
        // SAFETY: `response` was populated by rac_http_request_send above,
        // valid to free either way (send zero-initializes on early failure
        // paths too).
        unsafe { sys::rac_http_response_free(&mut response) };
    });
}

fn complete(
    manager: *mut sys::rac_telemetry_manager_t,
    ok: bool,
    body: *const c_char,
    error: *const c_char,
) {
    if manager.is_null() {
        return;
    }
    // SAFETY: `manager` was just checked non-null; `body`/`error` are either
    // NULL or valid NUL-terminated strings for the duration of this call.
    unsafe {
        sys::rac_telemetry_manager_http_complete(
            manager,
            if ok { sys::TRUE } else { sys::FALSE },
            body,
            error,
        )
    };
}

fn envelope(component: v1::SdkComponent) -> v1::SdkEvent {
    v1::SdkEvent {
        id: "wally-live-test".to_string(),
        timestamp_ms: 1,
        component: component as i32,
        source: "rust".to_string(),
        ..Default::default()
    }
}

/// Send one event and assert the backend accepted it (2xx).
fn send_and_assert(
    manager: *mut sys::rac_telemetry_manager_t,
    state: &mut LiveState,
    ev: v1::SdkEvent,
    label: &str,
) {
    state.called = false;
    let bytes = ev.encode_to_vec();
    // SAFETY: `manager` is a live handle for the duration of this test;
    // `bytes` is valid for the duration of this call.
    let _ = unsafe { sys::rac_telemetry_manager_track_proto(manager, bytes.as_ptr(), bytes.len()) };
    if !state.called {
        panic!("{label}: no POST was made");
    }
    if !state.ok {
        eprintln!(
            "  {label}: http={} body={}",
            state.status,
            if state.body.is_empty() {
                "(empty)"
            } else {
                &state.body
            }
        );
    } else {
        println!(
            "  {label}: accepted (http={}, {})",
            state.status, state.endpoint
        );
    }
    assert!(state.ok, "{label}");
}

#[test]
fn telemetry_live_round_trip() {
    println!("test_wally_telemetry_live");

    let live = std::env::var("WALLY_LIVE_TELEMETRY")
        .map(|v| v == "1")
        .unwrap_or(false);
    let base = std::env::var("RUNANYWHERE_BASE_URL").unwrap_or_default();
    let key = std::env::var("RUNANYWHERE_API_KEY").unwrap_or_default();
    let have_creds = !base.is_empty() && !key.is_empty();

    if !live || !have_creds {
        println!(
            "  skip: live telemetry test (needs WALLY_LIVE_TELEMETRY=1 and \
             RUNANYWHERE_BASE_URL + RUNANYWHERE_API_KEY)"
        );
        return;
    }

    let opts = GlobalOptions {
        quiet: true,
        ..Default::default()
    };
    let env = match bootstrap::bootstrap(&opts) {
        Ok(env) => env,
        Err(rc) => panic!("bootstrap succeeded: rc={rc}"),
    };
    let _ = &env;

    let manager = bootstrap::active_telemetry_manager();
    assert!(
        !manager.is_null(),
        "telemetry manager initialized (creds + auth)"
    );

    let mut state = LiveState {
        manager,
        called: false,
        status: 0,
        ok: false,
        endpoint: String::new(),
        body: String::new(),
    };
    // SAFETY: `manager` was just checked non-null; `live_post_cb` matches
    // `rac_telemetry_http_callback_t` exactly and is wrapped in
    // `catch_unwind`; `&mut state` outlives every synchronous call the SDK
    // makes through this callback within this function body.
    unsafe {
        sys::rac_telemetry_manager_set_http_callback(
            manager,
            Some(live_post_cb),
            &mut state as *mut LiveState as *mut c_void,
        )
    };

    // LLM
    {
        let mut ev = envelope(v1::SdkComponent::Llm);
        ev.event = Some(v1::sdk_event::Event::Generation(v1::GenerationEvent {
            kind: v1::GenerationEventKind::Completed as i32,
            model_id: "wally-live-test".to_string(),
            input_tokens: 10,
            output_tokens: 20,
            tokens_per_second: 40.0,
            prefill_duration_ms: 100,
            ..Default::default()
        }));
        send_and_assert(manager, &mut state, ev, "llm");
    }
    // Embeddings
    {
        let mut ev = envelope(v1::SdkComponent::Embeddings);
        ev.properties
            .insert("embedding_dimension".to_string(), "384".to_string());
        ev.properties
            .insert("total_tokens".to_string(), "8".to_string());
        ev.properties
            .insert("batch_size".to_string(), "1".to_string());
        ev.event = Some(v1::sdk_event::Event::Capability(
            v1::CapabilityOperationEvent {
                kind: v1::CapabilityOperationEventKind::EmbeddingsCompleted as i32,
                component: v1::SdkComponent::Embeddings as i32,
                model_id: "wally-live-test".to_string(),
                input_count: 1,
                output_count: 1,
                ..Default::default()
            },
        ));
        send_and_assert(manager, &mut state, ev, "embeddings");
    }
    // RAG
    {
        let mut ev = envelope(v1::SdkComponent::Rag);
        ev.properties.insert("top_k".to_string(), "5".to_string());
        ev.properties
            .insert("retrieval_time_ms".to_string(), "1".to_string());
        ev.properties
            .insert("embedding_model".to_string(), "wally-live-test".to_string());
        ev.properties
            .insert("query_token_count".to_string(), "10".to_string());
        ev.properties
            .insert("context_tokens".to_string(), "49".to_string());
        ev.event = Some(v1::sdk_event::Event::Capability(
            v1::CapabilityOperationEvent {
                kind: v1::CapabilityOperationEventKind::RagQueryCompleted as i32,
                component: v1::SdkComponent::Rag as i32,
                model_id: "wally-live-test".to_string(),
                output_count: 2,
                ..Default::default()
            },
        ));
        send_and_assert(manager, &mut state, ev, "rag");
    }
    // VLM
    {
        let mut ev = envelope(v1::SdkComponent::Vlm);
        ev.properties
            .insert("total_tokens".to_string(), "120".to_string());
        ev.properties
            .insert("tokens_per_second".to_string(), "100.0".to_string());
        ev.properties
            .insert("prompt_eval_time_ms".to_string(), "800".to_string());
        ev.event = Some(v1::sdk_event::Event::Capability(
            v1::CapabilityOperationEvent {
                kind: v1::CapabilityOperationEventKind::VlmCompleted as i32,
                component: v1::SdkComponent::Vlm as i32,
                model_id: "wally-live-test".to_string(),
                input_count: 1,
                output_count: 120,
                ..Default::default()
            },
        ));
        send_and_assert(manager, &mut state, ev, "vlm");
    }
    // LoRA (failure path — rides the LLM component, modality overridden to lora)
    {
        let mut ev = envelope(v1::SdkComponent::Llm);
        ev.properties
            .insert("adapter_id".to_string(), "wally-live-test".to_string());
        ev.properties
            .insert("adapter_size_bytes".to_string(), "4096".to_string());
        ev.event = Some(v1::sdk_event::Event::Capability(
            v1::CapabilityOperationEvent {
                kind: v1::CapabilityOperationEventKind::LoraFailed as i32,
                component: v1::SdkComponent::Llm as i32,
                model_id: "wally-live-test".to_string(),
                ..Default::default()
            },
        ));
        send_and_assert(manager, &mut state, ev, "lora");
    }

    bootstrap::shutdown();
}
