#!/usr/bin/env bash

# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Automated setup script for Podman on Linux
# Handles installation, socket activation, cgroup delegation,
# and headless-specific configuration (linger, XDG_RUNTIME_DIR).
#
# Usage:
#   bash scripts/setup-podman-linux.sh              # rootless (default)
#   bash scripts/setup-podman-linux.sh --rootful     # rootful mode
#   bash scripts/setup-podman-linux.sh --skip-install # skip podman installation

set -euo pipefail

# --- Argument parsing ---

MODE="rootless"
SKIP_INSTALL=false

for arg in "$@"; do
	case "$arg" in
	--rootful)
		MODE="rootful"
		;;
	--skip-install)
		SKIP_INSTALL=true
		;;
	--help | -h)
		echo "Usage: $0 [--rootful] [--skip-install]"
		echo ""
		echo "Options:"
		echo "  --rootful       Configure rootful Podman (default: rootless)"
		echo "  --skip-install  Skip Podman package installation"
		echo ""
		echo "Environment variables:"
		echo "  PODMAN_MIN_VERSION  Minimum Podman version (default: 4.0)"
		exit 0
		;;
	*)
		echo "Unknown argument: $arg"
		echo "Run '$0 --help' for usage."
		exit 1
		;;
	esac
done

PODMAN_MIN_VERSION="${PODMAN_MIN_VERSION:-4.0}"
DELEGATE_CONF="/etc/systemd/system/user@.service.d/delegate.conf"
DELEGATE_CONTENT="[Service]
Delegate=cpu cpuset io memory pids"
CGROUP_CHANGED=false

echo "=== OpenShell Podman Setup for Linux (${MODE}) ==="
echo ""

# --- Preflight checks ---

if [[ "$(uname -s)" != "Linux" ]]; then
	echo "❌ This script is for Linux only."
	exit 1
fi

if [[ "$(stat -fc %T /sys/fs/cgroup 2>/dev/null)" != "cgroup2fs" ]]; then
	echo "❌ cgroup v2 is not active on this system."
	echo ""
	echo "OpenShell requires cgroup v2. Verify with:"
	echo "  stat -fc %T /sys/fs/cgroup"
	echo ""
	echo "Expected output: cgroup2fs"
	echo "Refer to your distribution's documentation to enable cgroup v2."
	exit 1
fi

echo "✓ cgroup v2 is active"

# --- Detect package manager ---

PKG_MANAGER=""
if command -v dnf &>/dev/null; then
	PKG_MANAGER="dnf"
elif command -v apt-get &>/dev/null; then
	PKG_MANAGER="apt-get"
fi

# --- Install Podman ---

if [[ "${SKIP_INSTALL}" == "true" ]]; then
	if ! command -v podman &>/dev/null; then
		echo "❌ Podman is not installed and --skip-install was specified."
		exit 1
	fi
	echo "✓ Skipping installation (--skip-install)"
elif command -v podman &>/dev/null; then
	echo "✓ Podman is already installed ($(podman --version))"
else
	if [[ -z "${PKG_MANAGER}" ]]; then
		echo "❌ Cannot detect package manager (dnf or apt-get)."
		echo "   Install Podman manually and re-run with --skip-install."
		exit 1
	fi

	echo "Installing Podman via ${PKG_MANAGER}..."
	if [[ "${PKG_MANAGER}" == "dnf" ]]; then
		sudo dnf install -y podman
	elif [[ "${PKG_MANAGER}" == "apt-get" ]]; then
		sudo apt-get update
		sudo apt-get install -y podman
	fi

	if ! command -v podman &>/dev/null; then
		echo "❌ Podman installation failed."
		exit 1
	fi
	echo "✓ Podman installed ($(podman --version))"
fi

# Verify minimum version
PODMAN_VERSION="$(podman --version | grep -oP '\d+\.\d+' | head -1)"
if [[ "$(printf '%s\n' "${PODMAN_MIN_VERSION}" "${PODMAN_VERSION}" | sort -V | head -1)" != "${PODMAN_MIN_VERSION}" ]]; then
	echo "❌ Podman ${PODMAN_VERSION} is below the minimum required version ${PODMAN_MIN_VERSION}."
	exit 1
fi

echo "✓ Podman version ${PODMAN_VERSION} meets minimum (${PODMAN_MIN_VERSION})"

# --- Rootful setup ---

