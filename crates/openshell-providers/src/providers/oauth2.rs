// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{DiscoveredProvider, ProviderError, ProviderPlugin};

pub struct Oauth2Provider;

impl ProviderPlugin for Oauth2Provider {
    fn id(&self) -> &'static str {
        "oauth2"
    }

    fn discover_existing(&self) -> Result<Option<DiscoveredProvider>, ProviderError> {
        // OAuth2 credentials are obtained through the token-vending flow on the
        // server side, not from local environment state.
        Ok(None)
    }

    fn credential_env_vars(&self) -> &'static [&'static str] {
        &["OAUTH2_CLIENT_ID", "OAUTH2_CLIENT_SECRET"]
    }
}

#[cfg(test)]
mod tests {
    use super::Oauth2Provider;
    use crate::ProviderPlugin;

    #[test]
    fn oauth2_provider_discovery_is_empty() {
        let provider = Oauth2Provider;
        let discovered = provider.discover_existing().expect("discovery");
        assert!(discovered.is_none());
    }
}
