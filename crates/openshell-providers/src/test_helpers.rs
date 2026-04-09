// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::DiscoveryContext;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct MockDiscoveryContext {
    env: HashMap<String, String>,
    files: HashMap<PathBuf, String>,
    home: Option<PathBuf>,
}

impl MockDiscoveryContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    pub fn with_file(mut self, path: impl Into<PathBuf>, content: &str) -> Self {
        self.files.insert(path.into(), content.to_string());
        self
    }

    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }
}

impl DiscoveryContext for MockDiscoveryContext {
    fn env_var(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        self.files.get(path).cloned()
    }

    fn home_dir(&self) -> Option<PathBuf> {
        self.home.clone()
    }
}
