#!/usr/bin/env bash
# Server-side build: pull latest, build the static binary in Docker,
# install it to /usr/local/bin. Run as root (or with sudo).
set -euo pipefail
cd "$(dirname "$0")"

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
echo "✅ installed /usr/local/bin/hefesto (${arch})"
hefesto --help | head -2
