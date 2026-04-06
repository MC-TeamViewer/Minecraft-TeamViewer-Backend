from __future__ import annotations

import asyncio
import base64
import importlib
import json
from pathlib import Path
import sys
from types import MethodType

import httpx
import pytest
from starlette.requests import Request

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

import main as main_module


def _auth_headers(username: str = "admin", password: str = "secret") -> dict[str, str]:
    token = base64.b64encode(f"{username}:{password}".encode("utf-8")).decode("ascii")
    return {"Authorization": f"Basic {token}"}


def _load_main_module(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    monkeypatch.setenv("TEAMVIEWER_ADMIN_USERNAME", "admin")
    monkeypatch.setenv("TEAMVIEWER_ADMIN_PASSWORD", "secret")
    monkeypatch.setenv("TEAMVIEWER_DB_PATH", str(tmp_path / "admin-http.db"))
    monkeypatch.setenv("TZ", "Asia/Shanghai")
    return importlib.reload(main_module)


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


def _build_request(path: str, *, authorization: str | None = None) -> Request:
    headers = []
    if authorization is not None:
        headers.append((b"authorization", authorization.encode("utf-8")))
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
        "client": ("127.0.0.1", 12345),
        "server": ("testserver", 80),
    }

    async def receive():
        await asyncio.sleep(3600)
        return {"type": "http.disconnect"}

    request = Request(scope, receive)
    request.is_disconnected = MethodType(lambda self: asyncio.sleep(0, result=False), request)
    return request


