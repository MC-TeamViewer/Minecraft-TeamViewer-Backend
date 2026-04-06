from __future__ import annotations

import asyncio
import json
import sqlite3
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


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
            """
        )
        self._db.commit()

    async def close(self) -> None:
        if self._db is None:
            return
        self._db.close()
        self._db = None

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

    async def query_daily_metrics(self, *, days: int, room_code: str | None = None) -> dict[str, Any]:
        end_dt = self._local_datetime()
        start_dt = (end_dt - timedelta(days=max(days - 1, 0))).replace(hour=0, minute=0, second=0, microsecond=0)
        labels = [
            (start_dt + timedelta(days=index)).strftime("%Y-%m-%d")
            for index in range(days)
        ]
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
            "items": [
                {"bucket": label, "label": label, "activePlayers": counts[label]}
                for label in labels
            ],
        }

    async def query_hourly_metrics(self, *, hours: int, room_code: str | None = None) -> dict[str, Any]:
        end_dt = self._local_datetime().replace(minute=0, second=0, microsecond=0)
        start_dt = end_dt - timedelta(hours=max(hours - 1, 0))
        labels = [
            (start_dt + timedelta(hours=index)).strftime("%Y-%m-%dT%H:00:00")
            for index in range(hours)
        ]
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
            "items": [
                {"bucket": label, "label": label, "activePlayers": counts[label]}
                for label in labels
            ],
        }

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
                    "roomCode": row["room_code"],
                    "success": bool(row["success"]),
                    "remoteAddr": row["remote_addr"],
                    "detail": detail,
                }
            )

        next_before_id = items[-1]["id"] if items else None
        return {
            "items": items,
            "nextBeforeId": next_before_id,
            "limit": limit,
            "availableEventTypes": [str(row["event_type"]) for row in event_type_rows if row["event_type"]],
        }

    async def cleanup_retention(self) -> dict[str, int]:
        now = self._local_datetime()
        daily_cutoff = (now - timedelta(days=max(self.config.daily_retention_days, 1))).strftime("%Y-%m-%d")
        hourly_cutoff = (now - timedelta(days=max(self.config.hourly_retention_days, 1))).strftime("%Y-%m-%dT%H:00:00")
        audit_cutoff = (now - timedelta(days=max(self.config.audit_retention_days, 1))).strftime("%Y-%m-%d")

        async with self._lock:
            return self._cleanup_retention_sync(
                daily_cutoff,
                hourly_cutoff,
                audit_cutoff,
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

    async def _fetchall(self, sql: str, params: tuple[Any, ...] = ()) -> list[sqlite3.Row]:
        async with self._lock:
            return self._fetchall_sync(sql, params)

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
        daily_cutoff: str,
        hourly_cutoff: str,
        audit_cutoff: str,
    ) -> dict[str, int]:
        db = self._require_db()
        daily_cursor = db.execute("DELETE FROM daily_player_activity WHERE local_date < ?", (daily_cutoff,))
        hourly_cursor = db.execute("DELETE FROM hourly_player_activity WHERE local_hour < ?", (hourly_cutoff,))
        audit_cursor = db.execute("DELETE FROM audit_events WHERE local_date < ?", (audit_cutoff,))
        try:
            db.commit()
            return {
                "dailyDeleted": int(daily_cursor.rowcount or 0),
                "hourlyDeleted": int(hourly_cursor.rowcount or 0),
                "auditDeleted": int(audit_cursor.rowcount or 0),
            }
        finally:
            daily_cursor.close()
            hourly_cursor.close()
            audit_cursor.close()

    def _require_db(self) -> sqlite3.Connection:
        if self._db is None:
            raise RuntimeError("AdminStore is not initialized")
        return self._db

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
