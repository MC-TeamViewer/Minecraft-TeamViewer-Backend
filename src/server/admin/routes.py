import asyncio
import time
from pathlib import Path

from fastapi import Query, Request
from fastapi.responses import FileResponse, JSONResponse, StreamingResponse

from ..app import runtime
from .store import AdminStore
from .auth import (
    ADMIN_SESSION_COOKIE_NAME,
    admin_ui_unavailable_response,
    admin_unauthorized_response,
    authenticate_admin_request,
    build_admin_audit_payload,
    build_admin_bootstrap_payload,
    build_admin_daily_metrics_payload,
    build_admin_daily_traffic_payload,
    build_admin_hourly_metrics_payload,
    build_admin_hourly_traffic_payload,
    build_admin_live_traffic_payload,
    build_admin_overview_payload,
    build_admin_traffic_history_payload,
    build_admin_session_payload,
    build_session_cookie_settings,
    create_admin_session,
    default_traffic_granularity,
    end_admin_session,
    format_sse_event,
    get_admin_session_ttl_sec,
    is_admin_configured,
    normalize_optional_room_code,
    parse_login_payload,
    record_admin_access,
    record_audit_event,
    validate_traffic_history_params,
    validate_admin_credentials,
)
from .frontend import ADMIN_UI_INDEX_PATH, resolve_admin_asset_path
from .proxy_ip import get_request_remote_addr


async def admin_page(_request: Request):
    if not Path(ADMIN_UI_INDEX_PATH).exists():
        return admin_ui_unavailable_response()
    return FileResponse(ADMIN_UI_INDEX_PATH, media_type="text/html")


async def admin_assets(_request: Request, asset_path: str):
    asset_file = resolve_admin_asset_path(asset_path)
    if asset_file is None:
        if not Path(ADMIN_UI_INDEX_PATH).exists():
            return admin_ui_unavailable_response()
        return JSONResponse({"detail": "not_found"}, status_code=404)

    return FileResponse(Path(asset_file))


def _normalize_daily_start_date_or_response(value: str | None) -> tuple[str | None, None] | tuple[None, JSONResponse]:
    try:
        return AdminStore.normalize_local_date(value), None
    except ValueError:
        return None, JSONResponse({"detail": "invalid_start_date"}, status_code=422)


def _normalize_local_start_at_or_response(value: str | None) -> tuple[str | None, None] | tuple[None, JSONResponse]:
    try:
        return AdminStore.normalize_local_datetime(value), None
    except ValueError:
        return None, JSONResponse({"detail": "invalid_start_at"}, status_code=422)


async def admin_session_login(request: Request):
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    if not is_admin_configured():
        await record_audit_event(
            event_type="admin_auth_failed",
            actor_type="admin",
            success=False,
            remote_addr=get_request_remote_addr(request),
            detail={"reason": "admin_not_configured", "path": request.url.path},
        )
        return JSONResponse({"detail": "admin_not_configured"}, status_code=503)

    try:
        payload = await request.json()
    except Exception:
        payload = None
    username, password = parse_login_payload(payload)
    remote_addr = get_request_remote_addr(request)
    if not validate_admin_credentials(username, password):
        await record_audit_event(
            event_type="admin_auth_failed",
            actor_type="admin",
            actor_id=username,
            success=False,
            remote_addr=remote_addr,
            detail={"reason": "invalid_credentials", "path": request.url.path},
        )
        return admin_unauthorized_response("invalid_admin_credentials")

    session, raw_token = await create_admin_session(request, actor_id=str(username))
    await record_audit_event(
        event_type="admin_session_started",
        actor_type="admin",
        actor_id=session["actorId"],
        success=True,
        remote_addr=session.get("remoteAddr"),
        detail={"sessionId": session["sessionId"], "path": request.url.path},
    )
    response = JSONResponse(build_admin_session_payload(session))
    response.set_cookie(
        ADMIN_SESSION_COOKIE_NAME,
        raw_token,
        **build_session_cookie_settings(request, max_age_sec=get_admin_session_ttl_sec()),
    )
    return response


async def admin_session_current(request: Request):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    return JSONResponse(build_admin_session_payload(auth_result))


async def admin_session_logout(request: Request):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result

    await end_admin_session(request, auth_result, reason="logout")
    response = JSONResponse({"ok": True})
    response.delete_cookie(ADMIN_SESSION_COOKIE_NAME, path="/")
    return response


