// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `OAuth2` token vending service.
//!
//! Performs `OAuth2` token exchanges, caches access tokens per provider, and
//! deduplicates concurrent refresh requests. Used by
//! `resolve_provider_environment()` to transparently return current access
//! tokens for `OAuth2` providers.

use std::collections::HashMap;
use std::sync::Arc;

use openshell_core::proto::Provider;
use tokio::sync::Mutex;
use tracing::{debug, error, info};

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors returned by the token vending service.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("OAuth2 token exchange failed: {0}")]
    Exchange(String),

    #[error("OAuth2 provider misconfigured: {0}")]
    Config(String),

    #[error("HTTP request to token endpoint failed: {0}")]
    Http(String),

    #[error("token endpoint returned error: {status} — {body}")]
    EndpointError { status: u16, body: String },
}

// ---------------------------------------------------------------------------
// Token result
// ---------------------------------------------------------------------------

/// Result of a successful token acquisition.
#[derive(Debug, Clone)]
pub struct TokenResult {
    /// The current access token value.
    pub access_token: String,
    /// Seconds until this token expires.
    pub expires_in_secs: u32,
    /// If the `IdP` returned a rotated refresh token, it is captured here
    /// so the caller can persist it.
    pub new_refresh_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Cached token
// ---------------------------------------------------------------------------

/// Cached access token with expiry metadata.
#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    /// Absolute expiry time (monotonic).
    expires_at: tokio::time::Instant,
    /// Original TTL in seconds as reported by the `IdP`.
    ttl_secs: u32,
}

impl CachedToken {
    /// Returns `true` if the token is still usable (with safety margin).
    fn is_valid(&self) -> bool {
        let margin = std::cmp::max(60, u64::from(self.ttl_secs / 10));
        let margin_dur = std::time::Duration::from_secs(margin);
        tokio::time::Instant::now() + margin_dur < self.expires_at
    }
}

// ---------------------------------------------------------------------------
// Per-provider state
// ---------------------------------------------------------------------------

/// Per-provider token state, protected by a Tokio mutex for async refresh.
struct ProviderTokenState {
    cached: Option<CachedToken>,
}

// ---------------------------------------------------------------------------
// Token vending service
// ---------------------------------------------------------------------------

/// Token vending service. One per gateway process.
///
/// Caches access tokens per provider and deduplicates concurrent refresh
/// requests via a per-provider `Mutex`.
pub struct TokenVendingService {
    /// Per-provider token state, keyed by provider ID.
    state: dashmap::DashMap<String, Arc<Mutex<ProviderTokenState>>>,
    /// Shared HTTP client for token endpoint requests.
    http_client: reqwest::Client,
}

impl std::fmt::Debug for TokenVendingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenVendingService")
            .field("cached_providers", &self.state.len())
            .finish()
    }
}

