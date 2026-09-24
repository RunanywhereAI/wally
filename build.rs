//! Build-time wiring between the Rust crate, `versions.toml` and the CMake-resolved
//! SDK kit.
//!
//! CMake owns the kit: it finds it (or fetches it), checks the pins, and works out
//! the platform link closure. At configure time it writes
//! `build/generated/wally-build.env` (see cmake/WallyRust.cmake). This script reads
//! that file — never the command line — for:
//!
//! - `KIT_IDL_DIR`: the kit's `share/runanywhere/idl`. The Rust message types are
//!   generated from those exact `.proto` files by prost over a protox (pure Rust)
//!   compile, after the directory's SCHEMA_LOCK hash is checked against the pin in
//!   versions.toml. No protoc runs and nothing generated is committed.
//! - `HAS_*`: the engine/component capability flags the C++ build got as
//!   `WALLY_HAS_*` compile definitions; each becomes `cfg(wally_has_*)`.
//! - `DEFAULT_MODEL_ID`, `BAKED_CONSOLE_API_URL`, `BAKED_CONSOLE_WEB_ORIGIN`:
//!   exported to the crate as compile-time env values. The endpoints are only ever
//!   set for a dev-channel build; they live in the build tree, as the generated
//!   C++ header did, and are never printed.
//! - `CMAKE_FILE_API_REPLY` / `LINK_PROBE_TARGET`: where CMake recorded the link
//!   line of `wally_link_probe` — the kit closure the C++ `wally` executable had.
//!   The static library needs none of it; every binary cargo links does.
//!
//! Point `WALLY_BUILD_ENV` at another file to use a different CMake build dir.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=WALLY_BUILD_ENV");
    println!("cargo:rerun-if-changed=versions.toml");
    println!("cargo:rerun-if-changed=build.rs");

    let versions = read_versions(&manifest.join("versions.toml"));
    let product = versions
        .get("version")
        .expect("versions.toml is missing the product version");
    let cargo_version = env::var("CARGO_PKG_VERSION").unwrap();
    assert_eq!(
        product, &cargo_version,
        "Cargo.toml version {cargo_version} does not match versions.toml version {product}; \
         bump them together"
    );
    println!("cargo:rustc-env=WALLY_VERSION={product}");
    println!(
        "cargo:rustc-env=WALLY_PINNED_SDK_VERSION={}",
        versions
            .get("kit_version")
            .expect("versions.toml is missing kit_version")
    );
    // `wally about`/`wally version` show the pinned IDL schema (RUNANYWHERE_IDL_VERSION /
    // RUNANYWHERE_IDL_SCHEMA_SHA256 / RUNANYWHERE_IDL_PROTOC_VERSION in the C++ build, baked
    // from schema_lock.h). versions.toml is the same pin these macros are generated from, so
    // export it the same way as the other pins above instead of bridging the C header.
    for key in ["idl_version", "idl_schema_sha256", "idl_protoc_version"] {
        println!(
            "cargo:rustc-env=WALLY_{}={}",
            key.to_ascii_uppercase(),
            versions
                .get(key)
                .unwrap_or_else(|| panic!("versions.toml is missing {key}"))
        );
    }

    let env_file = env::var_os("WALLY_BUILD_ENV")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("build/generated/wally-build.env"));
    println!("cargo:rerun-if-changed={}", env_file.display());
    let build_env = match fs::read_to_string(&env_file) {
        Ok(text) => parse_env(&text),
        Err(_) => panic!(
            "{} not found. Configure CMake first so the SDK kit is resolved:\n  \
             cmake -B build -G Ninja -DCMAKE_PREFIX_PATH=/path/to/kit\n\
             or set WALLY_BUILD_ENV to the wally-build.env of another build dir.",
            env_file.display()
        ),
    };

    // Capability flags → cfg(wally_has_*), declared so check-cfg knows them.
    const CAPABILITIES: &[&str] = &[
        "LLAMACPP", "ONNX", "SHERPA", "MLX", "CLOUD", "NEURT", "QHEXRT", "RAG", "SERVER",
    ];
    for cap in CAPABILITIES {
        let name = format!("wally_has_{}", cap.to_ascii_lowercase());
        println!("cargo::rustc-check-cfg=cfg({name})");
        if build_env.get(&format!("HAS_{cap}")).map(String::as_str) == Some("1") {
            println!("cargo:rustc-cfg={name}");
        }
    }

    let default_model = build_env
        .get("DEFAULT_MODEL_ID")
        .filter(|v| !v.is_empty())
        .cloned()
        .unwrap_or_else(|| "glm-5.3-flash".to_string());
    println!("cargo:rustc-env=WALLY_DEFAULT_MODEL_ID={default_model}");
    for key in ["BAKED_CONSOLE_API_URL", "BAKED_CONSOLE_WEB_ORIGIN"] {
        let value = build_env.get(key).cloned().unwrap_or_default();
        println!("cargo:rustc-env=WALLY_{key}={value}");
    }

    let idl = PathBuf::from(
        build_env
            .get("KIT_IDL_DIR")
            .unwrap_or_else(|| panic!("{} has no KIT_IDL_DIR", env_file.display())),
    );
    generate_proto(&idl, &versions);

    link_native(&build_env);
}

