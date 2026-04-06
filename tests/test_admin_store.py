from __future__ import annotations

from datetime import datetime, timedelta
from pathlib import Path
import sys

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

from server.admin.store import AdminStore, AdminStoreConfig


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
async def test_metrics_queries_support_explicit_start_date_and_start_at(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "metrics-start.db")))
    await store.initialize()
    try:
        base = _local_base(8, 37)
        start_date = (base - timedelta(days=2)).strftime("%Y-%m-%d")

        await store.record_player_activity("player-1", "room-a", occurred_at=(base - timedelta(days=1)).timestamp())
        await store.record_player_activity("player-2", "room-a", occurred_at=base.timestamp())
        await store.record_player_activity("player-3", "room-a", occurred_at=(base + timedelta(hours=1)).timestamp())

        daily = await store.query_daily_metrics_with_start(days=4, room_code="room-a", start_date=start_date)
        hourly = await store.query_hourly_metrics_with_start(
            hours=4,
            room_code="room-a",
            start_at=base.strftime("%Y-%m-%dT%H:%M:%S"),
        )

        assert daily["startDate"] == start_date
        assert [item["bucket"] for item in daily["items"]] == [
            start_date,
            (base - timedelta(days=1)).strftime("%Y-%m-%d"),
            base.strftime("%Y-%m-%d"),
            (base + timedelta(days=1)).strftime("%Y-%m-%d"),
        ]
        assert [item["activePlayers"] for item in daily["items"]] == [0, 1, 2, 0]

        assert hourly["startAt"] == base.replace(minute=0, second=0, microsecond=0).strftime("%Y-%m-%dT%H:00:00")
        assert [item["bucket"] for item in hourly["items"]] == [
            base.replace(minute=0, second=0, microsecond=0).strftime("%Y-%m-%dT%H:00:00"),
            (base.replace(minute=0, second=0, microsecond=0) + timedelta(hours=1)).strftime("%Y-%m-%dT%H:00:00"),
            (base.replace(minute=0, second=0, microsecond=0) + timedelta(hours=2)).strftime("%Y-%m-%dT%H:00:00"),
            (base.replace(minute=0, second=0, microsecond=0) + timedelta(hours=3)).strftime("%Y-%m-%dT%H:00:00"),
        ]
        assert [item["activePlayers"] for item in hourly["items"]] == [1, 1, 0, 0]
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
            detail={"path": "/admin/api/session/login"},
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


@pytest.mark.asyncio
async def test_admin_sessions_support_lookup_touch_and_expire(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "session.db")))
    await store.initialize()
    try:
        session, raw_token = await store.create_admin_session(actor_id="admin", remote_addr="127.0.0.1", ttl_sec=3600)
        loaded = await store.get_admin_session_by_token(raw_token)
        assert loaded is not None
        assert loaded["sessionId"] == session["sessionId"]

        touched = await store.touch_admin_session(session["sessionId"], ttl_sec=7200)
        assert touched is not None
        assert touched["expiresAt"] > loaded["expiresAt"]

        store._execute(  # noqa: SLF001
            "UPDATE admin_sessions SET expires_at = 1 WHERE session_id = ?",
            (session["sessionId"],),
        )
        expired = await store.expire_admin_sessions()
        assert len(expired) == 1
        assert expired[0]["sessionId"] == session["sessionId"]
        assert expired[0]["endReason"] == "expired"
    finally:
        await store.close()


