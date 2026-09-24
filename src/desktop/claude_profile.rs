//! Points Claude Desktop at a wally endpoint and puts it back (port of
//! src/desktop/claude_profile.cpp).
//!
//! The app ships two deployment modes: "1p", which talks to Anthropic, and
//! "3p", which talks to a gateway you name. The 3p side keeps its own profile
//! tree beside the normal one, and none of it is reachable from Settings —
//! which is why the model picker and ANTHROPIC_BASE_URL both look like dead
//! ends. Neither is the mechanism.
//!
//! A gateway here speaks the Anthropic Messages API, which is exactly what
//! `wally::anthropic` already serves. So pointing Claude Desktop at a model we
//! serve is a matter of writing the profile and restarting the app.

use std::fs;
use std::path::Path;

use serde_json::{json, Value};

use crate::io::json::dump_pretty;
use crate::util::getenv;

/// Our profile's identity inside Claude Desktop's library. Fixed, so a second
/// run replaces the first rather than stacking entries up.
///
/// Shaped like the app's own ids; the tail is "RunAny" in hex, which makes it
/// recognisable in a file somebody is reading by hand.
const PROFILE_ID: &str = "00000000-0000-4000-8000-52756e416e79";

fn home() -> String {
    getenv("HOME").unwrap_or_default()
}

fn support_root(third_party: bool) -> String {
    let home = home();
    if home.is_empty() {
        return String::new();
    }
    let leaf = if third_party { "Claude-3p" } else { "Claude" };
    format!("{home}/Library/Application Support/{leaf}")
}

/// Reads a JSON object, treating "not there" and "empty" as an empty object.
///
/// A malformed file is reported rather than overwritten: it is the reader's
/// Claude Desktop configuration, and silently replacing it would lose whatever
/// else they had in there.
fn read_object(path: &str) -> Result<Value, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Ok(json!({})),
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(parsed) if parsed.is_object() => Ok(parsed),
        Ok(_) => Ok(json!({})),
        Err(failure) => Err(format!("could not read {path}: {failure}")),
    }
}

fn write_object(path: &str, value: &Value) -> Result<(), String> {
    if let Some(parent) = Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    // Closed before it is judged in C++ (a short write on a full disk surfaces
    // during the flush that close() performs); `fs::write` opens, writes and
    // closes in one call, so any failure at any of those stages is caught here.
    let text = format!("{}\n", dump_pretty(value, 2));
    fs::write(path, text).map_err(|_| format!("could not write {path}"))
}

fn set_deployment_mode(path: &str, mode: &str) -> Result<(), String> {
    let mut config = read_object(path)?;
    config
        .as_object_mut()
        .expect("read_object always returns an object")
        .insert("deploymentMode".to_string(), json!(mode));
    write_object(path, &config)
}

fn profile_path() -> String {
    format!("{}/configLibrary/{PROFILE_ID}.json", support_root(true))
}

fn meta_path() -> String {
    format!("{}/configLibrary/_meta.json", support_root(true))
}

/// Whether `entry`'s `"id"` field is our profile id.
fn is_our_entry(entry: &Value) -> bool {
    entry
        .as_object()
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        == Some(PROFILE_ID)
}

/// Where the app keeps its third-party profiles, for a message worth printing.
pub fn profile_directory() -> String {
    support_root(true)
}

