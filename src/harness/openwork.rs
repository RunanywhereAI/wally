//! The OpenWork desktop app (different-ai/openwork) against RunAnywhere
//! models.
//!
//! OpenWork is an Electron app that runs opencode 1.x as its engine and hands
//! that engine its own environment, so wally's provider rides in on
//! `OPENCODE_CONFIG_CONTENT`, which opencode merges after OpenWork's own
//! `OPENCODE_CONFIG` file (opencode 1.18.33 `config/config.ts`, the
//! CONFIG_CONTENT merge comes last). The app runs as a private instance on
//! wally's own profile under `state_dir()/openwork/`: its own Electron user
//! data, runtime database, server settings, opencode history and workspace,
//! so the person's OpenWork, opencode history and home folder are never
//! written. The profile is kept between runs, so chats carry over.
//!
//! Two OpenWork defaults are turned off for that profile, both verified on
//! OpenWork 0.18.63:
//! - A fresh profile routes chats to its opencode 2 engine, which is started
//!   with an allowlisted environment and never sees `OPENCODE_CONFIG_CONTENT`,
//!   so the RunAnywhere models would be missing from the picker. Any
//!   `OPENWORK_ENGINE_V2_PREVIEW` value other than `1`/`chat`/`sidecar` keeps
//!   the fresh-install upgrade off (`resolveFreshInstallEngine`).
//! - A profile with no workspace creates `~/OpenWork Chat` in the real home.
//!   A workspace list naming wally's own folder, written before the first
//!   launch, stops that and opens straight into a chat.

use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::io::json::dump;

use super::catalog_models::CatalogModel;
use super::opencode::open_code_provider;

/// The Electron app id every OpenWork edition ships under. The Cloud and
/// Enterprise editions share it but install as their own bundle name and
/// force a Den sign-in, so only the public `OpenWork.app` is launched.
pub const OPENWORK_BUNDLE_ID: &str = "com.differentai.openwork";

/// Points wally at an OpenWork executable or `.app` bundle somewhere other
/// than the standard install location.
pub const APP_OVERRIDE_VARIABLE: &str = "WALLY_OPENWORK_APP";

pub const DOWNLOAD: &str = "https://openworklabs.com/download";

/// macOS installs it with `brew install --cask openwork`. The npm package
/// named `openwork` is a different project and is never offered.
pub const BREW_INSTALL: &str = "brew install --cask openwork";

/// The hosted catalog's provider id; a local model gets its own, so both
/// lists show together without one id shadowing the other.
pub const HOSTED_PROVIDER: &str = "runanywhere";
pub const LOCAL_PROVIDER: &str = "runanywhere-local";

/// Where an installed OpenWork lives: a macOS bundle (launched through
/// `open`) or an executable started directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenWorkApp {
    Bundle(PathBuf),
    Executable(PathBuf),
}

/// wally's private OpenWork profile, all of it under one root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub root: PathBuf,
    pub user_data: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub workspace: PathBuf,
}

impl Profile {
    pub fn under(state_dir: &Path) -> Profile {
        let root = state_dir.join("openwork");
        Profile {
            user_data: root.join("user-data"),
            config: root.join("config"),
            data: root.join("data"),
            workspace: root.join("workspace"),
            root,
        }
    }