@pytest.mark.asyncio
async def test_traffic_queries_zero_fill_and_sum_totals(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "traffic.db")))
    await store.initialize()
    try:
        now_local = datetime.now().astimezone().replace(second=0, microsecond=0)
        current_minute = now_local.strftime("%Y-%m-%dT%H:%M:00")
        current_5m = now_local.replace(minute=(now_local.minute // 5) * 5).strftime("%Y-%m-%dT%H:%M:00")
        current_hour = now_local.replace(minute=0).strftime("%Y-%m-%dT%H:00:00")
        current_day = now_local.strftime("%Y-%m-%d")
        await store.apply_traffic_increments(
            minute_increments={
                ("application", current_minute, "player", "ingress"): 1200,
                ("application", current_minute, "player", "egress"): 800,
                ("application", current_minute, "web_map", "egress"): 400,
                ("wire", current_minute, "player", "ingress"): 900,
                ("wire", current_minute, "player", "egress"): 700,
            },
            hourly_increments={
                ("application", current_hour, "player", "ingress"): 1200,
                ("application", current_hour, "player", "egress"): 800,
                ("application", current_hour, "web_map", "egress"): 400,
                ("wire", current_hour, "player", "ingress"): 900,
                ("wire", current_hour, "player", "egress"): 700,
            },
            daily_increments={
                ("application", current_day, "player", "ingress"): 1200,
                ("application", current_day, "player", "egress"): 800,
                ("application", current_day, "web_map", "egress"): 400,
                ("wire", current_day, "player", "ingress"): 900,
                ("wire", current_day, "player", "egress"): 700,
            },
        )

        hourly = await store.query_hourly_traffic(hours=2)
        daily = await store.query_daily_traffic(days=2)
        history_1m = await store.query_traffic_history(range_preset="1h", granularity="1m")
        history_5m = await store.query_traffic_history(range_preset="6h", granularity="5m")
        wire_hourly = await store.query_hourly_traffic(hours=2, scope="wire")
        wire_history_1m = await store.query_traffic_history(range_preset="1h", granularity="1m", scope="wire")

        assert hourly["items"][-1]["playerIngressBytes"] == 1200
        assert hourly["items"][-1]["totalBytes"] == 2400
        assert hourly["totalIngressBytes"] == 1200
        assert hourly["totalEgressBytes"] == 1200
        assert hourly["items"][0]["totalBytes"] == 0

        assert daily["items"][-1]["totalBytes"] == 2400
        assert daily["totalBytes"] == 2400
        assert history_1m["range"] == "1h"
        assert history_1m["granularity"] == "1m"
        assert history_1m["items"][-1]["totalBytes"] == 2400
        assert history_5m["range"] == "6h"
        assert history_5m["granularity"] == "5m"
        assert any(item["bucket"] == current_5m and item["totalBytes"] == 2400 for item in history_5m["items"])
        assert wire_hourly["items"][-1]["totalBytes"] == 1600
        assert wire_hourly["items"][-1]["webMapEgressBytes"] == 0
        assert wire_history_1m["items"][-1]["totalBytes"] == 1600
    finally:
        await store.close()


@pytest.mark.asyncio
async def test_traffic_history_supports_explicit_start_at(tmp_path: Path) -> None:
    store = AdminStore(AdminStoreConfig(db_path=str(tmp_path / "traffic-start.db")))
    await store.initialize()
    try:
        start_dt = _local_base(11, 7)
        aligned_start = start_dt.replace(minute=5, second=0, microsecond=0)
        bucket_minute = (aligned_start + timedelta(minutes=12)).replace(second=0, microsecond=0)

        await store.apply_traffic_increments(
            minute_increments={
                ("application", bucket_minute.strftime("%Y-%m-%dT%H:%M:00"), "player", "ingress"): 1024,
                ("application", bucket_minute.strftime("%Y-%m-%dT%H:%M:00"), "web_map", "egress"): 256,
            },
            hourly_increments={},
            daily_increments={},
        )

        history = await store.query_traffic_history(
            range_preset="1h",
            granularity="5m",
            start_at=start_dt.strftime("%Y-%m-%dT%H:%M:%S"),
        )

        assert history["startAt"] == aligned_start.strftime("%Y-%m-%dT%H:%M:%S")
        assert history["bucketSeconds"] == 300
        assert history["items"][0]["bucket"] == aligned_start.strftime("%Y-%m-%dT%H:%M:00")
        target_bucket = (aligned_start + timedelta(minutes=10)).strftime("%Y-%m-%dT%H:%M:00")
        matched = next(item for item in history["items"] if item["bucket"] == target_bucket)
        assert matched["playerIngressBytes"] == 1024
        assert matched["webMapEgressBytes"] == 256
        assert history["totalBytes"] == 1280
    finally:
        await store.close()
