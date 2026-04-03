#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUF_BIN="${BUF_BIN:-/tmp/buf/bin/buf}"

if [[ ! -x "$BUF_BIN" ]]; then
  echo "buf binary not found at $BUF_BIN" >&2
  exit 1
fi

rm -rf "$ROOT_DIR/src/server/proto_generated/teamviewer/v1"
rm -rf "$ROOT_DIR/../Minecraft-TeamViewer-Web-Script/src/network/proto/teamviewer/v1"
find "$ROOT_DIR/../Minecraft_TeamViewer/common/src/main/java/fun/prof_chen/teamviewer/main_code/network/proto" \
  -maxdepth 1 \
  -type f \
  \( -name '*.java' -o -name '*.kt' \) \
  -delete

(
  cd "$ROOT_DIR"
  "$BUF_BIN" lint
  "$BUF_BIN" generate
)
