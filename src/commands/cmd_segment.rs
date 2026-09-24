//! `wally segment <image.ppm> --model <path>` — segmentation via the commons
//! segmentation service (image-in → per-class mask summary). Port of
//! src/commands/cmd_segment.cpp. Owner: the dormant vision/text modalities
//! port. Not registered in the app (LLM-only release); ported completely
//! anyway per the migration brief.
//!
//! Mirrors cmd_image's structure (bootstrap → resolve model → one commons path
//! → render) but consumes an image instead of producing one. The model
//! lifecycle is the standard service handle sequence the C ABI exposes:
//!   rac_segmentation_create(model)  → route to the ONNX SegFormer provider
//!     → rac_segmentation_initialize(model_path)  → load the ONNX bundle
//!     → rac_segmentation_segment(image, …, &result)  → source-sized class
//!       mask + per-class summaries
//!     → rac_segmentation_result_free / rac_segmentation_destroy
//! A `--model` naming an on-disk path is used verbatim; otherwise it is a
//! catalog id resolved + auto-pulled through commons. Input is a binary PPM
//! (P6); it is the dependency-free image format the CLI can decode without
//! libpng/libjpeg (`magick in.png out.ppm`). All preprocessing/inference
//! lives in the engine.

use std::ffi::{CStr, CString};

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, Validator, ValueType};
use crate::commands::model_setup::ensure_model_ready;
use crate::io::image_io;
use crate::io::output::{describe_result, error_line, result_line, status_line, table, JsonWriter};
use crate::sys;

/// Resolve through ensure_model_ready so a catalog id is never shadowed by a
/// same-named file in cwd. Local paths still work: model_ref requires a
/// separator (or Windows drive) before treating a ref as an on-disk path.
fn resolve_model_path(options: &GlobalOptions, reference: &str) -> Result<String, i32> {
    let model = ensure_model_ready(options, reference)?;
    Ok(model.primary_path)
}

