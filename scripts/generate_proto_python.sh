#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUF_BIN_DEFAULT="buf"
if [[ -x "/tmp/buf/bin/buf" ]]; then
  BUF_BIN_DEFAULT="/tmp/buf/bin/buf"
fi
BUF_BIN_RAW="${BUF_BIN:-$BUF_BIN_DEFAULT}"
PROTO_DIR="$ROOT_DIR/third_party/TeamViewRelay-Protocol/proto"
TEMPLATE_FILE="$ROOT_DIR/buf.gen.yaml"
OUT_DIR="$ROOT_DIR/src/server/proto_generated"

if [[ "$BUF_BIN_RAW" == */* ]]; then
  BUF_BIN="$BUF_BIN_RAW"
else
  BUF_BIN="$(command -v "$BUF_BIN_RAW" 2>/dev/null || true)"
fi

if [[ -z "${BUF_BIN:-}" || ! -x "$BUF_BIN" ]]; then
  echo "buf binary not found: ${BUF_BIN_RAW}" >&2
  exit 1
fi

if [[ ! -d "$PROTO_DIR" ]]; then
  echo "shared proto dir not found at $PROTO_DIR" >&2
  exit 1
fi

rm -rf "$OUT_DIR/teamviewer/v1"

"$BUF_BIN" generate "$PROTO_DIR" --template "$TEMPLATE_FILE"