async def admin_overview(request: Request):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    try:
        return JSONResponse(await build_admin_overview_payload())
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_events(
    request: Request,
    auditLimit: int = Query(default=100, ge=1, le=500),
    auditEventType: str | None = None,
    auditActorType: str | None = None,
    auditActorTypes: list[str] | None = Query(default=None),
    auditSuccess: bool | None = None,
    dailyDays: int = Query(default=30, ge=1, le=400),
    dailyStartDate: str | None = None,
    dailyRoomCode: str | None = None,
    hourlyHours: int = Query(default=48, ge=1, le=240),
    hourlyStartAt: str | None = None,
    hourlyRoomCode: str | None = None,
    trafficRange: str = Query(default="48h"),
    trafficGranularity: str | None = Query(default=None),
    trafficStartAt: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    resolved_traffic_granularity = trafficGranularity or default_traffic_granularity(trafficRange)
    try:
        normalized_traffic_range, normalized_traffic_granularity = validate_traffic_history_params(
            trafficRange,
            resolved_traffic_granularity,
        )
    except ValueError:
        return JSONResponse({"detail": "invalid_traffic_granularity"}, status_code=422)
    normalized_daily_start_date, daily_error = _normalize_daily_start_date_or_response(dailyStartDate)
    if daily_error is not None:
        return daily_error
    normalized_hourly_start_at, hourly_error = _normalize_local_start_at_or_response(hourlyStartAt)
    if hourly_error is not None:
        return hourly_error
    normalized_traffic_start_at, traffic_start_error = _normalize_local_start_at_or_response(trafficStartAt)
    if traffic_start_error is not None:
        return traffic_start_error

    await record_admin_access(request, auth_result, "admin_sse_connect")
    audit_event_type = auditEventType.strip() if isinstance(auditEventType, str) and auditEventType.strip() else None
    audit_actor_type = auditActorType.strip() if isinstance(auditActorType, str) and auditActorType.strip() else None
    audit_actor_types = tuple(item.strip() for item in (auditActorTypes or []) if isinstance(item, str) and item.strip())
    if not audit_actor_types and audit_actor_type:
        audit_actor_types = (audit_actor_type,)
    subscriber = await runtime.admin_sse_hub.subscribe(
        audit_limit=auditLimit,
        audit_event_type=audit_event_type,
        audit_actor_types=audit_actor_types,
        audit_success=auditSuccess,
        daily_days=dailyDays,
        daily_start_date=normalized_daily_start_date,
        daily_room_code=normalize_optional_room_code(dailyRoomCode),
        hourly_hours=hourlyHours,
        hourly_start_at=normalized_hourly_start_at,
        hourly_room_code=normalize_optional_room_code(hourlyRoomCode),
        traffic_range=normalized_traffic_range,
        traffic_granularity=normalized_traffic_granularity,
        traffic_start_at=normalized_traffic_start_at,
    )

    async def event_stream():
        try:
            try:
                bootstrap_payload = await build_admin_bootstrap_payload(
                    audit_limit=subscriber.audit_limit,
                    audit_event_type=subscriber.audit_event_type,
                    audit_actor_types=subscriber.audit_actor_types,
                    audit_success=subscriber.audit_success,
                    daily_days=subscriber.daily_days,
                    daily_start_date=subscriber.daily_start_date,
                    daily_room_code=subscriber.daily_room_code,
                    hourly_hours=subscriber.hourly_hours,
                    hourly_start_at=subscriber.hourly_start_at,
                    hourly_room_code=subscriber.hourly_room_code,
                    traffic_range=subscriber.traffic_range,
                    traffic_granularity=subscriber.traffic_granularity,
                    traffic_start_at=subscriber.traffic_start_at,
                )
            except Exception:
                runtime.admin_runtime_stats["sseErrors"] += 1
                raise
            yield format_sse_event("bootstrap", bootstrap_payload)

            while True:
                if await request.is_disconnected():
                    break

                try:
                    event_name = await asyncio.wait_for(subscriber.queue.get(), timeout=15.0)
                except asyncio.TimeoutError:
                    yield format_sse_event("heartbeat", {"serverTime": time.time()})
                    continue

                try:
                    if event_name == "overview":
                        payload = {"serverTime": time.time(), **(await build_admin_overview_payload())}
                    elif event_name == "daily_metrics":
                        payload = {
                            "serverTime": time.time(),
                            **(
                                await build_admin_daily_metrics_payload(
                                    days=subscriber.daily_days,
                                    room_code=subscriber.daily_room_code,
                                    start_date=subscriber.daily_start_date,
                                )
                            ),
                        }
                    elif event_name == "hourly_metrics":
                        payload = {
                            "serverTime": time.time(),
                            **(
                                await build_admin_hourly_metrics_payload(
                                    hours=subscriber.hourly_hours,
                                    room_code=subscriber.hourly_room_code,
                                    start_at=subscriber.hourly_start_at,
                                )
                            ),
                        }
                    elif event_name == "traffic_live":
                        payload = {"serverTime": time.time(), **(await build_admin_live_traffic_payload())}
                    elif event_name == "traffic_history":
                        payload = {
                            "serverTime": time.time(),
                            **(
                                await build_admin_traffic_history_payload(
                                    range_preset=subscriber.traffic_range,
                                    granularity=subscriber.traffic_granularity,
                                    start_at=subscriber.traffic_start_at,
                                )
                            ),
                        }
                    elif event_name == "audit":
                        payload = {
                            "serverTime": time.time(),
                            **(
                                await build_admin_audit_payload(
                                    limit=subscriber.audit_limit,
                                    event_type=subscriber.audit_event_type,
                                    actor_types=subscriber.audit_actor_types,
                                    success=subscriber.audit_success,
                                )
                            ),
                        }
                    else:
                        continue
                except Exception:
                    runtime.admin_runtime_stats["sseErrors"] += 1
                    raise

                yield format_sse_event(event_name, payload)
        finally:
            await runtime.admin_sse_hub.unsubscribe(subscriber)

    return StreamingResponse(
        event_stream(),
        media_type="text/event-stream",
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        },
    )


