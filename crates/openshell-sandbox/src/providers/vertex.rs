// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::secrets::{SecretResolver, placeholder_for_env_key};

const GOOGLE_OAUTH_HOST: &str = "oauth2.googleapis.com";

pub(crate) fn should_intercept_oauth_request(method: &str, host: &str, target: &str) -> bool {
    if !method.eq_ignore_ascii_case("POST") || target != "/token" {
        return false;
    }

    let normalized_host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(':')
        .next()
        .unwrap_or_default();
    normalized_host.eq_ignore_ascii_case(GOOGLE_OAUTH_HOST)
}

pub(crate) fn oauth_intercept_response(resolver: Option<&SecretResolver>) -> Vec<u8> {
    match resolve_vertex_access_token(resolver) {
        Some(access_token) => build_json_response(
            200,
            &serde_json::json!({
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": 3600,
            })
            .to_string(),
        ),
        None => build_json_response(
            502,
            &serde_json::json!({
                "error": "vertex_access_token_unavailable",
                "message": "OpenShell could not resolve a Vertex OAuth access token for this sandbox.",
            })
            .to_string(),
        ),
    }
}

fn resolve_vertex_access_token(resolver: Option<&SecretResolver>) -> Option<String> {
    if let Ok(token) = std::env::var("VERTEX_ACCESS_TOKEN")
        && !token.trim().is_empty()
    {
        return Some(token.trim().to_string());
    }

    resolver.and_then(|resolver| {
        resolver
            .resolve_placeholder(&placeholder_for_env_key("VERTEX_ACCESS_TOKEN"))
            .or_else(|| {
                resolver.resolve_placeholder(&placeholder_for_env_key("VERTEX_OAUTH_TOKEN"))
            })
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    })
}

fn build_json_response(status: u16, body: &str) -> Vec<u8> {
    let status_text = if status == 200 { "OK" } else { "Bad Gateway" };
    format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{oauth_intercept_response, should_intercept_oauth_request};
    use crate::secrets::SecretResolver;

    #[test]
    fn matches_google_oauth_token_exchange() {
        assert!(should_intercept_oauth_request(
            "POST",
            "oauth2.googleapis.com",
            "/token"
        ));
        assert!(should_intercept_oauth_request(
            "POST",
            "oauth2.googleapis.com:443",
            "/token"
        ));
        assert!(!should_intercept_oauth_request(
            "GET",
            "oauth2.googleapis.com",
            "/token"
        ));
    }

    #[test]
    fn builds_success_response_from_resolver_token() {
        let (_, resolver) = SecretResolver::from_provider_env(
            [("VERTEX_ACCESS_TOKEN".to_string(), "ya29.test".to_string())]
                .into_iter()
                .collect(),
        );
        let response = String::from_utf8(oauth_intercept_response(resolver.as_ref())).unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"access_token\":\"ya29.test\""));
    }
}
