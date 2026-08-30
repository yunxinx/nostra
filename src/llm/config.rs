//! Persisted provider configuration and validated runtime model resolution.
//!
//! Settings may retain invalid profiles so users can repair them. Generation
//! must cross the validation boundary in this module before building a request.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{CompatibilityProfile, GatewayError, Protocol};

pub type ProfileId = String;
pub type ModelId = String;

#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub id: ModelId,
    pub model_id: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub profile_id: ProfileId,
    pub model_id: ModelId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfile {
    pub id: ProfileId,
    pub name: String,
    pub base_url: String,
    pub api_key: SecretString,
    pub protocol: Protocol,
    pub compatibility: CompatibilityProfile,
    pub models: Vec<ModelConfig>,
}

/// Immutable provider catalog captured at a composition boundary.
///
/// Profile validation is performed once when the snapshot is created. Invalid
/// profiles remain in the snapshot so settings can keep presenting repairable
/// drafts; generation rejects a selection against the cached validation result.
#[derive(Clone, Debug)]
pub struct ProviderCatalogSnapshot {
    profiles: Arc<[ProviderProfile]>,
    validations: Arc<[Result<(), GatewayError>]>,
}

impl ProviderCatalogSnapshot {
    #[must_use]
    pub fn new(profiles: Vec<ProviderProfile>) -> Self {
        let validations: Vec<_> = profiles.iter().map(ProviderProfile::validate).collect();
        Self {
            profiles: Arc::from(profiles),
            validations: Arc::from(validations),
        }
    }

    #[must_use]
    pub fn profiles(&self) -> &[ProviderProfile] {
        &self.profiles
    }

    /// Resolve a selection using the validation performed for this snapshot.
    pub fn resolve_selection(
        &self,
        selection: &ModelSelection,
    ) -> Result<(&ProviderProfile, &ModelConfig), GatewayError> {
        let mut matching_profiles = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| profile.id == selection.profile_id);
        let (profile_index, profile) = matching_profiles
            .next()
            .ok_or_else(|| GatewayError::configuration("selected provider is unavailable"))?;
        if matching_profiles.next().is_some() {
            return Err(GatewayError::configuration("provider IDs must be unique"));
        }
        self.validations[profile_index].clone()?;
        let model = profile.resolve_model(&selection.model_id)?;
        Ok((profile, model))
    }
}

pub fn resolve_selection<'a>(
    profiles: &'a [ProviderProfile],
    selection: &ModelSelection,
) -> Result<(&'a ProviderProfile, &'a ModelConfig), GatewayError> {
    let mut matching_profiles = profiles
        .iter()
        .filter(|profile| profile.id == selection.profile_id);
    let profile = matching_profiles
        .next()
        .ok_or_else(|| GatewayError::configuration("selected provider is unavailable"))?;
    if matching_profiles.next().is_some() {
        return Err(GatewayError::configuration("provider IDs must be unique"));
    }
    profile.validate()?;
    let model = profile.resolve_model(&selection.model_id)?;
    Ok((profile, model))
}

impl ProviderProfile {
    /// Validate a profile for runtime use.
    ///
    /// Models with an empty upstream ID are retained as editable drafts, but
    /// the profile must contain at least one configured model. Drafts still
    /// require unique internal IDs and unique non-empty display names. All
    /// non-empty upstream model IDs must also be unique after trimming.
    pub fn validate(&self) -> Result<(), GatewayError> {
        if self.id.trim().is_empty() {
            return Err(GatewayError::configuration("provider ID must not be empty"));
        }
        if self.name.trim().is_empty() {
            return Err(GatewayError::configuration(
                "provider name must not be empty",
            ));
        }
        self.validated_base_url()?;

        let mut display_name_counts = HashMap::new();
        let mut upstream_id_counts = HashMap::new();
        for model in &self.models {
            if let Some(display_name) = model
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                *display_name_counts.entry(display_name).or_insert(0usize) += 1;
            }

            let upstream_id = model.model_id.trim();
            if !upstream_id.is_empty() {
                *upstream_id_counts.entry(upstream_id).or_insert(0usize) += 1;
            }
        }

