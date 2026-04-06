from __future__ import annotations

import asyncio
import json
import os
import secrets
from typing import Any

from fastapi import Request
from fastapi.responses import JSONResponse, PlainTextResponse

from ..app import runtime
from .frontend import admin_ui_ready
from .models import AdminObservabilityPayload
from .payloads import AdminPayloadService
from .proxy_ip import get_request_remote_addr, parse_bool_env
from .store import (
    ALLOWED_TRAFFIC_GRANULARITIES,
    DEFAULT_TRAFFIC_GRANULARITY,
    AdminStore,
    AdminStoreConfig,
)

ADMIN_SESSION_COOKIE_NAME = "teamviewer_admin_session"


def parse_positive_int_env(name: str, default: int, *, minimum: int, maximum: int) -> int:
    raw = os.getenv(name)
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError:
        return default
    return max(minimum, min(value, maximum))


def build_admin_store_config() -> AdminStoreConfig:
    return AdminStoreConfig(
        db_path=os.getenv("TEAMVIEWER_DB_PATH", runtime.DEFAULT_ADMIN_DB_PATH),
        audit_retention_days=parse_positive_int_env("TEAMVIEWER_AUDIT_RETENTION_DAYS", 90, minimum=1, maximum=3650),
        hourly_retention_days=parse_positive_int_env("TEAMVIEWER_HOURLY_RETENTION_DAYS", 90, minimum=1, maximum=3650),
        daily_retention_days=parse_positive_int_env("TEAMVIEWER_DAILY_RETENTION_DAYS", 400, minimum=1, maximum=3650),
    )


def get_admin_session_ttl_sec() -> int:
    return parse_positive_int_env("TEAMVIEWER_ADMIN_SESSION_TTL_SEC", 12 * 60 * 60, minimum=300, maximum=7 * 24 * 60 * 60)


def get_admin_observability_payload() -> AdminObservabilityPayload:
    return {
        "sseSubscribers": runtime.admin_sse_hub.subscriber_count(),
        "lastRetentionCleanup": runtime.admin_runtime_stats["lastRetentionCleanup"],
        "apiErrors": int(runtime.admin_runtime_stats["apiErrors"]),
        "sseErrors": int(runtime.admin_runtime_stats["sseErrors"]),
        "trustProxyHeaders": parse_bool_env("TEAMVIEWER_TRUST_PROXY_HEADERS", False),
    }


def admin_ui_unavailable_response() -> PlainTextResponse:
    return PlainTextResponse(
        "admin_ui_not_built: run `cd admin-ui && pnpm install && pnpm build` first",
        status_code=503,
    )


async def record_audit_event(
    *,
    event_type: str,
    actor_type: str,
    actor_id: str | None = None,
    room_code: str | None = None,
    success: bool = True,
    remote_addr: str | None = None,
    detail: dict | None = None,
    occurred_at: float | None = None,
) -> None:
    if runtime.admin_store is None:
        return
    try:
        await runtime.admin_store.record_audit_event(
            event_type=event_type,
            actor_type=actor_type,
            actor_id=actor_id,
            room_code=room_code,
            success=success,
            remote_addr=remote_addr,
            detail=detail,
            occurred_at=occurred_at,
        )
        trigger_admin_sse_audit()
    except Exception as exc:
        runtime.logger.warning("Failed to persist audit event type=%s: %s", event_type, exc)


async def record_player_activity(
    player_id: str | None,
    room_code: str | None,
    *,
    occurred_at: float | None = None,
) -> None:
    if runtime.admin_store is None or not isinstance(player_id, str) or not player_id:
        return
    try:
        await runtime.admin_store.record_player_activity(
            player_id,
            runtime.state.normalize_room_code(room_code),
            occurred_at=occurred_at,
        )
        trigger_admin_sse_metrics()
    except Exception as exc:
        runtime.logger.warning("Failed to persist player activity playerId=%s: %s", player_id, exc)


def admin_unauthorized_response(detail: str = "admin_session_required") -> JSONResponse:
    return JSONResponse({"detail": detail}, status_code=401)


