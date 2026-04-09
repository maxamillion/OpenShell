// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{DiscoveredProvider, DiscoveryContext, ProviderError, ProviderPlugin, RealDiscoveryContext};

pub struct GcpProvider;

/// Well-known path for gcloud Application Default Credentials.
const ADC_PATH: &str = ".config/gcloud/application_default_credentials.json";

/// Well-known path for gcloud active config name.
const ACTIVE_CONFIG_PATH: &str = ".config/gcloud/active_config";

/// Well-known path prefix for gcloud named configuration files.
const CONFIG_DIR: &str = ".config/gcloud/configurations";

impl ProviderPlugin for GcpProvider {
    fn id(&self) -> &'static str {
        "gcp"
    }

    fn discover_existing(&self) -> Result<Option<DiscoveredProvider>, ProviderError> {
        discover_gcp(&RealDiscoveryContext)
    }

    fn credential_env_vars(&self) -> &'static [&'static str] {
        &["GOOGLE_APPLICATION_CREDENTIALS"]
    }
}

/// Discover GCP credentials and config from the local machine.
///
/// Scans for credentials in priority order:
/// 1. `GOOGLE_APPLICATION_CREDENTIALS` env var → service account key file
/// 2. gcloud ADC file (`~/.config/gcloud/application_default_credentials.json`)
///
/// Then scans for project/region from env vars and gcloud config.
pub fn discover_gcp(context: &dyn DiscoveryContext) -> Result<Option<DiscoveredProvider>, ProviderError> {
    let mut discovered = DiscoveredProvider::default();

    // --- Auth credentials ---

    // 1. Try GOOGLE_APPLICATION_CREDENTIALS → service account key file.
    if let Some(path) = context.env_var("GOOGLE_APPLICATION_CREDENTIALS") {
        let path = std::path::Path::new(&path);
        if let Some(content) = context.read_file(path) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
                if parsed.get("type").and_then(|v| v.as_str()) == Some("service_account")
                    && parsed.get("private_key").and_then(|v| v.as_str()).is_some()
                    && parsed.get("client_email").and_then(|v| v.as_str()).is_some()
                {
                    discovered.credentials.insert(
                        "GCP_SERVICE_ACCOUNT_KEY".to_string(),
                        content,
                    );
                    // Extract project_id from the SA key as a fallback.
                    if let Some(project) = parsed.get("project_id").and_then(|v| v.as_str()) {
                        if !project.is_empty() {
                            discovered.config.insert("gcp_project".to_string(), project.to_string());
                        }
                    }
                }
            }
        }
    }

    // 2. Try gcloud ADC file (from `gcloud auth application-default login`).
    if discovered.credentials.is_empty() {
        if let Some(adc_content) = read_home_file(context, ADC_PATH) {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&adc_content) {
                if parsed.get("type").and_then(|v| v.as_str()) == Some("authorized_user") {
                    if let (Some(client_id), Some(client_secret), Some(refresh_token)) = (
                        parsed.get("client_id").and_then(|v| v.as_str()),
                        parsed.get("client_secret").and_then(|v| v.as_str()),
                        parsed.get("refresh_token").and_then(|v| v.as_str()),
                    ) {
                        discovered.credentials.insert(
                            "GCP_ADC_CLIENT_ID".to_string(),
                            client_id.to_string(),
                        );
                        discovered.credentials.insert(
                            "GCP_ADC_CLIENT_SECRET".to_string(),
                            client_secret.to_string(),
                        );
                        discovered.credentials.insert(
                            "GCP_ADC_REFRESH_TOKEN".to_string(),
                            refresh_token.to_string(),
                        );
                    }

                    // Use quota_project_id as a fallback for project.
                    if let Some(project) = parsed.get("quota_project_id").and_then(|v| v.as_str()) {
                        if !project.is_empty() {
                            discovered.config.entry("gcp_project".to_string())
                                .or_insert_with(|| project.to_string());
                        }
                    }
                }
            }
        }
    }

    // If no auth credentials found, bail out.
    if discovered.credentials.is_empty() {
        return Ok(None);
    }

    // --- Project ID ---
    // Check env vars, then gcloud config file.
    if !discovered.config.contains_key("gcp_project") {
        for var in &["CLOUDSDK_CORE_PROJECT", "GOOGLE_CLOUD_PROJECT"] {
            if let Some(val) = context.env_var(var) {
                if !val.trim().is_empty() {
                    discovered.config.insert("gcp_project".to_string(), val);
                    break;
                }
            }
        }
    }

    if !discovered.config.contains_key("gcp_project") {
        if let Some(project) = read_gcloud_config_value(context, "core", "project") {
            discovered.config.insert("gcp_project".to_string(), project);
        }
    }

    // --- Region ---
    if !discovered.config.contains_key("gcp_region") {
        if let Some(val) = context.env_var("CLOUDSDK_COMPUTE_REGION") {
            if !val.trim().is_empty() {
                discovered.config.insert("gcp_region".to_string(), val);
            }
        }
    }

    if !discovered.config.contains_key("gcp_region") {
        if let Some(region) = read_gcloud_config_value(context, "compute", "region") {
            discovered.config.insert("gcp_region".to_string(), region);
        }
    }

    Ok(Some(discovered))
}

