//! How the ChatGPT desktop app's Codex is pointed at wally's Responses
//! translator.
//!
//! The app takes no `-c` overrides, so the settings `wally codex` passes on
//! the command line go into the `config.toml` of a Codex home wally owns for
//! the app, as one block wally rewrites on every launch. The rest of that
//! file is the app's own (project trust, toggles) and is kept. The reader's
//! `~/.codex` and their own ChatGPT app are never touched.

use super::codex::{codex_settings, toml_string};

/// The bundle id of the ChatGPT app that carries Codex. The older chat-only
/// app has a different one and no Codex to point anywhere.
pub const CHATGPT_BUNDLE_ID: &str = "com.openai.codex";

const BLOCK_BEGIN: &str = "# wally: managed settings, rewritten on every `wally chatgpt` launch";
const BLOCK_END: &str = "# wally: end of managed settings";

/// The block of root-level settings for `model` behind the translator at
/// `base_url` (ending in `/v1`).
pub fn chatgpt_settings(
    model: &str,
    base_url: &str,
    catalog_path: Option<&str>,
    context_window: i64,
) -> Vec<String> {
    let mut settings = vec![format!("model={}", toml_string(model))];
    settings.extend(codex_settings(base_url, catalog_path, context_window));
    let efforts: Vec<String> = ENABLED_REASONING_EFFORTS
        .iter()
        .map(|effort| toml_string(effort))
        .collect();
    settings.push(format!(
        "desktop.enabled-reasoning-efforts=[{}]",
        efforts.join(", ")
    ));
    settings
}

/// The key a root-level `key=value` line sets, or None for anything else.
fn root_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    let (key, _) = trimmed.split_once('=')?;
    Some(key.trim())
}

/// One `[table]` of the file and the lines under it, kept as written.
struct Section {
    /// The table's name (`features`, `projects."/x"`); None for `[[array]]`
    /// headers, which are kept as they are.
    name: Option<String>,
    header: String,
    body: Vec<String>,
}

fn table_name(header: &str) -> Option<String> {
    let trimmed = header.trim();
    if trimmed.starts_with("[[") {
        return None;
    }
    let inner = trimmed.strip_prefix('[')?.split(']').next()?;
    Some(inner.trim().to_string())
}

/// `existing` (the home's `config.toml`, empty when there is none) with the
/// managed block replaced by `settings`.
///
/// The block goes first, because root-level keys must come before any table.
/// The file must never define a key or a table twice, which TOML refuses and
/// the app would then refuse on every launch, so:
/// - a root-level line elsewhere that sets one of the same keys is dropped;
/// - a dotted setting (`features.x=...`) whose table the file already has as
///   `[features]` is written inside that table instead, replacing only its
///   own key there, since the app may keep its own toggles in it;
/// - a table that is the setting itself (`[model_providers.wally]`) is wally's
///   and is dropped in favour of the managed line.
pub fn chatgpt_config(existing: &str, settings: &[String]) -> String {
    let ours: Vec<&str> = settings.iter().filter_map(|line| root_key(line)).collect();
    let mut root = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_BEGIN {
            in_block = true;
            continue;
        }
        if in_block {
            if trimmed == BLOCK_END {
                in_block = false;
            }
            continue;
        }
        if trimmed.starts_with('[') {
            sections.push(Section {
                name: table_name(trimmed),
                header: line.to_string(),
                body: Vec::new(),
            });
            continue;
        }
        match sections.last_mut() {
            Some(section) => section.body.push(line.to_string()),
            None => {
                if !root_key(line).is_some_and(|key| ours.contains(&key)) {
                    root.push(line.to_string());
                }
            }
        }
    }

    let mut block = Vec::new();
    for line in settings {
        let Some((table, key)) = root_key(line).and_then(|full| full.split_once('.')) else {
            block.push(line.clone());
            continue;
        };
        let own = format!("{table}.{key}");
        sections.retain(|section| {
            section
                .name
                .as_deref()
                .is_none_or(|name| name != own && !name.starts_with(&format!("{own}.")))
        });
        let value = line.split_once('=').map(|(_, value)| value).unwrap_or("");
        match sections
            .iter_mut()
            .find(|section| section.name.as_deref() == Some(table))
        {
            Some(section) => {
                section
                    .body
                    .retain(|body_line| root_key(body_line) != Some(key));
                let at = section
                    .body
                    .iter()
                    .rposition(|body_line| !body_line.trim().is_empty())
                    .map_or(0, |last| last + 1);
                section.body.insert(at, format!("{key}={value}"));
            }
            None => block.push(line.clone()),
        }
    }

    while root.first().is_some_and(|line| line.trim().is_empty()) {
        root.remove(0);
    }
    let mut out = String::new();
    out.push_str(BLOCK_BEGIN);
    out.push('\n');
    for line in &block {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(BLOCK_END);
    out.push('\n');
    let mut rest: Vec<String> = root;
    for section in sections {
        rest.push(section.header);
        rest.extend(section.body);
    }
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(&rest.join("\n"));
        out.push('\n');
    }
    out
}