if [[ "${MODE}" == "rootful" ]]; then
	echo ""
	echo "--- Rootful Configuration ---"
	echo ""

	# Enable rootful socket
	echo "Enabling rootful Podman socket..."
	sudo systemctl enable --now podman.socket

	if [[ -S /run/podman/podman.sock ]]; then
		echo "✓ Rootful socket is active at /run/podman/podman.sock"
	else
		echo "⚠️  Socket file not found at /run/podman/podman.sock"
		echo "   Check: sudo systemctl status podman.socket"
	fi

	# Verify with hello-world
	echo ""
	echo "Verifying Podman works..."
	if sudo podman run --rm docker.io/library/hello-world &>/dev/null; then
		echo "✓ Podman rootful mode is working"
	else
		echo "⚠️  hello-world container failed. Check: sudo podman run --rm docker.io/library/hello-world"
	fi

	echo ""
	echo "=== Rootful Setup Complete ==="
	echo ""
	echo "Environment variables (add to your shell profile):"
	echo "  export CONTAINER_HOST=\"unix:///run/podman/podman.sock\""
	echo "  export OPENSHELL_CONTAINER_RUNTIME=podman"
	echo ""

	# Detect shell profile
	SHELL_PROFILE="${HOME}/.bashrc"
	if [[ -n "${ZSH_VERSION:-}" ]] || [[ "$(basename "${SHELL:-}")" == "zsh" ]]; then
		SHELL_PROFILE="${HOME}/.zshrc"
	fi

	read -p "Add environment variables to ${SHELL_PROFILE}? (y/N) " -n 1 -r
	echo
	if [[ ${REPLY} =~ ^[Yy]$ ]]; then
		if ! grep -q "CONTAINER_HOST.*podman.sock" "${SHELL_PROFILE}" 2>/dev/null; then
			{
				echo ""
				echo "# OpenShell Podman environment (rootful)"
				echo "export CONTAINER_HOST=\"unix:///run/podman/podman.sock\""
				echo "export OPENSHELL_CONTAINER_RUNTIME=podman"
			} >>"${SHELL_PROFILE}"
			echo "✓ Added to ${SHELL_PROFILE}"
			echo "  Run: source ${SHELL_PROFILE}"
		else
			echo "⚠️  Environment variables already in ${SHELL_PROFILE}"
		fi
	fi

	echo ""
	echo "Next steps:"
	echo "  1. Source your shell profile or set environment variables"
	echo "  2. Run: openshell gateway start"
	exit 0
fi

# --- Rootless setup ---

echo ""
echo "--- Rootless Configuration ---"
echo ""

# Ensure XDG_RUNTIME_DIR is set (needed for rootless socket)
if [[ -z "${XDG_RUNTIME_DIR:-}" ]]; then
	export XDG_RUNTIME_DIR="/run/user/$(id -u)"
	echo "⚠️  XDG_RUNTIME_DIR was not set, using ${XDG_RUNTIME_DIR}"
fi

# Enable rootless socket
echo "Enabling rootless Podman socket..."
systemctl --user enable --now podman.socket

ROOTLESS_SOCKET="${XDG_RUNTIME_DIR}/podman/podman.sock"
if [[ -S "${ROOTLESS_SOCKET}" ]]; then
	echo "✓ Rootless socket is active at ${ROOTLESS_SOCKET}"
else
	echo "⚠️  Socket file not found at ${ROOTLESS_SOCKET}"
	echo "   Check: systemctl --user status podman.socket"
fi

# Configure cgroup delegation (idempotent)
echo ""
echo "Checking cgroup delegation..."
if [[ -f "${DELEGATE_CONF}" ]] && grep -q "Delegate=cpu cpuset io memory pids" "${DELEGATE_CONF}" 2>/dev/null; then
	echo "✓ Cgroup delegation already configured"
else
	echo "Configuring cgroup delegation (requires sudo)..."
	sudo mkdir -p "$(dirname "${DELEGATE_CONF}")"
	echo "${DELEGATE_CONTENT}" | sudo tee "${DELEGATE_CONF}" >/dev/null
	sudo systemctl daemon-reload
	CGROUP_CHANGED=true
	echo "✓ Cgroup delegation configured"
fi

