//! test_wally_unit.cpp cases owned by the catalog port. Port each C++ case below as a
//! `#[test] fn <same name>()` in this file, then delete its line from this list.
//! Test bodies are in explore-main/tests/test_wally_unit.cpp (grep the name).
//!
//! - [x] catalog_lookup
//! - [x] overlay_catalog
//! - [x] nvidia_sherpa_catalog
//! - [x] mlx_catalog_registration
//! - [x] hf_ref_registration

#[allow(unused_imports)]
use super::common;

use std::ffi::CString;
use std::sync::Once;

use wally::catalog::catalog;
use wally::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use wally::sys;

/// SDK init, done once per process the way tests/test_foundation_slice.rs does
/// it. Only `mlx_catalog_registration` and `hf_ref_registration` need a live
/// model registry; `catalog_lookup`/`overlay_catalog`/`nvidia_sherpa_catalog`
/// only read the static catalog table.
///
/// KNOWN GAP (flagged for integration): this is a per-file `Once`, not a
/// crate-shared one — tests/common/mod.rs has no SDK-init helper today. If
/// another area's `tests/test_wally_unit/*.rs` also calls `rac_init()`
/// independently, both run in the same `test_wally_unit` process and the
/// second call's behavior is unverified. Fold into a single shared
/// `tests/common` helper during integration.
fn init_sdk_once() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let home = std::env::temp_dir().join(format!("wally-catalog-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let home_c = CString::new(home.to_string_lossy().into_owned()).unwrap();

        // The adapter is retained by rac_init until process exit; give it a
        // stable address, as bootstrap does.
        // SAFETY: plain-data C struct; rac_desktop_adapter_init fills every slot.
        let adapter: &'static mut sys::rac_platform_adapter_t =
            Box::leak(Box::new(unsafe { std::mem::zeroed() }));
        // SAFETY: mirrors tests/test_foundation_slice.rs's SDK init sequence:
        // adapter init, base dir, logger config, rac_init, transport + engine
        // registration. Every call's return is checked.
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
    });
}

#[cfg(target_os = "macos")]
fn remove_registered_model(id: &str) {
    let Ok(id_c) = CString::new(id) else {
        return;
    };
    // SAFETY: `id_c` is a valid NUL-terminated string for the call; the
    // registry handle is the process-global singleton the kit owns.
    unsafe {
        sys::rac_model_registry_remove_proto(sys::rac_get_model_registry(), id_c.as_ptr());
    }
}

/// RAII cleanup for models registered by a test, mirroring the C++
/// test-local `RegisteredModelCleanup`.
#[cfg(target_os = "macos")]
struct RegisteredModelCleanup {
    ids: Vec<&'static str>,
}

#[cfg(target_os = "macos")]
impl Drop for RegisteredModelCleanup {
    fn drop(&mut self) {
        for id in &self.ids {
            remove_registered_model(id);
        }
    }
}

fn get_registered_model(id: &str) -> Result<v1::ModelInfo, String> {
    let id_c = CString::new(id).map_err(|e| e.to_string())?;
    let mut found = ProtoBuffer::new();
    // SAFETY: `id_c` is a valid NUL-terminated string for the call; `found`
    // is a freshly-initialised buffer.
    let rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(
            sys::rac_get_model_registry(),
            id_c.as_ptr(),
            found.as_mut_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        return Err(format!("registry get failed rc={rc}"));
    }
    parse_proto_buffer(found)
}

#[cfg(target_os = "macos")]
fn multi_file_count(model: &v1::ModelInfo) -> Option<usize> {
    match &model.artifact {
        Some(v1::model_info::Artifact::MultiFile(multi)) => Some(multi.files.len()),
        _ => None,
    }
}

