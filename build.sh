#!/usr/bin/env bash
# Server-side build: pull latest, build the static binary in Docker,
# install it to /usr/local/bin and keep a runtime copy next to the repo.
# Run as root (or with sudo).
set -euo pipefail
cd "$(dirname "$0")"

RUNTIME_DIR="${HEFESTO_RUNTIME_DIR:-/apps/sysdata/hefesto}"

if [ -d .git ]; then
  echo "🔄 git pull"
  git pull --ff-only
fi

echo "🔨 building (docker, native arch)"
if ! DOCKER_BUILDKIT=1 docker build --target artifact -o dist .; then
  echo "❌ build failed — the installed binary was NOT replaced" >&2
  exit 1
fi

arch="$(uname -m)"
install -m 755 dist/hefesto "/usr/local/bin/hefesto"
cp dist/hefesto "dist/hefesto-linux-${arch}"

# runtime copy, so the deploy folder always ships the same binary
mkdir -p "$RUNTIME_DIR"
install -m 755 dist/hefesto "$RUNTIME_DIR/hefesto"

echo "✅ installed /usr/local/bin/hefesto and $RUNTIME_DIR/hefesto (${arch})"
hefesto --help | head -2
