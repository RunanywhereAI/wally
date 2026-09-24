//! `wally image generate` — text-to-image via NeuRT (Core ML diffusion). Port
//! of src/commands/cmd_image.cpp. Owner: the dormant vision/text modalities
//! port. Not registered in the app (LLM-only release); ported completely
//! anyway per the migration brief.
//!
//! Canonical SDK flow, all heavy lifting in commons (mirrors cmd_run/cmd_embed):
//!   rac_model_lifecycle_load_proto(category=IMAGE_GENERATION, validate=true)
//!     -> auto-pulls / resolves the diffusion bundle and loads the coreml engine.
//!   rac_diffusion_generate_lifecycle_proto(DiffusionGenerationRequest)
//!     -> resolves the lifecycle-loaded model internally and returns a
//!        DiffusionResult (raw RGBA image_data + width/height).
//! The command only translates argv <-> proto bytes and writes the decoded image
//! to --out as PNG. Diffusion is NeuRT (Apple Neural Engine / Core ML); on other
//! platforms the command returns a clear error rather than crashing.
//!
//! A `--model` that names an existing on-disk path is registered as a local
//! CoreML bundle (directory of compiled .mlmodelc sub-models) and loaded without
//! a download -- the reliable path for a pre-fetched Apple SD bundle.

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, ValueType};
use crate::io::output::error_line;
#[cfg(wally_has_neurt)]
use crate::io::output::{result_line, status_line, JsonWriter};
#[cfg(wally_has_neurt)]
use crate::io::proto::{serialize, v1};

// Default diffusion model -- the built-in CoreML Stable Diffusion 1.5 catalog id
// (see catalog.cpp). Overridable with --model (catalog id / registered id / a
// local path to a compiled CoreML bundle).
const DEFAULT_DIFFUSION_MODEL: &str = "stable-diffusion-v1-5-coreml";

#[cfg(wally_has_neurt)]
fn path_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

// Register an already-present local CoreML bundle so the lifecycle loader can
// resolve it by id (no download). Directory resolution is a CLI concern.
#[cfg(wally_has_neurt)]
fn register_local_bundle(path: &str) -> Result<String, String> {
    use crate::io::output::describe_result;
    use crate::sys;

    let model_id = "local-diffusion-coreml".to_string();
    let model = v1::ModelInfo {
        id: model_id.clone(),
        name: "Local CoreML Diffusion".to_string(),
        category: v1::ModelCategory::ImageGeneration as i32,
        framework: v1::InferenceFramework::Coreml as i32,
        format: v1::ModelFormat::Mlpackage as i32,
        local_path: path.to_string(),
        source: v1::ModelSource::Local as i32,
        ..Default::default()
    };

    let bytes = serialize(&model);
    // SAFETY: rac_get_model_registry() returns the process-wide registry handle
    // (valid once bootstrap() has run); bytes is a valid slice for its own
    // length for the duration of the call.
    let rc = unsafe {
        sys::rac_model_registry_register_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
        )
    };
    if rc != sys::SUCCESS {
        return Err(format!(
            "failed to register local model: {}",
            describe_result(rc)
        ));
    }
    Ok(model_id)
}

#[cfg(wally_has_neurt)]
fn load_diffusion_model(
    options: &GlobalOptions,
    model_id: &str,
    validate_availability: bool,
) -> bool {
    use crate::io::proto::{parse_proto_buffer, ProtoBuffer};
    use crate::progress::progress_bar::DownloadProgressScope;
    use crate::sys;

    let _progress_scope =
        DownloadProgressScope::new(model_id, !options.no_progress && !options.json);
    let request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        category: Some(v1::ModelCategory::ImageGeneration as i32),
        validate_availability,
        ..Default::default()
    };

    let bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry handle
    // (valid once bootstrap() has run); out_buffer.as_mut_ptr() points at a
    // live, initialized rac_proto_buffer_t for the duration of the call.
    let proto_rc = unsafe {
        sys::rac_model_lifecycle_load_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result = match parse_proto_buffer::<v1::ModelLoadResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            // Matches cmd_image.cpp: parse_proto_buffer only ever writes
            // `error` on its own failure path. When parsing succeeds but
            // proto_rc still disagrees, C++'s `error` stays empty, so the
            // printed line is the bare prefix with nothing after the colon.
            error_line("diffusion model load failed: ");
            return false;
        }
        Err(error) => {
            error_line(&format!("diffusion model load failed: {error}"));
            return false;
        }
    };
    if let Some(error) = &result.error {
        let message = if error.message.is_empty() {
            "unknown error"
        } else {
            error.message.as_str()
        };
        error_line(&format!("diffusion model load failed: {message}"));
        return false;
    }
    if options.verbose {
        status_line(&format!("loaded {}", result.resolved_path));
    }
    true
}

