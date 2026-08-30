//! Provider profile persistence boundary shared by settings and generation UI.
//!
//! Mutations persist through preferences and advance a process-local catalog
//! revision. Menus use the same validated model projection as gateway routing,
//! preventing UI selection rules from drifting from generation rules.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use gpui::App;

use crate::{
    llm::{ModelConfig, ModelSelection, ProviderProfile, resolve_selection},
    preferences,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectableModel {
    pub selection: ModelSelection,
    pub profile_name: String,
    pub model_name: String,
}

static CATALOG_REVISION: AtomicU64 = AtomicU64::new(0);

pub fn catalog_revision() -> u64 {
    CATALOG_REVISION.load(Ordering::Relaxed)
}

fn update_preferences(cx: &mut App, change: impl FnOnce(&mut preferences::Preferences)) {
    #[cfg(test)]
    preferences::update_in_memory(cx, change);
    #[cfg(not(test))]
    preferences::update(cx, change);
}

fn update_catalog(cx: &mut App, change: impl FnOnce(&mut Vec<ProviderProfile>)) {
    update_preferences(cx, |prefs| change(&mut prefs.provider_profiles));
    CATALOG_REVISION.fetch_add(1, Ordering::Relaxed);
    cx.refresh_windows();
}

pub fn profiles(cx: &App) -> &[ProviderProfile] {
    &preferences::get(cx).provider_profiles
}

pub fn last_selection(cx: &App) -> Option<ModelSelection> {
    preferences::get(cx).last_model_selection.clone()
}

pub fn selectable_models(cx: &App) -> Vec<SelectableModel> {
    selectable_models_from(profiles(cx))
}

fn selectable_models_from(profiles: &[ProviderProfile]) -> Vec<SelectableModel> {
    let mut profile_id_counts = HashMap::new();
    for profile in profiles {
        *profile_id_counts
            .entry(profile.id.as_str())
            .or_insert(0usize) += 1;
    }

    profiles
        .iter()
        .filter(|profile| profile_id_counts.get(profile.id.as_str()) == Some(&1))
        .filter(|profile| profile.validate().is_ok())
        .flat_map(|profile| {
            profile
                .models
                .iter()
                .filter(|model| !model.model_id.trim().is_empty())
                .map(|model| {
                    let selection = ModelSelection {
                        profile_id: profile.id.clone(),
                        model_id: model.id.clone(),
                    };
                    let model_name = model
                        .display_name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(&model.model_id)
                        .to_string();
                    SelectableModel {
                        selection,
                        profile_name: profile.name.clone(),
                        model_name,
                    }
                })
        })
        .collect()
}

pub fn selection_is_available(selection: Option<&ModelSelection>, cx: &App) -> bool {
    selection.is_some_and(|selection| resolve_selection(profiles(cx), selection).is_ok())
}

pub fn find<'a>(id: &str, cx: &'a App) -> Option<&'a ProviderProfile> {
    profiles(cx).iter().find(|profile| profile.id == id)
}

pub fn add(profile: ProviderProfile, cx: &mut App) {
    update_catalog(cx, |profiles| profiles.push(profile));
}

pub fn update(id: &str, cx: &mut App, change: impl FnOnce(&mut ProviderProfile)) {
    update_catalog(cx, |profiles| {
        if let Some(profile) = profiles.iter_mut().find(|profile| profile.id == id) {
            change(profile);
        }
    });
}

pub fn remove(id: &str, cx: &mut App) {
    update_preferences(cx, |prefs| {
        remove_profile_from_preferences(id, prefs);
    });
    CATALOG_REVISION.fetch_add(1, Ordering::Relaxed);
    cx.refresh_windows();
}

fn remove_profile_from_preferences(id: &str, prefs: &mut preferences::Preferences) {
    prefs.provider_profiles.retain(|profile| profile.id != id);
    if prefs
        .last_model_selection
        .as_ref()
        .is_some_and(|selection| selection.profile_id == id)
    {
        prefs.last_model_selection = None;
    }
}

pub fn add_model(profile_id: &str, model: ModelConfig, cx: &mut App) {
    update(profile_id, cx, |profile| profile.models.push(model));
}

pub fn update_model(
    profile_id: &str,
    model_id: &str,
    cx: &mut App,
    change: impl FnOnce(&mut ModelConfig),
) {
    update(profile_id, cx, |profile| {
        if let Some(model) = profile.models.iter_mut().find(|model| model.id == model_id) {
            change(model);
        }
    });
}

#[cfg(test)]
pub(crate) fn update_model_in_memory(
    profile_id: &str,
    model_id: &str,
    cx: &mut App,
    change: impl FnOnce(&mut ModelConfig),
) {
    preferences::update_in_memory(cx, |prefs| {
        let Some(profile) = prefs
            .provider_profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        else {
            return;
        };
        let Some(model) = profile.models.iter_mut().find(|model| model.id == model_id) else {
            return;
        };
        change(model);
    });
    CATALOG_REVISION.fetch_add(1, Ordering::Relaxed);
    cx.refresh_windows();
}

pub fn remove_model(profile_id: &str, model_id: &str, cx: &mut App) {
    update_preferences(cx, |prefs| {
        if let Some(profile) = prefs
            .provider_profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        {
            profile.models.retain(|model| model.id != model_id);
        }
        if prefs
            .last_model_selection
            .as_ref()
            .is_some_and(|selection| {
                selection.profile_id == profile_id && selection.model_id == model_id
            })
        {
            prefs.last_model_selection = None;
        }
    });
    CATALOG_REVISION.fetch_add(1, Ordering::Relaxed);
    cx.refresh_windows();
}

pub fn select_model(selection: ModelSelection, cx: &mut App) {
    // Entity tests exercise the same ChatView::select_model -> provider
    // persistence boundary as production. Keep that path intact while avoiding
    // writes to the developer's real configuration directory under `cargo
    // test`; the in-memory global remains the authoritative observable state.
    update_preferences(cx, |prefs| prefs.last_model_selection = Some(selection));
}

#[cfg(test)]
mod tests {
    use crate::llm::{CompatibilityProfile, Protocol, SecretString};

    use super::*;

    fn profile(id: &str, name: &str, model_id: &str) -> ProviderProfile {
        ProviderProfile {
            id: id.into(),
            name: name.into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "model".into(),
                model_id: model_id.into(),
                display_name: None,
            }],
        }
    }

    #[test]
    fn selectable_models_use_the_generation_validation_boundary() {
        let profiles = vec![
            profile("valid", "Provider", "gpt"),
            profile("invalid-name", "", "gpt"),
            profile("invalid-model", "Provider", ""),
            profile("duplicate", "One", "gpt"),
            profile("duplicate", "Two", "gpt"),
        ];

        let selectable = selectable_models_from(&profiles);

        assert_eq!(selectable.len(), 1);
        assert_eq!(selectable[0].selection.profile_id, "valid");
        assert_eq!(selectable[0].model_name, "gpt");
    }

    #[test]
    fn incomplete_model_draft_does_not_hide_existing_selectable_models() {
        let mut profile = profile("provider", "Provider", "vendor/model");
        profile.models.push(ModelConfig {
            id: "draft".into(),
            model_id: String::new(),
            display_name: None,
        });

        let selectable = selectable_models_from(&[profile]);

        assert_eq!(selectable.len(), 1);
        assert_eq!(selectable[0].selection.model_id, "model");
        assert_eq!(selectable[0].model_name, "vendor/model");
    }
}
