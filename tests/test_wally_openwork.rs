//! `wally openwork`: the provider config OpenWork's engine gets, the private
//! profile it runs on, and how the app is found and started.

use std::path::{Path, PathBuf};

use serde_json::Value;

use wally::harness::{
    app_at, build_openwork_config, program, without_verbatim_prefix, workspace_id, workspace_list,
    CatalogModel, OpenWorkApp, Profile, Providers, HOSTED_PROVIDER, LOCAL_PROVIDER,
};

fn model(id: &str, context: i64) -> CatalogModel {
    CatalogModel {
        id: id.to_string(),
        context_window: context,
        ..Default::default()
    }
}

fn parse(config: &str) -> Value {
    serde_json::from_str(config).expect("the config is JSON")
}

#[test]
fn a_hosted_launch_lists_every_catalog_model_and_selects_the_launched_one() {
    let catalog = [
        model("glm-5.3-flash", 200_000),
        model("kimi-k3", 256_000),
        model("minimax-m3", 1_000_000),
    ];
    let doc = parse(&build_openwork_config(
        "glm-5.3-flash",
        &Providers {
            local: None,
            hosted: Some(("https://console.example.test/v1", "token", &catalog)),
        },
    ));
    let provider = &doc["provider"][HOSTED_PROVIDER];
    let ids: Vec<&str> = provider["models"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(ids.len(), 3);
    for entry in &catalog {
        assert!(ids.contains(&entry.id.as_str()), "{} is missing", entry.id);
    }
    assert_eq!(doc["model"], "runanywhere/glm-5.3-flash");
    assert_eq!(
        provider["options"]["baseURL"],
        "https://console.example.test/v1"
    );
    assert_eq!(provider["options"]["apiKey"], "token");
    assert!(doc["provider"].get(LOCAL_PROVIDER).is_none());
}

#[test]
fn a_local_launch_lists_the_hosted_catalog_beside_it_and_selects_the_local_model() {
    let local = [model("qwen3-4b-instruct-2507", 65_536)];
    let catalog = [model("glm-5.3-flash", 0), model("kimi-k3", 0)];
    let doc = parse(&build_openwork_config(
        "qwen3-4b-instruct-2507",
        &Providers {
            local: Some(("http://127.0.0.1:5000/v1", &local)),
            hosted: Some(("https://console.example.test/v1", "token", &catalog)),
        },
    ));
    assert_eq!(doc["model"], "runanywhere-local/qwen3-4b-instruct-2507");
    let on_machine = &doc["provider"][LOCAL_PROVIDER];
    assert_eq!(on_machine["options"]["baseURL"], "http://127.0.0.1:5000/v1");
    // A local server ignores the key, but opencode always sends one.
    assert_eq!(on_machine["options"]["apiKey"], "local");
    assert_eq!(
        on_machine["models"]["qwen3-4b-instruct-2507"]["limit"]["context"],
        65_536
    );
    assert_eq!(
        doc["provider"][HOSTED_PROVIDER]["models"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_local_launch_signed_out_has_only_the_local_provider() {
    let local = [model("qwen3-4b-instruct-2507", 65_536)];
    let doc = parse(&build_openwork_config(
        "qwen3-4b-instruct-2507",
        &Providers {
            local: Some(("http://127.0.0.1:5000/v1", &local)),
            hosted: None,
        },
    ));
    let providers: Vec<&String> = doc["provider"].as_object().unwrap().keys().collect();
    assert_eq!(providers, [LOCAL_PROVIDER]);
}

#[test]
fn every_provider_declares_the_opencode_harness() {
    let local = [model("a", 0)];
    let catalog = [model("b", 0)];
    let doc = parse(&build_openwork_config(
        "a",
        &Providers {
            local: Some(("http://127.0.0.1:1/v1", &local)),
            hosted: Some(("https://console.example.test/v1", "t", &catalog)),
        },
    ));
    for id in [LOCAL_PROVIDER, HOSTED_PROVIDER] {
        assert_eq!(
            doc["provider"][id]["options"]["headers"]["X-RA-Harness"], "opencode",
            "{id}"
        );
    }
}

#[test]
fn the_profile_keeps_every_path_it_sets_under_its_own_root() {
    let dir = tempfile::tempdir().expect("temp dir");
    let profile = Profile::under(dir.path());
    let root = dir.path().join("openwork");
    assert_eq!(profile.root, root);
    let mut paths = 0;
    for (name, value) in profile.environment() {
        // Every value is a switch (no separator at all) or a path.
        if !Path::new(&value).is_absolute() {
            assert!(
                !value.contains('/') && !value.contains('\\'),
                "{name}={value} is neither a switch nor an absolute path"
            );
            continue;
        }
        paths += 1;
        assert!(
            Path::new(&value).starts_with(&root),
            "{name}={value} is outside the profile"
        );
    }
    assert!(paths >= 9, "only {paths} paths were checked");
}

#[test]
fn the_profile_keeps_chats_on_the_engine_that_reads_wallys_provider() {
    let environment = Profile::under(Path::new("/s")).environment();
    let value = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    // Anything but 1, chat or sidecar turns the fresh-install v2 switch off.
    let preview = value("OPENWORK_ENGINE_V2_PREVIEW").expect("set");
    assert!(!["", "1", "chat", "sidecar"].contains(&preview));
    assert_eq!(value("OPENWORK_ENGINE_AUTO_UPGRADE"), Some("0"));
    assert_eq!(
        value("OPENWORK_ELECTRON_DISABLE_PROTOCOL_REGISTRATION"),
        Some("1")
    );
    // opencode's log and saved logins move into the profile.
    assert!(value("XDG_DATA_HOME").is_some());
    // The person's real home and opencode config stay in reach of the app.
    for kept in ["HOME", "XDG_CONFIG_HOME", "USERPROFILE", "APPDATA"] {
        assert_eq!(value(kept), None, "{kept} must not be overridden");
    }
}

#[test]
fn the_workspace_list_selects_its_one_workspace_by_openworks_id() {
    let path = "/state/runanywhere/openwork/workspace";
    let doc = parse(&workspace_list(path));
    let id = workspace_id(path);
    assert!(id.starts_with("ws_") && id.len() == 15);
    assert_eq!(doc["selectedId"], id.as_str());
    assert_eq!(doc["activeId"], id.as_str());
    assert_eq!(doc["workspaces"][0]["id"], id.as_str());
    assert_eq!(doc["workspaces"][0]["path"], path);
    assert_eq!(doc["workspaces"][0]["workspaceType"], "local");
}

#[test]
fn verbatim_prefixes_are_spelled_the_way_node_spells_them() {
    assert_eq!(
        without_verbatim_prefix(r"\\?\C:\Users\me\openwork\workspace"),
        r"C:\Users\me\openwork\workspace"
    );
    assert_eq!(
        without_verbatim_prefix(r"\\?\UNC\server\share\openwork"),
        r"\\server\share\openwork"
    );
    assert_eq!(
        without_verbatim_prefix(r"\\?\Volume{0}\openwork"),
        r"\\?\Volume{0}\openwork"
    );
    assert_eq!(
        without_verbatim_prefix("/state/openwork"),
        "/state/openwork"
    );
}

#[test]
fn the_workspace_path_is_the_resolved_folder() {
    let dir = tempfile::tempdir().expect("temp dir");
    let profile = Profile::under(dir.path());
    profile.create().unwrap();
    assert_eq!(
        Path::new(&profile.workspace_path()),
        std::fs::canonicalize(&profile.workspace)
            .unwrap()
            .as_path()
            .to_str()
            .map(without_verbatim_prefix)
            .map(PathBuf::from)
            .unwrap()
    );
}

#[test]
fn an_app_is_a_bundle_with_a_plist_or_an_executable_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let bundle = dir.path().join("OpenWork.app");
    std::fs::create_dir_all(bundle.join("Contents")).unwrap();
    assert_eq!(app_at(&bundle), None, "a bundle with no Info.plist");
    std::fs::write(bundle.join("Contents/Info.plist"), "<plist/>").unwrap();
    assert_eq!(app_at(&bundle), Some(OpenWorkApp::Bundle(bundle.clone())));

    let executable = dir.path().join("openwork");
    assert_eq!(app_at(&executable), None, "nothing there");
    std::fs::write(&executable, "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(app_at(&executable), None, "a file it may not execute");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(
        app_at(&executable),
        Some(OpenWorkApp::Executable(executable.clone()))
    );
}

#[test]
fn an_executable_is_started_as_it_is() {
    let app = OpenWorkApp::Executable(PathBuf::from("/opt/OpenWork/openwork"));
    assert_eq!(program(&app), PathBuf::from("/opt/OpenWork/openwork"));
}