/// Reads a possibly-null C string; `None` when the pointer is null.
fn opt_cstr(ptr: *const std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a NUL-terminated string alive for this call.
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

fn print_result(options: &GlobalOptions, model_ref: &str, result: &sys::rac_segmentation_result_t) {
    let model_id = opt_cstr(result.model_id).unwrap_or_else(|| model_ref.to_string());
    // SAFETY: class_summaries points at class_summary_count valid entries,
    // owned by `result` for the duration of this call.
    let classes: &[sys::rac_segmentation_class_summary_t] = if result.class_summaries.is_null() {
        &[]
    } else {
        unsafe {
            std::slice::from_raw_parts(result.class_summaries, result.class_summary_count)
        }
    };

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        json.field_str("model", &model_id)
            .field_i64("width", i64::from(result.width))
            .field_i64("height", i64::from(result.height))
            .field_i64("class_count", classes.len() as i64)
            .field_i64("processing_time_ms", result.processing_time_ms);
        json.begin_array("classes");
        for cls in classes {
            let label = opt_cstr(cls.label).unwrap_or_default();
            json.begin_array_object();
            json.field_i64("class_id", i64::from(cls.class_id))
                .field_str("label", &label)
                .field_i64("pixel_count", cls.pixel_count as i64)
                .field_f64("fraction", f64::from(cls.fraction));
            json.end_object();
        }
        json.end_array();
        json.end_object();
        result_line(json.str());
        return;
    }

    result_line(&format!("size\t{}x{}", result.width, result.height));
    if classes.is_empty() {
        result_line("(no classes)");
    } else {
        let header = ["class_id", "label", "pixels", "coverage"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let mut rows = Vec::with_capacity(classes.len());
        for cls in classes {
            let label = opt_cstr(cls.label).unwrap_or_default();
            let pct = format!("{:.1}%", f64::from(cls.fraction) * 100.0);
            rows.push(vec![
                cls.class_id.to_string(),
                label,
                cls.pixel_count.to_string(),
                pct,
            ]);
        }
        table(&header, &rows);
    }
    if options.verbose {
        status_line(&format!("({} ms)", result.processing_time_ms));
    }
}

fn run_segment(
    options: &GlobalOptions,
    image_path: &str,
    model_ref: &str,
    diagnostic_path: &str,
) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    if model_ref.is_empty() {
        error_line("--model is required (a segmentation model id or on-disk path)");
        return 2;
    }

    let model_path = match resolve_model_path(options, model_ref) {
        Ok(path) => path,
        Err(exit_code) => return exit_code,
    };

    let decoded = match image_io::read_ppm(image_path) {
        Ok(decoded) => decoded,
        Err(error) => {
            error_line(&error);
            return 1;
        }
    };

    let model_path_c = match CString::new(model_path.as_str()) {
        Ok(c) => c,
        Err(_) => {
            error_line("model path contains a NUL byte");
            return 1;
        }
    };

    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: model_path_c is a valid NUL-terminated string for the call;
    // `handle` is a valid out-pointer.
    let rc = unsafe { sys::rac_segmentation_create(model_path_c.as_ptr(), &mut handle) };
    if rc != sys::SUCCESS || handle.is_null() {
        error_line(&format!(
            "failed to create segmentation service: {}",
            describe_result(rc)
        ));
        return 1;
    }

    // SAFETY: handle is the live service created above; model_path_c is valid
    // for the duration of the call.
    let rc = unsafe { sys::rac_segmentation_initialize(handle, model_path_c.as_ptr()) };
    if rc != sys::SUCCESS {
        error_line(&format!(
            "failed to load segmentation model: {}",
            describe_result(rc)
        ));
        // SAFETY: handle is the live service created above.
        unsafe { sys::rac_segmentation_destroy(handle) };
        return 1;
    }

    let image = sys::rac_segmentation_image_t {
        data: decoded.rgb.as_ptr(),
        data_size: decoded.rgb.len(),
        width: decoded.width,
        height: decoded.height,
        stride_bytes: decoded.width as usize * 3,
        pixel_format: sys::RAC_SEGMENTATION_PIXEL_FORMAT_RGB8,
    };

    // SAFETY: RAC_SEGMENTATION_OPTIONS_DEFAULT is a plain-data extern static
    // provided by the kit, valid to copy.
    let mut seg_options = unsafe { sys::RAC_SEGMENTATION_OPTIONS_DEFAULT };
    seg_options.include_diagnostic_rgba = if diagnostic_path.is_empty() {
        sys::FALSE
    } else {
        sys::TRUE
    };

    // SAFETY: result is zero-initialized plain data; the kit fills it in.
    let mut result: sys::rac_segmentation_result_t = unsafe { std::mem::zeroed() };
    // SAFETY: handle is the live, initialized service; image/seg_options are
    // valid for the call, and decoded.rgb (pointed to by image.data) outlives
    // it.
    let rc = unsafe { sys::rac_segmentation_segment(handle, &image, &seg_options, &mut result) };
    if rc != sys::SUCCESS {
        error_line(&format!("segmentation failed: {}", describe_result(rc)));
        // SAFETY: result was zero-initialized/filled by the kit above; handle
        // is the live service created above.
        unsafe {
            sys::rac_segmentation_result_free(&mut result);
            sys::rac_segmentation_cleanup(handle);
            sys::rac_segmentation_destroy(handle);
        }
        return 1;
    }

    print_result(options, model_ref, &result);
    if !diagnostic_path.is_empty() {
        if result.diagnostic_rgba.is_null() || result.diagnostic_rgba_size == 0 {
            status_line("warning: engine returned no diagnostic image");
        } else {
            // SAFETY: diagnostic_rgba points at diagnostic_rgba_size valid
            // bytes, owned by `result` for the duration of this call.
            let rgba = unsafe {
                std::slice::from_raw_parts(result.diagnostic_rgba, result.diagnostic_rgba_size)
            };
            if let Err(error) = image_io::write_png(
                diagnostic_path,
                rgba,
                result.width as i32,
                result.height as i32,
            ) {
                error_line(&format!("failed to write {diagnostic_path}: {error}"));
                // SAFETY: as above.
                unsafe {
                    sys::rac_segmentation_result_free(&mut result);
                    sys::rac_segmentation_cleanup(handle);
                    sys::rac_segmentation_destroy(handle);
                }
                return 1;
            }
        }
    }

    // SAFETY: result and handle are still live and owned here; this is the
    // single cleanup path for the success case.
    unsafe {
        sys::rac_segmentation_result_free(&mut result);
        sys::rac_segmentation_cleanup(handle);
        sys::rac_segmentation_destroy(handle);
    }
    0
}

pub fn register_segment(app: &mut App) {
    let cmd = app.add_subcommand("segment", "Label every pixel of an image by class");
    cmd.add_option("image", ValueType::Text, "Input image (binary PPM / P6)")
        .required()
        .check(Validator::ExistingFile);
    cmd.add_option("--model,-m", ValueType::Text, "Segmentation model id or on-disk path")
        .required();
    cmd.add_option(
        "--diagnostic-image",
        ValueType::Text,
        "Also write the colored mask overlay to this PNG",
    );

    cmd.callback(|p, g| {
        run_segment(
            g,
            &p.get_str("image").unwrap_or_default(),
            &p.get_str("--model").unwrap_or_default(),
            &p.get_str("--diagnostic-image").unwrap_or_default(),
        )
    });
}