#[test]
fn catalog_lookup() {
    let entries = catalog::all();

    // all() lists only LLMs the linked kit can run (platform_supports() in
    // src/catalog/catalog.rs), so the floor and the probe rows track the kit
    // macros. The public windows-arm64 kit has no LLM backend at all: no
    // llama.cpp, and QHexRT reaches it only through the private overlay
    // public CI never sees -- so neither wally_has_llamacpp nor
    // wally_has_qhexrt is set there and the listed catalog is empty.
    #[cfg(wally_has_llamacpp)]
    let (min_entries, probe_id, probe_alias, probe_partial) = (10, "qwen3-0.6b", "qwen3", "qwen");
    #[cfg(all(not(wally_has_llamacpp), wally_has_qhexrt))]
    let (min_entries, probe_id, probe_alias, probe_partial) =
        (3, "lfm2_5_230m", "lfm2-230m-npu", "lfm2");
    #[cfg(all(not(wally_has_llamacpp), not(wally_has_qhexrt)))]
    let (min_entries, probe_id, probe_alias, probe_partial): (usize, &str, &str, &str) =
        (0, "", "", "");

    assert!(entries.len() >= min_entries, "catalog unexpectedly small");

    // No LLM backend and no Apple engines: platform_supports() must hide every
    // LLM row. A non-empty listing here means the kit gating broke.
    #[cfg(all(
        not(wally_has_llamacpp),
        not(wally_has_qhexrt),
        not(target_os = "macos")
    ))]
    assert_eq!(
        entries.len(),
        0,
        "expected an empty LLM catalog on a kit with no LLM backend"
    );

    if !probe_id.is_empty() {
        let by_id = catalog::find(probe_id);
        let by_alias = catalog::find(probe_alias);
        assert!(
            by_id.is_some(),
            "alias lookup should resolve to the same entry"
        );
        assert_eq!(
            by_id.map(|e| e.id),
            by_alias.map(|e| e.id),
            "alias lookup should resolve to the same entry"
        );
    }
    assert!(
        catalog::find("definitely-not-a-model").is_none(),
        "unknown id should return None"
    );
    if !probe_partial.is_empty() {
        assert!(
            !catalog::suggestions(probe_partial, 3).is_empty(),
            "expected suggestions for '{probe_partial}'"
        );
    }

    #[cfg(wally_has_llamacpp)]
    {
        let qwen_harness =
            catalog::find("qwen3-4b-instruct").expect("qwen3-4b-instruct should resolve");
        assert!(
            qwen_harness.harness_compatible
                && qwen_harness.context_length == 262144
                && catalog::find("qwen3-1.7b").is_none(),
            "only the certified Qwen3 4B GGUF row should remain harness-compatible"
        );
    }

    // Multi-file entries (VLM pairs, embeddings) must carry ≥2 required files.
    // smolvlm2 is a VLM, out of scope for the LLM-only cut (src/app.cpp,
    // src/catalog/catalog.cpp) -- left out, not deleted, so it comes back
    // when the cut reverts.

    // MLX entries are Apple-only; the catalog hides them off Apple, so these
    // lookups only resolve (and are only asserted) on Apple.
    #[cfg(target_os = "macos")]
    {
        let mlx_llm = catalog::find("mlx-qwen3").expect("mlx-qwen3 should resolve on Apple");
        assert_eq!(mlx_llm.framework, v1::InferenceFramework::Mlx);
        assert_eq!(mlx_llm.format, v1::ModelFormat::Safetensors);
        assert_eq!(mlx_llm.category, v1::ModelCategory::Language);
        assert_eq!(mlx_llm.files.len(), 9);
        assert!(mlx_llm.supports_thinking);

        let mlx_qwen_harness = catalog::find("mlx-qwen3-4b-instruct")
            .expect("mlx-qwen3-4b-instruct should resolve on Apple");
        assert!(
            mlx_qwen_harness.harness_compatible
                && mlx_qwen_harness.files.len() == 11
                && mlx_qwen_harness.context_length == 262144
                && catalog::find("mlx-qwen3-1.7b").is_none(),
            "only the certified Qwen3 4B MLX row should remain harness-compatible"
        );
    }
    #[cfg(not(target_os = "macos"))]
    assert!(
        catalog::find("mlx-qwen3").is_none(),
        "mlx-qwen3 should be hidden off Apple"
    );

    // maple-preview is a llama.cpp row; kits without that backend (the public
    // windows-arm64 kit) hide it, so the pinned-bundle check only applies where
    // the row is listed.
    #[cfg(wally_has_llamacpp)]
    {
        let maple_gguf = catalog::find("maple-preview")
            .expect("maple-preview should resolve to the pinned GGUF bundle");
        assert_eq!(maple_gguf.framework, v1::InferenceFramework::LlamaCpp);
        assert_eq!(maple_gguf.format, v1::ModelFormat::Gguf);
        assert_eq!(maple_gguf.download_size_bytes, 4984016416);
        assert_eq!(maple_gguf.context_length, 4096);
        assert!(maple_gguf.supports_thinking);
    }

    #[cfg(target_os = "macos")]
    {
        let mlx_maple = catalog::find("mlx-maple-preview")
            .expect("mlx-maple-preview should be a complete pinned MLX bundle");
        assert_eq!(mlx_maple.framework, v1::InferenceFramework::Mlx);
        assert_eq!(mlx_maple.format, v1::ModelFormat::Safetensors);
        assert_eq!(mlx_maple.category, v1::ModelCategory::Language);
        assert_eq!(mlx_maple.files.len(), 13);
        assert_eq!(mlx_maple.download_size_bytes, 5330252282);
        assert_eq!(mlx_maple.context_length, 128000);

        let mut total = 0i64;
        for file in mlx_maple.files {
            assert!(
                file.size_bytes > 0,
                "mlx-maple-preview files must use the pinned revision"
            );
            assert!(
                file.url
                    .contains("/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/"),
                "mlx-maple-preview files must use the pinned revision"
            );
            total += file.size_bytes;
        }
        assert_eq!(
            total, mlx_maple.download_size_bytes,
            "mlx-maple-preview file sizes must sum to the bundle size"
        );
    }

    // VLM (multimodal) and embedding catalog entries are out of scope for the
    // LLM-only cut (src/app.cpp, src/catalog/catalog.cpp) -- left out, not
    // deleted, so this comes back when the cut reverts.

    #[cfg(target_os = "macos")]
    {
        let nemotron_nano = catalog::find("mlx-nemotron-nano")
            .expect("mlx-nemotron-nano should be a complete pinned MLX bundle");
        assert_eq!(nemotron_nano.category, v1::ModelCategory::Language);
        assert_eq!(nemotron_nano.framework, v1::InferenceFramework::Mlx);
        assert_eq!(nemotron_nano.files.len(), 8);
        assert_eq!(nemotron_nano.download_size_bytes, 4534806075);
        assert_eq!(nemotron_nano.context_length, 131072);

        let nemotron_mini = catalog::find("mlx-nemotron-mini")
            .expect("mlx-nemotron-mini should be a complete pinned MLX bundle");
        assert_eq!(nemotron_mini.category, v1::ModelCategory::Language);
        assert_eq!(nemotron_mini.framework, v1::InferenceFramework::Mlx);
        assert_eq!(nemotron_mini.format, v1::ModelFormat::Safetensors);
        assert_eq!(nemotron_mini.files.len(), 6);
        assert_eq!(nemotron_mini.download_size_bytes, 2392679103);
        assert_eq!(nemotron_mini.context_length, 4096);
        for file in nemotron_mini.files {
            assert!(
                file.url
                    .contains("/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/"),
                "mlx-nemotron-mini files must use the pinned revision"
            );
        }
    }

    // Speech recognition entries are out of scope for the LLM-only cut
    // (src/app.cpp, src/catalog/catalog.cpp) -- left out, not deleted, so
    // this comes back when the cut reverts.
    let _ = entries;
}