async def admin_daily_metrics(
    request: Request,
    days: int = Query(default=30, ge=1, le=400),
    startDate: str | None = None,
    roomCode: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    normalized_start_date, start_error = _normalize_daily_start_date_or_response(startDate)
    if start_error is not None:
        return start_error
    try:
        payload = await build_admin_daily_metrics_payload(
            days=days,
            room_code=normalize_optional_room_code(roomCode),
            start_date=normalized_start_date,
        )
        return JSONResponse(payload)
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_hourly_metrics(
    request: Request,
    hours: int = Query(default=48, ge=1, le=240),
    startAt: str | None = None,
    roomCode: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    normalized_start_at, start_error = _normalize_local_start_at_or_response(startAt)
    if start_error is not None:
        return start_error
    try:
        payload = await build_admin_hourly_metrics_payload(
            hours=hours,
            room_code=normalize_optional_room_code(roomCode),
            start_at=normalized_start_at,
        )
        return JSONResponse(payload)
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_live_traffic(request: Request):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    await record_admin_access(request, auth_result, "admin_api_access")
    try:
        return JSONResponse(await build_admin_live_traffic_payload())
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_traffic_history(
    request: Request,
    range: str = Query(default="48h"),
    granularity: str | None = Query(default=None),
    startAt: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    await record_admin_access(request, auth_result, "admin_api_access")
    resolved_granularity = granularity or default_traffic_granularity(range)
    try:
        normalized_range, normalized_granularity = validate_traffic_history_params(range, resolved_granularity)
    except ValueError:
        return JSONResponse({"detail": "invalid_traffic_granularity"}, status_code=422)
    normalized_start_at, start_error = _normalize_local_start_at_or_response(startAt)
    if start_error is not None:
        return start_error
    try:
        return JSONResponse(
            await build_admin_traffic_history_payload(
                range_preset=normalized_range,
                granularity=normalized_granularity,
                start_at=normalized_start_at,
            )
        )
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_hourly_traffic(
    request: Request,
    hours: int = Query(default=48, ge=1, le=240),
    startAt: str | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    await record_admin_access(request, auth_result, "admin_api_access")
    normalized_start_at, start_error = _normalize_local_start_at_or_response(startAt)
    if start_error is not None:
        return start_error
    try:
        return JSONResponse(await build_admin_hourly_traffic_payload(hours=hours, start_at=normalized_start_at))
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_daily_traffic(
    request: Request,
    days: int = Query(default=30, ge=1, le=400),
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    await record_admin_access(request, auth_result, "admin_api_access")
    try:
        return JSONResponse(await build_admin_daily_traffic_payload(days=days))
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


async def admin_audit_log(
    request: Request,
    limit: int = Query(default=100, ge=1, le=500),
    beforeId: int | None = Query(default=None, ge=1),
    eventType: str | None = None,
    actorType: str | None = None,
    actorTypes: list[str] | None = Query(default=None),
    success: bool | None = None,
):
    auth_result = await authenticate_admin_request(request)
    if isinstance(auth_result, JSONResponse):
        return auth_result
    if runtime.admin_store is None:
        return JSONResponse({"detail": "admin_store_unavailable"}, status_code=503)

    await record_admin_access(request, auth_result, "admin_api_access")
    normalized_actor_types = [item.strip() for item in (actorTypes or []) if isinstance(item, str) and item.strip()]
    if not normalized_actor_types and isinstance(actorType, str) and actorType.strip():
        normalized_actor_types = [actorType.strip()]
    try:
        payload = await build_admin_audit_payload(
            limit=limit,
            before_id=beforeId,
            event_type=eventType.strip() if isinstance(eventType, str) and eventType.strip() else None,
            actor_type=actorType.strip() if isinstance(actorType, str) and actorType.strip() else None,
            actor_types=normalized_actor_types,
            success=success,
        )
        return JSONResponse(payload)
    except Exception:
        runtime.admin_runtime_stats["apiErrors"] += 1
        raise


def register_admin_routes(app) -> None:
    app.get("/admin")(admin_page)
    app.get("/admin/assets/{asset_path:path}")(admin_assets)
    app.post("/admin/api/session/login")(admin_session_login)
    app.get("/admin/api/session")(admin_session_current)
    app.post("/admin/api/session/logout")(admin_session_logout)
    app.get("/admin/api/overview")(admin_overview)
    app.get("/admin/api/events")(admin_events)
    app.get("/admin/api/metrics/daily")(admin_daily_metrics)
    app.get("/admin/api/metrics/hourly")(admin_hourly_metrics)
    app.get("/admin/api/traffic/live")(admin_live_traffic)
    app.get("/admin/api/traffic/history")(admin_traffic_history)
    app.get("/admin/api/traffic/hourly")(admin_hourly_traffic)
    app.get("/admin/api/traffic/daily")(admin_daily_traffic)
    app.get("/admin/api/audit")(admin_audit_log)
