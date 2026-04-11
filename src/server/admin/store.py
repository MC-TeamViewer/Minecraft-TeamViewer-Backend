from __future__ import annotations

import asyncio
import hashlib
import json
import secrets
import sqlite3
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

TRAFFIC_RANGE_SECONDS = {
    "1h": 60 * 60,
    "6h": 6 * 60 * 60,
    "24h": 24 * 60 * 60,
    "48h": 48 * 60 * 60,
    "7d": 7 * 24 * 60 * 60,
    "30d": 30 * 24 * 60 * 60,
}

TRAFFIC_GRANULARITY_SECONDS = {
    "1m": 60,
    "5m": 5 * 60,
    "15m": 15 * 60,
    "1h": 60 * 60,
    "1d": 24 * 60 * 60,
}

ALLOWED_TRAFFIC_GRANULARITIES = {
    "1h": ("1m", "5m"),
    "6h": ("1m", "5m", "15m"),
    "24h": ("5m", "15m", "1h"),
    "48h": ("15m", "1h"),
    "7d": ("1h", "1d"),
    "30d": ("1d",),
}

DEFAULT_TRAFFIC_GRANULARITY = {
    "1h": "1m",
    "6h": "5m",
    "24h": "15m",
    "48h": "1h",
    "7d": "1h",
    "30d": "1d",
}


@dataclass(slots=True)
class AdminStoreConfig:
    db_path: str
    audit_retention_days: int = 90
    hourly_retention_days: int = 90
    daily_retention_days: int = 400