/// Read a file relative to the user's home directory.
fn read_home_file(context: &dyn DiscoveryContext, relative: &str) -> Option<String> {
    let home = context.home_dir()?;
    let path = home.join(relative);
    context.read_file(&path)
}

/// Read a value from the active gcloud configuration file (INI format).
///
/// Reads `~/.config/gcloud/active_config` to determine the active profile,
/// then reads `~/.config/gcloud/configurations/config_<profile>` to find the
/// value under `[section] key = value`.
fn read_gcloud_config_value(
    context: &dyn DiscoveryContext,
    section: &str,
    key: &str,
) -> Option<String> {
    let active_config = read_home_file(context, ACTIVE_CONFIG_PATH)?;
    let profile = active_config.trim();
    if profile.is_empty() {
        return None;
    }

    let config_path = format!("{CONFIG_DIR}/config_{profile}");
    let content = read_home_file(context, &config_path)?;
    parse_ini_value(&content, section, key)
}

/// Minimal INI parser: find `[section]` then `key = value` under it.
fn parse_ini_value(content: &str, section: &str, key: &str) -> Option<String> {
    let target_section = format!("[{section}]");
    let mut in_section = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_section = trimmed == target_section;
            continue;
        }
        if in_section {
            if let Some((k, v)) = trimmed.split_once('=') {
                if k.trim() == key {
                    let value = v.trim().to_string();
                    if !value.is_empty() {
                        return Some(value);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::MockDiscoveryContext;

    fn sample_sa_key() -> String {
        serde_json::json!({
            "type": "service_account",
            "project_id": "test-project",
            "private_key_id": "key-id-123",
            "private_key": "-----BEGIN RSA PRIVATE KEY-----\nfake\n-----END RSA PRIVATE KEY-----\n",
            "client_email": "test@test-project.iam.gserviceaccount.com",
            "client_id": "123456789",
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token",
            "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
            "client_x509_cert_url": "https://www.googleapis.com/robot/v1/metadata/x509/test",
            "universe_domain": "googleapis.com"
        })
        .to_string()
    }

    fn sample_adc() -> String {
        serde_json::json!({
            "account": "",
            "client_id": "764086051850-6qr4p6gpi6hn506pt8ejuq83di341hur.apps.googleusercontent.com",
            "client_secret": "d-FL95Q19q7MQmFpd7hHD0Ty",
            "quota_project_id": "my-quota-project",
            "refresh_token": "1//0fMxpbzWG3WneCgYIARAAGA8SNwF-test",
            "type": "authorized_user",
            "universe_domain": "googleapis.com"
        })
        .to_string()
    }

    #[test]
    fn discovers_sa_key_from_env() {
        let ctx = MockDiscoveryContext::new()
            .with_env("GOOGLE_APPLICATION_CREDENTIALS", "/path/to/sa.json")
            .with_file("/path/to/sa.json", &sample_sa_key());

        let discovered = discover_gcp(&ctx).expect("discovery").expect("provider");
        assert!(discovered.credentials.contains_key("GCP_SERVICE_ACCOUNT_KEY"));
        assert_eq!(
            discovered.config.get("gcp_project"),
            Some(&"test-project".to_string())
        );
    }

    #[test]
    fn discovers_adc_credentials() {
        let ctx = MockDiscoveryContext::new()
            .with_home("/home/test")
            .with_file(
                format!("/home/test/{ADC_PATH}"),
                &sample_adc(),
            );

        let discovered = discover_gcp(&ctx).expect("discovery").expect("provider");
        assert!(discovered.credentials.contains_key("GCP_ADC_CLIENT_ID"));
        assert!(discovered.credentials.contains_key("GCP_ADC_CLIENT_SECRET"));
        assert!(discovered.credentials.contains_key("GCP_ADC_REFRESH_TOKEN"));
        assert_eq!(
            discovered.config.get("gcp_project"),
            Some(&"my-quota-project".to_string())
        );
    }

    #[test]
    fn sa_key_takes_priority_over_adc() {
        let ctx = MockDiscoveryContext::new()
            .with_env("GOOGLE_APPLICATION_CREDENTIALS", "/sa.json")
            .with_file("/sa.json", &sample_sa_key())
            .with_home("/home/test")
            .with_file(
                format!("/home/test/{ADC_PATH}"),
                &sample_adc(),
            );

        let discovered = discover_gcp(&ctx).expect("discovery").expect("provider");
        // Should use SA key, not ADC.
        assert!(discovered.credentials.contains_key("GCP_SERVICE_ACCOUNT_KEY"));
        assert!(!discovered.credentials.contains_key("GCP_ADC_CLIENT_ID"));
    }

    #[test]
    fn discovers_project_from_env() {
        let ctx = MockDiscoveryContext::new()
            .with_env("GOOGLE_APPLICATION_CREDENTIALS", "/sa.json")
            .with_file("/sa.json", &serde_json::json!({
                "type": "service_account",
                "private_key": "-----BEGIN RSA PRIVATE KEY-----\nfake\n-----END RSA PRIVATE KEY-----\n",
                "client_email": "test@proj.iam.gserviceaccount.com",
                "token_uri": "https://oauth2.googleapis.com/token"
            }).to_string())
            .with_env("CLOUDSDK_CORE_PROJECT", "env-project");

        let discovered = discover_gcp(&ctx).expect("discovery").expect("provider");
        // SA key had no project_id, so env var should be used.
        assert_eq!(
            discovered.config.get("gcp_project"),
            Some(&"env-project".to_string())
        );
    }

    #[test]
    fn discovers_region_from_env() {
        let ctx = MockDiscoveryContext::new()
            .with_env("GOOGLE_APPLICATION_CREDENTIALS", "/sa.json")
            .with_file("/sa.json", &sample_sa_key())
            .with_env("CLOUDSDK_COMPUTE_REGION", "us-central1");

        let discovered = discover_gcp(&ctx).expect("discovery").expect("provider");
        assert_eq!(
            discovered.config.get("gcp_region"),
            Some(&"us-central1".to_string())
        );
    }

    #[test]
    fn discovers_project_and_region_from_gcloud_config() {
        let ctx = MockDiscoveryContext::new()
            .with_env("GOOGLE_APPLICATION_CREDENTIALS", "/sa.json")
            .with_file("/sa.json", &serde_json::json!({
                "type": "service_account",
                "private_key": "-----BEGIN RSA PRIVATE KEY-----\nfake\n-----END RSA PRIVATE KEY-----\n",
                "client_email": "test@proj.iam.gserviceaccount.com",
                "token_uri": "https://oauth2.googleapis.com/token"
            }).to_string())
            .with_home("/home/test")
            .with_file("/home/test/.config/gcloud/active_config", "default")
            .with_file(
                "/home/test/.config/gcloud/configurations/config_default",
                "[core]\naccount = user@example.com\nproject = config-project\n\n[compute]\nregion = us-east5\n"
            );

        let discovered = discover_gcp(&ctx).expect("discovery").expect("provider");
        assert_eq!(
            discovered.config.get("gcp_project"),
            Some(&"config-project".to_string())
        );
        assert_eq!(
            discovered.config.get("gcp_region"),
            Some(&"us-east5".to_string())
        );
    }

    #[test]
    fn returns_none_when_no_credentials_found() {
        let ctx = MockDiscoveryContext::new();
        let result = discover_gcp(&ctx).expect("discovery");
        assert!(result.is_none());
    }

    #[test]
    fn parse_ini_extracts_values() {
        let content = "[core]\naccount = user@example.com\nproject = my-project\n\n[compute]\nregion = us-east5\n";
        assert_eq!(parse_ini_value(content, "core", "project"), Some("my-project".to_string()));
        assert_eq!(parse_ini_value(content, "compute", "region"), Some("us-east5".to_string()));
        assert_eq!(parse_ini_value(content, "core", "missing"), None);
        assert_eq!(parse_ini_value(content, "missing", "project"), None);
    }
}