/// `key = "value"` lines from versions.toml (flat by design; see its header).
fn read_versions(path: &Path) -> BTreeMap<String, String> {
    let text = fs::read_to_string(path).expect("cannot read versions.toml");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, rest)) = line.split_once('=') {
            let rest = rest.trim();
            if let Some(value) = rest.strip_prefix('"').and_then(|r| r.split_once('"')) {
                out.entry(key.trim().to_string())
                    .or_insert_with(|| value.0.to_string());
            }
        }
    }
    out
}

fn parse_env(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim_end_matches(['\r', '\n']).to_string(),
            )
        })
        .collect()
}

/// `\r\n` → `\n`; any other byte, lone `\r` included, is kept.
fn crlf_to_lf(bytes: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Verify the kit's IDL against the pin, then generate `runanywhere.v1` types.
///
/// IDL_SCHEMA_SHA256 is sha256 over "<basename>\n<sha256-of-contents>\n" for every
/// idl/*.proto in C-locale basename order (the SDK's idl/codegen/schema_lock.sh).
fn generate_proto(idl: &Path, versions: &BTreeMap<String, String>) {
    println!("cargo:rerun-if-changed={}", idl.display());
    let mut protos: Vec<PathBuf> = fs::read_dir(idl)
        .unwrap_or_else(|e| panic!("cannot read kit IDL dir {}: {e}", idl.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "proto"))
        .collect();
    protos.sort_by(|a, b| {
        a.file_name()
            .unwrap()
            .as_encoded_bytes()
            .cmp(b.file_name().unwrap().as_encoded_bytes())
    });

    let mut lock = Sha256::new();
    for proto in &protos {
        // The Windows kits ship their .proto files with CRLF line endings;
        // the lock is defined over the LF text the SDK hashed.
        let contents = crlf_to_lf(fs::read(proto).unwrap());
        let name = proto.file_name().unwrap().to_str().unwrap();
        lock.update(format!("{name}\n{}\n", hex::encode(Sha256::digest(&contents))).as_bytes());
    }
    let actual = hex::encode(lock.finalize());
    let pinned = versions
        .get("idl_schema_sha256")
        .expect("versions.toml is missing idl_schema_sha256");
    assert_eq!(
        &actual,
        pinned,
        "the kit IDL at {} does not match versions.toml (kit {actual}, pin {pinned}). \
         Consume the kit that matches the pin, or bump the pin after a schema change. \
         wally never regenerates the schema itself.",
        idl.display()
    );

    let fds = protox::compile(&protos, [idl]).expect("protox could not compile the kit IDL");
    prost_build::Config::new()
        .enable_type_names()
        .compile_fds(fds)
        .expect("prost-build failed on the kit IDL");
}

/// Hand the kit's link line to every artifact cargo itself links: integration
/// tests, unit-test harnesses and the binary. CMake computed it for
/// `wally_link_probe` (cmake/WallyRust.cmake); its file API reply records the
/// fragments in link order.
fn link_native(build_env: &BTreeMap<String, String>) {
    let (Some(reply), Some(target)) = (
        build_env
            .get("CMAKE_FILE_API_REPLY")
            .filter(|p| !p.is_empty()),
        build_env.get("LINK_PROBE_TARGET").filter(|p| !p.is_empty()),
    ) else {
        return;
    };
    let config = build_env
        .get("LINK_PROBE_CONFIG")
        .cloned()
        .unwrap_or_default();
    let reply = Path::new(reply);
    println!("cargo:rerun-if-changed={}", reply.display());
    let args = probe_link_args(reply, target, &config);
    for arg in &args {
        println!("cargo:rustc-link-arg={arg}");
    }
    // The Swift MLX host (scripts/build/build-mlx.sh) links the static library
    // itself and needs the same list; CMake names where to leave it.
    println!("cargo:rerun-if-env-changed=WALLY_NATIVE_LINK_ARGS_OUT");
    if let Some(out) = env::var_os("WALLY_NATIVE_LINK_ARGS_OUT") {
        let mut text = args.join("\n");
        text.push('\n');
        fs::write(&out, text)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", PathBuf::from(&out).display()));
    }
    // The kit is C++; CMake links through the C++ driver, which adds the C++
    // runtime implicitly. cargo links with the C driver, so name it.
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") | Ok("ios") => println!("cargo:rustc-link-arg=-lc++"),
        Ok("linux") => println!("cargo:rustc-link-arg=-lstdc++"),
        _ => {}
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} (re-run the CMake configure)",
            path.display()
        )
    });
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()))
}