def parse_login_payload(payload: Any) -> tuple[str | None, str | None]:
    if not isinstance(payload, dict):
        return None, None
    username = payload.get("username")
    password = payload.get("password")
    if not isinstance(username, str) or not isinstance(password, str):
        return None, None
    return username, password


def validate_admin_credentials(username: str | None, password: str | None) -> bool:
    configured_username = os.getenv("TEAMVIEWER_ADMIN_USERNAME", "admin")
    configured_password = os.getenv("TEAMVIEWER_ADMIN_PASSWORD", "admin")
    return (
        isinstance(username, str)
        and isinstance(password, str)
        and bool(configured_username)
        and bool(configured_password)
        and secrets.compare_digest(username, configured_username)
        and secrets.compare_digest(password, configured_password)
    )


def is_admin_configured() -> bool:
    return bool(os.getenv("TEAMVIEWER_ADMIN_USERNAME", "admin")) and bool(os.getenv("TEAMVIEWER_ADMIN_PASSWORD", "admin"))


def _request_uses_secure_transport(request: Request) -> bool:
    if request.url.scheme == "https":
        return True
    if not parse_bool_env("TEAMVIEWER_TRUST_PROXY_HEADERS", False):
        return False
    forwarded_proto = str(request.headers.get("x-forwarded-proto") or "").split(",", 1)[0].strip().lower()
    return forwarded_proto == "https"


def build_session_cookie_settings(request: Request, *, max_age_sec: int) -> dict[str, Any]:
    return {
        "httponly": True,
        "samesite": "lax",
        "secure": _request_uses_secure_transport(request),
        "max_age": max_age_sec,
        "path": "/",
    }


def build_admin_session_payload(session: dict[str, Any]) -> dict[str, Any]:
    return {
        "sessionId": session["sessionId"],
        "actorId": session["actorId"],
        "remoteAddr": session.get("remoteAddr"),
        "createdAt": int(session["createdAt"]),
        "lastSeenAt": int(session["lastSeenAt"]),
        "expiresAt": int(session["expiresAt"]),
    }


async def create_admin_session(request: Request, *, actor_id: str) -> tuple[dict[str, Any], str]:
    if runtime.admin_store is None:
        raise RuntimeError("admin_store_unavailable")
    return await runtime.admin_store.create_admin_session(
        actor_id=actor_id,
        remote_addr=get_request_remote_addr(request),
        ttl_sec=get_admin_session_ttl_sec(),
    )


def extract_admin_session_token(request: Request) -> str | None:
    raw_value = request.cookies.get(ADMIN_SESSION_COOKIE_NAME)
    if isinstance(raw_value, str) and raw_value.strip():
        return raw_value.strip()
    return None


async def authenticate_admin_request(request: Request) -> dict[str, Any] | JSONResponse:
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    raw_token = extract_admin_session_token(request)
    if raw_token is None:
        return admin_unauthorized_response()

    session = await runtime.admin_store.get_admin_session_by_token(raw_token)
    if session is None:
        return admin_unauthorized_response()

    now_ms = runtime.admin_store.timestamp_ms()
    ended_at = session.get("endedAt")
    if ended_at is not None:
        return admin_unauthorized_response("admin_session_ended")

    if int(session["expiresAt"]) <= now_ms:
        ended = await runtime.admin_store.end_admin_session(session["sessionId"], reason="expired")
        if ended is not None:
            await record_audit_event(
                event_type="admin_session_ended",
                actor_type="admin",
                actor_id=ended["actorId"],
                success=True,
                remote_addr=ended.get("remoteAddr"),
                detail={"sessionId": ended["sessionId"], "reason": "expired"},
            )
        return admin_unauthorized_response("admin_session_expired")

    touched = await runtime.admin_store.touch_admin_session(
        session["sessionId"],
        ttl_sec=get_admin_session_ttl_sec(),
    )
    if touched is None:
        return admin_unauthorized_response()
    return touched


async def end_admin_session(
    request: Request,
    session: dict[str, Any],
    *,
    reason: str,
) -> dict[str, Any] | None:
    if runtime.admin_store is None:
        return None
    ended = await runtime.admin_store.end_admin_session(session["sessionId"], reason=reason)
    if ended is None:
        return None
    await record_audit_event(
        event_type="admin_session_ended",
        actor_type="admin",
        actor_id=ended["actorId"],
        success=True,
        remote_addr=get_request_remote_addr(request),
        detail={"sessionId": ended["sessionId"], "reason": reason},
    )
    return ended