/// The `auth.json` that lets the app's Codex start without a ChatGPT sign-in:
/// API-key mode, with a key nothing reads. Requests go to the wally provider,
/// which takes its key from `WALLY_CODEX_API_KEY` instead. Only ever written
/// into wally's own home, and only when that home has none.
pub fn chatgpt_auth() -> &'static str {
    "{\n  \"OPENAI_API_KEY\": \"wally-local\",\n  \"auth_mode\": \"apikey\"\n}\n"
}

/// The app's first-run questions ("Which best describes your work?") are a
/// one-time onboarding it records in the Codex home's
/// `.codex-global-state.json`. wally's home is new to the app, so it would ask
/// on the first launch; this marks onboarding as skipped, in coding mode, the
/// state the app itself writes when the person skips it.
///
/// `existing` is that file's text, empty when there is none. Only the
/// onboarding keys are set; everything else the app wrote is kept. None when
/// the file is not a JSON object, which is then left for the app to deal with
/// rather than replaced.
pub fn with_onboarding_done(existing: &str, now_unix: i64) -> Option<String> {
    let mut state: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing).ok()?
    };
    let root = state.as_object_mut()?;
    let atoms = root
        .entry("electron-persisted-atom-state")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()?;
    let welcome = atoms
        .entry("electron:onboarding-welcome-v2-role-state")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()?;
    for (key, value) in [
        ("completedConversationalOnboarding", serde_json::json!(true)),
        ("conversationalOnboardingSkipped", serde_json::json!(true)),
        ("workMode", serde_json::json!("coding")),
        ("personalizedSuggestionsEnabled", serde_json::json!(false)),
    ] {
        welcome.entry(key).or_insert(value);
    }
    atoms
        .entry("electron:onboarding-projectless-completed")
        .or_insert(serde_json::json!(true));
    atoms
        .entry("last_completed_onboarding")
        .or_insert(serde_json::json!(now_unix));
    serde_json::to_string_pretty(&state).ok()
}