@pytest.mark.asyncio
async def test_admin_http_requires_basic_auth(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            page = await client.get("/admin")
            overview = await client.get("/admin/api/overview")

    assert page.status_code == 401
    assert page.headers["www-authenticate"].startswith("Basic")
    assert overview.status_code == 401

    async with main.app.router.lifespan_context(main.app):
        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            events = await client.get("/admin/api/events")

    assert events.status_code == 401


@pytest.mark.asyncio
async def test_admin_http_exposes_dashboard_metrics_and_audit(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        await main.admin_store.record_player_activity("player-1", "room-admin-test")
        await main.record_audit_event(
            event_type="player_handshake_success",
            actor_type="player",
            actor_id="player-1",
            room_code="room-admin-test",
            success=True,
            detail={"clientProtocol": "0.6.1"},
        )
        await main.record_audit_event(
            event_type="web_map_handshake_success",
            actor_type="web_map",
            actor_id="web-map-1",
            room_code="room-admin-test",
            success=True,
            detail={"clientProtocol": "0.6.1"},
        )

        main.state.connections["player-1"] = object()  # type: ignore[assignment]
        main.state.set_player_room("player-1", "room-admin-test")
        main.state.connection_caps["player-1"] = {
            "protocol": "0.6.1",
            "programVersion": "test-player-client",
            "remoteAddr": "127.0.0.1",
        }
        main.state.players["player-1"] = {
            "data": {
                "playerName": "Alice",
            }
        }
        main.state.web_map_connections["web-map-1"] = object()  # type: ignore[assignment]
        main.state.set_web_map_room("web-map-1", "room-admin-test")
        main.web_map_connection_meta["web-map-1"] = {
            "protocolVersion": "0.6.1",
            "programVersion": "squaremap-script",
            "displayName": "Web Map",
            "remoteAddr": "127.0.0.2",
        }

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            page = await client.get("/admin", headers=_auth_headers())
            overview = await client.get("/admin/api/overview", headers=_auth_headers())
            daily = await client.get("/admin/api/metrics/daily?days=2", headers=_auth_headers())
            hourly = await client.get("/admin/api/metrics/hourly?hours=3", headers=_auth_headers())
            audit = await client.get("/admin/api/audit?limit=200&success=true", headers=_auth_headers())

    overview_payload = overview.json()
    daily_payload = daily.json()
    hourly_payload = hourly.json()
    audit_payload = audit.json()
    audit_types = {item["eventType"] for item in audit_payload["items"]}

    assert page.status_code == 200
    assert "TeamViewRelay Admin" in page.text

    assert overview.status_code == 200
    assert overview_payload["playerConnections"] == 1
    assert overview_payload["webMapConnections"] == 1
    assert overview_payload["activeRooms"] == 1
    assert overview_payload["rooms"][0]["roomCode"] == "room-admin-test"
    assert len(overview_payload["connectionDetails"]) == 2
    assert any(item["displayName"] == "Alice" for item in overview_payload["connectionDetails"])
    assert any(item["programVersion"] == "squaremap-script" for item in overview_payload["connectionDetails"])
    assert overview_payload["dbPathMasked"].endswith("/admin-http.db")

    assert daily_payload["items"][-1]["activePlayers"] == 1
    assert hourly_payload["items"][-1]["activePlayers"] == 1
    assert "UTC" in hourly_payload["timezone"]

    assert audit.status_code == 200
    assert "player_handshake_success" in audit_types
    assert "web_map_handshake_success" in audit_types
    assert "admin_api_access" in audit_types


@pytest.mark.asyncio
async def test_admin_audit_supports_multi_actor_type_filter(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        await main.record_audit_event(
            event_type="player_handshake_success",
            actor_type="player",
            actor_id="player-1",
            success=True,
        )
        await main.record_audit_event(
            event_type="admin_auth_success",
            actor_type="admin",
            actor_id="admin",
            success=True,
        )
        await main.record_audit_event(
            event_type="backend_error",
            actor_type="system",
            actor_id="system",
            success=False,
        )

        transport = httpx.ASGITransport(app=main.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://testserver") as client:
            audit = await client.get(
                "/admin/api/audit?actorTypes=player&actorTypes=system",
                headers=_auth_headers(),
            )

    audit_payload = audit.json()
    actor_types = {item["actorType"] for item in audit_payload["items"]}
    assert audit.status_code == 200
    assert actor_types.issubset({"player", "system"})
    assert "admin" not in actor_types


@pytest.mark.asyncio
async def test_admin_sse_stream_emits_bootstrap_and_followup_events(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    main = _load_main_module(monkeypatch, tmp_path)

    async with main.app.router.lifespan_context(main.app):
        await main.admin_store.record_player_activity("player-1", "room-admin-test")
        request = _build_request("/admin/api/events", authorization=_auth_headers()["Authorization"])
        response = await main.admin_events(
            request,
            auditLimit=100,
            auditEventType=None,
            auditActorType=None,
            auditActorTypes=None,
            auditSuccess=None,
        )
        assert response.status_code == 200
        assert response.media_type == "text/event-stream"

        iterator = response.body_iterator
        lines = _byte_lines(iterator)

        bootstrap_name, bootstrap_payload = await _read_sse_event(lines, expected_names={"bootstrap"})
        assert bootstrap_name == "bootstrap"
        assert bootstrap_payload["overview"]["playerConnections"] == 0
        assert bootstrap_payload["dailyMetrics"]["items"]
        assert bootstrap_payload["hourlyMetrics"]["items"]
        assert isinstance(bootstrap_payload["audit"]["items"], list)

        main.state.connections["player-2"] = object()  # type: ignore[assignment]
        main.state.set_player_room("player-2", "room-admin-test")
        main.trigger_admin_sse_overview()

        overview_name, overview_payload = await _read_sse_event(lines, expected_names={"overview"})
        assert overview_name == "overview"
        assert overview_payload["playerConnections"] == 1

        await main.record_player_activity("player-2", "room-admin-test")
        metric_events = {
            (await _read_sse_event(lines, expected_names={"daily_metrics", "hourly_metrics"}))[0],
            (await _read_sse_event(lines, expected_names={"daily_metrics", "hourly_metrics"}))[0],
        }
        assert metric_events == {"daily_metrics", "hourly_metrics"}

        await main.record_audit_event(
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
