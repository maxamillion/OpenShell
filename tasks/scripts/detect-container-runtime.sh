#!/usr/bin/env bash

# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Detect container runtime: podman preferred over docker.
# Sets CONTAINER_RUNTIME to "podman" or "docker".
# Respects OPENSHELL_CONTAINER_RUNTIME override.
#
# Source this script at the top of any shell script that invokes docker/podman:
#   source "$(dirname "$0")/detect-container-runtime.sh"

detect_container_runtime() {
	# 1. Explicit override
	if [ -n "${OPENSHELL_CONTAINER_RUNTIME:-}" ]; then
		CONTAINER_RUNTIME="$OPENSHELL_CONTAINER_RUNTIME"
		return
	fi

	# 2. Probe for binaries on PATH (podman preferred)
	if command -v podman &>/dev/null; then
		CONTAINER_RUNTIME=podman
		return
	fi

	if command -v docker &>/dev/null; then
		CONTAINER_RUNTIME=docker
		return
	fi

	echo "Error: No container runtime found. Install podman or docker." >&2
	exit 1
}

# Auto-detect on source
detect_container_runtime
