#!/usr/bin/env python3
"""Implementation-independent black-box load test for TeamViewRelay.

Every Mod reporter and the single external-source reporter use the same
deterministic world truth. A dedicated non-default benchmark room isolates the
run from real players and reuses stable history UUIDs so repeated runs do not
grow persistent Tab History without bound. Remote targets require explicit
opt-in and build verification.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import secrets
import statistics
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlparse
from urllib.request import urlopen

import websockets

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "src"
if str(SRC) not in sys.path:
    sys.path.insert(0, str(SRC))

from server.core.codec import ProtobufMessageCodec  # noqa: E402
from server.proto_generated.teamviewer.v1 import teamviewer_pb2  # noqa: E402

CODEC = ProtobufMessageCodec()
PROTOCOL_VERSION = "0.7.0"
LOAD_TEST_SCHEMA_VERSION = 3


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, math.ceil(len(ordered) * fraction) - 1))
    return ordered[index]


def world_position(index: int, revision: int) -> tuple[float, float, float]:
    """One shared truth used by Mod clients and every external source."""
    return index * 8.0 + revision * 0.125, 64.0 + index % 3, index * -5.0


def fill_player(target, index: int, revision: int, run_id: str) -> None:
    x, y, z = world_position(index, revision)
    target.x = x
    target.y = y
    target.z = z
    target.vx = 2.5
    target.vy = 0.0
    target.vz = 0.0
    target.dimension = "minecraft:overworld"
    target.player_name = f"LoadMod{index}"
    target.player_uuid = f"{index + 1:08x}-0000-4000-8000-{int(run_id, 16):012x}"
    target.health = 20.0
    target.max_health = 20.0
    target.armor = float(index % 20)
    target.is_riding = False
    target.width = 0.6
    target.height = 1.8


def player_handshake(player_id: str, room: str, *, external: bool) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    request = envelope.player_handshake_request
    request.network_protocol_version = PROTOCOL_VERSION
    request.minimum_compatible_network_protocol_version = "0.6.1"
    request.local_program_version = "teamviewer-protocol-load-test"
    request.submit_player_id = player_id
    request.room_code = room
    request.client_role = (
        teamviewer_pb2.CLIENT_ROLE_EXTERNAL_SOURCE
        if external else teamviewer_pb2.CLIENT_ROLE_PLAYER
    )
    request.client_display_name = "Load external source" if external else player_id
    request.position_resolution = 0.125
    return envelope.SerializeToString()


def web_handshake(room: str) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    request = envelope.web_map_handshake_request
    request.network_protocol_version = PROTOCOL_VERSION
    request.minimum_compatible_network_protocol_version = "0.6.1"
    request.local_program_version = "teamviewer-protocol-load-test"
    request.room_code = room
    return envelope.SerializeToString()


def mod_report(player_id: str, index: int, revision: int, run_id: str, *, full: bool) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = player_id
    if full:
        fill_player(bundle.players_replace.players[player_id], index, revision, run_id)
    else:
        upsert = bundle.players_patch.upsert.add(id=player_id)
        x, y, z = world_position(index, revision)
        upsert.data.x = x
        upsert.data.y = y
        upsert.data.z = z
        upsert.data.vx = 2.5
    return envelope.SerializeToString()


def external_report(
    source_id: str,
    mod_ids: list[str],
    revision: int,
    run_id: str,
    *,
    full: bool,
) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = source_id
    for index, player_id in enumerate(mod_ids):
        if full:
            fill_player(bundle.players_replace.players[player_id], index, revision, run_id)
        else:
            upsert = bundle.players_patch.upsert.add(id=player_id)
            x, y, z = world_position(index, revision)
            upsert.data.x = x
            upsert.data.y = y
            upsert.data.z = z
            upsert.data.vx = 2.5
    return envelope.SerializeToString()


def history_fixture_report(source_id: str, count: int) -> bytes:
    """Seed the large state shape that caused the original production stall."""
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_PLAYER)
    bundle = envelope.player_report_bundle
    bundle.submit_player_id = source_id
    base_time_ms = 1_700_000_000_000
    for index in range(count):
        player_uuid = f"00000000-0000-4000-9000-{index:012x}"
        player_id = f"load-history-offline-{index:04d}"
        bundle.last_seen_players_replace.players[player_id].CopyFrom(
            teamviewer_pb2.LastSeenPlayerData(
                x=float(index * 13),
                y=50.0 + index % 80,
                z=float(index * -17),
                dimension="minecraft:overworld",
                player_name=f"HistoricalPlayer{index:04d}",
                player_uuid=player_uuid,
                last_seen_at_utc_ms=base_time_ms + index * 1_000,
                position_observed_at_utc_ms=base_time_ms + index * 1_000 - 250,
                offline_detected_at_utc_ms=base_time_ms + index * 1_000 + 500,
            )
        )
        bundle.tab_players_replace.tab_players.add(
            uuid=player_uuid,
            name=f"HistoricalPlayer{index:04d}",
        )
    return envelope.SerializeToString()


def resync_request() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    envelope.resync_request.reason = "load_fixture_size_verification"
    return envelope.SerializeToString()


def tab_history_sync_request(request_id: str) -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    request = envelope.tab_history_sync_request
    request.request_id = request_id
    request.preferred_mode = teamviewer_pb2.TAB_HISTORY_SYNC_MODE_FULL
    request.max_chunk_entries = 250
    request.allow_full_fallback = True
    return envelope.SerializeToString()


def tab_state_player_count(packet: dict) -> int:
    tab_state = packet.get("tabState")
    if not isinstance(tab_state, dict):
        return 0
    reports = tab_state.get("reports")
    if not isinstance(reports, dict):
        return 0
    return sum(
        len(report.get("players", []))
        for report in reports.values()
        if isinstance(report, dict)
    )


def apply_players(state: dict, packet: dict) -> None:
    if packet.get("type") == "snapshot_full":
        state.clear()
        state.update(packet.get("players", {}))
        return
    if packet.get("type") != "patch" or not isinstance(packet.get("players"), dict):
        return
    scope = packet["players"]
    for player_id in scope.get("delete", []):
        state.pop(player_id, None)
    for player_id, delta in scope.get("upsert", {}).items():
        player = state.setdefault(player_id, {})
        for key, value in delta.items():
            if value is None:
                player.pop(key, None)
            else:
                player[key] = value


@dataclass
class VirtualClient:
    kind: str
    socket: object
    client_id: str
    player_index: int | None = None
    state: dict = field(default_factory=dict)
    state_frames: int = 0
    bytes_received: int = 0
    disconnected: bool = False
    disconnect_detail: str | None = None
    reader: asyncio.Task | None = None
    heartbeat_task: asyncio.Task | None = None
    heartbeats_sent: int = 0
    heartbeats_received: int = 0
    heartbeat_timeouts: int = 0
    rtts: list[float] = field(default_factory=list)
    snapshot_full_max_bytes: int = 0
    last_seen_players_seen: int = 0
    tab_state_players_seen: int = 0
    tab_history_entries_received: int = 0
    tab_history_sync_complete: bool = False
    tab_history_error: str | None = None

    async def read(self, stopping: asyncio.Event) -> None:
        try:
            async for raw in self.socket:
                self.bytes_received += len(raw)
                packet = CODEC.decode(raw)
                if packet.get("type") in {"snapshot_full", "patch"}:
                    self.state_frames += 1
                    apply_players(self.state, packet)
                if packet.get("type") == "snapshot_full":
                    self.snapshot_full_max_bytes = max(self.snapshot_full_max_bytes, len(raw))
                    self.last_seen_players_seen = max(
                        self.last_seen_players_seen,
                        len(packet.get("lastSeenPlayers", {})),
                    )
                    self.tab_state_players_seen = max(
                        self.tab_state_players_seen,
                        tab_state_player_count(packet),
                    )
                elif packet.get("type") == "tab_history_sync_chunk":
                    self.tab_history_entries_received += len(packet.get("upsert", []))
                    if packet.get("errorCode"):
                        self.tab_history_error = str(packet.get("errorCode"))
                    if packet.get("final"):
                        self.tab_history_sync_complete = True
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            if not stopping.is_set():
                self.disconnected = True
                self.disconnect_detail = f"{type(exc).__name__}: {exc}"
        finally:
            # A normal WebSocket Close frame ends ``async for`` without an
            # exception. Count it too unless close() was initiated by us.
            if not stopping.is_set():
                self.disconnected = True
                if self.disconnect_detail is None:
                    code = getattr(self.socket, "close_code", None)
                    reason = getattr(self.socket, "close_reason", None)
                    self.disconnect_detail = f"close code={code!r}, reason={reason!r}"

    def start_heartbeat(self, timeout: float) -> bool:
        if (
            self.disconnected
            or self.heartbeat_task is not None
            and not self.heartbeat_task.done()
        ):
            return False
        self.heartbeats_sent += 1

        async def measure() -> None:
            started = time.monotonic()
            try:
                pong_waiter = await self.socket.ping()
                await asyncio.wait_for(pong_waiter, timeout)
                self.rtts.append(time.monotonic() - started)
                self.heartbeats_received += 1
            except asyncio.TimeoutError:
                self.heartbeat_timeouts += 1
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                self.disconnected = True
                self.disconnect_detail = f"heartbeat {type(exc).__name__}: {exc}"

        self.heartbeat_task = asyncio.create_task(measure())
        return True


class LoadRun:
    def __init__(self, base_url: str, room: str, run_id: str) -> None:
        self.ws_url = base_url.rstrip("/").replace("http://", "ws://").replace("https://", "wss://")
        self.room = room
        self.run_id = run_id
        self.mods: list[VirtualClient] = []
        self.webs: list[VirtualClient] = []
        self.externals: list[VirtualClient] = []
        self.stopping = asyncio.Event()
        self.connection_attempts = 0
        self.connection_failures: list[str] = []

    async def _connect_player(
        self,
        client_id: str,
        *,
        external: bool,
        index: int | None = None,
    ) -> VirtualClient:
        socket = await websockets.connect(
            f"{self.ws_url}/mc-client", max_size=16 * 1024 * 1024,
            open_timeout=10, close_timeout=3, ping_interval=None,
        )
        try:
            await socket.send(player_handshake(client_id, self.room, external=external))
            ack = CODEC.decode(await asyncio.wait_for(socket.recv(), 10))
            if not ack.get("ready"):
                raise RuntimeError(f"{client_id}: {ack.get('rejectReason') or ack.get('error')}")
        except Exception:
            await socket.close()
            raise
        client = VirtualClient("external" if external else "mod", socket, client_id, index)
        client.reader = asyncio.create_task(client.read(self.stopping))
        return client

    async def _connect_web(self, index: int) -> VirtualClient:
        socket = await websockets.connect(
            f"{self.ws_url}/web-map/ws", max_size=16 * 1024 * 1024,
            open_timeout=10, close_timeout=3, ping_interval=None,
        )
        try:
            await socket.send(web_handshake(self.room))
            ack = CODEC.decode(await asyncio.wait_for(socket.recv(), 10))
            if not ack.get("ready"):
                raise RuntimeError(f"web-{index}: {ack.get('rejectReason') or ack.get('error')}")
        except Exception:
            await socket.close()
            raise
        client = VirtualClient("web", socket, f"web-{index}")
        client.reader = asyncio.create_task(client.read(self.stopping))
        try:
            await socket.send(
                tab_history_sync_request(f"load-{self.run_id}-web-{index}")
            )
        except Exception:
            await socket.close()
            raise
        return client

    async def prepare_fixture(
        self,
        history_players: int,
        minimum_snapshot_bytes: int,
    ) -> dict:
        """Seed and verify history before any measured clients connect."""
        source_id = "load-benchmark-v3-external"
        self.connection_attempts += 1
        try:
            external = await self._connect_player(source_id, external=True)
        except Exception as exc:
            detail = f"external {source_id}: {type(exc).__name__}: {exc}"
            self.connection_failures.append(detail)
            raise RuntimeError(detail) from exc
        self.externals.append(external)

        fixture_payload = history_fixture_report(source_id, history_players)
        await external.socket.send(fixture_payload)

        probe = await websockets.connect(
            f"{self.ws_url}/web-map/ws",
            max_size=16 * 1024 * 1024,
            open_timeout=10,
            close_timeout=3,
            ping_interval=None,
        )
        try:
            await probe.send(web_handshake(self.room))
            ack = CODEC.decode(await asyncio.wait_for(probe.recv(), 10))
            if not ack.get("ready"):
                raise RuntimeError(
                    f"fixture web probe rejected: "
                    f"{ack.get('rejectReason') or ack.get('error')}"
                )

            web_snapshot_bytes = 0
            web_last_seen = 0
            web_tab_state_players = 0
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                await probe.send(resync_request())
                try:
                    while True:
                        raw = await asyncio.wait_for(probe.recv(), 2)
                        packet = CODEC.decode(raw)
                        if packet.get("type") != "snapshot_full":
                            continue
                        web_snapshot_bytes = len(raw)
                        web_last_seen = len(packet.get("lastSeenPlayers", {}))
                        web_tab_state_players = tab_state_player_count(packet)
                        break
                except asyncio.TimeoutError:
                    await asyncio.sleep(0.1)
                    continue
                if (
                    web_last_seen >= history_players
                    and web_tab_state_players >= history_players
                ):
                    break
                await asyncio.sleep(0.1)

            request_id = f"load-{self.run_id}-fixture-history"
            await probe.send(tab_history_sync_request(request_id))
            tab_history_entries = 0
            tab_history_bytes = 0
            tab_history_final = False
            tab_history_error = None
            deadline = time.monotonic() + 20
            while not tab_history_final and time.monotonic() < deadline:
                raw = await asyncio.wait_for(probe.recv(), 3)
                packet = CODEC.decode(raw)
                if (
                    packet.get("type") != "tab_history_sync_chunk"
                    or packet.get("requestId") != request_id
                ):
                    continue
                tab_history_bytes += len(raw)
                tab_history_entries += len(packet.get("upsert", []))
                tab_history_final = bool(packet.get("final"))
                if packet.get("errorCode"):
                    tab_history_error = str(packet.get("errorCode"))
        finally:
            await probe.close()

        player_probe_id = f"load-{self.run_id}-fixture-player-probe"
        player_probe = await websockets.connect(
            f"{self.ws_url}/mc-client",
            max_size=16 * 1024 * 1024,
            open_timeout=10,
            close_timeout=3,
            ping_interval=None,
        )
        try:
            await player_probe.send(
                player_handshake(player_probe_id, self.room, external=False)
            )
            ack = CODEC.decode(await asyncio.wait_for(player_probe.recv(), 10))
            if not ack.get("ready"):
                raise RuntimeError(
                    f"fixture player probe rejected: "
                    f"{ack.get('rejectReason') or ack.get('error')}"
                )
            player_snapshot_bytes = 0
            player_last_seen = 0
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                raw = await asyncio.wait_for(player_probe.recv(), 3)
                packet = CODEC.decode(raw)
                if packet.get("type") == "snapshot_full":
                    player_snapshot_bytes = len(raw)
                    player_last_seen = len(packet.get("lastSeenPlayers", {}))
                    break
        finally:
            await player_probe.close()

        fixture = {
            "historyPlayersRequested": history_players,
            "fixtureReportBytes": len(fixture_payload),
            "webSnapshotFullBytes": web_snapshot_bytes,
            "webSnapshotFullKiB": web_snapshot_bytes / 1024,
            "webLastSeenPlayers": web_last_seen,
            "webTabStatePlayers": web_tab_state_players,
            "playerSnapshotFullBytes": player_snapshot_bytes,
            "playerSnapshotFullKiB": player_snapshot_bytes / 1024,
            "playerLastSeenPlayers": player_last_seen,
            "tabHistoryEntriesSynced": tab_history_entries,
            "tabHistorySyncBytes": tab_history_bytes,
            "tabHistoryFinal": tab_history_final,
            "tabHistoryError": tab_history_error,
            "minimumSnapshotBytes": minimum_snapshot_bytes,
        }
        failures = []
        if web_last_seen < history_players:
            failures.append("web snapshot is missing last-seen history")
        if web_tab_state_players < history_players:
            failures.append("web snapshot is missing live Tab state")
        if player_last_seen < history_players:
            failures.append("player snapshot is missing last-seen history")
        if web_snapshot_bytes < minimum_snapshot_bytes:
            failures.append(
                f"web snapshot is only {web_snapshot_bytes / 1024:.1f} KiB; "
                f"minimum is {minimum_snapshot_bytes / 1024:.1f} KiB"
            )
        if not tab_history_final or tab_history_entries < history_players:
            failures.append("Tab History FULL sync did not return the seeded records")
        if tab_history_error:
            failures.append(f"Tab History FULL sync error: {tab_history_error}")
        fixture["passed"] = not failures
        fixture["failureReasons"] = failures
        if failures:
            raise RuntimeError("fixture verification failed: " + "; ".join(failures))
        await asyncio.sleep(0.1)
        return fixture

    async def scale(self, simulated_users: int) -> None:
        # One user is represented by one reporting Mod socket and one consuming
        # Web Map socket. The deployment has exactly one external source.
        external_count = 1
        mod_count = simulated_users
        web_count = simulated_users

        while len(self.mods) < mod_count:
            index = len(self.mods)
            client_id = f"load-{self.run_id}-mod-{index}"
            self.connection_attempts += 1
            try:
                client = await self._connect_player(client_id, external=False, index=index)
            except Exception as exc:
                self.connection_failures.append(
                    f"mod {client_id}: {type(exc).__name__}: {exc}"
                )
                break
            self.mods.append(client)
        while len(self.webs) < web_count:
            index = len(self.webs)
            self.connection_attempts += 1
            try:
                client = await self._connect_web(index)
            except Exception as exc:
                self.connection_failures.append(
                    f"web web-{index}: {type(exc).__name__}: {exc}"
                )
                break
            self.webs.append(client)
        while len(self.externals) < external_count:
            index = len(self.externals)
            client_id = f"load-{self.run_id}-external-{index}"
            self.connection_attempts += 1
            try:
                client = await self._connect_player(client_id, external=True)
            except Exception as exc:
                self.connection_failures.append(
                    f"external {client_id}: {type(exc).__name__}: {exc}"
                )
                break
            self.externals.append(client)

    async def report(self, revision: int, *, full: bool) -> None:
        mod_ids = [client.client_id for client in self.mods]
        payloads = [
            client.socket.send(mod_report(
                client.client_id, client.player_index, revision, self.run_id, full=full,
            ))
            for client in self.mods
        ]
        payloads.extend(
            client.socket.send(external_report(
                client.client_id, mod_ids, revision, self.run_id, full=full,
            ))
            for client in self.externals
        )
        await asyncio.gather(*payloads)

    def heartbeat_all(self, timeout: float) -> None:
        clients = self.mods + self.webs + self.externals
        for client in clients:
            client.start_heartbeat(timeout)

    async def wait_for_heartbeats(self) -> None:
        clients = self.mods + self.webs + self.externals
        pending = [
            client.heartbeat_task
            for client in clients
            if client.heartbeat_task is not None and not client.heartbeat_task.done()
        ]
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)

    def consistency(self, revision: int) -> dict:
        mod_ids = [client.client_id for client in self.mods]
        consumers = self.mods + self.webs
        missing = 0
        mismatched = 0
        checked = 0
        for client in consumers:
            for index, player_id in enumerate(mod_ids):
                player = client.state.get(player_id)
                if not isinstance(player, dict):
                    missing += 1
                    continue
                checked += 1
                expected = world_position(index, revision)
                actual = (player.get("x"), player.get("y"), player.get("z"))
                if actual != expected:
                    mismatched += 1
        return {"checked": checked, "missing": missing, "mismatched": mismatched}

    async def wait_for_consistency(self, revision: int, timeout: float) -> dict:
        started = time.monotonic()
        while True:
            result = self.consistency(revision)
            elapsed = time.monotonic() - started
            if result["missing"] == 0 and result["mismatched"] == 0:
                return {
                    **result,
                    "converged": True,
                    "convergenceMs": elapsed * 1000,
                    "timeoutSec": timeout,
                }
            if elapsed >= timeout:
                return {
                    **result,
                    "converged": False,
                    "convergenceMs": None,
                    "timeoutSec": timeout,
                }
            await asyncio.sleep(0.05)

    async def close(self) -> None:
        self.stopping.set()
        clients = self.mods + self.webs + self.externals
        await asyncio.gather(*(client.socket.close() for client in clients), return_exceptions=True)
        await asyncio.gather(
            *(client.reader for client in clients if client.reader is not None),
            return_exceptions=True,
        )


async def execute(
    base_url: str,
    room: str,
    stages: list[int],
    stage_duration: float,
    report_hz: float,
    settle_timeout: float,
    heartbeat_timeout: float,
    history_players: int,
    minimum_snapshot_bytes: int,
    minimum_rate_ratio: float,
    target_build_version: str | None,
) -> dict:
    run_id = secrets.token_hex(5)
    load = LoadRun(base_url, room, run_id)
    results = []
    revision = 0
    heartbeat_monitor_stop = asyncio.Event()
    heartbeat_monitor: asyncio.Task | None = None

    async def monitor_heartbeats() -> None:
        while not heartbeat_monitor_stop.is_set():
            load.heartbeat_all(heartbeat_timeout)
            try:
                await asyncio.wait_for(heartbeat_monitor_stop.wait(), 1.0)
            except asyncio.TimeoutError:
                pass

    try:
        fixture = await load.prepare_fixture(history_players, minimum_snapshot_bytes)
        heartbeat_monitor = asyncio.create_task(monitor_heartbeats())
        for simulated_users in stages:
            attempts_before = load.connection_attempts
            failures_before = len(load.connection_failures)
            heartbeat_baseline = {
                id(client): (
                    client.heartbeats_sent,
                    client.heartbeats_received,
                    client.heartbeat_timeouts,
                    len(client.rtts),
                    client.disconnected,
                    client.state_frames,
                    client.bytes_received,
                )
                for client in load.mods + load.webs + load.externals
            }
            empty_baseline = (0, 0, 0, 0, False, 0, 0)

            def baseline(client: VirtualClient) -> tuple:
                return heartbeat_baseline.get(id(client), empty_baseline)

            scale_started = time.monotonic()
            await load.scale(simulated_users)
            connection_setup_duration = time.monotonic() - scale_started
            clients = load.mods + load.webs + load.externals
            report_errors = 0
            revision += 1
            try:
                await load.report(revision, full=True)
            except Exception:
                report_errors += 1
            started = time.monotonic()
            next_report = started
            ticks = 0
            while time.monotonic() - started < stage_duration:
                revision += 1
                try:
                    await load.report(revision, full=False)
                except Exception:
                    report_errors += 1
                ticks += 1
                next_report += 1.0 / report_hz
                await asyncio.sleep(max(0.0, next_report - time.monotonic()))
            actual_duration = time.monotonic() - started
            # End every stage with an authoritative full frame, then measure
            # how long consumers need to catch up. This separates real data
            # disagreement from temporary broadcast queue lag.
            revision += 1
            try:
                await load.report(revision, full=True)
            except Exception:
                report_errors += 1
            consistency = await load.wait_for_consistency(revision, settle_timeout)
            await load.wait_for_heartbeats()
            consumers = load.mods + load.webs
            clients = consumers + load.externals
            rtts = [
                sample
                for client in clients
                for sample in client.rtts[baseline(client)[3]:]
            ]
            heartbeat_sent = sum(
                client.heartbeats_sent - baseline(client)[0]
                for client in clients
            )
            heartbeat_received = sum(
                client.heartbeats_received - baseline(client)[1]
                for client in clients
            )
            heartbeat_timeouts = sum(
                client.heartbeat_timeouts - baseline(client)[2]
                for client in clients
            )
            target_connections = simulated_users * 2 + 1
            stage_failures = load.connection_failures[failures_before:]
            active_connections = sum(not client.disconnected for client in clients)
            stage_result = {
                "simulatedUsers": simulated_users,
                "targetConnections": target_connections,
                "connections": len(clients),
                "activeConnections": active_connections,
                "unestablishedConnections": max(
                    0, target_connections - active_connections
                ),
                "connectionAttempts": load.connection_attempts - attempts_before,
                "connectionFailures": len(stage_failures),
                "connectionFailureDetails": stage_failures,
                "connectionSetupSec": connection_setup_duration,
                "modReporters": len(load.mods),
                "webConsumers": len(load.webs),
                "externalReporters": len(load.externals),
                "durationSec": stage_duration,
                "actualDurationSec": actual_duration,
                "reportHz": report_hz,
                "achievedReportHz": ticks / actual_duration,
                "reportRateRatio": (ticks / actual_duration) / report_hz,
                "ticks": ticks,
                "reportErrors": report_errors,
                "disconnects": sum(client.disconnected for client in clients),
                "disconnectsDuringStage": sum(
                    client.disconnected and not baseline(client)[4]
                    for client in clients
                ),
                "disconnectDetails": [
                    f"{client.kind} {client.client_id}: {client.disconnect_detail}"
                    for client in clients if client.disconnected
                ],
                "consumersWithoutState": sum(
                    client.state_frames - baseline(client)[5] == 0
                    for client in consumers
                ),
                "minimumStateFrames": min(
                    (
                        client.state_frames - baseline(client)[5]
                        for client in consumers
                    ),
                    default=0,
                ),
                "receivedMiB": sum(
                    client.bytes_received - baseline(client)[6]
                    for client in consumers
                )
                / 1024
                / 1024,
                "pingRttP50Ms": percentile(rtts, 0.50) * 1000,
                "pingRttP95Ms": percentile(rtts, 0.95) * 1000,
                "pingRttMaxMs": max(rtts, default=0.0) * 1000,
                "heartbeat": {
                    "timeoutSec": heartbeat_timeout,
                    "sent": heartbeat_sent,
                    "received": heartbeat_received,
                    "pending": sum(
                        client.heartbeat_task is not None
                        and not client.heartbeat_task.done()
                        for client in clients
                    ),
                    "timedOut": heartbeat_timeouts,
                    "clientsTimedOut": sum(
                        client.heartbeat_timeouts > baseline(client)[2]
                        for client in clients
                    ),
                },
                "historyFixture": {
                    "expectedPlayers": history_players,
                    "consumersWithoutLastSeenHistory": sum(
                        client.last_seen_players_seen < history_players
                        for client in consumers
                    ),
                    "webConsumersWithoutTabState": sum(
                        client.tab_state_players_seen < history_players
                        for client in load.webs
                    ),
                    "minimumPlayerSnapshotFullKiB": min(
                        (
                            client.snapshot_full_max_bytes / 1024
                            for client in load.mods
                        ),
                        default=0,
                    ),
                    "minimumWebSnapshotFullKiB": min(
                        (
                            client.snapshot_full_max_bytes / 1024
                            for client in load.webs
                        ),
                        default=0,
                    ),
                    "minimumWebTabHistoryEntries": min(
                        (
                            client.tab_history_entries_received
                            for client in load.webs
                        ),
                        default=0,
                    ),
                    "webTabHistorySyncIncomplete": sum(
                        not client.tab_history_sync_complete for client in load.webs
                    ),
                    "webTabHistoryErrors": [
                        f"{client.client_id}: {client.tab_history_error}"
                        for client in load.webs
                        if client.tab_history_error
                    ],
                },
                "consistency": consistency,
            }
            failure_reasons = []
            if stage_result["connectionFailures"]:
                failure_reasons.append("connection or handshake failure")
            if stage_result["unestablishedConnections"]:
                failure_reasons.append("target connection count not reached")
            if stage_result["reportErrors"]:
                failure_reasons.append("report send failure")
            if stage_result["disconnects"]:
                failure_reasons.append("connection dropped during the run")
            if stage_result["heartbeat"]["timedOut"]:
                failure_reasons.append("WebSocket Ping/Pong heartbeat timeout")
            if stage_result["historyFixture"]["consumersWithoutLastSeenHistory"]:
                failure_reasons.append("consumer did not receive last-seen history fixture")
            if stage_result["historyFixture"]["webConsumersWithoutTabState"]:
                failure_reasons.append("web consumer did not receive live Tab state fixture")
            if stage_result["historyFixture"]["webTabHistorySyncIncomplete"]:
                failure_reasons.append("web consumer Tab History FULL sync incomplete")
            if stage_result["historyFixture"]["webTabHistoryErrors"]:
                failure_reasons.append("web consumer Tab History FULL sync error")
            if stage_result["consumersWithoutState"]:
                failure_reasons.append("consumer received no state")
            if not stage_result["consistency"]["converged"]:
                failure_reasons.append("final state did not converge")
            if stage_result["pingRttP95Ms"] > 2_000:
                failure_reasons.append("heartbeat RTT p95 exceeded 2000 ms")
            if stage_result["reportRateRatio"] < minimum_rate_ratio:
                failure_reasons.append(
                    f"achieved report rate is below {minimum_rate_ratio:.0%} of target"
                )
            stage_result["passed"] = not failure_reasons
            stage_result["failureReasons"] = failure_reasons
            results.append(stage_result)
        return {
            "loadTestSchemaVersion": LOAD_TEST_SCHEMA_VERSION,
            "target": base_url,
            "targetBuildVersion": target_build_version,
            "room": room,
            "runId": run_id,
            "fixture": fixture,
            "passed": all(stage["passed"] for stage in results),
            "stages": results,
        }
    finally:
        heartbeat_monitor_stop.set()
        if heartbeat_monitor is not None:
            await heartbeat_monitor
        await load.wait_for_heartbeats()
        await load.close()


def parse_stages(raw: str) -> list[int]:
    stages = sorted({int(value.strip()) for value in raw.split(",") if value.strip()})
    if not stages or stages[0] < 1:
        raise ValueError("stages must contain simulated user counts >= 1")
    return stages


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:18767")
    parser.add_argument("--room", default="load-benchmark-v3")
    parser.add_argument("--stages", default="10,20,40")
    parser.add_argument("--stage-duration", type=float, default=30.0)
    parser.add_argument("--report-hz", type=float, default=10.0)
    parser.add_argument("--settle-timeout", type=float, default=10.0)
    parser.add_argument("--heartbeat-timeout", type=float, default=5.0)
    parser.add_argument("--history-players", type=int, default=1_000)
    parser.add_argument("--min-snapshot-kib", type=float, default=200.0)
    parser.add_argument("--min-rate-ratio", type=float, default=0.95)
    parser.add_argument(
        "--expected-build",
        help="exact /health buildVersion expected from the target implementation",
    )
    parser.add_argument("--allow-remote", action="store_true")
    parser.add_argument(
        "--allow-unverified-build",
        action="store_true",
        help="allow a remote target without checking /health buildVersion",
    )
    parser.add_argument("--unsafe-above-200", action="store_true")
    args = parser.parse_args()
    room = args.room.strip()
    if not room:
        parser.error("--room must not be empty")
    if room.lower() == "default":
        parser.error("the pressure test refuses to use the default room")
    host = (urlparse(args.url).hostname or "").lower()
    remote = host not in {"127.0.0.1", "localhost", "::1"}
    if remote and not args.allow_remote:
        parser.error("remote targets require --allow-remote")
    if remote and not args.expected_build and not args.allow_unverified_build:
        parser.error(
            "remote targets require --expected-build VERSION; use "
            "--allow-unverified-build only when version verification is impossible"
        )
    try:
        with urlopen(f"{args.url.rstrip('/')}/health", timeout=10) as response:
            health = json.load(response)
    except Exception as exc:
        parser.error(f"health preflight failed: {exc}")
    target_build_version = health.get("buildVersion")
    if args.expected_build and target_build_version != args.expected_build:
        parser.error(
            "target build mismatch: "
            f"buildVersion={target_build_version!r}, expected={args.expected_build!r}"
        )
    try:
        stages = parse_stages(args.stages)
    except ValueError as exc:
        parser.error(str(exc))
    if max(stages) * 2 + 1 > 200 and not args.unsafe_above_200:
        parser.error("more than 200 total sockets require --unsafe-above-200")
    if not 0.1 <= args.report_hz <= 20:
        parser.error("--report-hz must be between 0.1 and 20")
    if not 0.1 <= args.settle_timeout <= 60:
        parser.error("--settle-timeout must be between 0.1 and 60")
    if not 0.5 <= args.heartbeat_timeout <= 60:
        parser.error("--heartbeat-timeout must be between 0.5 and 60")
    if not 1 <= args.history_players <= 10_000:
        parser.error("--history-players must be between 1 and 10000")
    if not 1 <= args.min_snapshot_kib <= 16 * 1024:
        parser.error("--min-snapshot-kib must be between 1 and 16384")
    if not 0.1 <= args.min_rate_ratio <= 1:
        parser.error("--min-rate-ratio must be between 0.1 and 1")
    result = asyncio.run(execute(
        args.url,
        room,
        stages,
        max(1.0, args.stage_duration),
        args.report_hz,
        args.settle_timeout,
        args.heartbeat_timeout,
        args.history_players,
        int(args.min_snapshot_kib * 1024),
        args.min_rate_ratio,
        target_build_version,
    ))
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
