# Installing OpenShell with Podman on macOS (Apple Silicon)

This guide covers installing and running OpenShell with Podman (rootless mode) on **macOS Apple Silicon (ARM64)**.

**Platform-specific**: This guide is for macOS only. On macOS, Podman runs in a Linux VM, which requires specific setup. For Linux installations, the process is different (no Podman machine needed, native Podman with different socket paths and cgroup configuration).

## Prerequisites

### 1. Install Homebrew

```bash
# Check if already installed
which brew

# If not found, install it
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

### 2. Install Podman

```bash
brew install podman
```

### 3. Install Rust and Cargo

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Choose option 1 (default installation)

# Load Rust into current shell
source $HOME/.cargo/env

# Verify installation
rustc --version
cargo --version
```

### 4. Initialize Podman Machine

**Important**: Create a dedicated Podman machine for OpenShell with at least 8GB memory (required for building images).

**Note**: Only one Podman machine can run at a time on macOS. If you have an existing machine running, you need to stop it first.

```bash
# Check for running machines
podman machine list

# If another machine is running (e.g., podman-machine-default), stop it
podman machine stop podman-machine-default  # or the name of your running machine

# Initialize a dedicated OpenShell Podman machine
podman machine init openshell --memory 8192 --cpus 4

# Start the OpenShell Podman machine
podman machine start openshell

# Set it as the active/default machine
podman system connection default openshell

# Configure cgroup delegation (CRITICAL for rootless k3s)
podman machine ssh openshell 'echo "[Service]
Delegate=cpu cpuset io memory pids" | sudo tee /etc/systemd/system/user@.service.d/delegate.conf'

# Reload systemd in the VM
podman machine ssh openshell "sudo systemctl daemon-reload"

# Restart the machine for cgroup changes to take effect
podman machine stop openshell
podman machine start openshell
```

Verify Podman is running:

```bash
podman machine list
# Should show: openshell  running  (highlighted as default)

# Verify cgroup delegation
podman machine ssh openshell "cat /etc/systemd/system/user@.service.d/delegate.conf"
# Should show: Delegate=cpu cpuset io memory pids
```

## Clone Repository

Clone the OpenShell repository with Podman support:

```bash
# Navigate to your development directory
cd ~/development  # or your preferred location

# Clone the repository
git clone https://github.com/itdove/OpenShell.git
cd OpenShell

# Checkout the Podman-compatible branch
git checkout openshell-podman-itdove

# Verify you're on the correct branch
git branch --show-current
# Should show: openshell-podman-itdove
```

## Environment Setup

Get the socket path for the OpenShell Podman machine:

```bash
# Get the socket path
OPENSHELL_SOCKET=$(podman machine inspect openshell --format '{{.ConnectionInfo.PodmanSocket.Path}}')
echo "OpenShell Podman socket: $OPENSHELL_SOCKET"
```

Set required environment variables for this session:

```bash
export CONTAINER_HOST="unix://${OPENSHELL_SOCKET}"
export OPENSHELL_CONTAINER_RUNTIME=podman
```

**Make them permanent** (recommended):

```bash
# Get socket path
OPENSHELL_SOCKET=$(podman machine inspect openshell --format '{{.ConnectionInfo.PodmanSocket.Path}}')

# Add to .zshrc
echo "export CONTAINER_HOST=\"unix://${OPENSHELL_SOCKET}\"" >> ~/.zshrc
echo 'export OPENSHELL_CONTAINER_RUNTIME=podman' >> ~/.zshrc

# Reload shell configuration
source ~/.zshrc

# Verify
echo $CONTAINER_HOST
echo $OPENSHELL_CONTAINER_RUNTIME
```

## Build Cluster Image

Build the cluster container image (takes 10-15 minutes):

```bash
# From the OpenShell repository directory

OPENSHELL_CONTAINER_RUNTIME=podman tasks/scripts/docker-build-image.sh cluster \
  --build-arg TARGETARCH=arm64 \
  --build-arg BUILDARCH=arm64
```

Verify the image was built:

```bash
podman images | grep cluster
# Should show: localhost/openshell/cluster  dev
```

**CRITICAL**: Tag the image with the expected registry path:

```bash
podman tag localhost/openshell/cluster:dev ghcr.io/nvidia/openshell/cluster:dev
```

Verify both tags exist:

```bash
podman images | grep cluster
# Should show both:
# localhost/openshell/cluster       dev
# ghcr.io/nvidia/openshell/cluster  dev
```

## Build and Install CLI

Build the OpenShell CLI:

```bash
# Ensure you're in the OpenShell directory
cd OpenShell  # if not already there

# Ensure Rust is available
source $HOME/.cargo/env

# Build the CLI
cargo build --release -p openshell-cli
```

Install the binary to your PATH:

```bash
# Option 1: Install to ~/.local/bin (if it's in your PATH)
mkdir -p ~/.local/bin
cp target/release/openshell ~/.local/bin/openshell
chmod +x ~/.local/bin/openshell

# Option 2: Install to /usr/local/bin (requires sudo)
# sudo cp target/release/openshell /usr/local/bin/openshell
# sudo chmod +x /usr/local/bin/openshell
```

Verify installation:

```bash
openshell --version
which openshell
```

## Test Installation

Create your first OpenShell sandbox:

```bash
# Create a sandbox (gateway starts automatically)
openshell sandbox create

# Or create with a specific agent
openshell sandbox create -- claude
```

The gateway will start automatically on first use. You should see:
- "No gateway found — starting one automatically"
- Gateway starting and becoming healthy
- Sandbox being created

Connect to your sandbox:

```bash
# List sandboxes
openshell sandbox list

# Connect to a sandbox
openshell sandbox connect <sandbox-name>

# Or use the interactive TUI
openshell term
```

View logs:

```bash
openshell logs <sandbox-name>
```

Destroy a sandbox when done:

```bash
openshell sandbox destroy <sandbox-name>
```

## Cleanup

To remove everything and start fresh:

```bash
# Ensure you're in the OpenShell directory
cd OpenShell  # if not already there
bash cleanup-openshell-podman-macos.sh
```

To completely reset the OpenShell Podman machine (optional):

```bash
podman machine stop openshell
podman machine rm openshell
# Then follow installation steps to recreate
```

## Troubleshooting

### "command not found: cargo"

Load Rust environment:
```bash
source $HOME/.cargo/env
```

### "Failed to connect to Podman"

Check environment variables are set:
```bash
echo $CONTAINER_HOST
echo $OPENSHELL_CONTAINER_RUNTIME
```

Verify Podman machine is running:
```bash
podman machine list
podman machine start openshell  # if not running
```

### "Gateway not reachable" / "Connection refused"

Stale gateway metadata from a previous installation or different Podman machine:
```bash
# Destroy the stale gateway metadata
openshell gateway destroy --name openshell

# Then create a sandbox (gateway will start automatically)
openshell sandbox create
```

### Build fails with memory/killed errors

Increase Podman machine memory:
```bash
podman machine stop openshell
podman machine set openshell --memory 8192
podman machine start openshell
```

Check current memory allocation:
```bash
podman machine inspect openshell | grep -i memory
```

### "failed to find cpuset cgroup"

Verify cgroup delegation in Podman VM:
```bash
podman machine ssh openshell "cat /etc/systemd/system/user@.service.d/delegate.conf"
```

Should show `Delegate=cpu cpuset io memory pids`. If missing cpuset, reconfigure:
```bash
podman machine ssh openshell 'echo "[Service]
Delegate=cpu cpuset io memory pids" | sudo tee /etc/systemd/system/user@.service.d/delegate.conf'
podman machine ssh openshell "sudo systemctl daemon-reload"
podman machine stop openshell && podman machine start openshell
```

### Gateway fails to start / Image not found

Ensure the image is tagged correctly:
```bash
podman images | grep cluster
# Both should exist:
# localhost/openshell/cluster       dev
# ghcr.io/nvidia/openshell/cluster  dev

# If missing, retag:
podman tag localhost/openshell/cluster:dev ghcr.io/nvidia/openshell/cluster:dev
```

### View container logs

```bash
podman ps -a  # Get container ID
podman logs <container-id>
```

## Key Differences from Docker

This Podman-compatible installation includes:

- **Runtime detection**: Automatically detects Podman vs Docker
- **Rootless mode**: Uses `private` cgroupns mode for Podman
- **macOS networking**: Resolves actual gateway IP (10.89.0.1) instead of `host-gateway`
- **Build compatibility**: Dockerfile fixes for Podman builds (ENV TARGETARCH/BUILDARCH)
- **Cgroup delegation**: Requires cpuset delegation in Podman VM for k3s

## Environment Variables

- `CONTAINER_HOST`: Points to Podman socket (Podman's equivalent of DOCKER_HOST)
- `OPENSHELL_CONTAINER_RUNTIME`: Forces Podman code paths in build scripts and CLI

## Architecture

OpenShell on Podman runs:
1. **Podman VM** (Fedora, rootless): Provides container runtime
2. **Gateway container**: k3s cluster in a single container
3. **Sandboxes**: Individual environments running inside k3s

The gateway container runs with:
- Private cgroup namespace (rootless requirement)
- Cgroup delegation from host (cpuset, cpu, io, memory, pids)
- Network gateway IP resolution for macOS

## Additional Resources

- [OpenShell Documentation](https://docs.nvidia.com/openshell/latest/index.html)
- [Podman Documentation](https://docs.podman.io/)
- [k3s Rootless Mode](https://docs.k3s.io/advanced#running-k3s-in-docker)

## Contributing

This Podman support is in the `openshell-podman-itdove` branch. Once tested and stable, it will be contributed upstream to the main OpenShell repository.