/// Ports the smolvlm2 case that the comment at the top of `catalog_lookup`
/// (`// smolvlm2 is a VLM, out of scope for the LLM-only cut`) leaves out:
/// multi-file entries (VLM pairs, embeddings) must carry >= 2 required files.
/// Matches explore-main/tests/test_wally_unit.cpp's commented-out
/// `wally::catalog::find("smolvlm2")` case.
///
/// `#[ignore]`d following the convention in tests/test_wally_unit/diarize.rs.
/// Unlike diarize (gated only by command registration), this row is hidden
/// at the catalog data layer itself: `catalog::find`/`catalog::all` route
/// every lookup through `listed()`, which requires `is_llm()`
/// (src/catalog/catalog.rs), the same `is_llm()` cut the C++ reference
/// applies in src/catalog/catalog.cpp (see its comment "Delete is_llm and its
/// four uses below to restore the full catalog"). So this stays red under
/// `--ignored` until *both* the LLM-only cut is reverted in src/app.rs *and*
/// `is_llm()`'s filtering is removed from src/catalog/catalog.rs, not just
/// the former.
#[test]
#[ignore = "smolvlm2 is hidden by is_llm() in src/catalog/catalog.rs for the LLM-only release (mirrors C++ WALLY_LLM_ONLY_CUT in src/catalog/catalog.cpp)"]
fn catalog_lookup_vlm_two_file_artifact() {
    let vlm = catalog::find("smolvlm2").expect("smolvlm2 should resolve");
    assert_eq!(vlm.files.len(), 2, "smolvlm2 should be a two-file artifact");
}

