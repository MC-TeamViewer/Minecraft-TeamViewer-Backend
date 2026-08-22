import asyncio
import json
import time
from collections import deque
from contextlib import asynccontextmanager

from fastapi import FastAPI

from . import runtime
from ..admin.auth import (
    build_admin_store_config,
    build_connection_details,
    build_protobuf_connection_targets,
    build_room_overview,
    expire_admin_sessions,
    get_admin_observability_payload,
    record_audit_event,
)
from ..admin.payloads import AdminPayloadService
from ..admin.protobuf_stats import ProtobufStatsService
from ..admin.store import AdminStore
from ..admin.traffic import TrafficStatsService
from ..tab_history import TabHistoryStore, TabHistoryStoreConfig
from ..ws.sender import websocket_send_hub


_broadcast_duration_samples: deque[float] = deque(maxlen=240)
_event_loop_lag_samples: deque[float] = deque(maxlen=600)


def _p95_ms(samples: deque[float]) -> float:
    if not samples:
        return 0.0
    ordered = sorted(samples)
    index = min(len(ordered) - 1, max(0, int(len(ordered) * 0.95) - 1))
    return ordered[index] * 1000.0


async def run_broadcast_scheduler() -> None:
    previous_hz: float | None = None
    overrun_streak = 0
    healthy_streak = 0
    while True:
        tick_start = time.monotonic()
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
        elapsed = time.monotonic() - tick_start
        _broadcast_duration_samples.append(elapsed)
        runtime.admin_runtime_stats["broadcastLastMs"] = elapsed * 1000.0
        runtime.admin_runtime_stats["broadcastP95Ms"] = _p95_ms(_broadcast_duration_samples)

        if elapsed > interval_sec:
            runtime.admin_runtime_stats["broadcastOverruns"] += 1
            overrun_streak += 1
            healthy_streak = 0
            if overrun_streak >= 3:
                current_cap = runtime.state.performance_broadcast_hz_cap or runtime.state.broadcast_hz
                runtime.state.performance_broadcast_hz_cap = max(
                    runtime.state.MIN_BROADCAST_HZ,
                    min(current_cap, runtime.state.broadcast_hz) * 0.75,
                )
                overrun_streak = 0
        else:
            overrun_streak = 0
            if elapsed <= interval_sec * 0.6:
                healthy_streak += 1
            else:
                healthy_streak = 0
            if healthy_streak >= 40 and runtime.state.performance_broadcast_hz_cap is not None:
                next_cap = min(
                    runtime.state.DEFAULT_BROADCAST_HZ,
                    runtime.state.performance_broadcast_hz_cap * 1.25,
                )
                runtime.state.performance_broadcast_hz_cap = next_cap
                healthy_streak = 0
        await asyncio.sleep(max(0.0, interval_sec - elapsed))


async def run_event_loop_lag_monitor() -> None:
    interval_sec = 0.1
    expected = time.monotonic() + interval_sec
    while True:
        await asyncio.sleep(interval_sec)
        now = time.monotonic()
        _event_loop_lag_samples.append(max(0.0, now - expected))
        runtime.admin_runtime_stats["eventLoopLagP95Ms"] = _p95_ms(_event_loop_lag_samples)
        expected = now + interval_sec


async def run_admin_retention_scheduler() -> None:
    while True:
        # Startup already performs one cleanup.  Waiting first avoids a duplicate
        # pass racing with requests that begin immediately after readiness.
        await asyncio.sleep(6 * 60 * 60)
        try:
            if runtime.admin_store is not None:
                expired_sessions = await expire_admin_sessions()
                cleanup = await runtime.admin_store.cleanup_retention()
                if runtime.tab_history_store is not None:
                    cleanup["tabHistory"] = await runtime.tab_history_store.cleanup_retention()
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


async def run_admin_traffic_flush_scheduler() -> None:
    while True:
        try:
            if runtime.admin_traffic_service is not None:
                await runtime.admin_traffic_service.flush_pending()
            if runtime.admin_store is not None:
                await runtime.admin_store.flush_player_activity()
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
    runtime.tab_history_store = TabHistoryStore(
        TabHistoryStoreConfig(
            db_path=runtime.admin_store.config.db_path,
            retention_days=runtime.state.TAB_HISTORY_RETENTION_DAYS,
            delta_retention_days=runtime.state.TAB_HISTORY_DELTA_RETENTION_DAYS,
            observation_update_interval_sec=runtime.state.TAB_HISTORY_OBSERVATION_UPDATE_INTERVAL_SEC,
        )
    )
    await runtime.tab_history_store.initialize()
    runtime.admin_traffic_service = TrafficStatsService(admin_store=runtime.admin_store)
    runtime.admin_protobuf_stats_service = ProtobufStatsService(
        resolve_active_connections=build_protobuf_connection_targets,
    )
    runtime.admin_payload_service = AdminPayloadService(
        admin_store=runtime.admin_store,
        build_room_overview=build_room_overview,
        build_connection_details=build_connection_details,
        build_live_traffic=runtime.admin_traffic_service.build_live_payload,
        build_live_protobuf_traffic=runtime.admin_protobuf_stats_service.build_live_payload,
        get_broadcast_hz=lambda: runtime.state.broadcast_hz,
        get_sse_subscriber_count=runtime.admin_sse_hub.subscriber_count,
        get_observability_payload=get_admin_observability_payload,
    )
    expired_sessions = await expire_admin_sessions()
    cleanup = await runtime.admin_store.cleanup_retention()
    cleanup["tabHistory"] = await runtime.tab_history_store.cleanup_retention()
    cleanup["expiredSessionsEnded"] = expired_sessions
    runtime.admin_runtime_stats["lastRetentionCleanup"] = json.dumps(cleanup, ensure_ascii=False)
    runtime.admin_runtime_stats["apiErrors"] = 0
    runtime.admin_runtime_stats["sseErrors"] = 0
    runtime.admin_runtime_stats["broadcastLastMs"] = 0.0
    runtime.admin_runtime_stats["broadcastP95Ms"] = 0.0
    runtime.admin_runtime_stats["eventLoopLagP95Ms"] = 0.0
    runtime.admin_runtime_stats["broadcastOverruns"] = 0
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
    if runtime.event_loop_monitor_task is None or runtime.event_loop_monitor_task.done():
        runtime.event_loop_monitor_task = asyncio.create_task(run_event_loop_lag_monitor())
    try:
        yield
    finally:
        await runtime.admin_sse_hub.close()
        await websocket_send_hub.close()
        if runtime.event_loop_monitor_task is not None:
            runtime.event_loop_monitor_task.cancel()
            try:
                await runtime.event_loop_monitor_task
            except asyncio.CancelledError:
                pass
            runtime.event_loop_monitor_task = None
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
            await runtime.admin_store.flush_player_activity()
            await runtime.admin_store.close()
            runtime.admin_store = None
        if runtime.tab_history_store is not None:
            await runtime.tab_history_store.close()
            runtime.tab_history_store = None
        runtime.tab_history_subscriptions.clear()
        runtime.admin_payload_service = None
        runtime.admin_traffic_service = None
        runtime.admin_protobuf_stats_service = None