// image_data/image_media_type/width/height/seed_used/error moved off
// DiffusionResult onto DiffusionResult.images[0] (a DiffusionImage) --
// commons emits exactly one entry until the C ABI grows a list. Errors now
// travel out-of-band via the rac_proto_buffer_t status envelope (checked by
// parse_proto_buffer before this function is ever called).
#[cfg(wally_has_neurt)]
fn write_image(image_result: &v1::DiffusionImage, out_path: &str) -> Result<(), String> {
    use crate::io::image_io;

    let image = &image_result.data;
    if image.is_empty() {
        return Err("engine returned no image data".to_string());
    }
    // Every shipped C-ABI diffusion engine emits raw RGBA (media_type
    // "image/raw-rgba"); encode it to PNG. A future backend returning an
    // encoded container writes through verbatim.
    let is_raw_rgba = image_result.media_type == "image/raw-rgba"
        || (image_result.width > 0
            && image_result.height > 0
            && image.len() == image_result.width as usize * image_result.height as usize * 4);
    if is_raw_rgba {
        let expected = image_result.width as usize * image_result.height as usize * 4;
        if image.len() != expected {
            return Err("raw RGBA buffer length does not match width*height*4".to_string());
        }
        return image_io::write_png(out_path, image, image_result.width, image_result.height);
    }
    std::fs::write(out_path, image).map_err(|_| format!("cannot write {out_path}"))
}

#[allow(clippy::too_many_arguments)]
fn run_image_generate(
    options: &GlobalOptions,
    model: &str,
    prompt: &str,
    negative_prompt: &str,
    out_path: &str,
    steps: i32,
    guidance: f64,
    seed: i64,
) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }
    run_image_generate_body(
        options,
        model,
        prompt,
        negative_prompt,
        out_path,
        steps,
        guidance,
        seed,
    )
}

#[cfg(not(wally_has_neurt))]
#[allow(clippy::too_many_arguments, unused_variables)]
fn run_image_generate_body(
    options: &GlobalOptions,
    model: &str,
    prompt: &str,
    negative_prompt: &str,
    out_path: &str,
    steps: i32,
    guidance: f64,
    seed: i64,
) -> i32 {
    error_line("image generation (diffusion) requires NeuRT (Apple Neural Engine)");
    1
}