/// Ports the VLM/embedding block that the comment before the Apple-only
/// nemotron checks in `catalog_lookup`
/// (`// VLM (multimodal) and embedding catalog entries are out of scope for
/// the LLM-only cut`) leaves out. Matches the commented-out block in
/// explore-main/tests/test_wally_unit.cpp between that same comment and the
/// `mlx-nemotron-nano` Apple-only checks: mlx-qwen2-vl and mlx-fastvlm (MLX
/// VLM bundles), mlx-qwen3-embed (MLX embedding bundle), and three portable
/// llama.cpp GGUF embedding models with pinned revisions.
///
/// `#[ignore]`d following the convention in tests/test_wally_unit/diarize.rs.
/// Like `catalog_lookup_vlm_two_file_artifact` above, these rows are hidden
/// by `is_llm()`/`listed()` in src/catalog/catalog.rs (mirroring
/// src/catalog/catalog.cpp), a data-layer gate on top of src/app.rs's
/// command registration -- both need to revert for this to pass.
#[test]
#[ignore = "VLM/embed rows are hidden by is_llm() in src/catalog/catalog.rs for the LLM-only release (mirrors C++ WALLY_LLM_ONLY_CUT in src/catalog/catalog.cpp)"]
fn catalog_lookup_vlm_and_embedding_bundles() {
    let mlx_vlm = catalog::find("mlx-qwen2-vl").expect("mlx-qwen2-vl should resolve");
    assert_eq!(mlx_vlm.category, v1::ModelCategory::Multimodal);
    assert_eq!(mlx_vlm.framework, v1::InferenceFramework::Mlx);
    assert_eq!(
        mlx_vlm.files.len(),
        11,
        "mlx-qwen2-vl should be a complete MLX VLM bundle"
    );
    assert!(
        mlx_vlm
            .files
            .iter()
            .any(|f| f.filename == "preprocessor_config.json"),
        "MLX VLM catalog entry must include preprocessor_config.json"
    );

    let mlx_fastvlm = catalog::find("mlx-fastvlm").expect("mlx-fastvlm should resolve");
    assert_eq!(mlx_fastvlm.category, v1::ModelCategory::Multimodal);
    assert_eq!(mlx_fastvlm.framework, v1::InferenceFramework::Mlx);
    assert_eq!(
        mlx_fastvlm.files.len(),
        14,
        "mlx-fastvlm should be a complete MLX VLM bundle"
    );
    assert!(
        mlx_fastvlm
            .files
            .iter()
            .any(|f| f.filename == "processor_config.json"),
        "MLX FastVLM catalog entry must include processor config"
    );
    assert!(
        mlx_fastvlm.files.iter().any(|f| !f.required
            && (f.filename == "processing_fastvlm.py" || f.filename == "llava_qwen.py")),
        "MLX FastVLM catalog entry must include its optional companions"
    );

    let mlx_embed = catalog::find("mlx-qwen3-embed").expect("mlx-qwen3-embed should resolve");
    assert_eq!(mlx_embed.category, v1::ModelCategory::Embedding);
    assert_eq!(mlx_embed.framework, v1::InferenceFramework::Mlx);
    assert_eq!(
        mlx_embed.files.len(),
        11,
        "mlx-qwen3-embed should be a complete MLX embedding bundle"
    );

    // Portable llama.cpp GGUF embeddings; hidden entirely on a kit without
    // that backend (platform_supports() in src/catalog/catalog.rs), same as
    // the maple-preview check above.
    #[cfg(wally_has_llamacpp)]
    {
        struct PortableNvidiaEmbeddingCase {
            id: &'static str,
            alias: &'static str,
            revision: &'static str,
            download_size_bytes: i64,
        }
        let cases = [
            PortableNvidiaEmbeddingCase {
                id: "nemotron-3-embed-1b-q4_k_m",
                alias: "nemotron-3-embed",
                revision: "06df1fde6f7009c91f6cc3cd520081921929a678",
                download_size_bytes: 749352096,
            },
            PortableNvidiaEmbeddingCase {
                id: "llama-nemotron-embed-1b-v2-q4_k_m",
                alias: "llama-nemotron-embed",
                revision: "bf7c9832b1d76f86777379e58b7b74805ee58006",
                download_size_bytes: 807690624,
            },
            PortableNvidiaEmbeddingCase {
                id: "llama-embed-nemotron-8b-q4_k_m",
                alias: "llama-embed-nemotron",
                revision: "e7ae3cbae4f7693bbd75ec959bf293f39e1f2e25",
                download_size_bytes: 4625233184,
            },
        ];
        for case in cases {
            let by_id =
                catalog::find(case.id).unwrap_or_else(|| panic!("{} should resolve", case.id));
            let by_alias = catalog::find(case.alias)
                .unwrap_or_else(|| panic!("{} should resolve", case.alias));
            assert_eq!(
                by_id.id, by_alias.id,
                "{} should be an exact pinned llama.cpp embedding",
                case.id
            );
            assert_eq!(by_id.category, v1::ModelCategory::Embedding);
            assert_eq!(by_id.framework, v1::InferenceFramework::LlamaCpp);
            assert_eq!(by_id.format, v1::ModelFormat::Gguf);
            assert!(by_id.files.is_empty());
            let url = by_id.url.unwrap_or_else(|| {
                panic!("{} should be an exact pinned llama.cpp embedding", case.id)
            });
            assert_eq!(by_id.download_size_bytes, case.download_size_bytes);
            assert!(
                url.contains(case.revision),
                "{} should be an exact pinned llama.cpp embedding",
                case.id
            );
        }
    }
}

