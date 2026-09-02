//! Provider catalog persistence shared by settings and generation.
//!
//! The catalog lives in `providers.json`, not `preferences.json`. Settings and
//! generation both read the same live handle so routing cannot drift from the
//! editor. Preference schema changes therefore cannot discard API keys.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

use crate::{
    llm::{
        ModelConfig, ModelSelection, ProviderCatalogSnapshot, ProviderCatalogSource,
        ProviderProfile, resolve_selection,
    },
    preferences,
};

const FILE_NAME: &str = "providers.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCatalogDocument {
    pub profiles: Vec<ProviderProfile>,
    pub last_model_selection: Option<ModelSelection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectableModel {
    pub selection: ModelSelection,
    pub profile_name: String,
    pub model_name: String,
}

pub type CatalogSaver = Arc<dyn Fn(&ProviderCatalogDocument) -> anyhow::Result<()> + Send + Sync>;

struct CatalogState {
    document: Arc<Mutex<ProviderCatalogDocument>>,
    saver: CatalogSaver,
}

/// Application-scoped owner of provider profiles and the last model selection.
#[derive(Clone)]
pub struct ProviderCatalogHandle {
    state: Arc<CatalogState>,
}

impl ProviderCatalogHandle {
    pub fn json(document: ProviderCatalogDocument) -> Self {
        Self::with_saver(document, Arc::new(save))
    }

    pub fn in_memory(document: ProviderCatalogDocument) -> Self {
        Self::with_saver(document, Arc::new(|_| Ok(())))
    }

