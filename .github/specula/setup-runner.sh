#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
CONFIG="$SCRIPT_DIR/config.json"
config_value() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$CONFIG" "$1"
}

ROOT="${SPECULA_STATE_ROOT:-$(config_value state_root)}"
RUNNER_USER="${SPECULA_RUNNER_USER:-specula}"
SPECULA_REPOSITORY="$(config_value specula_repository)"
SPECULA_COMMIT="$(config_value specula_commit)"
COPILOT_VERSION=1.0.86
RUST_VERSION=1.95.0
CARGO_NEXTEST_VERSION=0.9.133
SOURCE="$(config_value specula_source)"
VENV="$(dirname -- "$(dirname -- "$(config_value specula_binary)")")"
TLA2TOOLS_SHA256=9d36716ffb5e49d1ba8fae4651eba59f3189887e12eb90e204a42d2e6e993fef
COMMUNITY_MODULES_SHA256=044e8ecdfbca92d51d7eb4469422c2a7da1fe25dc8ad39c4a90e6622d6da4d99

sudo env DEBIAN_FRONTEND=noninteractive apt-get update
sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y \
    bc binutils bison build-essential ca-certificates clang cmake cpio curl flex git gzip \
    gh libarchive-tools libelf-dev libssl-dev lld make maven ninja-build nodejs npm \
    openjdk-21-jdk-headless patch perl \
    pkg-config python3 python3-pip python3-venv rsync tar xz-utils zstd

if [[ ! -d "$SOURCE/.git" ]]; then
    sudo git clone "$SPECULA_REPOSITORY" "$SOURCE"
fi
sudo git -C "$SOURCE" fetch origin "$SPECULA_COMMIT"
sudo git -C "$SOURCE" checkout --detach "$SPECULA_COMMIT"
sudo git -C "$SOURCE" submodule update --init --recursive
if ! sudo -u "$RUNNER_USER" -H git config --global --get-all safe.directory |
    grep -Fxq "$SOURCE"; then
    sudo -u "$RUNNER_USER" -H git config --global --add safe.directory "$SOURCE"
fi

if [[ ! -x "$VENV/bin/specula" ]] ||
    ! "$VENV/bin/python" -c 'import copilot, jsonschema, mcp, specula' 2>/dev/null; then
    sudo rm -rf "$VENV"
    sudo python3 -m venv "$VENV"
    sudo "$VENV/bin/pip" install --upgrade pip uv
    sudo "$VENV/bin/pip" install -e "$SOURCE" \
        -r "$SOURCE/tools/context_control/requirements.txt" \
        -r "$SOURCE/tools/spec_analyzer/requirements.txt" \
        -r "$SOURCE/tools/trace_debugger/requirements.txt"
fi

sudo install -d -m 0755 "$SOURCE/lib"
if [[ ! -f "$SOURCE/lib/tla2tools.jar" ]]; then
    sudo curl --fail --location --proto '=https' --tlsv1.2 \
        --output "$SOURCE/lib/tla2tools.jar" \
        https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar
fi
if [[ ! -f "$SOURCE/lib/CommunityModules-deps.jar" ]]; then
    sudo curl --fail --location --proto '=https' --tlsv1.2 \
        --output "$SOURCE/lib/CommunityModules-deps.jar" \
        https://github.com/tlaplus/CommunityModules/releases/download/202505152026/CommunityModules-deps.jar
fi
printf '%s  %s\n' "$TLA2TOOLS_SHA256" "$SOURCE/lib/tla2tools.jar" |
    sha256sum --check --strict
printf '%s  %s\n' "$COMMUNITY_MODULES_SHA256" "$SOURCE/lib/CommunityModules-deps.jar" |
    sha256sum --check --strict
sudo mvn -q -f "$SOURCE/tools/cfa/pom.xml" package -DskipTests
for tool in trace_debugger spec_analyzer inv_checking_tool tlc_tools context_control; do
    sudo rm -rf "$SOURCE/tools/$tool/.venv"
    sudo ln -s "$VENV" "$SOURCE/tools/$tool/.venv"