    /// Creates the profile's folders. The error is for the person.
    pub fn create(&self) -> Result<(), String> {
        for dir in [&self.user_data, &self.config, &self.data, &self.workspace] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("could not create {} ({error})", dir.to_string_lossy()))?;
        }
        Ok(())
    }

    /// The desktop app's workspace list (`openwork-workspaces.json` in its
    /// Electron user data).
    pub fn workspace_list(&self) -> PathBuf {
        self.user_data.join("openwork-workspaces.json")
    }

    /// The workspace folder as OpenWork resolves it (`path.resolve` then
    /// `realpath` in `normalizeLocalWorkspacePath`): native separators and
    /// long names. Its id is a hash of this exact text, so a spelling with
    /// mixed separators or a Windows 8.3 short name would not be selected.
    /// The folder must already exist.
    pub fn workspace_path(&self) -> String {
        let resolved = std::fs::canonicalize(&self.workspace)
            .unwrap_or_else(|_| self.workspace.clone())
            .to_string_lossy()
            .into_owned();
        without_verbatim_prefix(&resolved).to_string()
    }

    /// Every variable that moves OpenWork and its engine onto this profile:
    /// OpenWork's own isolation set (`BLANK_SLATE_PATH_ENV_KEYS` in
    /// `apps/desktop/electron/blank-slate-profile.mjs`) minus the home and
    /// XDG folders, so the person's shell tools and opencode config still
    /// work inside the app, plus the two engine switches the module doc
    /// explains.
    pub fn environment(&self) -> Vec<(String, String)> {
        let path = |p: PathBuf| p.to_string_lossy().into_owned();
        vec![
            (
                "OPENWORK_ELECTRON_USERDATA".into(),
                path(self.user_data.clone()),
            ),
            (
                "OPENWORK_DESKTOP_BOOTSTRAP_PATH".into(),
                path(self.config.join("desktop-bootstrap.json")),
            ),
            (
                "OPENWORK_SERVER_CONFIG".into(),
                path(self.config.join("server.json")),
            ),
            (
                "OPENWORK_ENV_STORE".into(),
                path(self.config.join("env.json")),
            ),
            (
                "OPENWORK_TOKEN_STORE".into(),
                path(self.config.join("tokens.json")),
            ),
            (
                "OPENWORK_RUNTIME_DB".into(),
                path(self.config.join("runtime.sqlite")),
            ),
            ("OPENWORK_DATA_DIR".into(), path(self.data.clone())),
            ("OPENCODE_DB".into(), path(self.data.join("opencode.db"))),
            (
                "OPENWORK_ELECTRON_DISABLE_PROTOCOL_REGISTRATION".into(),
                "1".into(),
            ),
            ("OPENWORK_ENGINE_V2_PREVIEW".into(), "off".into()),
            ("OPENWORK_ENGINE_AUTO_UPGRADE".into(), "0".into()),
        ]
    }
}

/// `path` without the `\\?\` that `canonicalize` puts on a Windows drive
/// path, which Node's `realpath` never returns. Anything else is unchanged.
pub fn without_verbatim_prefix(path: &str) -> &str {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => path,
    }
}

/// The id OpenWork's desktop store gives a local workspace at `path`
/// (`stableWorkspaceId` in `apps/desktop/electron/workspace-store.mjs`). A
/// list whose selection does not match it is re-selected by the app.
pub fn workspace_id(path: &str) -> String {
    let digest = hex::encode(Sha256::digest(path.as_bytes()));
    format!("ws_{}", &digest[..12])
}

/// A workspace list holding only wally's workspace at `path`, selected.
pub fn workspace_list(path: &str) -> String {
    let id = workspace_id(path);
    dump(&json!({
        "selectedId": id,
        "watchedId": id,
        "activeId": id,
        "workspaces": [{
            "id": id,
            "name": "RunAnywhere",
            "displayName": "RunAnywhere",
            "path": path,
            "preset": "starter",
            "workspaceType": "local",
            "baseUrl": null,
            "directory": null,
        }],
    }))
}

/// Where the provider config for one launch points.
pub struct Providers<'a> {
    /// The local server and the one model it holds, when the launch is local.
    pub local: Option<(&'a str, &'a [CatalogModel])>,
    /// The hosted endpoint, its key and the account's catalog, when the person
    /// is signed in and the catalog could be read.
    pub hosted: Option<(&'a str, &'a str, &'a [CatalogModel])>,
}

/// The opencode config OpenWork's engine gets: the hosted catalog under
/// `runanywhere`, the local model under `runanywhere-local`, and `primary`
/// selected on whichever of them the launch is on (local when there is one).
pub fn build_openwork_config(primary: &str, providers: &Providers) -> String {
    let mut provider = serde_json::Map::new();
    if let Some((base_url, access_token, models)) = providers.hosted {
        provider.insert(
            HOSTED_PROVIDER.into(),
            open_code_provider("RunAnywhere", base_url, access_token, models),
        );
    }
    if let Some((base_url, models)) = providers.local {
        provider.insert(
            LOCAL_PROVIDER.into(),
            open_code_provider("RunAnywhere (this machine)", base_url, "", models),
        );
    }
    let selected = if providers.local.is_some() {
        LOCAL_PROVIDER
    } else {
        HOSTED_PROVIDER
    };
    dump(&json!({
        "provider": provider,
        "model": format!("{selected}/{primary}"),
    }))
}