impl TokenVendingService {
    /// Create a new token vending service.
    #[must_use]
    pub fn new() -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            state: dashmap::DashMap::new(),
            http_client,
        }
    }

    /// Get a current access token for an `OAuth2` provider.
    ///
    /// Returns the cached token if still valid (with safety margin).
    /// Otherwise performs a token refresh, caches the result, and returns it.
    /// Concurrent callers for the same provider are deduplicated — only one
    /// HTTP request to the `IdP` is made; others await the result.
    pub async fn get_or_refresh(&self, provider: &Provider) -> Result<TokenResult, TokenError> {
        let provider_id = &provider.id;

        // Get or create per-provider state.
        let state = self
            .state
            .entry(provider_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(ProviderTokenState { cached: None })))
            .clone();

        // Acquire the per-provider mutex. This serializes concurrent refresh
        // requests for the same provider — only the first caller performs the
        // HTTP request; others await the mutex and get the cached result.
        let mut guard = state.lock().await;

        // Check cache under the lock.
        if let Some(ref cached) = guard.cached
            && cached.is_valid()
        {
            debug!(
                provider_id = %provider_id,
                ttl_remaining_secs = cached.expires_at.duration_since(tokio::time::Instant::now()).as_secs(),
                "Returning cached OAuth2 access token"
            );
            return Ok(TokenResult {
                access_token: cached.access_token.clone(),
                expires_in_secs: cached.ttl_secs,
                new_refresh_token: None,
            });
        }

        // Cache miss or expired — perform token exchange.
        let result = match auth_method(provider) {
            AuthMethod::Oauth2 => {
                let config = OAuth2Config::from_provider(provider)?;
                self.exchange_oauth2_token(&config).await?
            }
            AuthMethod::Gcp => self.exchange_gcp_token(provider).await?,
            AuthMethod::Static => {
                return Err(TokenError::Config(
                    "static provider passed to token vending service".into(),
                ));
            }
        };

        info!(
            provider_id = %provider_id,
            provider_name = %provider.name,
            expires_in_secs = result.expires_in_secs,
            refresh_token_rotated = result.new_refresh_token.is_some(),
            "token exchange completed"
        );

        // Cache the new token.
        guard.cached = Some(CachedToken {
            access_token: result.access_token.clone(),
            expires_at: tokio::time::Instant::now()
                + std::time::Duration::from_secs(u64::from(result.expires_in_secs)),
            ttl_secs: result.expires_in_secs,
        });

        Ok(result)
    }

    /// Perform a standard OAuth2 HTTP token exchange.
    async fn exchange_oauth2_token(&self, config: &OAuth2Config) -> Result<TokenResult, TokenError> {
        let mut params = HashMap::new();
        params.insert("client_id", config.client_id.as_str());
        params.insert("client_secret", config.client_secret.as_str());
        params.insert("grant_type", config.grant_type.as_str());

        if let Some(ref refresh_token) = config.refresh_token {
            params.insert("refresh_token", refresh_token.as_str());
        }

        if let Some(ref scopes) = config.scopes {
            params.insert("scope", scopes.as_str());
        }

        debug!(
            token_endpoint = %config.token_endpoint,
            grant_type = %config.grant_type,
            "Sending OAuth2 token exchange request"
        );

        // Explicitly request JSON. Some OAuth2 providers (notably GitHub)
        // default to application/x-www-form-urlencoded responses without this.
        let response = self
            .http_client
            .post(&config.token_endpoint)
            .header("Accept", "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| TokenError::Http(e.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| TokenError::Http(format!("failed to read response body: {e}")))?;

        if status != 200 {
            let truncated = truncate_body(&body, 256);
            error!(
                token_endpoint = %config.token_endpoint,
                status,
                response_body = %truncated,
                "OAuth2 token endpoint returned error"
            );
            return Err(TokenError::EndpointError {
                status,
                body: truncated,
            });
        }

        let token_response: OAuth2TokenResponse = serde_json::from_str(&body)
            .map_err(|e| TokenError::Exchange(format!("failed to parse token response: {e}")))?;

        let access_token = token_response.access_token.ok_or_else(|| {
            TokenError::Exchange("token response missing 'access_token' field".into())
        })?;

        if access_token.is_empty() {
            return Err(TokenError::Exchange(
                "token response contains empty 'access_token'".into(),
            ));
        }

        // Fail-fast: reject access tokens containing header-injection
        // characters (CR, LF, NUL). The downstream `SecretResolver` also
        // validates during placeholder resolution, but catching it here
        // provides a clearer error and avoids caching a poisoned token.
        validate_token_value(&access_token)?;

        // Validate rotated refresh token if present. A poisoned refresh
        // token would be persisted to the store and break future refreshes.
        let new_refresh_token = match token_response.refresh_token {
            Some(rt) => {
                validate_token_value(&rt).map_err(|_| {
                    TokenError::Exchange(
                        "rotated refresh token contains prohibited control characters".into(),
                    )
                })?;
                Some(rt)
            }
            None => None,
        };

        // Default to 3600s if the IdP doesn't specify expires_in.
        let expires_in = token_response.expires_in.unwrap_or(3600);

        Ok(TokenResult {
            access_token,
            expires_in_secs: expires_in,
            new_refresh_token,
        })
    }

    /// Perform a GCP token exchange.
    ///
    /// Supports two sub-flows:
    /// - **Service account key**: signs a JWT with the SA private key and
    ///   exchanges it at the token endpoint via
    ///   `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`.
    /// - **ADC refresh token**: standard OAuth2 refresh_token grant using
    ///   credentials from `gcloud auth application-default login`.
    async fn exchange_gcp_token(&self, provider: &Provider) -> Result<TokenResult, TokenError> {
        if let Some(sa_key_json) = provider.credentials.get("GCP_SERVICE_ACCOUNT_KEY") {
            self.exchange_gcp_sa_token(sa_key_json).await
        } else {
            // ADC refresh token flow — reuse the standard OAuth2 exchange.
            let config = GcpAdcConfig::from_provider(provider)?;
            let oauth_config = OAuth2Config {
                token_endpoint: config.token_uri,
                grant_type: "refresh_token".to_string(),
                client_id: config.client_id,
                client_secret: config.client_secret,
                refresh_token: Some(config.refresh_token),
                scopes: None,
            };
            self.exchange_oauth2_token(&oauth_config).await
        }
    }

    /// Exchange a GCP service account JWT for an access token.
    async fn exchange_gcp_sa_token(&self, sa_key_json: &str) -> Result<TokenResult, TokenError> {
        let sa_key: GcpServiceAccountKey = serde_json::from_str(sa_key_json)
            .map_err(|e| TokenError::Config(format!("invalid GCP service account key: {e}")))?;

        let token_uri = sa_key
            .token_uri
            .as_deref()
            .unwrap_or("https://oauth2.googleapis.com/token");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| TokenError::Config(format!("system clock error: {e}")))?
            .as_secs();

        let claims = GcpJwtClaims {
            iss: &sa_key.client_email,
            scope: "https://www.googleapis.com/auth/cloud-platform",
            aud: token_uri,
            iat: now,
            exp: now + 3600,
        };

        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        if let Some(ref kid) = sa_key.private_key_id {
            header.kid = Some(kid.clone());
        }

        let encoding_key =
            jsonwebtoken::EncodingKey::from_rsa_pem(sa_key.private_key.as_bytes())
                .map_err(|e| TokenError::Config(format!("invalid RSA private key: {e}")))?;

        let jwt = jsonwebtoken::encode(&header, &claims, &encoding_key)
            .map_err(|e| TokenError::Exchange(format!("JWT signing failed: {e}")))?;

        debug!(
            token_endpoint = %token_uri,
            client_email = %sa_key.client_email,
            "Sending GCP JWT bearer token exchange"
        );

        let response = self
            .http_client
            .post(token_uri)
            .header("Accept", "application/json")
            .form(&[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:jwt-bearer",
                ),
                ("assertion", &jwt),
            ])
            .send()
            .await
            .map_err(|e| TokenError::Http(e.to_string()))?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| TokenError::Http(format!("failed to read response body: {e}")))?;

        if status != 200 {
            let truncated = truncate_body(&body, 256);
            error!(
                token_endpoint = %token_uri,
                status,
                response_body = %truncated,
                "GCP token endpoint returned error"
            );
            return Err(TokenError::EndpointError {
                status,
                body: truncated,
            });
        }

        let token_response: OAuth2TokenResponse = serde_json::from_str(&body)
            .map_err(|e| TokenError::Exchange(format!("failed to parse token response: {e}")))?;

        let access_token = token_response.access_token.ok_or_else(|| {
            TokenError::Exchange("token response missing 'access_token' field".into())
        })?;

        if access_token.is_empty() {
            return Err(TokenError::Exchange(
                "token response contains empty 'access_token'".into(),
            ));
        }

        validate_token_value(&access_token)?;

        // GCP SA token exchange never returns a refresh token.
        let expires_in = token_response.expires_in.unwrap_or(3600);

        Ok(TokenResult {
            access_token,
            expires_in_secs: expires_in,
            new_refresh_token: None,
        })
    }
}

impl Default for TokenVendingService {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// OAuth2 config extraction
// ---------------------------------------------------------------------------

/// Parsed `OAuth2` configuration from a Provider record.
struct OAuth2Config {
    token_endpoint: String,
    grant_type: String,
    client_id: String,
    client_secret: String,
    refresh_token: Option<String>,
    scopes: Option<String>,
}

impl OAuth2Config {
    fn from_provider(provider: &Provider) -> Result<Self, TokenError> {
        let config = &provider.config;
        let credentials = &provider.credentials;

        let token_endpoint = config
            .get("oauth_token_endpoint")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing oauth_token_endpoint in provider config".into())
            })?
            .clone();

