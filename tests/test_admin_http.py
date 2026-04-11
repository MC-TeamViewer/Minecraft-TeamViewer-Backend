from __future__ import annotations

import asyncio
from datetime import timedelta
import importlib
import json
from pathlib import Path
import re
import sys
from types import MethodType, SimpleNamespace

import httpx
import pytest
from starlette.requests import Request

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

import main as main_module
from server.admin import auth as admin_auth
from server.admin import routes as admin_routes
from server.admin.proxy_ip import get_websocket_remote_addr
from server.admin.traffic import record_websocket_traffic
from server.app import runtime as app_runtime


def _load_main_module(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    monkeypatch.setenv("TEAMVIEWER_ADMIN_USERNAME", "admin")
    monkeypatch.setenv("TEAMVIEWER_ADMIN_PASSWORD", "secret")
    monkeypatch.setenv("TEAMVIEWER_DB_PATH", str(tmp_path / "admin-http.db"))
    monkeypatch.setenv("TZ", "Asia/Shanghai")
    return importlib.reload(main_module)


class _ConnectedWebSocketStub(SimpleNamespace):
    async def send_bytes(self, _payload: bytes) -> None:
        return None


def _connected_websocket_stub():
    connected_state = SimpleNamespace(name="CONNECTED")
    return _ConnectedWebSocketStub(
        client_state=connected_state,
        application_state=connected_state,
        close_code=None,
        close_reason=None,
    )


async def _read_sse_event(lines, *, expected_names: set[str], timeout_sec: float = 4.0) -> tuple[str, dict]:
    deadline = asyncio.get_running_loop().time() + timeout_sec
    while True:
        remaining = max(0.1, deadline - asyncio.get_running_loop().time())
        event_name = None
        data_lines: list[str] = []
        while True:
            line = await asyncio.wait_for(anext(lines), timeout=remaining)
            if line == "":
                if event_name and data_lines:
                    payload = json.loads("\n".join(data_lines))
                    if event_name in expected_names:
                        return event_name, payload
                    break
                continue
            if line.startswith("event:"):
                event_name = line.split(":", 1)[1].strip()
            elif line.startswith("data:"):
                data_lines.append(line.split(":", 1)[1].lstrip())


async def _byte_lines(iterator):
    buffer = ""
    async for chunk in iterator:
        text = chunk.decode("utf-8") if isinstance(chunk, bytes) else str(chunk)
        buffer += text
        while "\n" in buffer:
            line, buffer = buffer.split("\n", 1)
            yield line.rstrip("\r")


def _build_request(
    path: str,
    *,
    cookie: str | None = None,
    extra_headers: dict[str, str] | None = None,
    client_host: str = "127.0.0.1",
) -> Request:
    headers = []
    if cookie is not None:
        headers.append((b"cookie", cookie.encode("utf-8")))
    for key, value in (extra_headers or {}).items():
        headers.append((key.lower().encode("utf-8"), value.encode("utf-8")))
    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": "GET",
        "scheme": "http",
        "path": path,
        "raw_path": path.encode("utf-8"),
        "query_string": b"",
        "headers": headers,
        "client": (client_host, 12345),
        "server": ("testserver", 80),
    }

    async def receive():
        await asyncio.sleep(3600)
        return {"type": "http.disconnect"}

    request = Request(scope, receive)
    request.is_disconnected = MethodType(lambda self: asyncio.sleep(0, result=False), request)
    return request


def _make_websocket_stub(*, host: str, headers: dict[str, str] | None = None):
    connected_state = SimpleNamespace(name="CONNECTED")
    return SimpleNamespace(
        client=SimpleNamespace(host=host),
        headers=headers or {},
        client_state=connected_state,
        application_state=connected_state,
        close_code=None,
        close_reason=None,
    )


async def _login(client: httpx.AsyncClient, username: str = "admin", password: str = "secret") -> httpx.Response:
    return await client.post(
        "/admin/api/session/login",
        json={"username": username, "password": password},
    )