/// The effort levels the app's model controls show, its desktop setting
/// `enabled-reasoning-efforts` (kept in `config.toml` under `[desktop]`). The
/// app drops any level a model lists that is not in it, and it starts without
/// `minimal` and `max`, so a model graded low/high/max would offer only low
/// and high. With these enabled, each model shows the levels its catalog
/// entry lists, except `none`, which the app keeps out of its controls.
const ENABLED_REASONING_EFFORTS: &[&str] = &[
    "minimal",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "persistent",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Vec<String> {
        chatgpt_settings(
            "glm-5.3-flash",
            "http://127.0.0.1:5000/v1",
            Some("/state/models.json"),
            1_048_567,
        )
    }

    #[test]
    fn a_fresh_home_gets_the_block_alone() {
        let config = chatgpt_config("", &settings());
        assert!(config.starts_with(BLOCK_BEGIN));
        assert!(config.contains("model='glm-5.3-flash'\n"));
        assert!(config.contains("model_provider='wally'\n"));
        assert!(config.contains("base_url='http://127.0.0.1:5000/v1'"));
        assert!(config.contains("model_catalog_json='/state/models.json'\n"));
        assert!(config.trim_end().ends_with(BLOCK_END));
    }

    #[test]
    fn the_apps_own_settings_survive_and_the_block_is_replaced() {
        let first = chatgpt_config("", &settings());
        let app_wrote =
            format!("{first}\n[projects.\"/Users/me/repo\"]\ntrust_level = \"trusted\"\n");
        let next = chatgpt_settings("mimo-v2.6-pro", "http://127.0.0.1:6000/v1", None, 0);
        let second = chatgpt_config(&app_wrote, &next);
        assert_eq!(second.matches(BLOCK_BEGIN).count(), 1);
        assert!(second.contains("model='mimo-v2.6-pro'"));
        assert!(!second.contains("glm-5.3-flash"));
        assert!(!second.contains("5000"));
        assert!(second.contains("[projects.\"/Users/me/repo\"]\ntrust_level = \"trusted\""));
    }

    #[test]
    fn a_root_key_the_app_set_outside_the_block_is_not_left_twice() {
        let existing =
            "model = \"gpt-5\"\nnotify = [\"x\"]\n\n[tui]\nmodel = \"kept, a table key\"\n";
        let config = chatgpt_config(existing, &settings());
        let root: Vec<&str> = config
            .lines()
            .take_while(|line| !line.trim_start().starts_with('['))
            .filter_map(root_key)
            .collect();
        assert_eq!(root.iter().filter(|key| **key == "model").count(), 1);
        assert!(root.contains(&"notify"));
        assert!(config.contains("[tui]\nmodel = \"kept, a table key\""));
    }

    /// The app may keep its own `[features]` toggles; wally's key goes into
    /// that table rather than being defined a second time above it.
    #[test]
    fn a_setting_whose_table_the_file_already_has_goes_inside_it() {
        let existing = "[features]\nsome_app_toggle = true\nenable_request_compression = true\n\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
        let config = chatgpt_config(existing, &settings());
        let block_end = config.find(BLOCK_END).unwrap();
        assert!(!config[..block_end].contains("features."));
        let features = config.split("[features]").nth(1).unwrap();
        let features = features.split("\n[").next().unwrap();
        assert!(features.contains("some_app_toggle = true"));
        assert!(features.contains("enable_request_compression=false"));
        assert!(!features.contains("enable_request_compression = true"));
        assert!(config.contains("[projects.\"/x\"]\ntrust_level = \"trusted\""));
        // Rewriting it again changes nothing.
        assert_eq!(chatgpt_config(&config, &settings()), config);
    }

    #[test]
    fn a_table_that_is_wallys_own_setting_is_replaced_by_the_managed_line() {
        let existing = "[model_providers.wally]\nname = \"old\"\nbase_url = \"http://stale/v1\"\n\n[model_providers.wally.extra]\nx = 1\n\n[model_providers.other]\nname = \"kept\"\n";
        let config = chatgpt_config(existing, &settings());
        assert!(!config.contains("[model_providers.wally]"));
        assert!(!config.contains("[model_providers.wally.extra]"));
        assert!(!config.contains("http://stale/v1"));
        assert!(config.contains("[model_providers.other]\nname = \"kept\""));
        assert_eq!(config.matches("model_providers.wally=").count(), 1);
    }

    #[test]
    fn a_providers_table_takes_wallys_provider_as_a_key() {
        let existing = "[model_providers]\nother = { name = \"kept\" }\n";
        let config = chatgpt_config(existing, &settings());
        let block_end = config.find(BLOCK_END).unwrap();
        assert!(!config[..block_end].contains("model_providers."));
        let providers = config.split("[model_providers]").nth(1).unwrap();
        assert!(providers.contains("other = { name = \"kept\" }"));
        assert!(providers.contains("wally={name='RunAnywhere'"));
        assert_eq!(chatgpt_config(&config, &settings()), config);
    }

    #[test]
    fn the_auth_file_is_api_key_mode_with_no_real_key() {
        let auth: serde_json::Value = serde_json::from_str(chatgpt_auth()).unwrap();
        assert_eq!(auth["auth_mode"], "apikey");
        assert_eq!(auth["OPENAI_API_KEY"], "wally-local");
    }

    #[test]
    fn onboarding_is_marked_skipped_in_a_new_home() {
        let state: serde_json::Value =
            serde_json::from_str(&with_onboarding_done("", 100).unwrap()).unwrap();
        let atoms = &state["electron-persisted-atom-state"];
        let welcome = &atoms["electron:onboarding-welcome-v2-role-state"];
        assert_eq!(welcome["completedConversationalOnboarding"], true);
        assert_eq!(welcome["conversationalOnboardingSkipped"], true);
        assert_eq!(welcome["workMode"], "coding");
        assert_eq!(atoms["electron:onboarding-projectless-completed"], true);
        assert_eq!(atoms["last_completed_onboarding"], 100);
    }

    #[test]
    fn what_the_app_wrote_is_kept_and_its_choices_win() {
        let existing = r#"{"desktop-first-seen-at-ms": 5, "electron-persisted-atom-state": {"x": 1, "electron:onboarding-welcome-v2-role-state": {"workMode": "writing"}, "last_completed_onboarding": 7}}"#;
        let state: serde_json::Value =
            serde_json::from_str(&with_onboarding_done(existing, 100).unwrap()).unwrap();
        assert_eq!(state["desktop-first-seen-at-ms"], 5);
        let atoms = &state["electron-persisted-atom-state"];
        assert_eq!(atoms["x"], 1);
        assert_eq!(
            atoms["electron:onboarding-welcome-v2-role-state"]["workMode"],
            "writing"
        );
        assert_eq!(atoms["last_completed_onboarding"], 7);
    }

    #[test]
    fn the_effort_levels_go_in_the_desktop_table_without_none() {
        let config = chatgpt_config("", &settings());
        assert!(config.contains(
            "desktop.enabled-reasoning-efforts=['minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'persistent']"
        ));
        assert!(!ENABLED_REASONING_EFFORTS.contains(&"none"));
        // Once the app has a [desktop] table of its own, the setting moves into it.
        let app_wrote = format!("{config}\n[desktop]\nfollowUpQueueMode = \"queue\"\n");
        let again = chatgpt_config(&app_wrote, &settings());
        let desktop = again.split("[desktop]").nth(1).unwrap();
        assert!(desktop.contains("followUpQueueMode = \"queue\""));
        assert!(desktop.contains("enabled-reasoning-efforts=['minimal'"));
        assert_eq!(again.matches("enabled-reasoning-efforts").count(), 1);
    }

    #[test]
    fn a_file_that_is_not_a_json_object_is_left_alone() {
        assert_eq!(with_onboarding_done("not json", 1), None);
        assert_eq!(with_onboarding_done("[1]", 1), None);
    }
}
