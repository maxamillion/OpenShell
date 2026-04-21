// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use openshell_core::proto::Provider;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tonic::Status;

const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REFRESH_MARGIN: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub(crate) struct CachedVertexToken {
    access_token: String,
    expires_at: Instant,
}

impl CachedVertexToken {
    fn is_fresh(&self) -> bool {
        Instant::now() + REFRESH_MARGIN < self.expires_at
    }
}

pub(crate) type VertexTokenCache = tokio::sync::Mutex<HashMap<String, CachedVertexToken>>;

pub(crate) fn new_vertex_token_cache() -> VertexTokenCache {
    tokio::sync::Mutex::new(HashMap::new())
}

#[derive(Debug, Deserialize)]
struct AdcCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    #[serde(rename = "type")]
    credential_type: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

pub(crate) async fn resolve_vertex_access_token(
    cache: &VertexTokenCache,
    provider: &Provider,
) -> Result<String, Status> {
    {
        let cache = cache.lock().await;
        if let Some(cached) = cache.get(&provider.name)
            && cached.is_fresh()
        {
            return Ok(cached.access_token.clone());
        }
    }

    let adc_json = provider
        .credentials
        .get("VERTEX_ADC")
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Status::failed_precondition(format!(
                "provider '{}' is missing VERTEX_ADC credentials required for Vertex AI",
                provider.name
            ))
        })?;

    let adc: AdcCredentials = serde_json::from_str(adc_json).map_err(|err| {
        Status::failed_precondition(format!(
            "provider '{}' has invalid VERTEX_ADC JSON: {err}",
            provider.name
        ))
    })?;
    if adc.credential_type != "authorized_user" {
        return Err(Status::failed_precondition(format!(
            "provider '{}' expects Application Default Credentials from \
             'gcloud auth application-default login' (type=authorized_user), found type={:?}",
            provider.name, adc.credential_type
        )));
    }

    let response = reqwest::Client::new()
        .post(GOOGLE_OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", adc.client_id.as_str()),
            ("client_secret", adc.client_secret.as_str()),
            ("refresh_token", adc.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|err| {
            Status::unavailable(format!(
                "failed to exchange Vertex ADC for an OAuth token: {err}"
            ))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(Status::failed_precondition(format!(
            "provider '{}' failed to exchange Vertex ADC for an OAuth token \
             (HTTP {status}): {body}",
            provider.name
        )));
    }

    let token: TokenResponse = response.json().await.map_err(|err| {
        Status::internal(format!(
            "provider '{}' returned an unreadable Vertex OAuth token response: {err}",
            provider.name
        ))
    })?;
    let access_token = token.access_token.trim().to_string();
    if access_token.is_empty() {
        return Err(Status::internal(format!(
            "provider '{}' returned an empty Vertex OAuth access token",
            provider.name
        )));
    }

    let expires_at =
        Instant::now() + Duration::from_secs(token.expires_in.max(REFRESH_MARGIN.as_secs() + 1));
    let mut cache = cache.lock().await;
    cache.insert(
        provider.name.clone(),
        CachedVertexToken {
            access_token: access_token.clone(),
            expires_at,
        },
    );
    Ok(access_token)
}