        let mut ids = HashSet::new();
        let mut configured_models = 0usize;
        for model in &self.models {
            let id = model.id.trim();
            if id.is_empty() {
                return Err(GatewayError::configuration("model ID must not be empty"));
            }
            if !ids.insert(id) {
                return Err(GatewayError::configuration("model IDs must be unique"));
            }

            if let Some(display_name) = model
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                && display_name_counts.get(display_name) != Some(&1)
            {
                return Err(GatewayError::configuration(
                    "model display names must be unique",
                ));
            }

            let upstream_id = model.model_id.trim();
            if upstream_id.is_empty() {
                continue;
            }
            configured_models += 1;
            if upstream_id_counts.get(upstream_id) != Some(&1) {
                return Err(GatewayError::configuration(
                    "upstream model IDs must be unique",
                ));
            }
        }
        if configured_models == 0 {
            return Err(GatewayError::configuration(
                "provider must contain at least one configured model",
            ));
        }
        Ok(())
    }

    pub(crate) fn upstream_model_id_is_available(
        &self,
        upstream_id: &str,
        excluding_id: &str,
    ) -> bool {
        model_value_is_available(&self.models, upstream_id, excluding_id, |model| {
            Some(model.model_id.as_str())
        })
    }

    pub(crate) fn display_name_is_available(&self, display_name: &str, excluding_id: &str) -> bool {
        model_value_is_available(&self.models, display_name, excluding_id, |model| {
            model.display_name.as_deref()
        })
    }

    pub fn validated_base_url(&self) -> Result<String, GatewayError> {
        let base = self.base_url.trim().trim_end_matches('/');
        let url = url::Url::parse(base).map_err(|_| {
            GatewayError::configuration("provider Base URL must be an absolute URL")
        })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(GatewayError::configuration(
                "provider Base URL must use http or https",
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(GatewayError::configuration(
                "provider Base URL must not contain a query or fragment",
            ));
        }
        let path = url.path().trim_end_matches('/');
        if path.ends_with("/chat/completions") || path.ends_with("/responses") {
            return Err(GatewayError::configuration(
                "provider Base URL must not include a protocol endpoint",
            ));
        }
        Ok(base.to_string())
    }

    pub fn resolve_model(&self, id: &str) -> Result<&ModelConfig, GatewayError> {
        self.models
            .iter()
            .find(|model| model.id == id && !model.model_id.trim().is_empty())
            .ok_or_else(|| GatewayError::configuration("selected model is unavailable"))
    }
}