@pytest.mark.asyncio
async def test_admin_page_is_public_but_api_requires_session(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            page = await client.get("/admin")
            overview = await client.get("/admin/api/overview")
            events = await client.get("/admin/api/events")
            assert page.status_code == 200, page.text
            assert "TeamViewRelay Admin" in page.text
            asset_match = re.search(r"/admin/assets/[^\"']+", page.text)
            assert asset_match is not None
            asset_response = await client.get(asset_match.group(0))

    assert asset_response.status_code == 200
    assert overview.status_code == 401
    assert events.status_code == 401


@pytest.mark.asyncio
async def test_admin_session_login_logout_and_audit(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            failed = await _login(client, password="wrong")
            assert failed.status_code == 401

            login = await _login(client)
            assert login.status_code == 200
            assert admin_auth.ADMIN_SESSION_COOKIE_NAME in login.headers.get("set-cookie", "")

            session_info = await client.get("/admin/api/session")
            assert session_info.status_code == 200
            assert session_info.json()["actorId"] == "admin"

            overview = await client.get("/admin/api/overview")
            assert overview.status_code == 200

            logout = await client.post("/admin/api/session/logout")
            assert logout.status_code == 200

            after_logout = await client.get("/admin/api/session")
            assert after_logout.status_code == 401

        audit_payload = await admin_auth.build_admin_audit_payload(limit=50)
        audit_types = [item["eventType"] for item in audit_payload["items"]]
        assert "admin_session_started" in audit_types
        assert "admin_session_ended" in audit_types
        assert "admin_api_access" in audit_types
        assert "admin_auth_failed" in audit_types
        assert "admin_auth_success" not in audit_types


@pytest.mark.asyncio
async def test_admin_http_exposes_dashboard_metrics_audit_and_traffic(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        await app_runtime.admin_store.record_player_activity("player-1", "room-admin-test")
        await app_runtime.admin_store.upsert_player_identity("player-1", "Alice")
        await admin_auth.record_audit_event(
            event_type="player_handshake_success",
            actor_type="player",
            actor_id="player-1",
            room_code="room-admin-test",
            success=True,
            detail={"clientProtocol": "0.6.1"},
        )
        await admin_auth.record_audit_event(
            event_type="web_map_handshake_success",
            actor_type="web_map",
            actor_id="web-map-1",
            room_code="room-admin-test",
            success=True,
            detail={"clientProtocol": "0.6.1"},
        )
        await app_runtime.admin_traffic_service.record(channel="player", direction="ingress", byte_count=2048)
        await app_runtime.admin_traffic_service.record(channel="web_map", direction="egress", byte_count=1024)
        await app_runtime.admin_traffic_service.flush_pending()
        current_local = app_runtime.admin_store.local_datetime().replace(second=0, microsecond=0)
        current_minute = current_local.strftime("%Y-%m-%dT%H:%M:00")
        current_hour = current_local.replace(minute=0).strftime("%Y-%m-%dT%H:00:00")
        current_day = current_local.strftime("%Y-%m-%d")
        await app_runtime.admin_store.apply_traffic_increments(
            minute_increments={
                ("application", current_minute, "player", "ingress"): 2048,
                ("application", current_minute, "web_map", "egress"): 1024,
            },
            hourly_increments={
                ("application", current_hour, "player", "ingress"): 2048,
                ("application", current_hour, "web_map", "egress"): 1024,
            },
            daily_increments={
                ("application", current_day, "player", "ingress"): 2048,
                ("application", current_day, "web_map", "egress"): 1024,
            },
        )

        app_runtime.state.connections["player-1"] = _connected_websocket_stub()  # type: ignore[assignment]
        app_runtime.state.set_player_room("player-1", "room-admin-test")
        app_runtime.state.connection_caps["player-1"] = {
            "protocol": "0.6.1",
            "programVersion": "test-player-client",
            "remoteAddr": "127.0.0.1",
        }
        app_runtime.state.players["player-1"] = {"data": {"playerName": "Alice"}}
        app_runtime.state.web_map_connections["web-map-1"] = _connected_websocket_stub()  # type: ignore[assignment]
        app_runtime.state.set_web_map_room("web-map-1", "room-admin-test")
        app_runtime.web_map_connection_meta["web-map-1"] = {
            "protocolVersion": "0.6.1",
            "programVersion": "squaremap-script",
            "displayName": "Web Map",
            "remoteAddr": "127.0.0.2",
        }

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            await _login(client)
            page = await client.get("/admin")
            overview = await client.get("/admin/api/overview")
            daily = await client.get("/admin/api/metrics/daily?days=2")
            hourly = await client.get("/admin/api/metrics/hourly?hours=3")
            live_traffic = await client.get("/admin/api/traffic/live")
            history_traffic = await client.get("/admin/api/traffic/history?range=6h&granularity=5m")
            hourly_traffic = await client.get("/admin/api/traffic/hourly?hours=2")
            daily_traffic = await client.get("/admin/api/traffic/daily?days=2")
            audit = await client.get("/admin/api/audit?limit=200&success=true")
            assert page.status_code == 200, page.text
            assert "TeamViewRelay Admin" in page.text
            asset_match = re.search(r"/admin/assets/[^\"']+", page.text)
            assert asset_match is not None
            asset_response = await client.get(asset_match.group(0))

    overview_payload = overview.json()
    daily_payload = daily.json()
    hourly_payload = hourly.json()
    live_traffic_payload = live_traffic.json()
    history_traffic_payload = history_traffic.json()
    hourly_traffic_payload = hourly_traffic.json()
    daily_traffic_payload = daily_traffic.json()
    audit_payload = audit.json()
    audit_types = {item["eventType"] for item in audit_payload["items"]}

    assert asset_response.status_code == 200

    assert overview.status_code == 200
    assert overview_payload["playerConnections"] == 1
    assert overview_payload["webMapConnections"] == 1
    assert overview_payload["activeRooms"] == 1
    assert overview_payload["rooms"][0]["roomCode"] == "room-admin-test"
    assert len(overview_payload["connectionDetails"]) == 2
    assert any(item["actorId"] == "player-1" for item in overview_payload["connectionDetails"])
    assert any(item["programVersion"] == "squaremap-script" for item in overview_payload["connectionDetails"])

    assert daily_payload["items"][-1]["activePlayers"] == 1
    assert hourly_payload["items"][-1]["activePlayers"] == 1
    assert "UTC" in hourly_payload["timezone"]

    assert live_traffic_payload["selectedLayer"] == "application"
    assert live_traffic_payload["application"]["totalIngressBps"] > 0
    assert live_traffic_payload["application"]["totalEgressBps"] > 0
    assert live_traffic_payload["wire"]["totalIngressBps"] == 0
    assert history_traffic_payload["range"] == "6h"
    assert history_traffic_payload["granularity"] == "5m"
    assert history_traffic_payload["selectedLayer"] == "application"
    assert history_traffic_payload["application"]["totalBytes"] >= 3072
    assert history_traffic_payload["wire"]["totalBytes"] == 0
    assert hourly_traffic_payload["totalBytes"] >= 3072
    assert daily_traffic_payload["totalBytes"] >= 3072

    assert audit.status_code == 200
    assert "player_handshake_success" in audit_types
    assert "web_map_handshake_success" in audit_types
    assert "admin_api_access" in audit_types
    assert "admin_session_started" in audit_types
    player_audit = next(item for item in audit_payload["items"] if item["actorType"] == "player" and item["actorId"] == "player-1")
    assert player_audit["resolvedActorName"] == "Alice"
    assert any(item["playerId"] == "player-1" and item["username"] == "Alice" for item in audit_payload["playerIdentityMappings"])


@pytest.mark.asyncio
async def test_admin_traffic_history_exposes_distinct_application_and_wire_layers(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        now_local = app_runtime.admin_store.local_datetime().replace(second=0, microsecond=0)
        current_minute = now_local.strftime("%Y-%m-%dT%H:%M:00")
        current_hour = now_local.replace(minute=0).strftime("%Y-%m-%dT%H:00:00")
        current_day = now_local.strftime("%Y-%m-%d")
        await app_runtime.admin_store.apply_traffic_increments(
            minute_increments={
                ("application", current_minute, "player", "ingress"): 5000,
                ("wire", current_minute, "player", "ingress"): 2300,
            },
            hourly_increments={
                ("application", current_hour, "player", "ingress"): 5000,
                ("wire", current_hour, "player", "ingress"): 2300,
            },
            daily_increments={
                ("application", current_day, "player", "ingress"): 5000,
                ("wire", current_day, "player", "ingress"): 2300,
            },
        )

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            await _login(client)
            history_traffic = await client.get("/admin/api/traffic/history?range=1h&granularity=1m")

    payload = history_traffic.json()

    assert history_traffic.status_code == 200
    assert payload["application"]["items"][-1]["playerIngressBytes"] == 5000
    assert payload["wire"]["items"][-1]["playerIngressBytes"] == 2300
    assert payload["application"]["totalBytes"] == 5000
    assert payload["wire"]["totalBytes"] == 2300
    assert payload["application"]["items"] != payload["wire"]["items"]


@pytest.mark.asyncio
async def test_admin_traffic_http_totals_include_previous_buckets_when_latest_bucket_is_empty(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        now_local = app_runtime.admin_store.local_datetime().replace(second=0, microsecond=0)
        current_5m = now_local.replace(minute=(now_local.minute // 5) * 5)
        previous_5m = current_5m - timedelta(minutes=5)
        current_hour = now_local.replace(minute=0)
        previous_hour = current_hour - timedelta(hours=1)
        current_day = now_local.strftime("%Y-%m-%d")

        await app_runtime.admin_store.apply_traffic_increments(
            minute_increments={
                ("application", previous_5m.strftime("%Y-%m-%dT%H:%M:00"), "player", "ingress"): 2048,
                ("application", previous_5m.strftime("%Y-%m-%dT%H:%M:00"), "web_map", "egress"): 1024,
            },
            hourly_increments={
                ("application", previous_hour.strftime("%Y-%m-%dT%H:00:00"), "player", "ingress"): 2048,
                ("application", previous_hour.strftime("%Y-%m-%dT%H:00:00"), "web_map", "egress"): 1024,
            },
            daily_increments={
                ("application", current_day, "player", "ingress"): 2048,
                ("application", current_day, "web_map", "egress"): 1024,
            },
        )

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            await _login(client)
            history_traffic = await client.get("/admin/api/traffic/history?range=6h&granularity=5m")
            hourly_traffic = await client.get("/admin/api/traffic/hourly?hours=2")

    history_payload = history_traffic.json()
    hourly_payload = hourly_traffic.json()

    assert history_traffic.status_code == 200
    assert history_payload["application"]["totalBytes"] == 3072
    assert history_payload["application"]["items"][-1]["totalBytes"] == 0
    assert any(
        item["bucket"] == previous_5m.strftime("%Y-%m-%dT%H:%M:00") and item["totalBytes"] == 3072
        for item in history_payload["application"]["items"]
    )

    assert hourly_traffic.status_code == 200
    assert hourly_payload["totalBytes"] == 3072
    assert hourly_payload["items"][-1]["totalBytes"] == 0
    assert any(
        item["bucket"] == previous_hour.strftime("%Y-%m-%dT%H:00:00") and item["totalBytes"] == 3072
        for item in hourly_payload["items"]
    )


@pytest.mark.asyncio
async def test_admin_audit_supports_multi_actor_type_filter(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        await admin_auth.record_audit_event(
            event_type="player_handshake_success",
            actor_type="player",
            actor_id="player-1",
            success=True,
        )
        await admin_auth.record_audit_event(
            event_type="admin_session_started",
            actor_type="admin",
            actor_id="admin",
            success=True,
        )
        await admin_auth.record_audit_event(
            event_type="backend_error",
            actor_type="system",
            actor_id="system",
            success=False,
        )

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            await _login(client)
            audit = await client.get("/admin/api/audit?actorTypes=player&actorTypes=system")

    audit_payload = audit.json()
    actor_types = {item["actorType"] for item in audit_payload["items"]}
    assert audit.status_code == 200
    assert actor_types.issubset({"player", "system"})
    assert "admin" not in actor_types


@pytest.mark.asyncio
async def test_admin_traffic_history_rejects_invalid_granularity(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            await _login(client)
            response = await client.get("/admin/api/traffic/history?range=30d&granularity=1h")

    assert response.status_code == 422
    assert response.json()["detail"] == "invalid_traffic_granularity"


@pytest.mark.asyncio
async def test_admin_metrics_and_traffic_history_support_explicit_starts(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        base = app_runtime.admin_store.local_datetime().replace(hour=8, minute=37, second=0, microsecond=0)
        daily_start = (base - timedelta(days=2)).strftime("%Y-%m-%d")
        hourly_start = base.strftime("%Y-%m-%dT%H:%M:%S")
        traffic_start = base.replace(minute=7).strftime("%Y-%m-%dT%H:%M:%S")
        traffic_bucket = base.replace(minute=17).strftime("%Y-%m-%dT%H:%M:00")

        await app_runtime.admin_store.record_player_activity("player-1", "room-started", occurred_at=base.timestamp())
        await app_runtime.admin_store.record_player_activity(
            "player-2",
            "room-started",
            occurred_at=(base + timedelta(hours=1)).timestamp(),
        )
        await app_runtime.admin_store.apply_traffic_increments(
            minute_increments={
                ("application", traffic_bucket, "player", "ingress"): 4096,
                ("wire", traffic_bucket, "player", "ingress"): 2048,
            },
            hourly_increments={},
            daily_increments={},
        )

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            await _login(client)
            daily = await client.get(f"/admin/api/metrics/daily?days=4&roomCode=room-started&startDate={daily_start}")
            hourly = await client.get(f"/admin/api/metrics/hourly?hours=4&roomCode=room-started&startAt={hourly_start}")
            traffic = await client.get(f"/admin/api/traffic/history?range=1h&granularity=5m&startAt={traffic_start}")

    daily_payload = daily.json()
    hourly_payload = hourly.json()
    traffic_payload = traffic.json()

    assert daily.status_code == 200
    assert daily_payload["startDate"] == daily_start
    assert daily_payload["items"][2]["activePlayers"] == 2

    assert hourly.status_code == 200
    assert hourly_payload["startAt"] == base.replace(minute=0, second=0, microsecond=0).strftime("%Y-%m-%dT%H:00:00")
    assert hourly_payload["items"][0]["activePlayers"] == 1
    assert hourly_payload["items"][1]["activePlayers"] == 1

    assert traffic.status_code == 200
    assert traffic_payload["startAt"] == base.replace(minute=5, second=0, microsecond=0).strftime("%Y-%m-%dT%H:%M:%S")
    assert traffic_payload["application"]["totalBytes"] == 4096
    assert traffic_payload["wire"]["totalBytes"] == 2048


@pytest.mark.asyncio
async def test_admin_sse_stream_emits_bootstrap_and_followup_events(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        session, raw_token = await app_runtime.admin_store.create_admin_session(
            actor_id="admin",
            remote_addr="127.0.0.1",
            ttl_sec=3600,
        )
        await admin_auth.record_audit_event(
            event_type="admin_session_started",
            actor_type="admin",
            actor_id=session["actorId"],
            success=True,
            detail={"sessionId": session["sessionId"]},
        )
        await app_runtime.admin_store.record_player_activity("player-1", "room-admin-test")
        await app_runtime.admin_traffic_service.record(channel="player", direction="ingress", byte_count=1024)
        await app_runtime.admin_traffic_service.flush_pending()
        await app_runtime.admin_store.upsert_player_identity("player-2", "Bob")
        current_local = app_runtime.admin_store.local_datetime().replace(second=0, microsecond=0)
        daily_start = (current_local - timedelta(days=2)).strftime("%Y-%m-%d")
        hourly_start = current_local.replace(minute=0).strftime("%Y-%m-%dT%H:%M:%S")
        traffic_start = current_local.replace(minute=(current_local.minute // 5) * 5).strftime("%Y-%m-%dT%H:%M:%S")

        request = _build_request(
            "/admin/api/events",
            cookie=f"{admin_auth.ADMIN_SESSION_COOKIE_NAME}={raw_token}",
        )
        response = await admin_routes.admin_events(
            request,
            auditLimit=100,
            auditEventType=None,
            auditActorType=None,
            auditActorTypes=None,
            auditSuccess=None,
            dailyDays=30,
            dailyStartDate=daily_start,
            dailyRoomCode=None,
            hourlyHours=48,
            hourlyStartAt=hourly_start,
            hourlyRoomCode=None,
            trafficRange="6h",
            trafficGranularity="5m",
            trafficStartAt=traffic_start,
        )
        assert response.status_code == 200
        assert response.media_type == "text/event-stream"

        iterator = response.body_iterator
        lines = _byte_lines(iterator)

        bootstrap_name, bootstrap_payload = await _read_sse_event(lines, expected_names={"bootstrap"})
        assert bootstrap_name == "bootstrap"
        assert bootstrap_payload["overview"]["playerConnections"] == 0
        assert bootstrap_payload["liveTraffic"]["application"]["totalIngressBps"] > 0
        assert bootstrap_payload["liveTraffic"]["wire"]["totalIngressBps"] == 0
        assert bootstrap_payload["trafficHistory"]["application"]["items"]
        assert bootstrap_payload["trafficHistory"]["wire"]["items"]
        assert bootstrap_payload["trafficHistory"]["range"] == "6h"
        assert bootstrap_payload["trafficHistory"]["granularity"] == "5m"
        assert bootstrap_payload["dailyMetrics"]["startDate"] == daily_start
        assert bootstrap_payload["hourlyMetrics"]["startAt"] == hourly_start
        assert bootstrap_payload["trafficHistory"]["startAt"] == traffic_start
        assert "availableEventTypes" in bootstrap_payload["audit"]
        assert any(item["playerId"] == "player-2" and item["username"] == "Bob" for item in bootstrap_payload["audit"]["playerIdentityMappings"])

        app_runtime.state.connections["player-2"] = _connected_websocket_stub()  # type: ignore[assignment]
        app_runtime.state.set_player_room("player-2", "room-admin-test")
        admin_auth.trigger_admin_sse_overview()

        overview_name, overview_payload = await _read_sse_event(lines, expected_names={"overview"})
        assert overview_name == "overview"
        assert overview_payload["playerConnections"] == 1

        await admin_auth.record_player_activity("player-2", "room-admin-test")
        metric_payloads = {}
        for _ in range(2):
            event_name, payload = await _read_sse_event(lines, expected_names={"daily_metrics", "hourly_metrics"})
            metric_payloads[event_name] = payload
        assert set(metric_payloads) == {"daily_metrics", "hourly_metrics"}
        assert metric_payloads["daily_metrics"]["startDate"] == daily_start
        assert metric_payloads["hourly_metrics"]["startAt"] == hourly_start

        await record_websocket_traffic(channel="web_map", direction="egress", byte_count=2048)
        await app_runtime.admin_traffic_service.flush_pending()
        traffic_payloads = {}
        for _ in range(2):
            event_name, payload = await _read_sse_event(
                lines,
                expected_names={"traffic_live", "traffic_history"},
                timeout_sec=8.0,
            )
            traffic_payloads[event_name] = payload
        assert set(traffic_payloads) == {"traffic_live", "traffic_history"}
        assert traffic_payloads["traffic_history"]["startAt"] == traffic_start

        await admin_auth.record_audit_event(
            event_type="player_disconnected",
            actor_type="player",
            actor_id="player-2",
            room_code="room-admin-test",
            success=True,
        )
        audit_name, audit_payload = await _read_sse_event(lines, expected_names={"audit"})
        assert audit_name == "audit"
        assert any(item["eventType"] == "player_disconnected" for item in audit_payload["items"])

        await iterator.aclose()


@pytest.mark.asyncio
async def test_expired_session_is_rejected_and_records_session_end(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        session, raw_token = await app_runtime.admin_store.create_admin_session(
            actor_id="admin",
            remote_addr="127.0.0.1",
            ttl_sec=3600,
        )
        app_runtime.admin_store._execute(  # noqa: SLF001
            "UPDATE admin_sessions SET expires_at = 1 WHERE session_id = ?",
            (session["sessionId"],),
        )

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            client.cookies.set(admin_auth.ADMIN_SESSION_COOKIE_NAME, raw_token)
            current = await client.get("/admin/api/session")

        audit_payload = await admin_auth.build_admin_audit_payload(limit=20, event_type="admin_session_ended")

    assert current.status_code == 401
    assert audit_payload["items"][0]["detail"]["reason"] == "expired"


@pytest.mark.asyncio
async def test_trusted_proxy_headers_update_admin_session_remote_addr(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("TEAMVIEWER_TRUST_PROXY_HEADERS", "true")
    monkeypatch.setenv("TEAMVIEWER_TRUSTED_PROXY_CIDRS", "127.0.0.1/32")
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            login = await client.post(
                "/admin/api/session/login",
                json={"username": "admin", "password": "secret"},
                headers={"x-forwarded-for": "203.0.113.10, 127.0.0.1"},
            )
            assert login.status_code == 200
        audit_payload = await admin_auth.build_admin_audit_payload(limit=20, event_type="admin_session_started")

    assert audit_payload["items"][0]["remoteAddr"] == "203.0.113.10"

    websocket = _make_websocket_stub(
        host="127.0.0.1",
        headers={"x-forwarded-for": "198.51.100.20, 127.0.0.1"},
    )
    assert get_websocket_remote_addr(websocket) == "198.51.100.20"


@pytest.mark.asyncio
async def test_untrusted_proxy_headers_are_ignored(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setenv("TEAMVIEWER_TRUST_PROXY_HEADERS", "true")
    monkeypatch.setenv("TEAMVIEWER_TRUSTED_PROXY_CIDRS", "10.0.0.0/8")
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            login = await client.post(
                "/admin/api/session/login",
                json={"username": "admin", "password": "secret"},
                headers={"x-forwarded-for": "203.0.113.10"},
            )
            assert login.status_code == 200
        audit_payload = await admin_auth.build_admin_audit_payload(limit=20, event_type="admin_session_started")

    assert audit_payload["items"][0]["remoteAddr"] == "127.0.0.1"
