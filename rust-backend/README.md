# TeamViewRelay Rust backend

The Rust backend is built directly from the protocol
submodule pinned by the parent repository. It does not copy or modify any
`.proto` file.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets

# Run Python and Rust on ports 18764/18765 first.
uv run python ../scripts/replay_compare.py
uv run python ../scripts/load_test_rust.py --duration 120
```

The replay covers full snapshots, patches, deletes, `clear_fields`, players,
entities, waypoints, last-seen players, battle chunks and metadata, Tab History
FULL/lookup/subscription behavior, and `/snapshot`.

## Canary deployment

Keep Python on the production port and start Rust on a separate port:

```bash
TEAMVIEWER_RUST_PORT=8766 docker compose -f docker-compose.rust.yml up -d --build
curl --fail http://127.0.0.1:8766/health

uv run python scripts/load_test_live.py \
  --url http://127.0.0.1:8766 \
  --room load-rust-canary \
  --stages 10,20,40 \
  --history-players 1000 \
  --min-snapshot-kib 200 \
  --expected-build team-view-relay-rust-v1.0.0-proto0.7.0
```

Mirror test traffic or route a small explicitly selected cohort to Rust. Use a
separate database volume during canary evaluation. Do not point Python and Rust
at the same SQLite file concurrently.

## Rollback

Route the canary cohort back to the Python service, then stop Rust:

```bash
docker compose -f docker-compose.rust.yml down
```

The Python container and its database remain untouched, so rollback does not
require a data migration.

## Current parity

The Rust relay now implements compressed WebSockets, room-scoped full/patch
delivery, source arbitration, timeout and pre-expiry refresh behavior, scoped
same-server filtering, canonical periodic digests, all Web Map commands, Battle
Chunk retention, Last Seen, Tab reports, Tab History, traffic/protobuf metrics,
admin authentication, SQLite history, and the admin diagnostic APIs.

The send path keeps one coalescing state slot per connection and computes every
patch from the last snapshot actually written by that connection. A replaced
pending frame therefore does not force repeated large full snapshots.

Production cutover still requires a sustained large-fixture canary on the target
host, review of the remaining bandwidth-budget items documented in
`../docs/broadcast-backpressure-known-issue.md`, and a rollback rehearsal. The
Rust database must remain separate from the Python database.