fn model_value_is_available<'a>(
    models: &'a [ModelConfig],
    candidate: &str,
    excluding_id: &str,
    value: impl Fn(&'a ModelConfig) -> Option<&'a str>,
) -> bool {
    let candidate = candidate.trim();
    candidate.is_empty()
        || models.iter().all(|model| {
            model.id == excluding_id || value(model).is_none_or(|value| value.trim() != candidate)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_serializes_plaintext_but_never_formats_it() {
        let secret = SecretString::new("highly-secret");
        assert_eq!(
            serde_json::to_string(&secret).expect("serialize"),
            "\"highly-secret\""
        );
        assert!(!format!("{secret:?} {secret}").contains("highly-secret"));
    }

    #[test]
    fn validation_rejects_ambiguous_or_endpoint_bearing_profiles() {
        let profile = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1/responses?x=1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "m".into(),
                model_id: "gpt".into(),
                display_name: None,
            }],
        };
        assert!(profile.validate().is_err());

        let mut duplicate = profile;
        duplicate.base_url = "https://example.com/v1".into();
        duplicate.models.push(duplicate.models[0].clone());
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn incomplete_model_drafts_do_not_invalidate_configured_models() {
        let mut profile = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "configured".into(),
                model_id: "vendor/model".into(),
                display_name: Some("Model".into()),
            }],
        };
        let selection = ModelSelection {
            profile_id: profile.id.clone(),
            model_id: profile.models[0].id.clone(),
        };
        profile.models.push(ModelConfig {
            id: "draft".into(),
            model_id: String::new(),
            display_name: None,
        });

        assert!(profile.validate().is_ok());
        assert!(resolve_selection(&[profile], &selection).is_ok());
    }

    #[test]
    fn validation_rejects_duplicate_upstream_ids_and_display_names() {
        let profile = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![
                ModelConfig {
                    id: "first".into(),
                    model_id: "vendor/model".into(),
                    display_name: Some("Model".into()),
                },
                ModelConfig {
                    id: "second".into(),
                    model_id: "vendor/model".into(),
                    display_name: Some("Other".into()),
                },
            ],
        };
        assert!(profile.validate().is_err());

        let mut duplicate_name = profile;
        duplicate_name.models[1].model_id = "vendor/other".into();
        duplicate_name.models[1].display_name = Some("Model".into());
        assert!(duplicate_name.validate().is_err());
    }

    #[test]
    fn validation_rejects_duplicate_display_name_on_an_incomplete_draft() {
        let mut profile = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "configured".into(),
                model_id: "vendor/model".into(),
                display_name: Some("Model".into()),
            }],
        };
        profile.models.push(ModelConfig {
            id: "draft".into(),
            model_id: String::new(),
            display_name: Some(" Model ".into()),
        });

        assert!(profile.validate().is_err());
    }

    #[test]
    fn validation_compares_trimmed_model_values() {
        let mut duplicate_name = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![
                ModelConfig {
                    id: "first".into(),
                    model_id: "vendor/first".into(),
                    display_name: Some("Model".into()),
                },
                ModelConfig {
                    id: "second".into(),
                    model_id: "vendor/second".into(),
                    display_name: Some(" Model ".into()),
                },
            ],
        };
        assert!(duplicate_name.validate().is_err());

        duplicate_name.models[1].display_name = Some("Other".into());
        duplicate_name.models[1].model_id = " vendor/first ".into();
        assert!(duplicate_name.validate().is_err());
    }

    #[test]
    fn empty_draft_values_do_not_participate_in_uniqueness_checks() {
        let profile = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![
                ModelConfig {
                    id: "configured".into(),
                    model_id: "vendor/model".into(),
                    display_name: Some("Model".into()),
                },
                ModelConfig {
                    id: "draft-one".into(),
                    model_id: String::new(),
                    display_name: None,
                },
                ModelConfig {
                    id: "draft-two".into(),
                    model_id: "   ".into(),
                    display_name: Some("   ".into()),
                },
            ],
        };

        assert!(profile.validate().is_ok());
    }

    #[test]
    fn selection_rejects_duplicate_profile_ids() {
        let profile = ProviderProfile {
            id: "p".into(),
            name: "Provider".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "m".into(),
                model_id: "gpt".into(),
                display_name: None,
            }],
        };
        assert!(
            resolve_selection(
                &[profile.clone(), profile],
                &ModelSelection {
                    profile_id: "p".into(),
                    model_id: "m".into(),
                }
            )
            .is_err()
        );
    }

    #[test]
    fn catalog_snapshot_caches_validation_and_keeps_repairable_profiles() {
        let mut invalid = ProviderProfile {
            id: "invalid".into(),
            name: "Invalid".into(),
            base_url: "https://example.com/v1/responses".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "model".into(),
                model_id: "vendor/model".into(),
                display_name: None,
            }],
        };
        let valid = ProviderProfile {
            id: "valid".into(),
            name: "Valid".into(),
            base_url: "https://example.com/v1".into(),
            api_key: SecretString::default(),
            protocol: Protocol::Responses,
            compatibility: CompatibilityProfile::default(),
            models: vec![ModelConfig {
                id: "model".into(),
                model_id: "vendor/model".into(),
                display_name: None,
            }],
        };
        let snapshot = ProviderCatalogSnapshot::new(vec![invalid.clone(), valid]);

        assert_eq!(snapshot.profiles().len(), 2);
        assert!(
            snapshot
                .resolve_selection(&ModelSelection {
                    profile_id: "invalid".into(),
                    model_id: "model".into(),
                })
                .is_err()
        );
        assert!(
            snapshot
                .resolve_selection(&ModelSelection {
                    profile_id: "valid".into(),
                    model_id: "model".into(),
                })
                .is_ok()
        );

        invalid.base_url = "https://example.com/v1".into();
        assert!(
            snapshot
                .resolve_selection(&ModelSelection {
                    profile_id: "invalid".into(),
                    model_id: "model".into(),
                })
                .is_err()
        );
    }
}