async def expire_admin_sessions() -> int:
    if runtime.admin_store is None:
        return 0
    expired = await runtime.admin_store.expire_admin_sessions()
    for session in expired:
        await record_audit_event(
            event_type="admin_session_ended",
            actor_type="admin",
            actor_id=session["actorId"],
            success=True,
            remote_addr=session.get("remoteAddr"),
            detail={"sessionId": session["sessionId"], "reason": "expired"},
        )
    return len(expired)


async def record_admin_access(request: Request, session: dict[str, Any], access_type: str) -> None:
    await record_audit_event(
        event_type=access_type,
        actor_type="admin",
        actor_id=session["actorId"],
        success=True,
        remote_addr=get_request_remote_addr(request),
        detail={
            "sessionId": session["sessionId"],
            "path": request.url.path,
            "method": request.method,
            "query": str(request.url.query or ""),
        },
    )


def normalize_optional_room_code(room_code: str | None) -> str | None:
    if room_code is None:
        return None
    text = str(room_code).strip()
    if not text:
        return None
    return runtime.state.normalize_room_code(text)


def build_room_overview() -> list[dict]:
    room_index: dict[str, dict] = {}

    def ensure_room(room_code: str) -> dict:
        room = room_index.get(room_code)
        if room is None:
            room = {
                "roomCode": room_code,
                "playerConnections": 0,
                "webMapConnections": 0,
                "playerIds": [],
                "webMapIds": [],
            }
            room_index[room_code] = room
        return room

    for player_id in sorted(runtime.state.connections.keys()):
        room_code = runtime.state.get_player_room(player_id)
        room = ensure_room(room_code)
        room["playerConnections"] += 1
        room["playerIds"].append(player_id)

    for web_map_id in sorted(runtime.state.web_map_connections.keys()):
        room_code = runtime.state.get_web_map_room(web_map_id)
        room = ensure_room(room_code)
        room["webMapConnections"] += 1
        room["webMapIds"].append(web_map_id)

    return [room_index[key] for key in sorted(room_index.keys())]


def build_connection_details() -> list[dict]:
    details: list[dict] = []

    for player_id in sorted(runtime.state.connections.keys()):
        room_code = runtime.state.get_player_room(player_id)
        caps = runtime.state.connection_caps.get(player_id, {})
        player_node = runtime.state.players.get(player_id, {})
        player_data = player_node.get("data", {}) if isinstance(player_node, dict) else {}
        if not isinstance(player_data, dict):
            player_data = {}
        player_name = str(player_data.get("playerName") or "").strip()
        display_name = player_name or player_id
        details.append(
            {
                "channel": "player",
                "actorId": player_id,
                "displayName": display_name,
                "roomCode": room_code,
                "protocolVersion": caps.get("protocol"),
                "programVersion": caps.get("programVersion"),
                "remoteAddr": caps.get("remoteAddr"),
            }
        )

    for web_map_id in sorted(runtime.state.web_map_connections.keys()):
        room_code = runtime.state.get_web_map_room(web_map_id)
        meta = runtime.web_map_connection_meta.get(web_map_id, {})
        program_version = meta.get("programVersion")
        display_name = str(meta.get("displayName") or program_version or "Web Map").strip() or "Web Map"
        details.append(
            {
                "channel": "web_map",
                "actorId": web_map_id,
                "displayName": display_name,
                "roomCode": room_code,
                "protocolVersion": meta.get("protocolVersion"),
                "programVersion": program_version,
                "remoteAddr": meta.get("remoteAddr"),
            }
        )

    return details


def ensure_admin_payload_service() -> AdminPayloadService:
    if runtime.admin_payload_service is None:
        raise RuntimeError("admin_store_unavailable")
    return runtime.admin_payload_service


async def build_admin_overview_payload() -> dict:
    return await ensure_admin_payload_service().build_overview_payload()


async def build_admin_daily_metrics_payload(days: int = 30, room_code: str | None = None) -> dict:
    return await ensure_admin_payload_service().build_daily_metrics_payload(days=days, room_code=room_code)