#[test]
fn overlay_catalog() {
    struct Row {
        id: &'static str,
        alias: &'static str,
        category: v1::ModelCategory,
        framework: v1::InferenceFramework,
    }
    // Every row is conditional; on a kit with no overlay this is empty.
    #[cfg(wally_has_qhexrt)]
    let rows: &[Row] = &[Row {
        id: "lfm2-230m-npu",
        alias: "lfm2_5_230m",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Qhexrt,
    }];
    #[cfg(not(wally_has_qhexrt))]
    let rows: &[Row] = &[];

    for row in rows {
        let by_alias = catalog::find(row.id);
        let by_id = catalog::find(row.alias);
        assert!(
            by_alias.is_some(),
            "{} should be a folder-URL overlay catalog row",
            row.id
        );
        assert_eq!(
            by_alias.map(|e| e.id),
            by_id.map(|e| e.id),
            "{} should be a folder-URL overlay catalog row",
            row.id
        );
        let entry = by_alias.unwrap();
        assert_eq!(entry.category, row.category);
        assert_eq!(entry.framework, row.framework);
        assert!(entry.url.is_some());
    }

    // The other half of the gate: a backend this kit did not ship must not be
    // listed or resolvable, or a user would be offered a model this binary can
    // never run.
    #[cfg(not(wally_has_qhexrt))]
    assert!(
        catalog::find("lfm2-230m-npu").is_none(),
        "lfm2-230m-npu must be hidden in a kit without QHexRT"
    );

    // Both spellings: the row's own alias and the `ane-<merge_key>` form.
    // Unconditional while the ane-cut is in effect.
    assert!(
        catalog::find("lfm2-230m-ane").is_none(),
        "ANE rows must not be listed (ane-cut)"
    );
    assert!(
        catalog::find("ane-lfm2.5-350m").is_none(),
        "ANE rows must not be listed (ane-cut)"
    );
}