done

if [[ ! -x /usr/local/bin/copilot ]] ||
    ! /usr/local/bin/copilot --version | grep -Fq "$COPILOT_VERSION"; then
    sudo npm install --global "@github/copilot@$COPILOT_VERSION"
fi

home="$(getent passwd "$RUNNER_USER" | cut -d: -f6)"
sudo -u "$RUNNER_USER" -H bash -lc "
    if ! command -v rustup >/dev/null 2>&1; then
        curl --fail --proto '=https' --tlsv1.2 --silent --show-error https://sh.rustup.rs |
            sh -s -- -y --profile minimal --default-toolchain '$RUST_VERSION'
    fi
    export PATH=\"\$HOME/.cargo/bin:\$PATH\"
    rustup toolchain install '$RUST_VERSION' --profile minimal
    rustup default '$RUST_VERSION'
    if ! cargo nextest --version 2>/dev/null | grep -Fq '$CARGO_NEXTEST_VERSION'; then
        cargo install --locked cargo-nextest --version '$CARGO_NEXTEST_VERSION'
    fi
"
sudo ln -sf "$home/.cargo/bin/rustc" /usr/local/bin/rustc
sudo ln -sf "$home/.cargo/bin/cargo" /usr/local/bin/cargo
sudo ln -sf "$home/.cargo/bin/cargo-nextest" /usr/local/bin/cargo-nextest
sudo ln -sf "$VENV/bin/specula" /usr/local/bin/specula
sudo ln -sf "$VENV/bin/uv" /usr/local/bin/uv

gid="$(id -g "$RUNNER_USER")"
sudo mkdir -p "$ROOT"/{repos,source,state,reports}
sudo chown -R "$RUNNER_USER:$gid" "$ROOT"
if [[ -c /dev/kvm ]]; then
    sudo chgrp "$gid" /dev/kvm
    sudo chmod 0660 /dev/kvm
fi
sudo install -d -m 0700 -o "$RUNNER_USER" -g "$gid" "$home/.agents" "$home/.copilot"
sudo -u "$RUNNER_USER" "$VENV/bin/python" "$SOURCE/src/specula/skill_install.py" \
    --source "$SOURCE/skills" --target "$home/.agents/skills"

sudo tee "$home/.copilot/mcp-config.json" >/dev/null <<EOF
{
  "mcpServers": {
    "tracedebugger": {"type": "local", "command": "$VENV/bin/python", "args": ["$SOURCE/tools/trace_debugger/mcp_server.py"], "tools": ["*"], "env": {"SPECULA_ROOT": "$SOURCE"}},
    "spec_analyzer": {"type": "local", "command": "$VENV/bin/python", "args": ["$SOURCE/tools/spec_analyzer/mcp_server.py"], "tools": ["*"], "env": {"SPECULA_ROOT": "$SOURCE"}},
    "inv_checking_tool": {"type": "local", "command": "$VENV/bin/python", "args": ["$SOURCE/tools/inv_checking_tool/mcp_server.py"], "tools": ["*"], "env": {"SPECULA_ROOT": "$SOURCE"}}
  }
}
EOF
sudo chown "$RUNNER_USER:$gid" "$home/.copilot/mcp-config.json"
sudo chmod 0600 "$home/.copilot/mcp-config.json"

sudo -u "$RUNNER_USER" test -r /dev/kvm
sudo -u "$RUNNER_USER" test -w /dev/kvm
sudo -u "$RUNNER_USER" "$VENV/bin/specula" --version
sudo -u "$RUNNER_USER" copilot --version
sudo -u "$RUNNER_USER" gh --version
sudo -u "$RUNNER_USER" java -version
sudo -u "$RUNNER_USER" javac -version
sudo -u "$RUNNER_USER" rustc --version
printf 'Specula runner provisioning complete for %s.\n' "$RUNNER_USER"
