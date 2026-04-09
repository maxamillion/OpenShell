// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

pub trait DiscoveryContext {
    fn env_var(&self, key: &str) -> Option<String>;

    /// Read a file to string. Returns `None` if the file does not exist or
    /// cannot be read.
    fn read_file(&self, path: &Path) -> Option<String> {
        let _ = path;
        None
    }

    /// Return the user's home directory. Returns `None` if unknown.
    fn home_dir(&self) -> Option<PathBuf> {
        None
    }
}

pub struct RealDiscoveryContext;

impl DiscoveryContext for RealDiscoveryContext {
    fn env_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
    }
}