#[test]
fn nvidia_sherpa_catalog() {
    // Sherpa-ONNX (speech recognition) is out of scope for the LLM-only cut
    // (src/app.cpp, src/catalog/catalog.cpp): the C++ body is entirely
    // commented out and returns pass immediately.
}

#[test]
fn mlx_catalog_registration() {
    init_sdk_once();

    let rc = catalog::register_all();
    assert_eq!(rc, sys::SUCCESS, "catalog registration failed rc={rc}");

    // MLX is an Apple-only backend; off Apple the catalog deliberately does
    // not register its MLX rows, so the per-model checks below only run on
    // Apple.
    #[cfg(target_os = "macos")]
    {
        let _cleanup = RegisteredModelCleanup {
            ids: vec![
                "mlx-qwen3-0.6b-4bit",
                "mlx-maple-preview-2bit",
                "mlx-llama-3.2-1b-instruct-4bit",
                "mlx-qwen2-vl-2b-instruct-4bit",
                "mlx-fastvlm-0.5b-bf16",
                "mlx-qwen3-embedding-0.6b-4bit-dwq",
                "mlx-qwen3-asr-0.6b-8bit",
                "mlx-glm-asr-nano-2512-4bit",
                "mlx-llama-3.1-nemotron-nano-8b-v1-4bit",
                "mlx-nemotron-mini-4b-instruct-4bit",
                "mlx-parakeet-ctc-1.1b",
                "mlx-parakeet-tdt-0.6b-v2",
                "mlx-parakeet-tdt-0.6b-v3",
                "mlx-parakeet-rnnt-1.1b",
                "mlx-nemotron-3.5-asr-streaming-0.6b-8bit",
                "mlx-qwen3-tts-12hz-0.6b-base-8bit",
                "mlx-soprano-1.1-80m-5bit",
                "sherpa-nemo-parakeet-tdt-0.6b-v2-int8",
                "sherpa-nemo-parakeet-tdt-0.6b-v3-int8",
                "sherpa-nemo-parakeet-ctc-1.1b-int8",
                "sherpa-nemo-canary-180m-flash-int8",
                "sherpa-nemotron-3.5-asr-streaming-0.6b-320ms-int8",
            ],
        };

        let qwen =
            get_registered_model("mlx-qwen3-0.6b-4bit").expect("registered MLX Qwen3 metadata");
        assert_eq!(qwen.framework, v1::InferenceFramework::Mlx as i32);
        assert_eq!(qwen.format, v1::ModelFormat::Safetensors as i32);
        assert_eq!(qwen.category, v1::ModelCategory::Language as i32);
        assert_eq!(
            multi_file_count(&qwen),
            Some(9),
            "registered MLX Qwen3 metadata is incomplete"
        );
        assert_eq!(qwen.download_size_bytes, 351383618);
        assert!(qwen.supports_thinking);

        let maple =
            get_registered_model("mlx-maple-preview-2bit").expect("registered MLX Maple metadata");
        assert_eq!(maple.framework, v1::InferenceFramework::Mlx as i32);
        assert_eq!(maple.format, v1::ModelFormat::Safetensors as i32);
        assert_eq!(maple.category, v1::ModelCategory::Language as i32);
        assert_eq!(
            multi_file_count(&maple),
            Some(13),
            "registered MLX Maple metadata is incomplete"
        );
        assert_eq!(maple.download_size_bytes, 5330252282);
        assert_eq!(maple.context_length, 128000);

        // VLM, embedding and ASR registrations are out of scope for the
        // LLM-only cut (src/app.cpp, src/catalog/catalog.cpp) -- left out,
        // not deleted, so this comes back when the cut reverts.

        struct RegisteredNvidiaCase {
            id: &'static str,
            expected_files: usize,
            expected_size: i64,
        }
        let registered_nvidia_cases = [
            RegisteredNvidiaCase {
                id: "mlx-llama-3.1-nemotron-nano-8b-v1-4bit",
                expected_files: 8,
                expected_size: 4534806075,
            },
            RegisteredNvidiaCase {
                id: "mlx-nemotron-mini-4b-instruct-4bit",
                expected_files: 6,
                expected_size: 2392679103,
            },
        ];
        for case in registered_nvidia_cases {
            let model = get_registered_model(case.id).unwrap_or_else(|e| {
                panic!(
                    "registered NVIDIA MLX metadata is incomplete: {}: {e}",
                    case.id
                )
            });
            assert_eq!(model.framework, v1::InferenceFramework::Mlx as i32);
            assert_eq!(model.format, v1::ModelFormat::Safetensors as i32);
            assert_eq!(
                multi_file_count(&model),
                Some(case.expected_files),
                "registered NVIDIA MLX metadata is incomplete: {}",
                case.id
            );
            assert_eq!(model.download_size_bytes, case.expected_size);
        }

        // Sherpa-ONNX (speech recognition) and TTS registrations are out of
        // scope for the LLM-only cut (src/app.cpp, src/catalog/catalog.cpp)
        // -- left out, not deleted, so this comes back when the cut reverts.
    }
    #[cfg(not(target_os = "macos"))]
    {
        // The inverse of the Apple assertions above: register_all()
        // (src/catalog/catalog.rs) skips every MLX row off Apple via
        // platform_supports(), so a registry lookup for one must fail here.
        assert!(
            get_registered_model("mlx-qwen3-0.6b-4bit").is_err(),
            "mlx-qwen3-0.6b-4bit should not be registered off Apple"
        );
    }
}