/// The OpenWork to launch: `WALLY_OPENWORK_APP` when set, else the public
/// edition in its standard location for this platform.
pub fn locate_app() -> Option<OpenWorkApp> {
    if let Some(path) = crate::util::getenv(APP_OVERRIDE_VARIABLE).filter(|p| !p.is_empty()) {
        return app_at(Path::new(&path));
    }
    standard_locations().iter().find_map(|path| app_at(path))
}

/// What is at `path`, if it is an OpenWork wally can launch.
pub fn app_at(path: &Path) -> Option<OpenWorkApp> {
    if path.extension().is_some_and(|e| e == "app") {
        return (path.join("Contents/Info.plist").is_file())
            .then(|| OpenWorkApp::Bundle(path.to_path_buf()));
    }
    path.is_file()
        .then(|| OpenWorkApp::Executable(path.to_path_buf()))
}

#[cfg(target_os = "macos")]
fn standard_locations() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = crate::util::getenv("HOME").filter(|h| !h.is_empty()) {
        roots.push(Path::new(&home).join("Applications"));
    }
    roots
        .into_iter()
        .map(|root| root.join("OpenWork.app"))
        .filter(|bundle| bundle_id(bundle).as_deref() == Some(OPENWORK_BUNDLE_ID))
        .collect()
}

#[cfg(target_os = "macos")]
fn bundle_id(bundle: &Path) -> Option<String> {
    let output = std::process::Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleIdentifier"])
        .arg(bundle.join("Contents/Info.plist"))
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// electron-builder's per-user NSIS install folder for productName OpenWork.
#[cfg(windows)]
fn standard_locations() -> Vec<PathBuf> {
    crate::util::getenv("LOCALAPPDATA")
        .filter(|dir| !dir.is_empty())
        .map(|dir| vec![Path::new(&dir).join(r"Programs\OpenWork\OpenWork.exe")])
        .unwrap_or_default()
}

/// The AppImage or a packaged `openwork` on PATH.
#[cfg(not(any(target_os = "macos", windows)))]
fn standard_locations() -> Vec<PathBuf> {
    let path = crate::util::getenv("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .flat_map(|dir| [dir.join("openwork"), dir.join("OpenWork.AppImage")])
        .collect()
}

/// The bundle's main executable, `CFBundleExecutable` in OpenWork 0.18.63's
/// Info.plist.
pub const BUNDLE_EXECUTABLE: &str = "OpenWork";

/// The program that starts `app`. A bundle is started from its own
/// executable rather than through `open`, so the profile environment, which
/// carries the account token, is handed to the child directly instead of
/// sitting in `open`'s arguments, and the app is wally's child: Ctrl-C reaches
/// it from the terminal and its exit ends the run.
pub fn program(app: &OpenWorkApp) -> PathBuf {
    match app {
        OpenWorkApp::Bundle(bundle) => bundle.join("Contents/MacOS").join(BUNDLE_EXECUTABLE),
        OpenWorkApp::Executable(path) => path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_id_matches_openwork_s_own() {
        // Read back from OpenWork 0.18.63, which re-selected this id for a
        // workspace at that path.
        assert_eq!(
            workspace_id(
                "/Users/home/.claude/projects/-Users-home-RustroverProjects-wally/openwork-probe/ws"
            ),
            "ws_b1d2a3b5c6b2"
        );
    }

    #[test]
    fn a_bundle_starts_from_its_own_executable() {
        let app = OpenWorkApp::Bundle(PathBuf::from("/Applications/OpenWork.app"));
        assert_eq!(
            program(&app),
            PathBuf::from("/Applications/OpenWork.app/Contents/MacOS/OpenWork")
        );
    }
}
