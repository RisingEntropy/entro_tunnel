#!/usr/bin/env bash
# Build static-ish Linux x86_64 binaries (server + CLI) for deployment.
#
# Uses an amd64 Debian-bullseye Rust container so the result runs on glibc >=2.31
# (Ubuntu 20.04+). On Apple Silicon this runs under emulation (slower).
#
# Output: target-linux/release/entrotunnel-{server,cli}
set -euo pipefail
cd "$(dirname "$0")/.."

# Feature set: tcp only = pure Rust, zero C deps. Pass "full" for tls/ws/quic.
FEATURES="${1:-tcp}"
if [ "$FEATURES" = "full" ]; then
  FEATURE_ARGS=""        # default features (tcp+tls+ws+quic)
else
  FEATURE_ARGS="--no-default-features --features tcp"
fi

docker run --rm --platform linux/amd64 \
  -e CARGO_TARGET_DIR=/work/target-linux \
  -v "$PWD":/work -w /work \
  -v et-cargo-registry:/usr/local/cargo/registry \
  rust:1.90-bullseye \
  cargo build --release -p entrotunnel-server -p entrotunnel-cli $FEATURE_ARGS

echo "built:"
ls -lh target-linux/release/entrotunnel-server target-linux/release/entrotunnel-cli
