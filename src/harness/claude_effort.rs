//! How a hosted model's reasoning, as /v1/models describes it, shows in the
//! Claude apps' effort control.
//!
//! Claude Code and Claude Desktop draw the effort control from the Claude
//! model they take an entry to be, and Claude models come with one of two
//! menus: low to max with `xhigh` (Opus 5, Sonnet 5, Opus 4.8 and later), or
//! low, medium, high and max (Opus 4.6, Sonnet 4.6). Each model is listed as
//! the Claude model whose menu is closest to the levels it grades by; a model
//! that only switches thinking on or off is listed as a Claude model with no
//! effort control (Claude Desktop drops an entry whose name is not a Claude
//! id). Which model takes which menu comes from the model list, never from
//! its name.

use super::catalog_models::CatalogModel;

/// The effort control a model is given in the Claude apps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortMenu {
    /// It only switches thinking on or off: no effort control.
    None,
    /// Low, medium, high and max.
    WithoutXhigh,
    /// Low, medium, high, xhigh and max.
    Full,
}

/// The menu `model` gets, or None when the model list does not describe its
/// reasoning (a local model, a failed read).
pub fn effort_menu(model: &CatalogModel) -> Option<EffortMenu> {
    let reasoning = model.reasoning.as_ref()?;
    Some(if reasoning.levels.is_empty() {
        EffortMenu::None
    } else if reasoning.levels.contains(&"xhigh") {
        EffortMenu::Full
    } else {
        EffortMenu::WithoutXhigh
    })
}

/// The highest Claude effort level `model` grades by, for Claude Desktop's
/// per-entry `maxEffort`: levels above it are not offered. Claude has no level
/// below low, so a model whose highest is `minimal` is capped at low.
pub fn max_effort(model: &CatalogModel) -> Option<&'static str> {
    let highest = *model.reasoning.as_ref()?.levels.last()?;
    Some(match highest {
        "max" => "max",
        "xhigh" => "xhigh",
        "high" => "high",
        "medium" => "medium",
        _ => "low",
    })
}

/// The Claude model Claude Code takes `model` to be (`modelPicker`'s
/// `behavesAs`), for its effort menu. None for a model that keeps its own id.
pub fn behaves_as(model: &CatalogModel) -> Option<&'static str> {
    match effort_menu(model)? {
        EffortMenu::None => None,
        EffortMenu::WithoutXhigh => Some("claude-opus-4-6"),
        EffortMenu::Full => Some("claude-opus-4-8"),
    }
}

/// Claude ids Claude Desktop lists with the full menu. Each picker entry
/// needs its own name, so a model takes the first one still free.
const FULL_MENU_NAMES: &[&str] = &[
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-haiku-5-5",
    "claude-fable-5",
];

/// Claude ids Claude Desktop lists with the menu that stops at max without
/// xhigh. Both size a model at 200K unless its entry offers 1M.
const WITHOUT_XHIGH_NAMES: &[&str] = &["claude-sonnet-4-6", "claude-opus-4-6"];

/// Claude ids Claude Desktop lists with no effort control. Both size a model
/// at 200K unless its entry offers 1M.
const NO_EFFORT_NAMES: &[&str] = &["claude-haiku-4-5-20251001", "claude-sonnet-4-5"];

/// The Claude id each of `models` is listed under in Claude Desktop, in
/// order, the one whose effort menu matches the model's levels. None for a
/// model the list does not describe (the caller names those by place) and
/// for one whose menu has no free Claude id left: each entry needs its own
/// name, and the app drops any that is not a Claude id.
pub fn desktop_names(models: &[CatalogModel]) -> Vec<Option<String>> {
    let mut taken: Vec<&str> = Vec::new();
    models
        .iter()
        .map(|model| {
            let pool = match effort_menu(model)? {
                EffortMenu::None => NO_EFFORT_NAMES,
                EffortMenu::Full => FULL_MENU_NAMES,
                EffortMenu::WithoutXhigh => WITHOUT_XHIGH_NAMES,
            };
            let name = pool.iter().find(|name| !taken.contains(name))?;
            taken.push(name);
            Some(name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ReasoningSupport;

    fn model(id: &str, levels: &[&'static str]) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            reasoning: Some(ReasoningSupport {
                levels: levels.to_vec(),
                default_level: levels.first().copied(),
                thinking_off: true,
            }),
            ..Default::default()
        }
    }

    /// The four hosted models as /v1/models describes them today.
    fn today() -> Vec<CatalogModel> {
        vec![
            model("glm-5.3-flash", &["low", "high", "max"]),
            model("deepseek-v4.1-flash", &["low", "high", "xhigh", "max"]),
            model("mimo-v2.6-pro", &[]),
            model("muse-spark", &["minimal", "low", "medium", "high", "xhigh"]),
        ]
    }

    #[test]
    fn each_model_takes_the_menu_its_levels_call_for() {
        let menus: Vec<_> = today().iter().map(effort_menu).collect();
        assert_eq!(
            menus,
            [
                Some(EffortMenu::WithoutXhigh),
                Some(EffortMenu::Full),
                Some(EffortMenu::None),
                Some(EffortMenu::Full),
            ]
        );
        assert_eq!(effort_menu(&CatalogModel::default()), None);
    }

    #[test]
    fn the_cap_is_the_highest_level_the_model_grades_by() {
        let caps: Vec<_> = today().iter().map(max_effort).collect();
        assert_eq!(caps, [Some("max"), Some("max"), None, Some("xhigh")]);
        assert_eq!(max_effort(&model("tiny", &["minimal"])), Some("low"));
    }

    #[test]
    fn every_desktop_name_is_a_distinct_claude_id() {
        assert_eq!(
            desktop_names(&today()),
            [
                Some("claude-sonnet-4-6".to_string()),
                Some("claude-opus-5".to_string()),
                Some("claude-haiku-4-5-20251001".to_string()),
                Some("claude-sonnet-5".to_string()),
            ]
        );
    }

    #[test]
    fn a_model_past_the_last_free_name_is_left_off() {
        let models: Vec<CatalogModel> = (0..3)
            .map(|i| model(&format!("m{i}"), &["low", "max"]))
            .collect();
        assert_eq!(desktop_names(&models)[2], None);
    }

    #[test]
    fn claude_code_borrows_the_menu_but_not_for_an_on_off_model() {
        let picked: Vec<_> = today().iter().map(behaves_as).collect();
        assert_eq!(
            picked,
            [
                Some("claude-opus-4-6"),
                Some("claude-opus-4-8"),
                None,
                Some("claude-opus-4-8"),
            ]
        );
    }
}