    pub fn with_saver(document: ProviderCatalogDocument, saver: CatalogSaver) -> Self {
        Self {
            state: Arc::new(CatalogState {
                document: Arc::new(Mutex::new(document)),
                saver,
            }),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> ProviderCatalogDocument {
        match self.state.document.lock() {
            Ok(document) => document.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn replace(&self, document: ProviderCatalogDocument) -> anyhow::Result<()> {
        {
            let mut current = match self.state.document.lock() {
                Ok(current) => current,
                Err(poisoned) => poisoned.into_inner(),
            };
            *current = document.clone();
        }
        (self.state.saver)(&document)
    }

    pub fn replace_in_memory(&self, document: ProviderCatalogDocument) {
        let mut current = match self.state.document.lock() {
            Ok(current) => current,
            Err(poisoned) => poisoned.into_inner(),
        };
        *current = document;
    }

    pub fn update(&self, f: impl FnOnce(&mut ProviderCatalogDocument)) -> anyhow::Result<()> {
        let mut document = self.snapshot();
        f(&mut document);
        self.replace(document)
    }

    pub fn update_in_memory(&self, f: impl FnOnce(&mut ProviderCatalogDocument)) {
        let mut document = self.snapshot();
        f(&mut document);
        self.replace_in_memory(document);
    }
}

impl ProviderCatalogSource for ProviderCatalogHandle {
    fn catalog(&self) -> ProviderCatalogSnapshot {
        ProviderCatalogSnapshot::new(self.snapshot().profiles)
    }
}

pub struct ProviderCatalog {
    document: ProviderCatalogDocument,
    handle: ProviderCatalogHandle,
}

impl Global for ProviderCatalog {}

pub fn init_global(document: ProviderCatalogDocument, cx: &mut App) {
    init_global_with_handle(ProviderCatalogHandle::json(document), cx);
}

pub fn init_global_with_handle(handle: ProviderCatalogHandle, cx: &mut App) {
    let document = handle.snapshot();
    cx.set_global(ProviderCatalog { document, handle });
}

/// Return the live catalog handle, seeding an empty in-memory catalog in tests
/// that never called [`init_global`].
pub fn ensure_global(cx: &mut App) -> ProviderCatalogHandle {
    if cx.try_global::<ProviderCatalog>().is_some() {
        handle(cx)
    } else {
        let handle = ProviderCatalogHandle::in_memory(ProviderCatalogDocument::default());
        init_global_with_handle(handle.clone(), cx);
        handle
    }
}

pub fn handle(cx: &App) -> ProviderCatalogHandle {
    cx.global::<ProviderCatalog>().handle.clone()
}

#[cfg(test)]
pub(crate) fn test_handle(cx: &App) -> ProviderCatalogHandle {
    cx.try_global::<ProviderCatalog>()
        .map(|catalog| catalog.handle.clone())
        .unwrap_or_else(|| ProviderCatalogHandle::in_memory(ProviderCatalogDocument::default()))
}

#[cfg(test)]
pub fn get(cx: &App) -> &ProviderCatalogDocument {
    &cx.global::<ProviderCatalog>().document
}

fn update_catalog(
    catalog_handle: &ProviderCatalogHandle,
    cx: &mut App,
    change: impl FnOnce(&mut ProviderCatalogDocument),
) {
    #[cfg(test)]
    update_with_in_memory(cx, catalog_handle, change);
    #[cfg(not(test))]
    update_with(cx, catalog_handle, change);
}

#[cfg(not(test))]
pub fn update_with(
    cx: &mut App,
    handle: &ProviderCatalogHandle,
    f: impl FnOnce(&mut ProviderCatalogDocument),
) {
    let catalog = cx.global_mut::<ProviderCatalog>();
    catalog.handle = handle.clone();
    f(&mut catalog.document);
    let snapshot = catalog.document.clone();
    if let Err(error) = handle.replace(snapshot) {
        crate::logging::error(
            "providers",
            format_args!("failed to save provider catalog: {error:?}"),
        );
    }
}

#[cfg(test)]
pub fn update_with_in_memory(
    cx: &mut App,
    handle: &ProviderCatalogHandle,
    f: impl FnOnce(&mut ProviderCatalogDocument),
) {
    let catalog = cx.global_mut::<ProviderCatalog>();
    catalog.handle = handle.clone();
    f(&mut catalog.document);
    handle.replace_in_memory(catalog.document.clone());
}

#[cfg(test)]
pub fn profiles(cx: &App) -> &[ProviderProfile] {
    &get(cx).profiles
}

pub fn profiles_from(document: &ProviderCatalogDocument) -> &[ProviderProfile] {
    &document.profiles
}

#[cfg(test)]
pub fn last_selection(cx: &App) -> Option<ModelSelection> {
    get(cx).last_model_selection.clone()
}

pub fn last_selection_from(document: &ProviderCatalogDocument) -> Option<ModelSelection> {
    document.last_model_selection.clone()
}

pub fn selectable_models_from_catalog(document: &ProviderCatalogDocument) -> Vec<SelectableModel> {
    selectable_models_from(profiles_from(document))
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

pub fn selection_is_available_from(
    selection: Option<&ModelSelection>,
    document: &ProviderCatalogDocument,
) -> bool {
    selection.is_some_and(|selection| resolve_selection(profiles_from(document), selection).is_ok())
}

pub fn find_in<'a>(id: &str, document: &'a ProviderCatalogDocument) -> Option<&'a ProviderProfile> {
    profiles_from(document)
        .iter()
        .find(|profile| profile.id == id)
}

pub fn add(profile: ProviderProfile, catalog_handle: &ProviderCatalogHandle, cx: &mut App) {
    update_catalog(catalog_handle, cx, |document| {
        document.profiles.push(profile)
    });
    cx.refresh_windows();
}

pub fn update(
    id: &str,
    catalog_handle: &ProviderCatalogHandle,
    cx: &mut App,
    change: impl FnOnce(&mut ProviderProfile),
) {
    update_catalog(catalog_handle, cx, |document| {
        if let Some(profile) = document
            .profiles
            .iter_mut()
            .find(|profile| profile.id == id)
        {
            change(profile);
        }
    });
    cx.refresh_windows();
}

pub fn remove(id: &str, catalog_handle: &ProviderCatalogHandle, cx: &mut App) {
    update_catalog(catalog_handle, cx, |document| {
        remove_profile_from_document(id, document);
    });
    cx.refresh_windows();
}

fn remove_profile_from_document(id: &str, document: &mut ProviderCatalogDocument) {
    document.profiles.retain(|profile| profile.id != id);
    if document
        .last_model_selection
        .as_ref()
        .is_some_and(|selection| selection.profile_id == id)
    {
        document.last_model_selection = None;
    }
}

pub fn add_model(
    profile_id: &str,
    model: ModelConfig,
    catalog_handle: &ProviderCatalogHandle,
    cx: &mut App,
) {
    update(profile_id, catalog_handle, cx, |profile| {
        profile.models.push(model)
    });
}

pub fn update_model(
    profile_id: &str,
    model_id: &str,
    catalog_handle: &ProviderCatalogHandle,
    cx: &mut App,
    change: impl FnOnce(&mut ModelConfig),
) {
    update(profile_id, catalog_handle, cx, |profile| {
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
    let handle = test_handle(cx);
    update_with_in_memory(cx, &handle, |document| {
        let Some(profile) = document
            .profiles
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
    cx.refresh_windows();
}

pub fn remove_model(
    profile_id: &str,
    model_id: &str,
    catalog_handle: &ProviderCatalogHandle,
    cx: &mut App,
) {
    update_catalog(catalog_handle, cx, |document| {
        if let Some(profile) = document
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
        {
            profile.models.retain(|model| model.id != model_id);
        }
        if document
            .last_model_selection
            .as_ref()
            .is_some_and(|selection| {
                selection.profile_id == profile_id && selection.model_id == model_id
            })
        {
            document.last_model_selection = None;
        }
    });
    cx.refresh_windows();
}

pub fn select_model(
    selection: ModelSelection,
    catalog_handle: &ProviderCatalogHandle,
    cx: &mut App,
) {
    update_catalog(catalog_handle, cx, |document| {
        document.last_model_selection = Some(selection);
    });
}

pub fn path() -> Option<PathBuf> {
    crate::paths::nostra_config_dir().map(|directory| directory.join(FILE_NAME))
}

/// Load the current-schema catalog. A missing file is empty; a corrupt file
/// is left on disk and yields an empty in-memory document.
pub fn load() -> ProviderCatalogDocument {
    path().map_or_else(ProviderCatalogDocument::default, |path| {
        load_from_path(&path)
    })
}

fn load_from_path(catalog_path: &Path) -> ProviderCatalogDocument {
    match std::fs::read_to_string(catalog_path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(document) => document,
            Err(error) => {
                crate::logging::error(
                    "providers",
                    format_args!("provider catalog is unreadable and was left untouched: {error}"),
                );
                ProviderCatalogDocument::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ProviderCatalogDocument::default()
        }
        Err(error) => {
            crate::logging::error(
                "providers",
                format_args!("failed to read provider catalog: {error}"),
            );
            ProviderCatalogDocument::default()
        }
    }
}

pub fn save(document: &ProviderCatalogDocument) -> anyhow::Result<()> {
    let Some(path) = path() else {
        anyhow::bail!("no config directory available on this platform");
    };
    save_to_path(&path, document)
}

fn save_to_path(path: &Path, document: &ProviderCatalogDocument) -> anyhow::Result<()> {
    preferences::atomic_write_json(path, document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompatibilityProfile, Protocol, SecretString};

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

    #[test]
    fn catalog_document_round_trips_plaintext_secrets() {
        let document = ProviderCatalogDocument {
            profiles: vec![ProviderProfile {
                id: "profile-1".into(),
                name: "Local gateway".into(),
                base_url: "http://localhost:8080/v1".into(),
                api_key: SecretString::new("plain-text-key"),
                protocol: Protocol::Responses,
                compatibility: CompatibilityProfile::default(),
                models: vec![ModelConfig {
                    id: "model-1".into(),
                    model_id: "gpt-compatible".into(),
                    display_name: Some("Local model".into()),
                }],
            }],
            last_model_selection: Some(ModelSelection {
                profile_id: "profile-1".into(),
                model_id: "model-1".into(),
            }),
        };

        let json = serde_json::to_string(&document).expect("serialize");
        assert!(json.contains("plain-text-key"));
        let back: ProviderCatalogDocument = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, document);
    }

    #[test]
    fn catalog_document_rejects_unknown_nested_fields() {
        let document = ProviderCatalogDocument {
            profiles: vec![profile("profile-1", "Provider", "gpt")],
            last_model_selection: Some(ModelSelection {
                profile_id: "profile-1".into(),
                model_id: "model".into(),
            }),
        };

        for path in [
            "/profiles/0/legacy",
            "/profiles/0/models/0/legacy",
            "/profiles/0/compatibility/legacy",
            "/last_model_selection/legacy",
        ] {
            let mut value = serde_json::to_value(&document).expect("serialize");
            value
                .pointer_mut(path.rsplit_once('/').map_or("", |(parent, _)| parent))
                .and_then(serde_json::Value::as_object_mut)
                .expect("object")
                .insert("legacy".into(), serde_json::Value::Bool(true));
            assert!(
                serde_json::from_value::<ProviderCatalogDocument>(value).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn catalog_handle_reflects_live_profile_edits() {
        let handle = ProviderCatalogHandle::in_memory(ProviderCatalogDocument::default());
        assert!(handle.catalog().profiles().is_empty());

        handle.update_in_memory(|document| {
            document
                .profiles
                .push(profile("provider", "Provider", "vendor/model"));
        });

        assert_eq!(handle.catalog().profiles().len(), 1);
        assert_eq!(handle.catalog().profiles()[0].id, "provider");
    }

    #[test]
    fn unreadable_catalog_file_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog_path = directory.path().join(FILE_NAME);
        std::fs::write(&catalog_path, "{not json").expect("write corrupt catalog");
        let loaded = load_from_path(&catalog_path);
        assert_eq!(loaded, ProviderCatalogDocument::default());
        assert_eq!(
            std::fs::read_to_string(&catalog_path).expect("read"),
            "{not json"
        );
    }

    #[test]
    fn missing_catalog_file_stays_empty_and_is_not_created() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog_path = directory.path().join(FILE_NAME);
        assert_eq!(
            load_from_path(&catalog_path),
            ProviderCatalogDocument::default()
        );
        assert!(!catalog_path.exists());
    }

    #[test]
    fn unreadable_preferences_do_not_discard_an_existing_catalog() {
        let directory = tempfile::tempdir().expect("temp dir");
        let catalog_path = directory.path().join(FILE_NAME);
        let document = ProviderCatalogDocument {
            profiles: vec![profile("keep", "Keep", "gpt")],
            last_model_selection: Some(ModelSelection {
                profile_id: "keep".into(),
                model_id: "model".into(),
            }),
        };
        save_to_path(&catalog_path, &document).expect("write catalog");
        assert!(
            serde_json::from_str::<crate::preferences::Preferences>(r#"{"language":"en"}"#)
                .is_err()
        );
        assert_eq!(load_from_path(&catalog_path), document);
        let on_disk: ProviderCatalogDocument =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).expect("read catalog"))
                .expect("catalog json");
        assert_eq!(on_disk, document);
    }
}