async def build_admin_hourly_metrics_payload(hours: int = 48, room_code: str | None = None) -> dict:
    return await ensure_admin_payload_service().build_hourly_metrics_payload(hours=hours, room_code=room_code)


async def build_admin_live_traffic_payload() -> dict:
    return await ensure_admin_payload_service().build_live_traffic_payload()


async def build_admin_hourly_traffic_payload(hours: int = 48) -> dict:
    return await ensure_admin_payload_service().build_hourly_traffic_payload(hours=hours)


async def build_admin_daily_traffic_payload(days: int = 30) -> dict:
    return await ensure_admin_payload_service().build_daily_traffic_payload(days=days)


def validate_traffic_history_params(range_preset: str, granularity: str) -> tuple[str, str]:
    return AdminStore.normalize_traffic_history_params(range_preset, granularity)


def traffic_range_options() -> tuple[str, ...]:
    return tuple(ALLOWED_TRAFFIC_GRANULARITIES.keys())


def traffic_granularity_options(range_preset: str) -> tuple[str, ...]:
    return ALLOWED_TRAFFIC_GRANULARITIES.get(range_preset, ())


def default_traffic_granularity(range_preset: str) -> str:
    return DEFAULT_TRAFFIC_GRANULARITY.get(range_preset, "1h")


async def build_admin_traffic_history_payload(range_preset: str = "48h", granularity: str = "1h") -> dict:
    normalized_range, normalized_granularity = validate_traffic_history_params(range_preset, granularity)
    return await ensure_admin_payload_service().build_traffic_history_payload(
        range_preset=normalized_range,
        granularity=normalized_granularity,
    )


async def build_admin_audit_payload(
    *,
    limit: int = 100,
    before_id: int | None = None,
    event_type: str | None = None,
    actor_type: str | None = None,
    actor_types: list[str] | tuple[str, ...] | None = None,
    success: bool | None = None,
) -> dict:
    return await ensure_admin_payload_service().build_audit_payload(
        limit=limit,
        before_id=before_id,
        event_type=event_type,
        actor_type=actor_type,
        actor_types=actor_types,
        success=success,
    )


async def build_admin_bootstrap_payload(
    *,
    audit_limit: int = 100,
    audit_event_type: str | None = None,
    audit_actor_types: tuple[str, ...] = (),
    audit_success: bool | None = None,
    daily_days: int = 30,
    daily_room_code: str | None = None,
    hourly_hours: int = 48,
    hourly_room_code: str | None = None,
    traffic_range: str = "48h",
    traffic_granularity: str = "1h",
) -> dict:
    return await ensure_admin_payload_service().build_bootstrap_payload(
        audit_limit=audit_limit,
        audit_event_type=audit_event_type,
        audit_actor_types=audit_actor_types,
        audit_success=audit_success,
        daily_days=daily_days,
        daily_room_code=daily_room_code,
        hourly_hours=hourly_hours,
        hourly_room_code=hourly_room_code,
        traffic_range=traffic_range,
        traffic_granularity=traffic_granularity,
    )


def format_sse_event(event_name: str, payload: dict) -> str:
    return f"event: {event_name}\ndata: {json.dumps(payload, ensure_ascii=False)}\n\n"


def trigger_admin_sse_overview() -> None:
    if runtime.admin_payload_service is not None:
        runtime.admin_payload_service.invalidate("overview")
    asyncio.create_task(runtime.admin_sse_hub.broadcast("overview"))


def trigger_admin_sse_metrics() -> None:
    if runtime.admin_payload_service is not None:
        runtime.admin_payload_service.invalidate("daily_metrics", "hourly_metrics")
    runtime.admin_sse_hub.schedule_broadcast("daily_metrics", delay_sec=1.0)
    runtime.admin_sse_hub.schedule_broadcast("hourly_metrics", delay_sec=1.0)


def trigger_admin_sse_audit() -> None:
    if runtime.admin_payload_service is not None:
        runtime.admin_payload_service.invalidate("audit")
    runtime.admin_sse_hub.schedule_broadcast("audit", delay_sec=1.0)


def admin_ui_is_ready() -> bool:
    return admin_ui_ready()
