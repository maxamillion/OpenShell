// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    DiscoveredProvider, DiscoveryContext, ProviderDiscoverySpec, ProviderError, ProviderPlugin,
    RealDiscoveryContext, discover_with_spec,
};

pub struct VertexProvider;

pub const SPEC: ProviderDiscoverySpec = ProviderDiscoverySpec {
    id: "vertex",
    credential_env_vars: &["ANTHROPIC_VERTEX_PROJECT_ID"],
};

const ADC_ENV_VAR: &str = "VERTEX_ADC";
const REGION_ENV_VAR: &str = "ANTHROPIC_VERTEX_REGION";
const CLAUDE_VERTEX_FLAG: &str = "CLAUDE_CODE_USE_VERTEX";
const STANDARD_ADC_PATH: &str = ".config/gcloud/application_default_credentials.json";

impl VertexProvider {
    fn discover_with_context(
        context: &dyn DiscoveryContext,
        standard_adc: Option<String>,
    ) -> Result<Option<DiscoveredProvider>, ProviderError> {
        let mut discovered = discover_with_spec(&SPEC, context)?.ok_or_else(|| {
            ProviderError::UnsupportedProvider(
                "Vertex AI discovery requires ANTHROPIC_VERTEX_PROJECT_ID to be set".to_string(),
            )
        })?;

        if let Some(region) = context.env_var(REGION_ENV_VAR)
            && !region.trim().is_empty()
        {
            discovered.config.insert(REGION_ENV_VAR.to_string(), region);
        }

        let adc_json = context
            .env_var(ADC_ENV_VAR)
            .filter(|value| !value.trim().is_empty())
            .or(standard_adc)
            .ok_or_else(|| {
                ProviderError::UnsupportedProvider(
                    "Vertex AI discovery requires Google Application Default Credentials. \
                     Set VERTEX_ADC or run 'gcloud auth application-default login'."
                        .to_string(),
                )
            })?;

        validate_adc_json(&adc_json)?;
        discovered
            .credentials
            .insert(ADC_ENV_VAR.to_string(), adc_json);
        discovered
            .credentials
            .insert(CLAUDE_VERTEX_FLAG.to_string(), "1".to_string());

        Ok(Some(discovered))
    }

    fn read_adc_from_standard_path() -> Option<String> {
        let home = std::env::var("HOME").ok()?;
        let path = std::path::Path::new(&home).join(STANDARD_ADC_PATH);
        std::fs::read_to_string(path).ok()
    }
}

impl ProviderPlugin for VertexProvider {
    fn id(&self) -> &'static str {
        SPEC.id
    }

    fn discover_existing(&self) -> Result<Option<DiscoveredProvider>, ProviderError> {
        Self::discover_with_context(&RealDiscoveryContext, Self::read_adc_from_standard_path())
    }

    fn credential_env_vars(&self) -> &'static [&'static str] {
        SPEC.credential_env_vars
    }
}

fn validate_adc_json(adc_json: &str) -> Result<(), ProviderError> {
    let json: serde_json::Value = serde_json::from_str(adc_json).map_err(|err| {
        ProviderError::UnsupportedProvider(format!(
            "VERTEX_ADC is not valid JSON: {err}. Expected Google Application Default Credentials JSON."
        ))
    })?;

    let object = json.as_object().ok_or_else(|| {
        ProviderError::UnsupportedProvider(
            "VERTEX_ADC must be a JSON object with Google Application Default Credentials"
                .to_string(),
        )
    })?;

    let credential_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if credential_type != "authorized_user" {
        return Err(ProviderError::UnsupportedProvider(format!(
            "Vertex AI discovery currently expects Application Default Credentials from \
             'gcloud auth application-default login' (type=authorized_user), found type={credential_type:?}"
        )));
    }

    for key in ["client_id", "client_secret", "refresh_token"] {
        let present = object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        if !present {
            return Err(ProviderError::UnsupportedProvider(format!(
                "VERTEX_ADC is missing required field '{key}'"
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ADC_ENV_VAR, CLAUDE_VERTEX_FLAG, REGION_ENV_VAR, SPEC, VertexProvider, validate_adc_json,
    };
    use crate::test_helpers::MockDiscoveryContext;

    const VALID_ADC_JSON: &str = r#"{
  "client_id": "client-id",
  "client_secret": "client-secret",
  "refresh_token": "refresh-token",
  "type": "authorized_user"
}"#;

    #[test]
    fn discovers_vertex_project_and_adc_from_env() {
        let ctx = MockDiscoveryContext::new()
            .with_env("ANTHROPIC_VERTEX_PROJECT_ID", "my-gcp-project")
            .with_env(REGION_ENV_VAR, "us-east5")
            .with_env(ADC_ENV_VAR, VALID_ADC_JSON);
        let discovered = VertexProvider::discover_with_context(&ctx, None)
            .expect("discovery")
            .expect("provider");

        assert_eq!(
            discovered.credentials.get("ANTHROPIC_VERTEX_PROJECT_ID"),
            Some(&"my-gcp-project".to_string())
        );
        assert_eq!(
            discovered.credentials.get(ADC_ENV_VAR),
            Some(&VALID_ADC_JSON.to_string())
        );
        assert_eq!(
            discovered.credentials.get(CLAUDE_VERTEX_FLAG),
            Some(&"1".to_string())
        );
        assert_eq!(
            discovered.config.get(REGION_ENV_VAR),
            Some(&"us-east5".to_string())
        );
    }

    #[test]
    fn discovers_vertex_adc_from_standard_path_fallback() {
        let ctx =
            MockDiscoveryContext::new().with_env("ANTHROPIC_VERTEX_PROJECT_ID", "my-gcp-project");
        let discovered =
            VertexProvider::discover_with_context(&ctx, Some(VALID_ADC_JSON.to_string()))
                .expect("discovery")
                .expect("provider");

        assert_eq!(
            discovered.credentials.get(ADC_ENV_VAR),
            Some(&VALID_ADC_JSON.to_string())
        );
    }

    #[test]
    fn rejects_missing_project_id() {
        let ctx = MockDiscoveryContext::new().with_env(ADC_ENV_VAR, VALID_ADC_JSON);
        let err =
            VertexProvider::discover_with_context(&ctx, None).expect_err("missing project id");
        assert!(err.to_string().contains("ANTHROPIC_VERTEX_PROJECT_ID"));
    }

    #[test]
    fn rejects_missing_adc_credentials() {
        let ctx =
            MockDiscoveryContext::new().with_env("ANTHROPIC_VERTEX_PROJECT_ID", "my-gcp-project");
        let err = VertexProvider::discover_with_context(&ctx, None).expect_err("missing adc");
        assert!(err.to_string().contains("Application Default Credentials"));
    }

    #[test]
    fn rejects_malformed_adc_json() {
        let err = validate_adc_json("{not-json}").expect_err("invalid json");
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn rejects_missing_required_adc_fields() {
        let err =
            validate_adc_json(r#"{"client_id":"x","client_secret":"y","type":"authorized_user"}"#)
                .expect_err("missing refresh token");
        assert!(err.to_string().contains("refresh_token"));
    }

    #[test]
    fn vertex_spec_discovers_project_id() {
        let ctx =
            MockDiscoveryContext::new().with_env("ANTHROPIC_VERTEX_PROJECT_ID", "my-gcp-project");
        let discovered = crate::discover_with_spec(&SPEC, &ctx)
            .expect("discovery")
            .expect("provider");
        assert_eq!(
            discovered.credentials.get("ANTHROPIC_VERTEX_PROJECT_ID"),
            Some(&"my-gcp-project".to_string())
        );
    }
}