/// Writes the gateway profile, marks it applied, and switches both config
/// trees to third-party mode.
///
/// Takes effect on the app's next launch, never on a running one. `models` is
/// (Anthropic family name -> real id) pairs, the first the default: each
/// becomes a picker entry whose `name` the app maps to a family and whose
/// `labelOverride` shows the real id.
pub fn apply_gateway(
    base_url: &str,
    api_key: &str,
    models: &[(String, String)],
    display_name: &str,
) -> Result<(), String> {
    if home().is_empty() {
        return Err("no home directory to write the profile into".to_string());
    }

    let mut profile = read_object(&profile_path())?;
    {
        let obj = profile
            .as_object_mut()
            .expect("read_object always returns an object");
        obj.insert("inferenceProvider".to_string(), json!("gateway"));
        obj.insert("inferenceGatewayBaseUrl".to_string(), json!(base_url));
        obj.insert("inferenceGatewayApiKey".to_string(), json!(api_key));
        obj.insert("inferenceGatewayAuthScheme".to_string(), json!("bearer"));
        obj.insert("deploymentDisplayName".to_string(), json!(display_name));
        // Skip the first-run deployment-mode chooser: the gateway is already
        // wired, so a fresh install should go straight in rather than open the
        // wizard.
        obj.insert("disableDeploymentModeChooser".to_string(), json!(true));
        obj.insert("chatTabEnabled".to_string(), json!(true));
        // Cowork reaches plugins and MCP servers over the network, and a profile
        // that does not say so leaves it unable to use them.
        obj.insert("coworkEgressAllowedHosts".to_string(), json!(["*"]));
        obj.insert("autoModeEnabled".to_string(), json!(false));
        // Cowork is the surface being asked for, and the app disables surfaces
        // it is not told to keep.
        obj.insert("coworkTabEnabled".to_string(), json!(true));
        // Named rather than discovered. Discovery works when a gateway
        // advertises a model the app recognises as usable; ours advertises one
        // id and the app answers "Gateway returned no usable models", which is
        // exactly the case its own error message says to solve by listing the
        // model here. A plain id string is a valid entry, and the first entry
        // is the default. `name` has to be an id the app can map onto an
        // Anthropic family or it drops the entry: "expected a gateway model
        // that maps to an Anthropic model". `labelOverride` is what the picker
        // actually shows, so the row names the model that really answers
        // rather than the one we route under.
        let inference: Vec<Value> = models
            .iter()
            .map(|(name, label)| json!({"name": name, "labelOverride": label}))
            .collect();
        obj.insert("inferenceModels".to_string(), Value::Array(inference));
        // Asked for explicitly: with inferenceModels set the app would
        // otherwise skip discovery, and the picker then has nothing to
        // reconcile the served model against.
        obj.insert("modelDiscoveryEnabled".to_string(), json!(true));
    }
    write_object(&profile_path(), &profile)?;

    let mut meta = read_object(&meta_path())?;
    {
        let entries: Vec<Value> = meta
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            // Drop any previous version of ours; keep everybody else's.
            .filter(|entry| !is_our_entry(entry))
            .chain(std::iter::once(
                json!({"id": PROFILE_ID, "name": display_name}),
            ))
            .collect();
        let obj = meta
            .as_object_mut()
            .expect("read_object always returns an object");
        obj.insert("appliedId".to_string(), json!(PROFILE_ID));
        obj.insert("entries".to_string(), Value::Array(entries));
    }
    write_object(&meta_path(), &meta)?;

    // Both trees: the app reads the normal one to decide which mode it is in.
    set_deployment_mode(
        &format!("{}/claude_desktop_config.json", support_root(true)),
        "3p",
    )?;
    set_deployment_mode(
        &format!("{}/claude_desktop_config.json", support_root(false)),
        "3p",
    )?;
    Ok(())
}

/// Puts Claude Desktop back on Anthropic and strips the keys we wrote.
///
/// Safe to call when nothing was applied, and it only removes our own
/// profile: a gateway somebody else configured is left alone.
pub fn restore_gateway() -> Result<(), String> {
    if home().is_empty() {
        return Ok(());
    }
    set_deployment_mode(
        &format!("{}/claude_desktop_config.json", support_root(false)),
        "1p",
    )?;
    set_deployment_mode(
        &format!("{}/claude_desktop_config.json", support_root(true)),
        "1p",
    )?;

    let mut meta = read_object(&meta_path())?;
    if !meta.as_object().is_some_and(|o| o.is_empty()) {
        let entries = meta
            .get("entries")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| !is_our_entry(entry))
                    .cloned()
                    .collect()
            });
        let applied_is_ours = meta.get("appliedId").and_then(Value::as_str) == Some(PROFILE_ID);
        let obj = meta.as_object_mut().expect("checked non-empty above");
        if applied_is_ours {
            obj.remove("appliedId");
        }
        if let Some(entries) = entries {
            obj.insert("entries".to_string(), Value::Array(entries));
        }
        write_object(&meta_path(), &meta)?;
    }

    let mut profile = read_object(&profile_path())?;
    if profile.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(());
    }
    let obj = profile.as_object_mut().expect("checked non-empty above");
    for key in [
        "inferenceProvider",
        "inferenceGatewayBaseUrl",
        "inferenceGatewayApiKey",
        "inferenceGatewayAuthScheme",
        "deploymentDisplayName",
        "inferenceModels",
        "coworkEgressAllowedHosts",
        "autoModeEnabled",
        "coworkTabEnabled",
        "modelDiscoveryEnabled",
    ] {
        obj.remove(key);
    }
    write_object(&profile_path(), &profile)
}

/// True when our profile is the one Claude Desktop has applied.
pub fn gateway_applied() -> bool {
    match read_object(&meta_path()) {
        Ok(meta) => meta.get("appliedId").and_then(Value::as_str) == Some(PROFILE_ID),
        Err(_) => false,
    }
}