        let grant_type = config
            .get("oauth_grant_type")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing oauth_grant_type in provider config".into())
            })?
            .clone();

        let client_id = credentials
            .get("OAUTH_CLIENT_ID")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing OAUTH_CLIENT_ID in provider credentials".into())
            })?
            .clone();

        let client_secret = credentials
            .get("OAUTH_CLIENT_SECRET")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing OAUTH_CLIENT_SECRET in provider credentials".into())
            })?
            .clone();

        let refresh_token = credentials.get("OAUTH_REFRESH_TOKEN").cloned();
        let scopes = config.get("oauth_scopes").cloned();

        Ok(Self {
            token_endpoint,
            grant_type,
            client_id,
            client_secret,
            refresh_token,
            scopes,
        })
    }
}

// ---------------------------------------------------------------------------
// OAuth2 token response (RFC 6749 §5.1)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct OAuth2TokenResponse {
    access_token: Option<String>,
    #[allow(dead_code)]
    token_type: Option<String>,
    expires_in: Option<u32>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    scope: Option<String>,
}

// ---------------------------------------------------------------------------
// GCP-specific types
// ---------------------------------------------------------------------------

/// Parsed GCP service account JSON key file.
#[derive(Debug, serde::Deserialize)]
struct GcpServiceAccountKey {
    client_email: String,
    private_key: String,
    private_key_id: Option<String>,
    token_uri: Option<String>,
    #[allow(dead_code)]
    project_id: Option<String>,
}

/// JWT claims for GCP service account token exchange (RFC 7523).
#[derive(Debug, serde::Serialize)]
struct GcpJwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// GCP Application Default Credentials (refresh token flow).
#[derive(Debug)]
struct GcpAdcConfig {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_uri: String,
}

