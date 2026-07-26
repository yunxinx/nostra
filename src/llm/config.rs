//! Persisted provider configuration and validated runtime model resolution.
//!
//! Settings may retain invalid profiles so users can repair them. Generation
//! must cross the validation boundary in this module before building a request.

use std::{collections::HashSet, fmt};

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
        if self.models.is_empty() {
            return Err(GatewayError::configuration(
                "provider must contain at least one model",
            ));
        }
        let mut ids = HashSet::new();
        for model in &self.models {
            if model.id.trim().is_empty() || model.model_id.trim().is_empty() {
                return Err(GatewayError::configuration(
                    "model ID and upstream model ID must not be empty",
                ));
            }
            if !ids.insert(model.id.as_str()) {
                return Err(GatewayError::configuration("model IDs must be unique"));
            }
        }
        Ok(())
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
}