fn probe_link_args(reply: &Path, target: &str, config: &str) -> Vec<String> {
    let mut indexes: Vec<PathBuf> = fs::read_dir(reply)
        .unwrap_or_else(|e| {
            panic!(
                "no CMake file API reply at {}: {e}. Re-run the CMake configure (CMake older \
                 than 3.27 writes the reply from the second configure on).",
                reply.display()
            )
        })
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("index-"))
        })
        .collect();
    indexes.sort();
    let index = read_json(
        indexes
            .last()
            .expect("the CMake file API reply has no index"),
    );
    let codemodel_file = index["reply"]["codemodel-v2"]["jsonFile"]
        .as_str()
        .expect("the CMake file API reply has no codemodel-v2");
    let codemodel = read_json(&reply.join(codemodel_file));
    let configurations = codemodel["configurations"]
        .as_array()
        .expect("codemodel without configurations");
    let configuration = configurations
        .iter()
        .find(|c| c["name"].as_str() == Some(config))
        .or_else(|| configurations.first())
        .expect("codemodel without a configuration");
    let target_file = configuration["targets"]
        .as_array()
        .and_then(|targets| targets.iter().find(|t| t["name"].as_str() == Some(target)))
        .and_then(|t| t["jsonFile"].as_str())
        .unwrap_or_else(|| panic!("the CMake codemodel has no target {target}"));
    let target_json = read_json(&reply.join(target_file));
    let fragments = target_json["link"]["commandFragments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut args = Vec::new();
    for fragment in &fragments {
        if let Some(text) = fragment["fragment"].as_str() {
            args.extend(
                split_fragment(text)
                    .into_iter()
                    .filter(|a| !is_compile_only(a)),
            );
        }
    }
    args
}

/// GCC-style drivers get CMAKE_CXX_FLAGS on the link line too ("-O3 -DNDEBUG");
/// harmless to a driver, fatal once passed straight to ld (the Swift host's
/// -Xlinker path). Keep only what linking needs.
fn is_compile_only(arg: &str) -> bool {
    if arg.starts_with("-Wl,") {
        return false;
    }
    if let Some(msvc) = arg.strip_prefix('/') {
        return is_msvc_compile_only(msvc);
    }
    if matches!(arg, "-MD" | "-MDd" | "-MT" | "-MTd") {
        return true;
    }
    [
        "-D",
        "-U",
        "-I",
        "-O",
        "-g",
        "-W",
        "-std=",
        "-isystem",
        "-pedantic",
    ]
    .iter()
    .any(|p| arg.starts_with(p))
}

/// MSVC spells CMAKE_CXX_FLAGS with `/` (`/DWIN32 /EHsc /O2 /Ob2 /DNDEBUG`), and
/// `link.exe` warns LNK4044 on each one it is handed. `/D` is a define unless it
/// is one of the linker's own `/D…` options.
fn is_msvc_compile_only(opt: &str) -> bool {
    const LINKER_D: [&str; 11] = [
        "DEBUG",
        "DEBUGTYPE",
        "DEF",
        "DEFAULTLIB",
        "DELAY",
        "DELAYLOAD",
        "DELAYSIGN",
        "DEPENDENTLOADFLAG",
        "DLL",
        "DRIVER",
        "DYNAMICBASE",
    ];
    if let Some(define) = opt.strip_prefix('D') {
        let name = define.split([':', '=']).next().unwrap_or(define);
        let head = format!("D{name}").to_ascii_uppercase();
        return !LINKER_D.iter().any(|l| head == *l);
    }
    matches!(
        opt,
        "GR" | "GR-" | "MD" | "MDd" | "MT" | "MTd" | "Zi" | "Z7" | "utf-8" | "bigobj" | "MP"
    ) || opt.starts_with("EH")
        || opt.starts_with("std:")
        || (opt.starts_with('O') && opt.len() <= 3 && !opt.contains(':'))
        || (opt.len() == 2 && opt.starts_with('W') && opt.as_bytes()[1].is_ascii_digit())
}

/// A file API fragment can hold several arguments ("-framework IOKit",
/// "kernel32.lib user32.lib"); CMake double-quotes an argument with spaces.
fn split_fragment(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            c => {
                current.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(current);
    }
    out
}