class AdminStore:
    def __init__(self, config: AdminStoreConfig) -> None:
        self.config = config
        self._db: sqlite3.Connection | None = None
        self._lock = asyncio.Lock()

    async def initialize(self) -> None:
        db_path = Path(self.config.db_path)
        db_path.parent.mkdir(parents=True, exist_ok=True)

        self._db = sqlite3.connect(db_path, check_same_thread=False)
        self._db.row_factory = sqlite3.Row
        self._db.execute("PRAGMA journal_mode=WAL")
        self._db.execute("PRAGMA busy_timeout=5000")
        self._db.executescript(
            """
            CREATE TABLE IF NOT EXISTS daily_player_activity (
                local_date TEXT NOT NULL,
                player_id TEXT NOT NULL,
                room_code TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                PRIMARY KEY (local_date, player_id, room_code)
            );

            CREATE TABLE IF NOT EXISTS hourly_player_activity (
                local_hour TEXT NOT NULL,
                player_id TEXT NOT NULL,
                room_code TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                PRIMARY KEY (local_hour, player_id, room_code)
            );

            CREATE TABLE IF NOT EXISTS audit_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                occurred_at INTEGER NOT NULL,
                local_date TEXT NOT NULL,
                local_hour TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor_type TEXT NOT NULL,
                actor_id TEXT,
                room_code TEXT,
                success INTEGER NOT NULL,
                remote_addr TEXT,
                detail_json TEXT
            );

            CREATE TABLE IF NOT EXISTS player_identity_mappings (
                player_id TEXT PRIMARY KEY,
                username TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS admin_sessions (
                session_id TEXT PRIMARY KEY,
                token_hash TEXT NOT NULL UNIQUE,
                actor_id TEXT NOT NULL,
                remote_addr TEXT,
                created_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                ended_at INTEGER,
                end_reason TEXT
            );

            CREATE TABLE IF NOT EXISTS hourly_traffic_bytes (
                local_hour TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                PRIMARY KEY (local_hour, channel, direction)
            );

            CREATE TABLE IF NOT EXISTS minute_traffic_bytes (
                local_minute TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                PRIMARY KEY (local_minute, channel, direction)
            );

            CREATE TABLE IF NOT EXISTS minute_wire_traffic_bytes (
                local_minute TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                PRIMARY KEY (local_minute, channel, direction)
            );

            CREATE TABLE IF NOT EXISTS daily_traffic_bytes (
                local_date TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                PRIMARY KEY (local_date, channel, direction)
            );

            CREATE TABLE IF NOT EXISTS hourly_wire_traffic_bytes (
                local_hour TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                PRIMARY KEY (local_hour, channel, direction)
            );

            CREATE TABLE IF NOT EXISTS daily_wire_traffic_bytes (
                local_date TEXT NOT NULL,
                channel TEXT NOT NULL,
                direction TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                PRIMARY KEY (local_date, channel, direction)
            );

            CREATE INDEX IF NOT EXISTS idx_daily_player_activity_date
                ON daily_player_activity (local_date);

            CREATE INDEX IF NOT EXISTS idx_daily_player_activity_room_date
                ON daily_player_activity (room_code, local_date);

            CREATE INDEX IF NOT EXISTS idx_hourly_player_activity_hour
                ON hourly_player_activity (local_hour);

            CREATE INDEX IF NOT EXISTS idx_hourly_player_activity_room_hour
                ON hourly_player_activity (room_code, local_hour);

            CREATE INDEX IF NOT EXISTS idx_audit_events_occurred_at
                ON audit_events (occurred_at DESC);

            CREATE INDEX IF NOT EXISTS idx_audit_events_filters
                ON audit_events (event_type, actor_type, success, id DESC);

            CREATE INDEX IF NOT EXISTS idx_player_identity_mappings_updated_at
                ON player_identity_mappings (updated_at DESC, player_id ASC);

            CREATE INDEX IF NOT EXISTS idx_admin_sessions_token_hash
                ON admin_sessions (token_hash);

            CREATE INDEX IF NOT EXISTS idx_admin_sessions_expiry
                ON admin_sessions (expires_at, ended_at);
            """
        )
        self._db.commit()

    async def close(self) -> None:
        if self._db is None:
            return
        self._db.close()
        self._db = None

    def local_datetime(self, value: float | None = None) -> datetime:
        return self._local_datetime(value)

    def timestamp_ms(self, value: float | None = None) -> int:
        return self._to_timestamp_ms(value)

    async def record_player_activity(
        self,
        player_id: str,
        room_code: str,
        *,
        occurred_at: float | None = None,
    ) -> None:
        if not player_id:
            return

        stamp_ms = self._to_timestamp_ms(occurred_at)
        local_dt = self._local_datetime(occurred_at)
        local_date = local_dt.strftime("%Y-%m-%d")
        local_hour = local_dt.strftime("%Y-%m-%dT%H:00:00")

        async with self._lock:
            self._execute_many(
                [
                    (
                        """
                        INSERT INTO daily_player_activity (
                            local_date,
                            player_id,
                            room_code,
                            first_seen_at,
                            last_seen_at
                        ) VALUES (?, ?, ?, ?, ?)
                        ON CONFLICT(local_date, player_id, room_code) DO UPDATE SET
                            last_seen_at = excluded.last_seen_at
                        """,
                        (local_date, player_id, room_code, stamp_ms, stamp_ms),
                    ),
                    (
                        """
                        INSERT INTO hourly_player_activity (
                            local_hour,
                            player_id,
                            room_code,
                            first_seen_at,
                            last_seen_at
                        ) VALUES (?, ?, ?, ?, ?)
                        ON CONFLICT(local_hour, player_id, room_code) DO UPDATE SET
                            last_seen_at = excluded.last_seen_at
                        """,
                        (local_hour, player_id, room_code, stamp_ms, stamp_ms),
                    ),
                ],
            )

    async def record_audit_event(
        self,
        *,
        event_type: str,
        actor_type: str,
        actor_id: str | None = None,
        room_code: str | None = None,
        success: bool = True,
        remote_addr: str | None = None,
        detail: dict[str, Any] | None = None,
        occurred_at: float | None = None,
    ) -> None:
        stamp_ms = self._to_timestamp_ms(occurred_at)
        local_dt = self._local_datetime(occurred_at)
        detail_json = json.dumps(detail or {}, ensure_ascii=False, separators=(",", ":"))

        async with self._lock:
            self._execute(
                """
                INSERT INTO audit_events (
                    occurred_at,
                    local_date,
                    local_hour,
                    event_type,
                    actor_type,
                    actor_id,
                    room_code,
                    success,
                    remote_addr,
                    detail_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    stamp_ms,
                    local_dt.strftime("%Y-%m-%d"),
                    local_dt.strftime("%Y-%m-%dT%H:00:00"),
                    event_type,
                    actor_type,
                    actor_id,
                    room_code,
                    1 if success else 0,
                    remote_addr,
                    detail_json,
                ),
            )

    async def upsert_player_identity(
        self,
        player_id: str,
        username: str,
        *,
        occurred_at: float | None = None,
    ) -> bool:
        normalized_player_id = str(player_id or "").strip()
        normalized_username = str(username or "").strip()
        if not normalized_player_id or not normalized_username:
            return False

        current = await self._fetchone(
            """
            SELECT username
            FROM player_identity_mappings
            WHERE player_id = ?
            LIMIT 1
            """,
            (normalized_player_id,),
        )
        if current is not None and str(current["username"] or "").strip() == normalized_username:
            return False

        stamp_ms = self._to_timestamp_ms(occurred_at)
        async with self._lock:
            self._execute(
                """
                INSERT INTO player_identity_mappings (
                    player_id,
                    username,
                    created_at,
                    updated_at
                ) VALUES (?, ?, ?, ?)
                ON CONFLICT(player_id) DO UPDATE SET
                    username = excluded.username,
                    updated_at = excluded.updated_at
                """,
                (normalized_player_id, normalized_username, stamp_ms, stamp_ms),
            )
        return True

    async def query_player_identities(self) -> list[dict[str, Any]]:
        rows = await self._fetchall(
            """
            SELECT player_id, username, updated_at
            FROM player_identity_mappings
            ORDER BY updated_at DESC, player_id ASC
            """
        )
        return [
            {
                "playerId": str(row["player_id"]),
                "username": str(row["username"]),
                "updatedAt": int(row["updated_at"]),
            }
            for row in rows
        ]

    async def create_admin_session(
        self,
        *,
        actor_id: str,
        remote_addr: str | None,
        ttl_sec: int,
        occurred_at: float | None = None,
    ) -> tuple[dict[str, Any], str]:
        raw_token = secrets.token_urlsafe(32)
        session_id = secrets.token_hex(8)
        stamp_ms = self._to_timestamp_ms(occurred_at)
        expires_at = stamp_ms + max(1, int(ttl_sec)) * 1000
        token_hash = self.hash_session_token(raw_token)

        async with self._lock:
            self._execute(
                """
                INSERT INTO admin_sessions (
                    session_id,
                    token_hash,
                    actor_id,
                    remote_addr,
                    created_at,
                    last_seen_at,
                    expires_at,
                    ended_at,
                    end_reason
                ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL)
                """,
                (session_id, token_hash, actor_id, remote_addr, stamp_ms, stamp_ms, expires_at),
            )

        return (
            {
                "sessionId": session_id,
                "actorId": actor_id,
                "remoteAddr": remote_addr,
                "createdAt": stamp_ms,
                "lastSeenAt": stamp_ms,
                "expiresAt": expires_at,
                "endedAt": None,
                "endReason": None,
            },
            raw_token,
        )

    async def get_admin_session_by_token(self, raw_token: str) -> dict[str, Any] | None:
        if not isinstance(raw_token, str) or not raw_token:
            return None
        token_hash = self.hash_session_token(raw_token)
        row = await self._fetchone(
            """
            SELECT
                session_id,
                actor_id,
                remote_addr,
                created_at,
                last_seen_at,
                expires_at,
                ended_at,
                end_reason
            FROM admin_sessions
            WHERE token_hash = ?
            LIMIT 1
            """,
            (token_hash,),
        )
        if row is None:
            return None
        return self._serialize_admin_session_row(row)

    async def touch_admin_session(
        self,
        session_id: str,
        *,
        ttl_sec: int,
        occurred_at: float | None = None,
    ) -> dict[str, Any] | None:
        stamp_ms = self._to_timestamp_ms(occurred_at)
        expires_at = stamp_ms + max(1, int(ttl_sec)) * 1000
        async with self._lock:
            self._execute(
                """
                UPDATE admin_sessions
                SET last_seen_at = ?, expires_at = ?
                WHERE session_id = ? AND ended_at IS NULL
                """,
                (stamp_ms, expires_at, session_id),
            )
        return await self.get_admin_session_by_id(session_id)

    async def get_admin_session_by_id(self, session_id: str) -> dict[str, Any] | None:
        row = await self._fetchone(
            """
            SELECT
                session_id,
                actor_id,
                remote_addr,
                created_at,
                last_seen_at,
                expires_at,
                ended_at,
                end_reason
            FROM admin_sessions
            WHERE session_id = ?
            LIMIT 1
            """,
            (session_id,),
        )
        if row is None:
            return None
        return self._serialize_admin_session_row(row)

    async def end_admin_session(
        self,
        session_id: str,
        *,
        reason: str,
        occurred_at: float | None = None,
    ) -> dict[str, Any] | None:
        current = await self.get_admin_session_by_id(session_id)
        if current is None or current.get("endedAt") is not None:
            return None

        stamp_ms = self._to_timestamp_ms(occurred_at)
        async with self._lock:
            self._execute(
                """
                UPDATE admin_sessions
                SET ended_at = ?, end_reason = ?
                WHERE session_id = ? AND ended_at IS NULL
                """,
                (stamp_ms, reason, session_id),
            )
        ended = await self.get_admin_session_by_id(session_id)
        return ended

    async def expire_admin_sessions(self, *, occurred_at: float | None = None) -> list[dict[str, Any]]:
        stamp_ms = self._to_timestamp_ms(occurred_at)
        rows = await self._fetchall(
            """
            SELECT
                session_id,
                actor_id,
                remote_addr,
                created_at,
                last_seen_at,
                expires_at,
                ended_at,
                end_reason
            FROM admin_sessions
            WHERE ended_at IS NULL AND expires_at <= ?
            ORDER BY expires_at ASC
            """,
            (stamp_ms,),
        )
        if not rows:
            return []

        async with self._lock:
            for row in rows:
                self._execute(
                    """
                    UPDATE admin_sessions
                    SET ended_at = ?, end_reason = 'expired'
                    WHERE session_id = ? AND ended_at IS NULL
                    """,
                    (stamp_ms, row["session_id"]),
                )
        return [
            {
                **self._serialize_admin_session_row(row),
                "endedAt": stamp_ms,
                "endReason": "expired",
            }
            for row in rows
        ]

    async def apply_traffic_increments(
        self,
        *,
        minute_increments: dict[tuple[str, str, str, str], int],
        hourly_increments: dict[tuple[str, str, str, str], int],
        daily_increments: dict[tuple[str, str, str, str], int],
    ) -> None:
        statements: list[tuple[str, tuple[Any, ...]]] = []
        for (scope, local_minute, channel, direction), amount in minute_increments.items():
            if int(amount) <= 0:
                continue
            statements.append(
                (
                    self._traffic_upsert_sql(scope=scope, bucket_kind="minute"),
                    (local_minute, channel, direction, int(amount)),
                )
            )
        for (scope, local_hour, channel, direction), amount in hourly_increments.items():
            if int(amount) <= 0:
                continue
            statements.append(
                (
                    self._traffic_upsert_sql(scope=scope, bucket_kind="hourly"),
                    (local_hour, channel, direction, int(amount)),
                )
            )
        for (scope, local_date, channel, direction), amount in daily_increments.items():
            if int(amount) <= 0:
                continue
            statements.append(
                (
                    self._traffic_upsert_sql(scope=scope, bucket_kind="daily"),
                    (local_date, channel, direction, int(amount)),
                )
            )
        if not statements:
            return

        async with self._lock:
            self._execute_many(statements)

    async def query_daily_metrics(self, *, days: int, room_code: str | None = None) -> dict[str, Any]:
        return await self.query_daily_metrics_with_start(days=days, room_code=room_code, start_date=None)

    async def query_daily_metrics_with_start(
        self,
        *,
        days: int,
        room_code: str | None = None,
        start_date: str | None = None,
    ) -> dict[str, Any]:
        normalized_start_date = self.normalize_local_date(start_date)
        if normalized_start_date is not None:
            start_dt = datetime.strptime(normalized_start_date, "%Y-%m-%d")
        else:
            end_dt = self._local_datetime()
            start_dt = (end_dt - timedelta(days=max(days - 1, 0))).replace(hour=0, minute=0, second=0, microsecond=0)
        labels = [(start_dt + timedelta(days=index)).strftime("%Y-%m-%d") for index in range(days)]
        counts = {label: 0 for label in labels}

        if room_code:
            rows = await self._fetchall(
                """
                SELECT local_date, COUNT(DISTINCT player_id) AS active_players
                FROM daily_player_activity
                WHERE room_code = ? AND local_date BETWEEN ? AND ?
                GROUP BY local_date
                ORDER BY local_date
                """,
                (room_code, labels[0], labels[-1]),
            )
        else:
            rows = await self._fetchall(
                """
                SELECT local_date, COUNT(DISTINCT player_id) AS active_players
                FROM daily_player_activity
                WHERE local_date BETWEEN ? AND ?
                GROUP BY local_date
                ORDER BY local_date
                """,
                (labels[0], labels[-1]),
            )

        for row in rows:
            counts[str(row["local_date"])] = int(row["active_players"])

        return {
            "timezone": self.timezone_label,
            "roomCode": room_code,
            "days": days,
            "startDate": normalized_start_date,
            "items": [{"bucket": label, "label": label, "activePlayers": counts[label]} for label in labels],
        }

    async def query_hourly_metrics(self, *, hours: int, room_code: str | None = None) -> dict[str, Any]:
        return await self.query_hourly_metrics_with_start(hours=hours, room_code=room_code, start_at=None)

    async def query_hourly_metrics_with_start(
        self,
        *,
        hours: int,
        room_code: str | None = None,
        start_at: str | None = None,
    ) -> dict[str, Any]:
        normalized_start_at = self.normalize_local_datetime(start_at)
        if normalized_start_at is not None:
            start_dt = self._floor_datetime(
                datetime.strptime(normalized_start_at, "%Y-%m-%dT%H:%M:%S"),
                60 * 60,
            )
        else:
            end_dt = self._local_datetime().replace(minute=0, second=0, microsecond=0)
            start_dt = end_dt - timedelta(hours=max(hours - 1, 0))
        labels = [(start_dt + timedelta(hours=index)).strftime("%Y-%m-%dT%H:00:00") for index in range(hours)]
        counts = {label: 0 for label in labels}

        if room_code:
            rows = await self._fetchall(
                """
                SELECT local_hour, COUNT(DISTINCT player_id) AS active_players
                FROM hourly_player_activity
                WHERE room_code = ? AND local_hour BETWEEN ? AND ?
                GROUP BY local_hour
                ORDER BY local_hour
                """,
                (room_code, labels[0], labels[-1]),
            )
        else:
            rows = await self._fetchall(
                """
                SELECT local_hour, COUNT(DISTINCT player_id) AS active_players
                FROM hourly_player_activity
                WHERE local_hour BETWEEN ? AND ?
                GROUP BY local_hour
                ORDER BY local_hour
                """,
                (labels[0], labels[-1]),
            )

        for row in rows:
            counts[str(row["local_hour"])] = int(row["active_players"])

        return {
            "timezone": self.timezone_label,
            "roomCode": room_code,
            "hours": hours,
            "startAt": self._format_local_datetime(start_dt) if normalized_start_at is not None else None,
            "items": [{"bucket": label, "label": label, "activePlayers": counts[label]} for label in labels],
        }

    async def query_hourly_traffic(
        self,
        *,
        hours: int,
        scope: str = "application",
        start_at: str | None = None,
    ) -> dict[str, Any]:
        normalized_start_at = self.normalize_local_datetime(start_at)
        if normalized_start_at is not None:
            start_dt = self._floor_datetime(
                datetime.strptime(normalized_start_at, "%Y-%m-%dT%H:%M:%S"),
                60 * 60,
            )
        else:
            end_dt = self._local_datetime().replace(minute=0, second=0, microsecond=0)
            start_dt = end_dt - timedelta(hours=max(hours - 1, 0))
        labels = [(start_dt + timedelta(hours=index)).strftime("%Y-%m-%dT%H:00:00") for index in range(hours)]
        table_name = self._traffic_table_name(scope=scope, bucket_kind="hourly")
        rows = await self._fetchall(
            f"""
            SELECT local_hour, channel, direction, bytes
            FROM {table_name}
            WHERE local_hour BETWEEN ? AND ?
            ORDER BY local_hour ASC
            """,
            (labels[0], labels[-1]),
        )
        return self._build_traffic_payload(
            labels=labels,
            rows=rows,
            hours=hours,
            start_at=self._format_local_datetime(start_dt) if normalized_start_at is not None else None,
        )

    async def query_daily_traffic(self, *, days: int, scope: str = "application") -> dict[str, Any]:
        end_dt = self._local_datetime()
        start_dt = (end_dt - timedelta(days=max(days - 1, 0))).replace(hour=0, minute=0, second=0, microsecond=0)
        labels = [(start_dt + timedelta(days=index)).strftime("%Y-%m-%d") for index in range(days)]
        table_name = self._traffic_table_name(scope=scope, bucket_kind="daily")
        rows = await self._fetchall(
            f"""
            SELECT local_date, channel, direction, bytes
            FROM {table_name}
            WHERE local_date BETWEEN ? AND ?
            ORDER BY local_date ASC
            """,
            (labels[0], labels[-1]),
        )
        return self._build_traffic_payload(labels=labels, rows=rows, days=days)

    async def query_traffic_history(
        self,
        *,
        range_preset: str,
        granularity: str,
        scope: str = "application",
        start_at: str | None = None,
    ) -> dict[str, Any]:
        normalized_range, normalized_granularity = self.normalize_traffic_history_params(range_preset, granularity)
        bucket_seconds = TRAFFIC_GRANULARITY_SECONDS[normalized_granularity]
        bucket_count = TRAFFIC_RANGE_SECONDS[normalized_range] // bucket_seconds
        normalized_start_at = self.normalize_local_datetime(start_at)
        if normalized_granularity in {"1m", "5m", "15m"}:
            return await self._query_minute_traffic_history(
                scope=scope,
                range_preset=normalized_range,
                granularity=normalized_granularity,
                bucket_seconds=bucket_seconds,
                bucket_count=bucket_count,
                start_at=normalized_start_at,
            )
        if normalized_granularity == "1h":
            return await self._query_hourly_traffic_history(
                scope=scope,
                range_preset=normalized_range,
                granularity=normalized_granularity,
                bucket_seconds=bucket_seconds,
                bucket_count=bucket_count,
                start_at=normalized_start_at,
            )
        return await self._query_daily_traffic_history(
            scope=scope,
            range_preset=normalized_range,
            granularity=normalized_granularity,
            bucket_seconds=bucket_seconds,
            bucket_count=bucket_count,
            start_at=normalized_start_at,
        )

    async def query_audit_events(
        self,
        *,
        limit: int,
        before_id: int | None = None,
        event_type: str | None = None,
        actor_type: str | None = None,
        actor_types: list[str] | tuple[str, ...] | None = None,
        success: bool | None = None,
    ) -> dict[str, Any]:
        sql = [
            """
            SELECT
                id,
                occurred_at,
                local_date,
                local_hour,
                event_type,
                actor_type,
                actor_id,
                room_code,
                success,
                remote_addr,
                detail_json
            FROM audit_events
            WHERE 1 = 1
            """
        ]
        params: list[Any] = []

        if before_id is not None:
            sql.append("AND id < ?")
            params.append(before_id)
        if event_type:
            sql.append("AND event_type = ?")
            params.append(event_type)
        normalized_actor_types = [item for item in (actor_types or []) if isinstance(item, str) and item.strip()]
        if actor_type and not normalized_actor_types:
            sql.append("AND actor_type = ?")
            params.append(actor_type)
        elif normalized_actor_types:
            placeholders = ", ".join("?" for _ in normalized_actor_types)
            sql.append(f"AND actor_type IN ({placeholders})")
            params.extend(normalized_actor_types)
        if success is not None:
            sql.append("AND success = ?")
            params.append(1 if success else 0)

        sql.append("ORDER BY id DESC LIMIT ?")
        params.append(limit)
        rows = await self._fetchall("\n".join(sql), tuple(params))
        event_type_rows = await self._fetchall(
            """
            SELECT DISTINCT event_type
            FROM audit_events
            ORDER BY event_type ASC
            """
        )
        player_identity_mappings = await self.query_player_identities()
        identity_by_player_id = {
            item["playerId"]: item["username"]
            for item in player_identity_mappings
            if item["playerId"] and item["username"]
        }

        items = []
        for row in rows:
            detail_json = row["detail_json"]
            try:
                detail = json.loads(detail_json) if detail_json else {}
            except json.JSONDecodeError:
                detail = {"raw": detail_json}
            items.append(
                {
                    "id": int(row["id"]),
                    "occurredAt": int(row["occurred_at"]),
                    "localDate": row["local_date"],
                    "localHour": row["local_hour"],
                    "eventType": row["event_type"],
                    "actorType": row["actor_type"],
                    "actorId": row["actor_id"],
                    "resolvedActorName": (
                        identity_by_player_id.get(str(row["actor_id"]))
                        if row["actor_type"] == "player" and row["actor_id"]
                        else None
                    ),
                    "roomCode": row["room_code"],
                    "success": bool(row["success"]),
                    "remoteAddr": row["remote_addr"],
                    "detail": detail,
                }
            )

        next_before_id = items[-1]["id"] if items else None
        return {
            "items": items,
            "playerIdentityMappings": player_identity_mappings,
            "nextBeforeId": next_before_id,
            "limit": limit,
            "availableEventTypes": [str(row["event_type"]) for row in event_type_rows if row["event_type"]],
        }

    async def cleanup_retention(self) -> dict[str, int]:
        now = self._local_datetime()
        daily_cutoff = (now - timedelta(days=max(self.config.daily_retention_days, 1))).strftime("%Y-%m-%d")
        hourly_cutoff = (now - timedelta(days=max(self.config.hourly_retention_days, 1))).strftime("%Y-%m-%dT%H:00:00")
        minute_cutoff = (now - timedelta(days=max(self.config.hourly_retention_days, 1))).strftime("%Y-%m-%dT%H:%M:00")
        audit_cutoff = (now - timedelta(days=max(self.config.audit_retention_days, 1))).strftime("%Y-%m-%d")
        session_cutoff_ms = int(
            (
                now - timedelta(days=max(self.config.audit_retention_days, 1))
            ).astimezone(timezone.utc).timestamp()
            * 1000
        )

        async with self._lock:
            return self._cleanup_retention_sync(
                daily_cutoff=daily_cutoff,
                hourly_cutoff=hourly_cutoff,
                minute_cutoff=minute_cutoff,
                audit_cutoff=audit_cutoff,
                session_cutoff_ms=session_cutoff_ms,
            )

    @property
    def timezone_label(self) -> str:
        local_now = self._local_datetime()
        offset = local_now.utcoffset() or timedelta()
        total_minutes = int(offset.total_seconds() // 60)
        sign = "+" if total_minutes >= 0 else "-"
        absolute_minutes = abs(total_minutes)
        hours, minutes = divmod(absolute_minutes, 60)
        tz_name = local_now.tzname() or "local"
        return f"{tz_name} (UTC{sign}{hours:02d}:{minutes:02d})"

    @property
    def masked_db_path(self) -> str:
        path = Path(self.config.db_path)
        parent_name = path.parent.name if path.parent.name else ""
        if parent_name:
            return f".../{parent_name}/{path.name}"
        return f".../{path.name}"

    @staticmethod
    def hash_session_token(raw_token: str) -> str:
        return hashlib.sha256(raw_token.encode("utf-8")).hexdigest()

    async def _fetchall(self, sql: str, params: tuple[Any, ...] = ()) -> list[sqlite3.Row]:
        async with self._lock:
            return self._fetchall_sync(sql, params)

    async def _fetchone(self, sql: str, params: tuple[Any, ...] = ()) -> sqlite3.Row | None:
        async with self._lock:
            db = self._require_db()
            cursor = db.execute(sql, params)
            try:
                return cursor.fetchone()
            finally:
                cursor.close()

    def _fetchall_sync(self, sql: str, params: tuple[Any, ...]) -> list[sqlite3.Row]:
        db = self._require_db()
        cursor = db.execute(sql, params)
        try:
            return cursor.fetchall()
        finally:
            cursor.close()

    def _execute(self, sql: str, params: tuple[Any, ...]) -> None:
        db = self._require_db()
        cursor = db.execute(sql, params)
        cursor.close()
        db.commit()

    def _execute_many(self, statements: list[tuple[str, tuple[Any, ...]]]) -> None:
        db = self._require_db()
        for sql, params in statements:
            cursor = db.execute(sql, params)
            cursor.close()
        db.commit()

    def _cleanup_retention_sync(
        self,
        *,
        daily_cutoff: str,
        hourly_cutoff: str,
        minute_cutoff: str,
        audit_cutoff: str,
        session_cutoff_ms: int,
    ) -> dict[str, int]:
        db = self._require_db()
        daily_cursor = db.execute("DELETE FROM daily_player_activity WHERE local_date < ?", (daily_cutoff,))
        hourly_cursor = db.execute("DELETE FROM hourly_player_activity WHERE local_hour < ?", (hourly_cutoff,))
        minute_traffic_cursor = db.execute("DELETE FROM minute_traffic_bytes WHERE local_minute < ?", (minute_cutoff,))
        minute_wire_traffic_cursor = db.execute("DELETE FROM minute_wire_traffic_bytes WHERE local_minute < ?", (minute_cutoff,))
        hourly_traffic_cursor = db.execute("DELETE FROM hourly_traffic_bytes WHERE local_hour < ?", (hourly_cutoff,))
        hourly_wire_traffic_cursor = db.execute("DELETE FROM hourly_wire_traffic_bytes WHERE local_hour < ?", (hourly_cutoff,))
        daily_traffic_cursor = db.execute("DELETE FROM daily_traffic_bytes WHERE local_date < ?", (daily_cutoff,))
        daily_wire_traffic_cursor = db.execute("DELETE FROM daily_wire_traffic_bytes WHERE local_date < ?", (daily_cutoff,))
        audit_cursor = db.execute("DELETE FROM audit_events WHERE local_date < ?", (audit_cutoff,))
        session_cursor = db.execute(
            """
            DELETE FROM admin_sessions
            WHERE COALESCE(ended_at, expires_at) < ?
            """,
            (session_cutoff_ms,),
        )
        try:
            db.commit()
            return {
                "dailyDeleted": int(daily_cursor.rowcount or 0),
                "hourlyDeleted": int(hourly_cursor.rowcount or 0),
                "minuteTrafficDeleted": int(minute_traffic_cursor.rowcount or 0),
                "minuteWireTrafficDeleted": int(minute_wire_traffic_cursor.rowcount or 0),
                "hourlyTrafficDeleted": int(hourly_traffic_cursor.rowcount or 0),
                "hourlyWireTrafficDeleted": int(hourly_wire_traffic_cursor.rowcount or 0),
                "dailyTrafficDeleted": int(daily_traffic_cursor.rowcount or 0),
                "dailyWireTrafficDeleted": int(daily_wire_traffic_cursor.rowcount or 0),
                "auditDeleted": int(audit_cursor.rowcount or 0),
                "sessionDeleted": int(session_cursor.rowcount or 0),
            }
        finally:
            daily_cursor.close()
            hourly_cursor.close()
            minute_traffic_cursor.close()
            minute_wire_traffic_cursor.close()
            hourly_traffic_cursor.close()
            hourly_wire_traffic_cursor.close()
            daily_traffic_cursor.close()
            daily_wire_traffic_cursor.close()
            audit_cursor.close()
            session_cursor.close()

    def _require_db(self) -> sqlite3.Connection:
        if self._db is None:
            raise RuntimeError("AdminStore is not initialized")
        return self._db

    @classmethod
    def normalize_traffic_history_params(cls, range_preset: str, granularity: str) -> tuple[str, str]:
        normalized_range = str(range_preset or "").strip()
        normalized_granularity = str(granularity or "").strip()
        allowed = ALLOWED_TRAFFIC_GRANULARITIES.get(normalized_range)
        if allowed is None or normalized_granularity not in allowed:
            raise ValueError("invalid_traffic_granularity")
        return normalized_range, normalized_granularity

    @staticmethod
    def _traffic_table_name(*, scope: str, bucket_kind: str) -> str:
        if scope == "application":
            mapping = {
                "minute": "minute_traffic_bytes",
                "hourly": "hourly_traffic_bytes",
                "daily": "daily_traffic_bytes",
            }
        elif scope == "wire":
            mapping = {
                "minute": "minute_wire_traffic_bytes",
                "hourly": "hourly_wire_traffic_bytes",
                "daily": "daily_wire_traffic_bytes",
            }
        else:
            raise ValueError(f"invalid_traffic_scope:{scope}")
        table_name = mapping.get(bucket_kind)
        if table_name is None:
            raise ValueError(f"invalid_traffic_bucket_kind:{bucket_kind}")
        return table_name

    def _traffic_upsert_sql(self, *, scope: str, bucket_kind: str) -> str:
        table_name = self._traffic_table_name(scope=scope, bucket_kind=bucket_kind)
        bucket_column = {
            "minute": "local_minute",
            "hourly": "local_hour",
            "daily": "local_date",
        }[bucket_kind]
        return f"""
            INSERT INTO {table_name} ({bucket_column}, channel, direction, bytes)
            VALUES (?, ?, ?, ?)
            ON CONFLICT({bucket_column}, channel, direction) DO UPDATE SET
                bytes = bytes + excluded.bytes
        """

    async def _query_minute_traffic_history(
        self,
        *,
        scope: str,
        range_preset: str,
        granularity: str,
        bucket_seconds: int,
        bucket_count: int,
        start_at: str | None = None,
    ) -> dict[str, Any]:
        if start_at is not None:
            start_bucket = self._floor_datetime(
                datetime.strptime(start_at, "%Y-%m-%dT%H:%M:%S"),
                bucket_seconds,
            )
            end_bucket = start_bucket + timedelta(seconds=bucket_seconds * max(bucket_count - 1, 0))
            query_end_dt = end_bucket + timedelta(seconds=max(bucket_seconds - 60, 0))
        else:
            current_minute = self._local_datetime().replace(second=0, microsecond=0)
            end_bucket = self._floor_datetime(current_minute, bucket_seconds)
            start_bucket = end_bucket - timedelta(seconds=bucket_seconds * max(bucket_count - 1, 0))
            query_end_dt = current_minute
        query_start = start_bucket.strftime("%Y-%m-%dT%H:%M:00")
        query_end = query_end_dt.strftime("%Y-%m-%dT%H:%M:00")
        labels = self._build_traffic_labels(start_bucket=start_bucket, bucket_count=bucket_count, bucket_seconds=bucket_seconds)
        table_name = self._traffic_table_name(scope=scope, bucket_kind="minute")
        rows = await self._fetchall(
            f"""
            SELECT local_minute, channel, direction, bytes
            FROM {table_name}
            WHERE local_minute BETWEEN ? AND ?
            ORDER BY local_minute ASC
            """,
            (query_start, query_end),
        )
        aggregated: list[dict[str, Any]] = []
        grouped: dict[tuple[str, str, str], int] = {}
        for row in rows:
            minute_dt = datetime.strptime(str(row["local_minute"]), "%Y-%m-%dT%H:%M:00")
            bucket_dt = self._floor_datetime(minute_dt, bucket_seconds)
            bucket_label = self._format_traffic_bucket_label(bucket_dt, bucket_seconds)
            key = (bucket_label, str(row["channel"]), str(row["direction"]))
            grouped[key] = grouped.get(key, 0) + int(row["bytes"] or 0)
        for (bucket_label, channel, direction), amount in grouped.items():
            aggregated.append(
                {
                    "local_hour": bucket_label,
                    "channel": channel,
                    "direction": direction,
                    "bytes": amount,
                }
            )
        return self._build_traffic_payload(
            labels=labels,
            rows=aggregated,
            range_preset=range_preset,
            granularity=granularity,
            bucket_seconds=bucket_seconds,
            start_at=self._format_local_datetime(start_bucket) if start_at is not None else None,
        )

    async def _query_hourly_traffic_history(
        self,
        *,
        scope: str,
        range_preset: str,
        granularity: str,
        bucket_seconds: int,
        bucket_count: int,
        start_at: str | None = None,
    ) -> dict[str, Any]:
        if start_at is not None:
            start_dt = self._floor_datetime(
                datetime.strptime(start_at, "%Y-%m-%dT%H:%M:%S"),
                bucket_seconds,
            )
        else:
            end_dt = self._local_datetime().replace(minute=0, second=0, microsecond=0)
            start_dt = end_dt - timedelta(hours=max(bucket_count - 1, 0))
        labels = self._build_traffic_labels(start_bucket=start_dt, bucket_count=bucket_count, bucket_seconds=bucket_seconds)
        table_name = self._traffic_table_name(scope=scope, bucket_kind="hourly")
        rows = await self._fetchall(
            f"""
            SELECT local_hour, channel, direction, bytes
            FROM {table_name}
            WHERE local_hour BETWEEN ? AND ?
            ORDER BY local_hour ASC
            """,
            (labels[0], labels[-1]),
        )
        return self._build_traffic_payload(
            labels=labels,
            rows=rows,
            range_preset=range_preset,
            granularity=granularity,
            bucket_seconds=bucket_seconds,
            start_at=self._format_local_datetime(start_dt) if start_at is not None else None,
        )

    async def _query_daily_traffic_history(
        self,
        *,
        scope: str,
        range_preset: str,
        granularity: str,
        bucket_seconds: int,
        bucket_count: int,
        start_at: str | None = None,
    ) -> dict[str, Any]:
        if start_at is not None:
            start_dt = self._floor_datetime(
                datetime.strptime(start_at, "%Y-%m-%dT%H:%M:%S"),
                bucket_seconds,
            )
        else:
            end_dt = self._local_datetime().replace(hour=0, minute=0, second=0, microsecond=0)
            start_dt = end_dt - timedelta(days=max(bucket_count - 1, 0))
        labels = self._build_traffic_labels(start_bucket=start_dt, bucket_count=bucket_count, bucket_seconds=bucket_seconds)
        table_name = self._traffic_table_name(scope=scope, bucket_kind="daily")
        rows = await self._fetchall(
            f"""
            SELECT local_date, channel, direction, bytes
            FROM {table_name}
            WHERE local_date BETWEEN ? AND ?
            ORDER BY local_date ASC
            """,
            (labels[0], labels[-1]),
        )
        return self._build_traffic_payload(
            labels=labels,
            rows=rows,
            range_preset=range_preset,
            granularity=granularity,
            bucket_seconds=bucket_seconds,
            start_at=self._format_local_datetime(start_dt) if start_at is not None else None,
        )

    def _build_traffic_payload(
        self,
        *,
        labels: list[str],
        rows: list[sqlite3.Row] | list[dict[str, Any]],
        hours: int | None = None,
        days: int | None = None,
        range_preset: str | None = None,
        granularity: str | None = None,
        bucket_seconds: int | None = None,
        start_at: str | None = None,
    ) -> dict[str, Any]:
        items = {
            label: {
                "bucket": label,
                "label": label,
                "playerIngressBytes": 0,
                "playerEgressBytes": 0,
                "webMapIngressBytes": 0,
                "webMapEgressBytes": 0,
                "totalIngressBytes": 0,
                "totalEgressBytes": 0,
                "totalBytes": 0,
            }
            for label in labels
        }
        total_ingress = 0
        total_egress = 0

        for row in rows:
            label = str(row["local_hour"] if "local_hour" in row.keys() else row["local_date"])
            if label not in items:
                continue
            channel = str(row["channel"])
            direction = str(row["direction"])
            amount = int(row["bytes"] or 0)
            series_key = self._traffic_series_key(channel, direction)
            if series_key is None:
                continue
            item = items[label]
            item[series_key] += amount
            if direction == "ingress":
                item["totalIngressBytes"] += amount
                total_ingress += amount
            else:
                item["totalEgressBytes"] += amount
                total_egress += amount
            item["totalBytes"] += amount

        payload: dict[str, Any] = {
            "timezone": self.timezone_label,
            "items": [items[label] for label in labels],
            "totalIngressBytes": total_ingress,
            "totalEgressBytes": total_egress,
            "totalBytes": total_ingress + total_egress,
        }
        if hours is not None:
            payload["hours"] = hours
        if days is not None:
            payload["days"] = days
        if range_preset is not None:
            payload["range"] = range_preset
        if granularity is not None:
            payload["granularity"] = granularity
        if bucket_seconds is not None:
            payload["bucketSeconds"] = bucket_seconds
        if start_at is not None:
            payload["startAt"] = start_at
        return payload

    @staticmethod
    def _floor_datetime(value: datetime, bucket_seconds: int) -> datetime:
        if bucket_seconds >= 24 * 60 * 60:
            return value.replace(hour=0, minute=0, second=0, microsecond=0)
        if bucket_seconds >= 60 * 60:
            return value.replace(minute=0, second=0, microsecond=0)
        bucket_minutes = max(bucket_seconds // 60, 1)
        floored_minute = (value.minute // bucket_minutes) * bucket_minutes
        return value.replace(minute=floored_minute, second=0, microsecond=0)

    def _build_traffic_labels(self, *, start_bucket: datetime, bucket_count: int, bucket_seconds: int) -> list[str]:
        return [
            self._format_traffic_bucket_label(start_bucket + timedelta(seconds=bucket_seconds * index), bucket_seconds)
            for index in range(bucket_count)
        ]

    @staticmethod
    def normalize_local_date(value: str | None) -> str | None:
        if value is None:
            return None
        text = str(value).strip()
        if not text:
            return None
        return datetime.strptime(text, "%Y-%m-%d").strftime("%Y-%m-%d")

    @staticmethod
    def normalize_local_datetime(value: str | None) -> str | None:
        if value is None:
            return None
        text = str(value).strip()
        if not text:
            return None
        normalized = text.replace("Z", "+00:00")
        dt = datetime.fromisoformat(normalized)
        if dt.tzinfo is not None:
            local_tz = datetime.now().astimezone().tzinfo
            dt = dt.astimezone(local_tz)
        return dt.replace(tzinfo=None, microsecond=0).strftime("%Y-%m-%dT%H:%M:%S")

    @staticmethod
    def _format_traffic_bucket_label(value: datetime, bucket_seconds: int) -> str:
        if bucket_seconds >= 24 * 60 * 60:
            return value.strftime("%Y-%m-%d")
        return value.strftime("%Y-%m-%dT%H:%M:00")

    @staticmethod
    def _format_local_datetime(value: datetime) -> str:
        return value.replace(microsecond=0).strftime("%Y-%m-%dT%H:%M:%S")

    @staticmethod
    def _traffic_series_key(channel: str, direction: str) -> str | None:
        mapping = {
            ("player", "ingress"): "playerIngressBytes",
            ("player", "egress"): "playerEgressBytes",
            ("web_map", "ingress"): "webMapIngressBytes",
            ("web_map", "egress"): "webMapEgressBytes",
        }
        return mapping.get((channel, direction))

    @staticmethod
    def _serialize_admin_session_row(row: sqlite3.Row) -> dict[str, Any]:
        return {
            "sessionId": str(row["session_id"]),
            "actorId": str(row["actor_id"]),
            "remoteAddr": row["remote_addr"],
            "createdAt": int(row["created_at"]),
            "lastSeenAt": int(row["last_seen_at"]),
            "expiresAt": int(row["expires_at"]),
            "endedAt": int(row["ended_at"]) if row["ended_at"] is not None else None,
            "endReason": row["end_reason"],
        }

    @staticmethod
    def _to_timestamp_ms(value: float | None) -> int:
        if value is None:
            return int(datetime.now(tz=timezone.utc).timestamp() * 1000)
        return int(float(value) * 1000)

    @staticmethod
    def _local_datetime(value: float | None = None) -> datetime:
        if value is None:
            return datetime.now().astimezone()
        return datetime.fromtimestamp(float(value), tz=timezone.utc).astimezone()
