from __future__ import annotations

from datetime import datetime, timedelta
from pathlib import Path
import sys

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.admin_store import AdminStore, AdminStoreConfig


def _local_base(hour: int = 10, minute: int = 0) -> datetime:
    return datetime.now().astimezone().replace(hour=hour, minute=minute, second=0, microsecond=0)


@pytest.mark.asyncio
async def test_player_activity_upserts_same_hour_and_expands_cross_hour_and_day(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "admin.db")))
    await store.initialize()
    try:
        base = _local_base(9, 15)
        same_hour = base + timedelta(minutes=20)
        next_hour = base + timedelta(hours=1)
        next_day = base + timedelta(days=1)

        await store.record_player_activity("player-1", "room-a", occurred_at=base.timestamp())
        await store.record_player_activity("player-1", "room-a", occurred_at=same_hour.timestamp())
        await store.record_player_activity("player-1", "room-a", occurred_at=next_hour.timestamp())
        await store.record_player_activity("player-1", "room-a", occurred_at=next_day.timestamp())

        daily_rows = await store._fetchall(  # noqa: SLF001
            "SELECT local_date, player_id, room_code FROM daily_player_activity ORDER BY local_date"
        )
        hourly_rows = await store._fetchall(  # noqa: SLF001
            "SELECT local_hour, player_id, room_code FROM hourly_player_activity ORDER BY local_hour"
        )

        assert len(daily_rows) == 2
        assert len(hourly_rows) == 3
        assert daily_rows[0]["player_id"] == "player-1"
        assert hourly_rows[0]["room_code"] == "room-a"
    finally:
        await store.close()


@pytest.mark.asyncio
async def test_metrics_queries_support_global_distinct_and_room_filter(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "metrics.db")))
    await store.initialize()
    try:
        now_local = datetime.now().astimezone().replace(minute=0, second=0, microsecond=0)
        current_ts = now_local.timestamp()

        await store.record_player_activity("player-1", "room-a", occurred_at=current_ts)
        await store.record_player_activity("player-1", "room-b", occurred_at=current_ts)
        await store.record_player_activity("player-2", "room-a", occurred_at=current_ts)

        hourly_global = await store.query_hourly_metrics(hours=1)
        hourly_room_a = await store.query_hourly_metrics(hours=1, room_code="room-a")
        daily_global = await store.query_daily_metrics(days=1)

        assert hourly_global["items"][-1]["activePlayers"] == 2
        assert hourly_room_a["items"][-1]["activePlayers"] == 2
        assert daily_global["items"][-1]["activePlayers"] == 2
        assert "UTC" in hourly_global["timezone"]
    finally:
        await store.close()


@pytest.mark.asyncio
async def test_audit_query_supports_filters_and_pagination(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "audit.db")))
    await store.initialize()
    try:
        await store.record_audit_event(
            event_type="player_handshake_success",
            actor_type="player",
            actor_id="player-1",
            success=True,
            detail={"path": "/mc-client"},
        )
        await store.record_audit_event(
            event_type="admin_auth_failed",
            actor_type="admin",
            actor_id="admin",
            success=False,
            detail={"path": "/admin"},
        )
        await store.record_audit_event(
            event_type="web_map_handshake_success",
            actor_type="web_map",
            actor_id="web-map-1",
            success=True,
            detail={"path": "/web-map/ws"},
        )

        failed_only = await store.query_audit_events(limit=10, success=False)
        page_one = await store.query_audit_events(limit=2)
        page_two = await store.query_audit_events(limit=2, before_id=page_one["nextBeforeId"])

        assert len(failed_only["items"]) == 1
        assert failed_only["items"][0]["eventType"] == "admin_auth_failed"
        assert len(page_one["items"]) == 2
        assert page_one["items"][0]["id"] > page_one["items"][1]["id"]
        assert page_two["items"]
        assert page_two["items"][0]["id"] < page_one["items"][-1]["id"]
    finally:
        await store.close()