// HF explicit-file refs normalize inside commons
// (rac_register_model_from_url_proto) now — verify through the production ABI
// that the saved entry carries the expected resolve/main download URL.
// Explicit-file refs never hit the network (only repo-level refs do, and none
// appear here).
#[test]
fn hf_ref_registration() {
    init_sdk_once();

    struct Case {
        input: &'static str,
        expected_download_url: &'static str,
    }
    let cases = [
        Case {
            input: "hf.co/Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf",
            expected_download_url:
                "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf",
        },
        Case {
            input: "huggingface.co/org/repo/sub/dir/file.gguf",
            expected_download_url: "https://huggingface.co/org/repo/resolve/main/sub/dir/file.gguf",
        },
        Case {
            input: "https://huggingface.co/org/repo/resolve/main/f.gguf",
            expected_download_url: "https://huggingface.co/org/repo/resolve/main/f.gguf",
        },
        Case {
            input: "https://huggingface.co/org/repo/blob/main/sub/f.gguf",
            expected_download_url: "https://huggingface.co/org/repo/resolve/main/sub/f.gguf",
        },
        Case {
            input: "https://example.com/m.gguf",
            expected_download_url: "https://example.com/m.gguf",
        },
    ];

    for case in cases {
        let request = v1::RegisterModelFromUrlRequest {
            url: case.input.to_string(),
            ..Default::default()
        };
        let bytes = serialize(&request);
        let mut out = ProtoBuffer::new();
        // SAFETY: `bytes` is alive for the call; `out` is a freshly-initialised buffer.
        let rc = unsafe {
            sys::rac_register_model_from_url_proto(bytes.as_ptr(), bytes.len(), out.as_mut_ptr())
        };
        assert_eq!(rc, sys::SUCCESS, "input: {}", case.input);
        let saved: v1::ModelInfo =
            parse_proto_buffer(out).unwrap_or_else(|e| panic!("input: {}: {e}", case.input));
        assert_eq!(
            saved.download_url, case.expected_download_url,
            "input: {}",
            case.input
        );
    }
}
