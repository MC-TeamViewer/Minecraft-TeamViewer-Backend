import asyncio
import json
import time
from contextlib import asynccontextmanager

from fastapi import FastAPI

from . import runtime
from ..admin.auth import (
    build_admin_store_config,
    build_connection_details,
    build_room_overview,
    expire_admin_sessions,
    get_admin_observability_payload,
    record_audit_event,
)
from ..admin.payloads import AdminPayloadService
from ..admin.store import AdminStore
from ..admin.traffic import TrafficStatsService


async def run_broadcast_scheduler() -> None:
    previous_hz: float | None = None
    while True:
        tick_start = time.time()
        try:
            current_hz = runtime.state.update_broadcast_hz_for_congestion()
            if previous_hz is None or abs(current_hz - previous_hz) > 1e-6:
                await runtime.broadcaster.broadcast_report_rate_hints(
                    reason="startup" if previous_hz is None else "congestion"
                )
                previous_hz = current_hz

            await runtime.broadcaster.broadcast_updates()
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            runtime.logger.exception("Broadcast scheduler error: %s", exc)
            await record_audit_event(
                event_type="backend_error",
                actor_type="system",
                success=False,
                detail={
                    "scope": "broadcast_scheduler",
                    "errorType": type(exc).__name__,
                    "message": str(exc),
                },
            )

        interval_sec = 1.0 / max(runtime.state.MIN_BROADCAST_HZ, runtime.state.broadcast_hz)
        elapsed = time.time() - tick_start
        await asyncio.sleep(max(0.0, interval_sec - elapsed))


async def run_admin_retention_scheduler() -> None:
    while True:
        try:
            if runtime.admin_store is not None:
                cleanup = await runtime.admin_store.cleanup_retention()
                expired_sessions = await expire_admin_sessions()
                cleanup["expiredSessionsEnded"] = expired_sessions
                runtime.admin_runtime_stats["lastRetentionCleanup"] = json.dumps(cleanup, ensure_ascii=False)
                runtime.logger.info("Admin retention cleanup completed: %s", cleanup)
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            runtime.logger.exception("Admin retention cleanup error: %s", exc)
            runtime.admin_runtime_stats["apiErrors"] += 1
            await record_audit_event(
                event_type="backend_error",
                actor_type="system",
                success=False,
                detail={
                    "scope": "admin_retention_scheduler",
                    "errorType": type(exc).__name__,
                    "message": str(exc),
                },
            )
        await asyncio.sleep(6 * 60 * 60)


async def run_admin_traffic_flush_scheduler() -> None:
    while True:
        try:
            if runtime.admin_traffic_service is not None:
                await runtime.admin_traffic_service.flush_pending()
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            runtime.logger.exception("Admin traffic flush error: %s", exc)
            runtime.admin_runtime_stats["apiErrors"] += 1
            await record_audit_event(
                event_type="backend_error",
                actor_type="system",
                success=False,
                detail={
                    "scope": "admin_traffic_flush",
                    "errorType": type(exc).__name__,
                    "message": str(exc),
                },
            )
        await asyncio.sleep(5.0)


@asynccontextmanager
async def lifespan(_app: FastAPI):
    runtime.admin_store = AdminStore(build_admin_store_config())
    await runtime.admin_store.initialize()
    runtime.admin_traffic_service = TrafficStatsService(admin_store=runtime.admin_store)
    runtime.admin_payload_service = AdminPayloadService(
        admin_store=runtime.admin_store,
        build_room_overview=build_room_overview,
        build_connection_details=build_connection_details,
        build_live_traffic=runtime.admin_traffic_service.build_live_payload,
        get_broadcast_hz=lambda: runtime.state.broadcast_hz,
        get_sse_subscriber_count=runtime.admin_sse_hub.subscriber_count,
        get_observability_payload=get_admin_observability_payload,
    )
    cleanup = await runtime.admin_store.cleanup_retention()
    cleanup["expiredSessionsEnded"] = await expire_admin_sessions()
    runtime.admin_runtime_stats["lastRetentionCleanup"] = json.dumps(cleanup, ensure_ascii=False)
    runtime.admin_runtime_stats["apiErrors"] = 0
    runtime.admin_runtime_stats["sseErrors"] = 0
    runtime.logger.info(
        "Admin store initialized db=%s timezone=%s cleanup=%s",
        runtime.admin_store.masked_db_path,
        runtime.admin_store.timezone_label,
        cleanup,
    )
    if runtime.broadcast_task is None or runtime.broadcast_task.done():
        runtime.broadcast_task = asyncio.create_task(run_broadcast_scheduler())
    if runtime.admin_retention_task is None or runtime.admin_retention_task.done():
        runtime.admin_retention_task = asyncio.create_task(run_admin_retention_scheduler())
    if runtime.admin_traffic_flush_task is None or runtime.admin_traffic_flush_task.done():
        runtime.admin_traffic_flush_task = asyncio.create_task(run_admin_traffic_flush_scheduler())
    try:
        yield
    finally:
        await runtime.admin_sse_hub.close()
        if runtime.admin_traffic_flush_task is not None:
            runtime.admin_traffic_flush_task.cancel()
            try:
                await runtime.admin_traffic_flush_task
            except asyncio.CancelledError:
                pass
            runtime.admin_traffic_flush_task = None
        if runtime.admin_retention_task is not None:
            runtime.admin_retention_task.cancel()
            try:
                await runtime.admin_retention_task
            except asyncio.CancelledError:
                pass
            runtime.admin_retention_task = None
        if runtime.broadcast_task is not None:
            runtime.broadcast_task.cancel()
            try:
                await runtime.broadcast_task
            except asyncio.CancelledError:
                pass
            runtime.broadcast_task = None
        if runtime.admin_store is not None:
            if runtime.admin_traffic_service is not None:
                await runtime.admin_traffic_service.flush_pending()
            await runtime.admin_store.close()
            runtime.admin_store = None
        runtime.admin_payload_service = None
        runtime.admin_traffic_service = None