impl GcpAdcConfig {
    fn from_provider(provider: &Provider) -> Result<Self, TokenError> {
        let credentials = &provider.credentials;

        let client_id = credentials
            .get("GCP_ADC_CLIENT_ID")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing GCP_ADC_CLIENT_ID in provider credentials".into())
            })?
            .clone();

        let client_secret = credentials
            .get("GCP_ADC_CLIENT_SECRET")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing GCP_ADC_CLIENT_SECRET in provider credentials".into())
            })?
            .clone();

        let refresh_token = credentials
            .get("GCP_ADC_REFRESH_TOKEN")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                TokenError::Config("missing GCP_ADC_REFRESH_TOKEN in provider credentials".into())
            })?
            .clone();

        Ok(Self {
            client_id,
            client_secret,
            refresh_token,
            token_uri: "https://oauth2.googleapis.com/token".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Truncate a response body for safe logging.
fn truncate_body(body: &str, max_len: usize) -> String {
    if body.len() > max_len {
        let end = (0..=max_len)
            .rev()
            .find(|&i| body.is_char_boundary(i))
            .unwrap_or(0);
        format!("{}...", &body[..end])
    } else {
        body.to_string()
    }
}

// ---------------------------------------------------------------------------
// Helper functions for provider classification
// ---------------------------------------------------------------------------

/// Reject access tokens containing header-injection characters.
///
/// Mirrors the `validate_resolved_secret()` check in the sandbox's
/// `SecretResolver`, but applied at the token vending layer for fail-fast
/// behavior. Prevents caching a poisoned token that would be rejected
/// downstream on every request.
fn validate_token_value(value: &str) -> Result<(), TokenError> {
    if value
        .bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == b'\0')
    {
        return Err(TokenError::Exchange(
            "access token contains prohibited control characters (CR, LF, or NUL)".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Auth method detection
// ---------------------------------------------------------------------------

/// Authentication method for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Static credentials — no server-side token exchange.
    Static,
    /// Standard OAuth2 (client_credentials or refresh_token grant).
    Oauth2,
    /// GCP service account JWT bearer or ADC refresh token.
    Gcp,
}

/// Determine the auth method for a provider based on its config map.
pub fn auth_method(provider: &Provider) -> AuthMethod {
    match provider.config.get("auth_method").map(|s| s.as_str()) {
        Some("oauth2") => AuthMethod::Oauth2,
        Some("gcp") => AuthMethod::Gcp,
        _ => AuthMethod::Static,
    }
}

/// Determine if a provider uses `OAuth2` based on its config map.
pub fn is_oauth2_provider(provider: &Provider) -> bool {
    auth_method(provider) == AuthMethod::Oauth2
}

/// Returns `true` if the provider uses GCP auth.
pub fn is_gcp_provider(provider: &Provider) -> bool {
    auth_method(provider) == AuthMethod::Gcp
}

/// Returns `true` if the provider requires server-side token exchange.
pub fn is_token_vending_provider(provider: &Provider) -> bool {
    matches!(auth_method(provider), AuthMethod::Oauth2 | AuthMethod::Gcp)
}

/// `OAuth2` credential keys that must never be injected into sandbox env.
pub fn is_oauth2_internal_credential(key: &str) -> bool {
    matches!(
        key,
        "OAUTH_CLIENT_ID" | "OAUTH_CLIENT_SECRET" | "OAUTH_REFRESH_TOKEN"
    )
}

/// GCP credential keys that must never be injected into sandbox env.
///
/// These contain long-lived secrets (service account private key, ADC
/// client credentials) used by the gateway for token exchange. Leaking
/// them into the sandbox would allow the agent to mint arbitrary access
/// tokens.
pub fn is_gcp_internal_credential(key: &str) -> bool {
    matches!(
        key,
        "GCP_SERVICE_ACCOUNT_KEY"
            | "GCP_ADC_CLIENT_ID"
            | "GCP_ADC_CLIENT_SECRET"
            | "GCP_ADC_REFRESH_TOKEN"
    )
}

/// Returns `true` if the credential key is internal to any auth method
/// and should never be injected into the sandbox.
pub fn is_internal_credential(provider: &Provider, key: &str) -> bool {
    match auth_method(provider) {
        AuthMethod::Static => false,
        AuthMethod::Oauth2 => is_oauth2_internal_credential(key),
        AuthMethod::Gcp => is_gcp_internal_credential(key),
    }
}

/// Derive the access token env var key for an `OAuth2` provider.
pub fn oauth_access_token_key(provider: &Provider) -> String {
    provider
        .config
        .get("oauth_access_token_env")
        .filter(|v| !v.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| format!("{}_ACCESS_TOKEN", provider.r#type.to_ascii_uppercase()))
}

/// Validate OAuth2-specific configuration at provider creation time.
///
/// Returns `Ok(())` for non-OAuth2 providers (no-op).
#[allow(clippy::result_large_err)]
pub fn validate_oauth2_config(provider: &Provider) -> Result<(), tonic::Status> {
    let config = &provider.config;

    if !is_oauth2_provider(provider) {
        return Ok(());
    }

    // Required config keys.
    for key in &["oauth_token_endpoint", "oauth_grant_type"] {
        if config.get(*key).is_none_or(|v| v.trim().is_empty()) {
            return Err(tonic::Status::invalid_argument(format!(
                "OAuth2 provider requires config key '{key}'"
            )));
        }
    }

    let token_endpoint = config.get("oauth_token_endpoint").unwrap();
    if !token_endpoint.starts_with("https://") {
        return Err(tonic::Status::invalid_argument(
            "oauth_token_endpoint must use HTTPS",
        ));
    }

    let grant_type = config.get("oauth_grant_type").unwrap();
    match grant_type.as_str() {
        "refresh_token" => {
            for key in &[
                "OAUTH_CLIENT_ID",
                "OAUTH_CLIENT_SECRET",
                "OAUTH_REFRESH_TOKEN",
            ] {
                if !provider.credentials.contains_key(*key) {
                    return Err(tonic::Status::invalid_argument(format!(
                        "OAuth2 refresh_token grant requires credential '{key}'"
                    )));
                }
            }
        }
        "client_credentials" => {
            for key in &["OAUTH_CLIENT_ID", "OAUTH_CLIENT_SECRET"] {
                if !provider.credentials.contains_key(*key) {
                    return Err(tonic::Status::invalid_argument(format!(
                        "OAuth2 client_credentials grant requires credential '{key}'"
                    )));
                }
            }
        }
        other => {
            return Err(tonic::Status::invalid_argument(format!(
                "unsupported oauth_grant_type: '{other}' (expected 'refresh_token' or 'client_credentials')"
            )));
        }
    }

    Ok(())
}

/// Validate GCP-specific configuration at provider creation time.
///
/// Returns `Ok(())` for non-GCP providers (no-op).
#[allow(clippy::result_large_err)]
pub fn validate_gcp_config(provider: &Provider) -> Result<(), tonic::Status> {
    if !is_gcp_provider(provider) {
        return Ok(());
    }

    // Must have either SA key or ADC credentials.
    let has_sa_key = provider.credentials.contains_key("GCP_SERVICE_ACCOUNT_KEY");
    let has_adc = provider.credentials.contains_key("GCP_ADC_CLIENT_ID")
        && provider.credentials.contains_key("GCP_ADC_CLIENT_SECRET")
        && provider.credentials.contains_key("GCP_ADC_REFRESH_TOKEN");

    if !has_sa_key && !has_adc {
        return Err(tonic::Status::invalid_argument(
            "GCP provider requires either GCP_SERVICE_ACCOUNT_KEY credential \
             or GCP_ADC_CLIENT_ID + GCP_ADC_CLIENT_SECRET + GCP_ADC_REFRESH_TOKEN",
        ));
    }

    // Validate SA key JSON structure if present.
    if has_sa_key {
        let key_json = provider.credentials.get("GCP_SERVICE_ACCOUNT_KEY").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(key_json).map_err(|e| {
            tonic::Status::invalid_argument(format!(
                "GCP_SERVICE_ACCOUNT_KEY is not valid JSON: {e}"
            ))
        })?;

        for field in &["private_key", "client_email"] {
            if parsed
                .get(field)
                .and_then(|v| v.as_str())
                .is_none_or(|s| s.is_empty())
            {
                return Err(tonic::Status::invalid_argument(format!(
                    "GCP_SERVICE_ACCOUNT_KEY missing required field '{field}'"
                )));
            }
        }

        let key_type = parsed.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if key_type != "service_account" {
            return Err(tonic::Status::invalid_argument(format!(
                "GCP_SERVICE_ACCOUNT_KEY type must be 'service_account', got '{key_type}'"
            )));
        }

        // Validate token_uri is HTTPS if explicitly set.
        if let Some(token_uri) = parsed.get("token_uri").and_then(|v| v.as_str()) {
            if !token_uri.is_empty() && !token_uri.starts_with("https://") {
                return Err(tonic::Status::invalid_argument(
                    "GCP_SERVICE_ACCOUNT_KEY token_uri must use HTTPS",
                ));
            }
        }
    }

    Ok(())
}

/// Build the GCP-specific environment variable map.
///
/// Produces the access token and all tool-specific project/region aliases
/// from the provider's config. Called by `resolve_provider_environment()`
/// after a successful GCP token exchange.
pub fn build_gcp_env(
    provider: &Provider,
    access_token: &str,
) -> HashMap<String, String> {
    let mut env = HashMap::new();

    // Access token — the env var recognized by gcloud SDK and ADC-aware
    // libraries for pre-authenticated bearer tokens.
    env.insert(
        "CLOUDSDK_AUTH_ACCESS_TOKEN".to_string(),
        access_token.to_string(),
    );

    // Project ID — all known tool aliases.
    if let Some(project) = provider.config.get("gcp_project") {
        for key in &[
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "CLOUDSDK_CORE_PROJECT",
            "ANTHROPIC_VERTEX_PROJECT_ID",
            "GCP_PROJECT_ID",
        ] {
            env.insert((*key).to_string(), project.clone());
        }
    }

    // Region — all known tool aliases.
    if let Some(region) = provider.config.get("gcp_region") {
        for key in &[
            "CLOUD_ML_REGION",
            "GOOGLE_CLOUD_REGION",
            "VERTEX_LOCATION",
            "GCP_LOCATION",
        ] {
            env.insert((*key).to_string(), region.clone());
        }
    }

    // Mode flag for Claude Code.
    env.insert("CLAUDE_CODE_USE_VERTEX".to_string(), "1".to_string());

    env
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_oauth2_provider(grant_type: &str, include_refresh: bool) -> Provider {
        let mut credentials = HashMap::new();
        credentials.insert("OAUTH_CLIENT_ID".into(), "test-client-id".into());
        credentials.insert("OAUTH_CLIENT_SECRET".into(), "test-client-secret".into());
        if include_refresh {
            credentials.insert("OAUTH_REFRESH_TOKEN".into(), "test-refresh-token".into());
        }

        let mut config = HashMap::new();
        config.insert("auth_method".into(), "oauth2".into());
        config.insert("oauth_grant_type".into(), grant_type.into());
        config.insert(
            "oauth_token_endpoint".into(),
            "https://auth.example.com/oauth/token".into(),
        );

        Provider {
            id: "test-id".into(),
            name: "test-oauth".into(),
            r#type: "github".into(),
            credentials,
            config,
        }
    }

    fn make_static_provider() -> Provider {
        let mut credentials = HashMap::new();
        credentials.insert("GITHUB_TOKEN".into(), "ghp-static-token".into());

        Provider {
            id: "static-id".into(),
            name: "test-static".into(),
            r#type: "github".into(),
            credentials,
            config: HashMap::new(),
        }
    }

    // --- is_oauth2_provider ---

    #[test]
    fn detects_oauth2_provider() {
        let provider = make_oauth2_provider("refresh_token", true);
        assert!(is_oauth2_provider(&provider));
    }

    #[test]
    fn static_provider_is_not_oauth2() {
        let provider = make_static_provider();
        assert!(!is_oauth2_provider(&provider));
    }

    #[test]
    fn empty_config_is_not_oauth2() {
        let provider = Provider {
            id: String::new(),
            name: "empty".into(),
            r#type: "generic".into(),
            credentials: HashMap::new(),
            config: HashMap::new(),
        };
        assert!(!is_oauth2_provider(&provider));
    }

    // --- is_oauth2_internal_credential ---

    #[test]
    fn identifies_internal_credentials() {
        assert!(is_oauth2_internal_credential("OAUTH_CLIENT_ID"));
        assert!(is_oauth2_internal_credential("OAUTH_CLIENT_SECRET"));
        assert!(is_oauth2_internal_credential("OAUTH_REFRESH_TOKEN"));
    }

    #[test]
    fn non_oauth_keys_are_not_internal() {
        assert!(!is_oauth2_internal_credential("GITHUB_TOKEN"));
        assert!(!is_oauth2_internal_credential("ANTHROPIC_API_KEY"));
        assert!(!is_oauth2_internal_credential("OAUTH_ACCESS_TOKEN"));
    }

    // --- oauth_access_token_key ---

    #[test]
    fn derives_access_token_key_from_type() {
        let provider = make_oauth2_provider("refresh_token", true);
        assert_eq!(oauth_access_token_key(&provider), "GITHUB_ACCESS_TOKEN");
    }

    #[test]
    fn uses_custom_access_token_key_when_configured() {
        let mut provider = make_oauth2_provider("refresh_token", true);
        provider
            .config
            .insert("oauth_access_token_env".into(), "MY_CUSTOM_TOKEN".into());
        assert_eq!(oauth_access_token_key(&provider), "MY_CUSTOM_TOKEN");
    }

    #[test]
    fn ignores_empty_custom_access_token_key() {
        let mut provider = make_oauth2_provider("refresh_token", true);
        provider
            .config
            .insert("oauth_access_token_env".into(), "  ".into());
        assert_eq!(oauth_access_token_key(&provider), "GITHUB_ACCESS_TOKEN");
    }

    // --- validate_oauth2_config ---

    #[test]
    fn validates_refresh_token_grant() {
        let provider = make_oauth2_provider("refresh_token", true);
        assert!(validate_oauth2_config(&provider).is_ok());
    }

    #[test]
    fn validates_client_credentials_grant() {
        let provider = make_oauth2_provider("client_credentials", false);
        assert!(validate_oauth2_config(&provider).is_ok());
    }

    #[test]
    fn skips_validation_for_static_providers() {
        let provider = make_static_provider();
        assert!(validate_oauth2_config(&provider).is_ok());
    }

    #[test]
    fn rejects_missing_token_endpoint() {
        let mut provider = make_oauth2_provider("refresh_token", true);
        provider.config.remove("oauth_token_endpoint");
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("oauth_token_endpoint"));
    }

    #[test]
    fn rejects_http_token_endpoint() {
        let mut provider = make_oauth2_provider("refresh_token", true);
        provider.config.insert(
            "oauth_token_endpoint".into(),
            "http://insecure.example.com/token".into(),
        );
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("HTTPS"));
    }

    #[test]
    fn rejects_missing_grant_type() {
        let mut provider = make_oauth2_provider("refresh_token", true);
        provider.config.remove("oauth_grant_type");
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("oauth_grant_type"));
    }

    #[test]
    fn rejects_unsupported_grant_type() {
        let provider = make_oauth2_provider("implicit", false);
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("implicit"));
    }

    #[test]
    fn rejects_refresh_token_grant_without_refresh_token() {
        let provider = make_oauth2_provider("refresh_token", false);
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("OAUTH_REFRESH_TOKEN"));
    }

    #[test]
    fn rejects_missing_client_id() {
        let mut provider = make_oauth2_provider("client_credentials", false);
        provider.credentials.remove("OAUTH_CLIENT_ID");
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("OAUTH_CLIENT_ID"));
    }

    #[test]
    fn rejects_missing_client_secret() {
        let mut provider = make_oauth2_provider("client_credentials", false);
        provider.credentials.remove("OAUTH_CLIENT_SECRET");
        let err = validate_oauth2_config(&provider).unwrap_err();
        assert!(err.message().contains("OAUTH_CLIENT_SECRET"));
    }

    // --- OAuth2Config::from_provider ---

    #[test]
    fn extracts_config_from_provider() {
        let provider = make_oauth2_provider("refresh_token", true);
        let config = OAuth2Config::from_provider(&provider).unwrap();
        assert_eq!(
            config.token_endpoint,
            "https://auth.example.com/oauth/token"
        );
        assert_eq!(config.grant_type, "refresh_token");
        assert_eq!(config.client_id, "test-client-id");
        assert_eq!(config.client_secret, "test-client-secret");
        assert_eq!(config.refresh_token, Some("test-refresh-token".into()));
        assert!(config.scopes.is_none());
    }

    #[test]
    fn extracts_scopes_when_present() {
        let mut provider = make_oauth2_provider("client_credentials", false);
        provider
            .config
            .insert("oauth_scopes".into(), "api read_user".into());
        let config = OAuth2Config::from_provider(&provider).unwrap();
        assert_eq!(config.scopes, Some("api read_user".into()));
    }

    // --- TokenVendingService (unit-level) ---

    #[tokio::test]
    async fn token_vending_service_constructs() {
        let _service = TokenVendingService::new();
    }

    // --- validate_token_value ---

    #[test]
    fn validates_clean_token() {
        assert!(validate_token_value("gho_abc123XYZ").is_ok());
    }

    #[test]
    fn rejects_token_with_newline() {
        let err = validate_token_value("token\ninjection").unwrap_err();
        assert!(matches!(err, TokenError::Exchange(_)));
    }

    #[test]
    fn rejects_token_with_carriage_return() {
        let err = validate_token_value("token\rinjection").unwrap_err();
        assert!(matches!(err, TokenError::Exchange(_)));
    }

    #[test]
    fn rejects_token_with_null_byte() {
        let err = validate_token_value("token\0injection").unwrap_err();
        assert!(matches!(err, TokenError::Exchange(_)));
    }

    #[test]
    fn cached_token_validity() {
        let valid = CachedToken {
            access_token: "tok".into(),
            expires_at: tokio::time::Instant::now() + std::time::Duration::from_secs(3600),
            ttl_secs: 3600,
        };
        assert!(valid.is_valid());

        let expired = CachedToken {
            access_token: "tok".into(),
            expires_at: tokio::time::Instant::now(),
            ttl_secs: 3600,
        };
        assert!(!expired.is_valid());
    }

    // --- Integration tests (mock HTTP server) ---

    #[tokio::test]
    async fn exchange_token_with_mock_server() {
        use tokio::net::TcpListener;

        // Start a minimal mock HTTPS-less HTTP server for testing.
        // In production, token endpoints are HTTPS. For unit tests we
        // bypass the HTTPS requirement by constructing OAuth2Config directly.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn mock server that returns a valid token response.
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.contains("grant_type=client_credentials"));
            assert!(request.contains("client_id=test-id"));
            assert!(request.contains("client_secret=test-secret"));

            let body =
                r#"{"access_token":"mock-access-token","token_type":"Bearer","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });

        let service = TokenVendingService::new();
        let config = OAuth2Config {
            token_endpoint: format!("http://127.0.0.1:{}", addr.port()),
            grant_type: "client_credentials".into(),
            client_id: "test-id".into(),
            client_secret: "test-secret".into(),
            refresh_token: None,
            scopes: None,
        };

        let result = service.exchange_oauth2_token(&config).await.unwrap();
        assert_eq!(result.access_token, "mock-access-token");
        assert_eq!(result.expires_in_secs, 3600);
        assert!(result.new_refresh_token.is_none());
    }

    #[tokio::test]
    async fn exchange_token_with_refresh_rotation() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap();

            let body = r#"{"access_token":"new-access","token_type":"Bearer","expires_in":7200,"refresh_token":"new-refresh"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });

        let service = TokenVendingService::new();
        let config = OAuth2Config {
            token_endpoint: format!("http://127.0.0.1:{}", addr.port()),
            grant_type: "refresh_token".into(),
            client_id: "test-id".into(),
            client_secret: "test-secret".into(),
            refresh_token: Some("old-refresh".into()),
            scopes: None,
        };

        let result = service.exchange_oauth2_token(&config).await.unwrap();
        assert_eq!(result.access_token, "new-access");
        assert_eq!(result.expires_in_secs, 7200);
        assert_eq!(result.new_refresh_token, Some("new-refresh".into()));
    }

    #[tokio::test]
    async fn exchange_token_error_response() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap();

            let body = r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });

        let service = TokenVendingService::new();
        let config = OAuth2Config {
            token_endpoint: format!("http://127.0.0.1:{}", addr.port()),
            grant_type: "refresh_token".into(),
            client_id: "test-id".into(),
            client_secret: "test-secret".into(),
            refresh_token: Some("revoked-token".into()),
            scopes: None,
        };

        let err = service.exchange_oauth2_token(&config).await.unwrap_err();
        assert!(matches!(err, TokenError::EndpointError { status: 400, .. }));
    }

    #[tokio::test]
    async fn caching_returns_same_token() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Server only handles one connection — second call must hit cache.
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap();

            let body = r#"{"access_token":"cached-token","token_type":"Bearer","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });

        let service = TokenVendingService::new();
        let provider = Provider {
            id: "cache-test".into(),
            name: "cache-test".into(),
            r#type: "github".into(),
            credentials: [
                ("OAUTH_CLIENT_ID".into(), "id".into()),
                ("OAUTH_CLIENT_SECRET".into(), "secret".into()),
            ]
            .into_iter()
            .collect(),
            config: [
                ("auth_method".into(), "oauth2".into()),
                ("oauth_grant_type".into(), "client_credentials".into()),
                (
                    "oauth_token_endpoint".into(),
                    format!("http://127.0.0.1:{}", addr.port()),
                ),
            ]
            .into_iter()
            .collect(),
        };

        let result1 = service.get_or_refresh(&provider).await.unwrap();
        assert_eq!(result1.access_token, "cached-token");

        // Second call should return cached token (server has closed).
        let result2 = service.get_or_refresh(&provider).await.unwrap();
        assert_eq!(result2.access_token, "cached-token");
    }

    // --- GCP auth method detection ---

    #[test]
    fn detects_gcp_provider() {
        let provider = make_gcp_sa_provider();
        assert!(is_gcp_provider(&provider));
        assert!(!is_oauth2_provider(&provider));
        assert_eq!(auth_method(&provider), AuthMethod::Gcp);
    }

    #[test]
    fn gcp_internal_credentials_are_filtered() {
        assert!(is_gcp_internal_credential("GCP_SERVICE_ACCOUNT_KEY"));
        assert!(is_gcp_internal_credential("GCP_ADC_CLIENT_ID"));
        assert!(is_gcp_internal_credential("GCP_ADC_CLIENT_SECRET"));
        assert!(is_gcp_internal_credential("GCP_ADC_REFRESH_TOKEN"));
        assert!(!is_gcp_internal_credential("CLOUDSDK_AUTH_ACCESS_TOKEN"));
        assert!(!is_gcp_internal_credential("GOOGLE_CLOUD_PROJECT"));
    }

    #[test]
    fn is_internal_credential_dispatches_by_auth_method() {
        let gcp = make_gcp_sa_provider();
        assert!(is_internal_credential(&gcp, "GCP_SERVICE_ACCOUNT_KEY"));
        assert!(!is_internal_credential(&gcp, "OAUTH_CLIENT_ID"));

        let oauth = make_oauth2_provider("client_credentials", false);
        assert!(is_internal_credential(&oauth, "OAUTH_CLIENT_ID"));
        assert!(!is_internal_credential(&oauth, "GCP_SERVICE_ACCOUNT_KEY"));

        let static_prov = make_static_provider();
        assert!(!is_internal_credential(&static_prov, "GCP_SERVICE_ACCOUNT_KEY"));
        assert!(!is_internal_credential(&static_prov, "OAUTH_CLIENT_ID"));
    }

    // --- GCP validation ---

    #[test]
    fn validates_gcp_sa_key_provider() {
        let provider = make_gcp_sa_provider();
        assert!(validate_gcp_config(&provider).is_ok());
    }

    #[test]
    fn validates_gcp_adc_provider() {
        let provider = make_gcp_adc_provider();
        assert!(validate_gcp_config(&provider).is_ok());
    }

    #[test]
    fn rejects_gcp_without_credentials() {
        let mut provider = make_gcp_sa_provider();
        provider.credentials.clear();
        let err = validate_gcp_config(&provider).unwrap_err();
        assert!(err.message().contains("GCP provider requires"));
    }

    #[test]
    fn rejects_gcp_sa_key_missing_private_key() {
        let mut provider = make_gcp_sa_provider();
        let mut sa_key: serde_json::Value = serde_json::from_str(
            provider.credentials.get("GCP_SERVICE_ACCOUNT_KEY").unwrap(),
        )
        .unwrap();
        sa_key.as_object_mut().unwrap().remove("private_key");
        provider.credentials.insert(
            "GCP_SERVICE_ACCOUNT_KEY".into(),
            sa_key.to_string(),
        );
        let err = validate_gcp_config(&provider).unwrap_err();
        assert!(err.message().contains("private_key"));
    }

    #[test]
    fn rejects_gcp_sa_key_wrong_type() {
        let mut provider = make_gcp_sa_provider();
        let mut sa_key: serde_json::Value = serde_json::from_str(
            provider.credentials.get("GCP_SERVICE_ACCOUNT_KEY").unwrap(),
        )
        .unwrap();
        sa_key["type"] = serde_json::json!("authorized_user");
        provider.credentials.insert(
            "GCP_SERVICE_ACCOUNT_KEY".into(),
            sa_key.to_string(),
        );
        let err = validate_gcp_config(&provider).unwrap_err();
        assert!(err.message().contains("service_account"));
    }

    #[test]
    fn rejects_gcp_sa_key_http_token_uri() {
        let mut provider = make_gcp_sa_provider();
        let mut sa_key: serde_json::Value = serde_json::from_str(
            provider.credentials.get("GCP_SERVICE_ACCOUNT_KEY").unwrap(),
        )
        .unwrap();
        sa_key["token_uri"] = serde_json::json!("http://evil.com/token");
        provider.credentials.insert(
            "GCP_SERVICE_ACCOUNT_KEY".into(),
            sa_key.to_string(),
        );
        let err = validate_gcp_config(&provider).unwrap_err();
        assert!(err.message().contains("HTTPS"));
    }

    #[test]
    fn skips_gcp_validation_for_non_gcp_providers() {
        let provider = make_static_provider();
        assert!(validate_gcp_config(&provider).is_ok());
    }

    // --- GCP env projection ---

    #[test]
    fn build_gcp_env_includes_all_aliases() {
        let provider = make_gcp_sa_provider();
        let env = build_gcp_env(&provider, "ya29.test-token");

        assert_eq!(env.get("CLOUDSDK_AUTH_ACCESS_TOKEN"), Some(&"ya29.test-token".to_string()));
        assert_eq!(env.get("GOOGLE_CLOUD_PROJECT"), Some(&"test-project".to_string()));
        assert_eq!(env.get("ANTHROPIC_VERTEX_PROJECT_ID"), Some(&"test-project".to_string()));
        assert_eq!(env.get("GCP_PROJECT_ID"), Some(&"test-project".to_string()));
        assert_eq!(env.get("CLOUD_ML_REGION"), Some(&"us-east5".to_string()));
        assert_eq!(env.get("VERTEX_LOCATION"), Some(&"us-east5".to_string()));
        assert_eq!(env.get("GCP_LOCATION"), Some(&"us-east5".to_string()));
        assert_eq!(env.get("CLAUDE_CODE_USE_VERTEX"), Some(&"1".to_string()));
    }

    #[test]
    fn build_gcp_env_handles_missing_config() {
        let mut provider = make_gcp_sa_provider();
        provider.config.remove("gcp_project");
        provider.config.remove("gcp_region");
        let env = build_gcp_env(&provider, "ya29.token");

        // Access token and mode flag should still be set.
        assert_eq!(env.get("CLOUDSDK_AUTH_ACCESS_TOKEN"), Some(&"ya29.token".to_string()));
        assert_eq!(env.get("CLAUDE_CODE_USE_VERTEX"), Some(&"1".to_string()));
        // Project/region aliases should be absent.
        assert!(!env.contains_key("GOOGLE_CLOUD_PROJECT"));
        assert!(!env.contains_key("CLOUD_ML_REGION"));
    }

    // --- GCP ADC config extraction ---

    #[test]
    fn extracts_gcp_adc_config() {
        let provider = make_gcp_adc_provider();
        let config = GcpAdcConfig::from_provider(&provider).unwrap();
        assert_eq!(config.client_id, "test-client-id");
        assert_eq!(config.client_secret, "test-client-secret");
        assert_eq!(config.refresh_token, "test-refresh-token");
        assert_eq!(config.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn rejects_gcp_adc_missing_client_id() {
        let mut provider = make_gcp_adc_provider();
        provider.credentials.remove("GCP_ADC_CLIENT_ID");
        let err = GcpAdcConfig::from_provider(&provider).unwrap_err();
        assert!(matches!(err, TokenError::Config(_)));
    }

    // --- GCP SA JWT exchange (mock server) ---

    #[tokio::test]
    async fn gcp_sa_token_exchange_with_mock_server() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.contains("grant_type=urn"));
            assert!(request.contains("assertion="));

            let body =
                r#"{"access_token":"ya29.mock-access-token","token_type":"Bearer","expires_in":3599}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes())
                .await
                .unwrap();
        });

        let service = TokenVendingService::new();

        // Build a SA key JSON with the mock server URL as token_uri.
        // Use a real RSA key for JWT signing.
        let sa_key = serde_json::json!({
            "type": "service_account",
            "project_id": "test-project",
            "private_key_id": "key-id-123",
            "private_key": TEST_RSA_KEY,
            "client_email": "test@test-project.iam.gserviceaccount.com",
            "token_uri": format!("http://127.0.0.1:{}", addr.port()),
        })
        .to_string();

        let result = service.exchange_gcp_sa_token(&sa_key).await.unwrap();
        assert_eq!(result.access_token, "ya29.mock-access-token");
        assert_eq!(result.expires_in_secs, 3599);
        assert!(result.new_refresh_token.is_none());
    }

    // --- Test helpers ---

    fn make_gcp_sa_provider() -> Provider {
        let sa_key = serde_json::json!({
            "type": "service_account",
            "project_id": "test-project",
            "private_key_id": "key-id-123",
            "private_key": TEST_RSA_KEY,
            "client_email": "test@test-project.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token",
        })
        .to_string();

        let mut credentials = HashMap::new();
        credentials.insert("GCP_SERVICE_ACCOUNT_KEY".into(), sa_key);

        let mut config = HashMap::new();
        config.insert("auth_method".into(), "gcp".into());
        config.insert("gcp_project".into(), "test-project".into());
        config.insert("gcp_region".into(), "us-east5".into());

        Provider {
            id: "test-gcp-id".into(),
            name: "test-gcp".into(),
            r#type: "gcp".into(),
            credentials,
            config,
        }
    }

    fn make_gcp_adc_provider() -> Provider {
        let mut credentials = HashMap::new();
        credentials.insert("GCP_ADC_CLIENT_ID".into(), "test-client-id".into());
        credentials.insert("GCP_ADC_CLIENT_SECRET".into(), "test-client-secret".into());
        credentials.insert("GCP_ADC_REFRESH_TOKEN".into(), "test-refresh-token".into());

        let mut config = HashMap::new();
        config.insert("auth_method".into(), "gcp".into());
        config.insert("gcp_project".into(), "test-project".into());
        config.insert("gcp_region".into(), "us-east5".into());

        Provider {
            id: "test-gcp-adc-id".into(),
            name: "test-gcp-adc".into(),
            r#type: "gcp".into(),
            credentials,
            config,
        }
    }

    /// RSA private key for testing JWT signing.
    /// Generated with: openssl genrsa -traditional 2048
    const TEST_RSA_KEY: &str = "\
-----BEGIN RSA PRIVATE KEY-----
MIIEpQIBAAKCAQEA0mkDsctJVW3KX8oENu9uOP/N8v1lUs7uzdiYlxJph4IpS8Ld
oHrMUU8YZs0g+HZRxcZk05xf71sS/Slcw1Cy0VqXShnH4ORwXBMpeDWyFVDSLdlJ
ljg8KQlQUFzsvXt9cjDynGTJ0qX6VC8pGrCLh2AaCbKOyJxfXgnKv4I3lE7AwnBy
EgE+YVNEuif2VeUpmA/4Cgekx9s3OgeFeoARyOgEpJ3P3NcdYY/co1jql+Trz+5E
4MdD3zU5i4vJF3056kLQInSFlvUyOGi4vX9Cn3lWA5+Cw5hpF/Jf/2Y/24EmKN/W
S3AClmYAVehyjcAQe6Wbndk6IjtQ0XnoZssTZwIDAQABAoIBAAcvAarIy3yorm+d
yI4Nl6BHj4L7xsFQglOx0Ofbf5HaTkmhYgqwFpiyEB22ZClHdNxBPUECRj44SEov
ZtTeRPSj2KV1gt75PaLPHqvVfXp/02UwVXRVACzQfhb4TTbc5/gFlsrjIAbaltTX
9VnNbD4XeFwbZgeQWystP2hRbE9aXxOxMB5klGxlxQQdVsIjpz92FDl/rmH5vkt0
JEG3pGYvNnJDlqd4aZ3haHQOh2eN99yqZJRDvnRnBO8wzD8MI+u6WI/EY9bphvMM
Wa4qPwO/RathgVsi8UVAoYuWnUKIzhIkw1LubLMw4ICncI75ghNmW2dGTCzk33vm
I7Ds5vECgYEA+Y5c9DYYBT2krHvQedFEF11mpCFzTRltQchml/wYRycUoPcbN0Pn
RpyR8Yntm1MmyNnwbw5hWYWszeK/ZSN1xbfVa8Qbs3q1dWSed4m9F3Pes2IZrLa/
2dlq8Z5DIqdes4YhYlnBhHgzAd7x1i0aCLxc7f1si9b1NFUS6bPlilcCgYEA19fi
xe4jIyyy1n9nAQLk/Qyv4qFeLmrant4ZjXW4ucancI1MfOcS1uhXwHv9BQEyN/DB
rtiYv0Gfm6G8Q+9oiYzXMPvmVoJg0aRU1yOidJ/fWZ4gquZtFCua2O4wzciiwBl3
hTLBIIO7d5FXm+3/qtqi6mwdV0K+mkCbcXKzNXECgYEAsxZ2II8dR82H+nvUDUee
/MF7YjfbHa4smPOupE02QwGJrUYH0u4475R2q4aW7EuM3sB/6cLBG9RxQUMCpRsA
boZRc0fFtVRPGlK94f0HpOfzHno9AJQZM8XyGDjB5wGDVYPhO0o8NMtpl2Md29x7
/V7ntaaTGfuF9itDKlF+XXsCgYEAz7Vd8nE++Qt7sjT6D5wUdnuuCp6VPn/vkHBV
EK9Xq9dCrGodUBkiJROD1qS6kQVcqT6TdEnVfD+Pc7pJrOqHo86YCvht6ZQfzb6h
MkOFg0uSKPClqTCDiaSIp/dXmcBY9hnLza9Q8JQ0ZFTGhTScE3PA0WxnM+D0AQbp
T1w8ntECgYEAyDI0unmk/BPpR1hHqbbda+k+L4LwJO9JHKFZEA1LiZb926sho6PG
k6gjpj9bQQy7z8ZR4DJvEmq/+TZvseuRHc1apt3Cj1zx6IfFJ/2ssG0BUsTXGch0
9TXfsbqr51yo3fbXV7sqbmAllLi5Mvr7h+KJDkNfPKoTj8rkh/vFo5U=
-----END RSA PRIVATE KEY-----";
}
