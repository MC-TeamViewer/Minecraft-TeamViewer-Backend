#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROTO_DIR="$ROOT_DIR/third_party/TeamViewRelay-Protocol/proto"
OUT_DIR="$ROOT_DIR/src/server/proto_generated"
UV_BIN="${UV_BIN:-$(command -v uv 2>/dev/null || true)}"

if [[ -z "$UV_BIN" || ! -x "$UV_BIN" ]]; then
  echo "uv binary not found" >&2
  exit 1
fi

if [[ ! -d "$PROTO_DIR" ]]; then
  echo "shared proto dir not found at $PROTO_DIR" >&2
  exit 1
fi

rm -rf "$OUT_DIR/teamviewer/v1"
mkdir -p "$OUT_DIR/teamviewer/v1"

"$UV_BIN" run --with 'grpcio-tools==1.76.0' \
  python -m grpc_tools.protoc \
  -I "$PROTO_DIR" \
  --python_out="$OUT_DIR" \
  "$PROTO_DIR/teamviewer/v1/teamviewer.proto"

touch "$OUT_DIR/teamviewer/v1/__init__.py"