#[cfg(wally_has_neurt)]
#[allow(clippy::too_many_arguments)]
fn run_image_generate_body(
    options: &GlobalOptions,
    model: &str,
    prompt: &str,
    negative_prompt: &str,
    out_path: &str,
    steps: i32,
    guidance: f64,
    seed: i64,
) -> i32 {
    use crate::catalog::model_ref;
    use crate::io::proto::{parse_proto_buffer, ProtoBuffer};
    use crate::sys;

    if prompt.is_empty() {
        error_line("--prompt is required");
        return 2;
    }
    if out_path.is_empty() {
        error_line("--out is required");
        return 2;
    }

    // Resolve the model: an existing on-disk path is a local CoreML bundle;
    // otherwise fall through to the catalog / registry / URL resolver.
    let model_id;
    let mut validate_availability = true;
    if path_exists(model) {
        model_id = match register_local_bundle(model) {
            Ok(id) => id,
            Err(error) => {
                error_line(&error);
                return 1;
            }
        };
        validate_availability = false; // already on disk
    } else {
        let resolve_options = model_ref::ResolveOptions {
            has_category: true,
            category: v1::ModelCategory::ImageGeneration,
            has_framework: true,
            framework: v1::InferenceFramework::Coreml,
        };
        model_id = match model_ref::resolve(model, Some(&resolve_options)) {
            Ok(resolved) => resolved.model_id,
            Err((_, error)) => {
                error_line(&error);
                return 1;
            }
        };
    }

    if !load_diffusion_model(options, &model_id, validate_availability) {
        return 1;
    }

    let mut gen_options = v1::DiffusionGenerationOptions {
        prompt: prompt.to_string(),
        seed: Some(seed),
        ..Default::default()
    };
    if !negative_prompt.is_empty() {
        gen_options.negative_prompt = negative_prompt.to_string();
    }
    if steps > 0 {
        gen_options.steps = steps;
    }
    if guidance > 0.0 {
        gen_options.guidance_scale = guidance as f32;
    }

    let request = v1::DiffusionGenerationRequest {
        options: Some(gen_options),
        model_id: Some(model_id.clone()),
    };

    let bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes is a valid slice for its own length; out_buffer.as_mut_ptr()
    // points at a live, initialized rac_proto_buffer_t.
    let proto_rc = unsafe {
        sys::rac_diffusion_generate_lifecycle_proto(
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // DiffusionResult carries no error field of its own any more -- failures
    // travel out-of-band on the rac_proto_buffer_t status envelope, already
    // checked below by parse_proto_buffer. commons emits exactly one image.
    let result = match parse_proto_buffer::<v1::DiffusionResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            // Matches cmd_image.cpp: parse_proto_buffer only ever writes
            // `error` on its own failure path. When parsing succeeds but
            // proto_rc still disagrees, C++'s `error` stays empty, so the
            // printed line is the bare prefix with nothing after the colon.
            error_line("image generation failed: ");
            return 1;
        }
        Err(error) => {
            error_line(&format!("image generation failed: {error}"));
            return 1;
        }
    };
    if result.images.is_empty() {
        error_line("image generation failed: engine returned no image");
        return 1;
    }
    let image_result = &result.images[0];

    if let Err(error) = write_image(image_result, out_path) {
        error_line(&format!("failed to write image: {error}"));
        return 1;
    }

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        json.field_str("model", &model_id)
            .field_str("output", out_path)
            .field_i64("width", i64::from(image_result.width))
            .field_i64("height", i64::from(image_result.height))
            .field_i64("seed", image_result.seed_used)
            .field_i64("total_ms", result.total_time_ms);
        json.end_object();
        result_line(json.str());
    } else {
        result_line(out_path);
        if options.verbose {
            status_line(&format!(
                "({}x{}, seed {}, {} ms)",
                image_result.width,
                image_result.height,
                image_result.seed_used,
                result.total_time_ms
            ));
        }
    }
    0
}

pub fn register_image(app: &mut App) {
    let cmd = app.add_subcommand(
        "image",
        "Make images from text (CoreML diffusion, Apple only)",
    );
    cmd.require_subcommand(1, 1);

    let generate = cmd.add_subcommand("generate", "Render an image from a prompt");
    generate
        .add_option(
            "--model,-m",
            ValueType::Text,
            &format!(
                "Diffusion model id, a registered id, or a local CoreML bundle path (default: {DEFAULT_DIFFUSION_MODEL})"
            ),
        )
        .default_val(DEFAULT_DIFFUSION_MODEL);
    generate
        .add_option("--prompt,-p", ValueType::Text, "What to draw")
        .required();
    generate.add_option(
        "--negative-prompt,--negative",
        ValueType::Text,
        "What to keep out of the image",
    );
    generate.add_option(
        "--steps",
        ValueType::Int,
        "Denoising steps to run (0 = model default)",
    );
    generate.add_option(
        "--guidance-scale,--guidance",
        ValueType::Float,
        "How closely to follow the prompt (0 = model default)",
    );
    generate.add_option(
        "--seed",
        ValueType::Int64,
        "Fix the RNG for a repeatable image (-1 = random)",
    );
    generate
        .add_option("--output,-o,--out", ValueType::Text, "PNG file to write")
        .required();

    generate.callback(|p, g| {
        run_image_generate(
            g,
            &p.get_str("--model")
                .unwrap_or_else(|| DEFAULT_DIFFUSION_MODEL.to_string()),
            &p.get_str("--prompt").unwrap_or_default(),
            &p.get_str("--negative-prompt").unwrap_or_default(),
            &p.get_str("--output").unwrap_or_default(),
            p.get_i64("--steps").unwrap_or(0) as i32,
            p.get_f64("--guidance-scale").unwrap_or(0.0),
            p.get_i64("--seed").unwrap_or(-1),
        )
    });
}
