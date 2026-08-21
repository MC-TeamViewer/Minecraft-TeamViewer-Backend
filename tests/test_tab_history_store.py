from __future__ import annotations

import hashlib
from pathlib import Path
import sys

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.tab_history import TabHistoryStore, TabHistoryStoreConfig


@pytest.fixture
async def store(tmp_path):
    value = TabHistoryStore(
        TabHistoryStoreConfig(
            db_path=str(tmp_path / "history.db"),
            retention_days=400,
            delta_retention_days=30,
            observation_update_interval_sec=300,
        )
    )
    await value.initialize()
    try:
        yield value
    finally:
        await value.close()


async def test_full_delta_and_lookup_are_room_scoped(store: TabHistoryStore):
    player_uuid = "12345678-1234-5678-9234-567812345678"
    assert await store.upsert_players(
        "alpha",
        [(player_uuid, {"name": "Player", "scoreboardTeamId": "town_a", "scoreboardSuffix": "!"})],
        observed_at_ms=1_000,
    )
    assert await store.upsert_players(
        "beta",
        [(player_uuid, {"name": "Player", "scoreboardTeamId": "town_b"})],
        observed_at_ms=1_000,
    )

    alpha = await store.sync(
        "alpha",
        preferred_mode="TAB_HISTORY_SYNC_MODE_FULL",
        base_revision=None,
        base_digest=None,
        allow_full_fallback=False,
    )
    assert alpha["head"]["recordCount"] == 1
    assert alpha["upsert"][0]["player"]["scoreboardTeamId"] == "town_a"

    head, results = await store.lookup("beta", [{"uuid": player_uuid}, {"name": "  PLAYER  "}])
    assert head["recordCount"] == 1
    assert [entry["player"]["scoreboardTeamId"] for entry in results[0]["entries"]] == ["town_b"]
    assert len(results[1]["entries"]) == 1

    base = alpha["head"]
    assert await store.upsert_players(
        "alpha",
        [(None, {"uuid": player_uuid, "name": "Player", "scoreboardTeamId": "town_c"})],
        observed_at_ms=2_000,
    )
    delta = await store.sync(
        "alpha",
        preferred_mode="TAB_HISTORY_SYNC_MODE_DELTA",
        base_revision=base["revision"],
        base_digest=base["digestSha256"],
        allow_full_fallback=False,
    )
    assert delta["mode"] == "TAB_HISTORY_SYNC_MODE_DELTA"
    assert delta["upsert"][0]["player"]["scoreboardTeamId"] == "town_c"


async def test_uuid_fallback_rich_text_degrade_and_retention_tombstone(store: TabHistoryStore):
    player_uuid = "12345678-1234-5678-9234-567812345678"
    assert await store.upsert_players(
        "room",
        [
            (
                player_uuid,
                {
                    "name": "Player",
                    "displayName": "AB",
                    "formattedDisplayName": {
                        "plainText": "mismatch",
                        "spans": [{"text": "A", "bold": True}, {"text": "B", "bold": True}],
                    },
                },
            )
        ],
        observed_at_ms=1_000,
    )
    full = await store.sync(
        "room",
        preferred_mode="TAB_HISTORY_SYNC_MODE_FULL",
        base_revision=None,
        base_digest=None,
        allow_full_fallback=False,
    )
    formatted = full["upsert"][0]["player"]["formattedDisplayName"]
    assert formatted == {"plainText": "AB", "spans": [{"text": "AB"}]}
    base = full["head"]

    store.config.retention_days = 1
    cleanup = await store.cleanup_retention(now_ms=1_000 + 2 * 86400 * 1000)
    assert cleanup["expiredEntries"] == 1
    delta = await store.sync(
        "room",
        preferred_mode="TAB_HISTORY_SYNC_MODE_DELTA",
        base_revision=base["revision"],
        base_digest=base["digestSha256"],
        allow_full_fallback=False,
    )
    assert delta["deleteUuids"] == [player_uuid]
    assert delta["head"]["digestSha256"] == hashlib.sha256(b"").digest()


async def test_history_rejects_legacy_prefix_copied_into_display_name(store: TabHistoryStore):
    player_uuid = "12345678-1234-5678-9234-567812345678"
    assert await store.upsert_players(
        "room",
        [(player_uuid, {
            "name": "Player",
            "displayName": "[利雅得] Player",
            "prefixedName": "nt00011bf146084c",
            "scoreboardPrefix": "nt00011bf146084c",
        })],
        observed_at_ms=1_000,
    )
    assert not await store.upsert_players(
        "room",
        [(player_uuid, {
            "name": "Player",
            "displayName": "nt00011bf146084c",
            "prefixedName": "nt00011bf146084c",
            "scoreboardPrefix": "nt00011bf146084c",
        })],
        observed_at_ms=2_000,
    )

    full = await store.sync(
        "room",
        preferred_mode="TAB_HISTORY_SYNC_MODE_FULL",
        base_revision=None,
        base_digest=None,
        allow_full_fallback=False,
    )
    assert full["upsert"][0]["player"]["displayName"] == "[利雅得] Player"


async def test_admin_listing_and_delete_writes_tombstone(store: TabHistoryStore):
    player_a = "12345678-1234-5678-9234-567812345678"
    player_b = "22345678-1234-5678-9234-567812345678"
    assert await store.upsert_players(
        "alpha",
        [
            (player_a, {"name": "Alice", "displayName": "[A] Alice"}),
            (player_b, {"name": "Bob", "displayName": "[B] Bob"}),
        ],
        observed_at_ms=1_000,
    )
    initial_head = await store.head("alpha")

    listed = await store.list_entries(room_code="alpha", search="alice", page=1, page_size=50)
    assert listed["total"] == 1
    assert listed["items"][0]["playerUuid"] == player_a
    assert listed["items"][0]["player"]["displayName"] == "[A] Alice"
    assert listed["availableRooms"] == ["alpha"]

    deleted = await store.delete_entries("alpha", [player_a], occurred_at_ms=2_000)
    assert deleted == [player_a]
    assert await store.delete_entries("alpha", [player_a], occurred_at_ms=2_100) == []

    delta = await store.sync(
        "alpha",
        preferred_mode="TAB_HISTORY_SYNC_MODE_DELTA",
        base_revision=initial_head["revision"],
        base_digest=initial_head["digestSha256"],
        allow_full_fallback=False,
    )
    assert delta["deleteUuids"] == [player_a]
    assert delta["head"]["recordCount"] == 1
    assert delta["head"]["revision"] == initial_head["revision"] + 1