# Headless detection and configuration
echo ""
echo "Checking for graphical session..."
if [[ -z "${DISPLAY:-}" ]] && [[ -z "${WAYLAND_DISPLAY:-}" ]]; then
	echo "  No graphical session detected (headless/SSH system)"

	# loginctl enable-linger
	echo ""
	CURRENT_LINGER="$(loginctl show-user "${USER}" --property=Linger --value 2>/dev/null || echo "no")"
	if [[ "${CURRENT_LINGER}" == "yes" ]]; then
		echo "✓ Login lingering already enabled"
	else
		echo "Enabling login lingering (keeps services alive after SSH disconnect)..."
		loginctl enable-linger "${USER}"
		echo "✓ Login lingering enabled"
	fi

	# XDG_RUNTIME_DIR check
	echo ""
	if [[ -d "${XDG_RUNTIME_DIR}" ]]; then
		echo "✓ XDG_RUNTIME_DIR is set and exists (${XDG_RUNTIME_DIR})"
	else
		echo "⚠️  XDG_RUNTIME_DIR (${XDG_RUNTIME_DIR}) does not exist."
		echo "   Add the following to your shell profile:"
		echo "     export XDG_RUNTIME_DIR=/run/user/\$(id -u)"
	fi
else
	echo "  Graphical session detected (desktop/laptop)"
	echo "✓ Headless-specific steps skipped (login lingering, XDG_RUNTIME_DIR)"
fi

# Verify subuid/subgid
echo ""
echo "Checking subuid/subgid configuration..."
SUBUID_OK=true
SUBGID_OK=true

if ! grep -q "^${USER}:" /etc/subuid 2>/dev/null; then
	SUBUID_OK=false
fi
if ! grep -q "^${USER}:" /etc/subgid 2>/dev/null; then
	SUBGID_OK=false
fi

if [[ "${SUBUID_OK}" == "true" ]] && [[ "${SUBGID_OK}" == "true" ]]; then
	echo "✓ subuid/subgid entries exist for ${USER}"
else
	echo "⚠️  Missing subuid/subgid entries for ${USER}"
	read -p "Attempt to add them now? (requires sudo) (y/N) " -n 1 -r
	echo
	if [[ ${REPLY} =~ ^[Yy]$ ]]; then
		if [[ "${SUBUID_OK}" == "false" ]]; then
			if sudo usermod --add-subuids 100000-165535 "${USER}" 2>/dev/null; then
				echo "✓ Added subuid range for ${USER}"
			else
				echo "  Falling back to manual entry..."
				echo "${USER}:100000:65536" | sudo tee -a /etc/subuid >/dev/null
				echo "✓ Added subuid range for ${USER}"
			fi
		fi
		if [[ "${SUBGID_OK}" == "false" ]]; then
			if sudo usermod --add-subgids 100000-165535 "${USER}" 2>/dev/null; then
				echo "✓ Added subgid range for ${USER}"
			else
				echo "  Falling back to manual entry..."
				echo "${USER}:100000:65536" | sudo tee -a /etc/subgid >/dev/null
				echo "✓ Added subgid range for ${USER}"
			fi
		fi
		echo "Restarting Podman socket after subuid/subgid changes..."
		systemctl --user restart podman.socket
	else
		echo "  Skipped. Rootless containers may fail without subuid/subgid entries."
		echo "  See: https://github.com/LobsterTrap/OpenShell/blob/midstream/docs/get-started/install-podman-linux.md"
	fi
fi

# Verify setup
echo ""
echo "--- Verification ---"
echo ""

CGROUP_VERSION="$(podman info --format '{{.Host.CgroupVersion}}' 2>/dev/null || echo "unknown")"
if [[ "${CGROUP_VERSION}" == "v2" ]]; then
	echo "✓ Podman reports cgroup v2"
else
	echo "⚠️  Podman reports cgroup version: ${CGROUP_VERSION} (expected v2)"
fi

echo ""
echo "Running hello-world container..."
if podman run --rm docker.io/library/hello-world &>/dev/null; then
	echo "✓ Podman rootless mode is working"
else
	echo "⚠️  hello-world container failed."
	echo "   Try: podman run --rm docker.io/library/hello-world"
	echo "   Check the troubleshooting section of the setup guide."
fi

# Summary
echo ""
echo "=== Rootless Setup Complete ==="
echo ""

if [[ "${CGROUP_CHANGED}" == "true" ]]; then
	echo "⚠️  Cgroup delegation was configured. You must log out and back"
	echo "   in for the changes to take full effect before starting the"
	echo "   OpenShell gateway."
	echo ""
fi

echo "Next steps:"
if [[ "${CGROUP_CHANGED}" == "true" ]]; then
	echo "  1. Log out and back in (required for cgroup delegation)"
	echo "  2. Run: openshell gateway start"
else
	echo "  1. Run: openshell gateway start"
fi
