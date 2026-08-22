from __future__ import annotations

import asyncio
import hashlib
import json
import sqlite3
import time
import unicodedata
import uuid as uuid_lib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(slots=True)
class TabHistoryStoreConfig:
    db_path: str
    retention_days: int = 400
    delta_retention_days: int = 30
    observation_update_interval_sec: int = 300
    max_formatted_text_spans: int = 64
    max_formatted_text_utf8_bytes: int = 4096


class TabHistoryStore:
    """Persistent latest-known Tab label mirror, partitioned by room."""

    def __init__(self, config: TabHistoryStoreConfig) -> None:
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
            CREATE TABLE IF NOT EXISTS tab_history_heads (
                room_code TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                digest_sha256 BLOB NOT NULL,
                record_count INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tab_history_entries (
                room_code TEXT NOT NULL,
                player_uuid TEXT NOT NULL,
                normalized_name TEXT,
                player_json TEXT NOT NULL,
                label_signature TEXT NOT NULL,
                label_first_observed_at INTEGER NOT NULL,
                last_observed_at INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                etag_sha256 BLOB NOT NULL,
                PRIMARY KEY (room_code, player_uuid)
            );

            CREATE TABLE IF NOT EXISTS tab_history_deltas (
                room_code TEXT NOT NULL,
                revision INTEGER NOT NULL,
                player_uuid TEXT NOT NULL,
                operation TEXT NOT NULL,
                entry_json TEXT,
                occurred_at INTEGER NOT NULL,
                PRIMARY KEY (room_code, revision, player_uuid)
            );

            CREATE TABLE IF NOT EXISTS tab_history_revisions (
                room_code TEXT NOT NULL,
                revision INTEGER NOT NULL,
                digest_sha256 BLOB NOT NULL,
                record_count INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (room_code, revision)
            );

            CREATE INDEX IF NOT EXISTS idx_tab_history_entries_name
                ON tab_history_entries (room_code, normalized_name);
            CREATE INDEX IF NOT EXISTS idx_tab_history_entries_last_observed
                ON tab_history_entries (last_observed_at);
            CREATE INDEX IF NOT EXISTS idx_tab_history_deltas_time
                ON tab_history_deltas (occurred_at);
            """
        )
        self._db.commit()

    async def close(self) -> None:
        if self._db is None:
            return
        self._db.close()
        self._db = None

    @staticmethod
    def normalize_uuid(value: Any) -> str | None:
        text = str(value or "").strip()
        if not text:
            return None
        try:
            return str(uuid_lib.UUID(text))
        except (ValueError, AttributeError):
            return None

    @staticmethod
    def normalize_name(value: Any) -> str:
        return unicodedata.normalize("NFKC", str(value or "").strip()).casefold()

    @staticmethod
    def _optional_text(value: Any, max_bytes: int = 4096) -> str | None:
        if value is None:
            return None
        text = str(value)
        if len(text.encode("utf-8")) > max_bytes:
            return text.encode("utf-8")[:max_bytes].decode("utf-8", errors="ignore")
        return text

    def _sanitize_formatted_text(self, value: Any, plain_fallback: str | None) -> dict[str, Any] | None:
        if not isinstance(value, dict):
            return None
        raw_spans = value.get("spans")
        if not isinstance(raw_spans, list):
            return None

        spans: list[dict[str, Any]] = []
        total_bytes = 0
        style_keys = (
            "colorArgb",
            "shadowColorArgb",
            "bold",
            "italic",
            "underlined",
            "strikethrough",
            "obfuscated",
            "fontId",
        )
        for raw_span in raw_spans:
            if not isinstance(raw_span, dict):
                continue
            text = str(raw_span.get("text") or "")
            if not text:
                continue
            total_bytes += len(text.encode("utf-8"))
            if total_bytes > self.config.max_formatted_text_utf8_bytes:
                return self._plain_formatted_text(plain_fallback)
            span: dict[str, Any] = {"text": text}
            for key in style_keys:
                if raw_span.get(key) is not None:
                    span[key] = raw_span[key]
            if spans and all(spans[-1].get(key) == span.get(key) for key in style_keys):
                spans[-1]["text"] += text
            else:
                spans.append(span)
            if len(spans) > self.config.max_formatted_text_spans:
                return self._plain_formatted_text(plain_fallback)

        plain_text = "".join(str(span["text"]) for span in spans)
        expected_plain = str(value.get("plainText") or plain_fallback or "")
        if expected_plain != plain_text:
            return self._plain_formatted_text(plain_fallback or expected_plain)
        return {"plainText": plain_text, "spans": spans}

    @staticmethod
    def _plain_formatted_text(value: str | None) -> dict[str, Any] | None:
        if value is None:
            return None
        return {"plainText": value, "spans": [{"text": value}] if value else []}

    def sanitize_player(self, raw: Any, *, fallback_uuid: Any = None) -> dict[str, Any] | None:
        if not isinstance(raw, dict):
            return None
        player_uuid = self.normalize_uuid(raw.get("uuid") or raw.get("playerUUID") or fallback_uuid)
        if player_uuid is None:
            return None

        player: dict[str, Any] = {"uuid": player_uuid}
        for source, target, limit in (
            ("name", "name", 256),
            ("playerName", "name", 256),
            ("displayName", "displayName", 4096),
            ("prefixedName", "prefixedName", 4096),
            ("scoreboardTeamId", "scoreboardTeamId", 256),
            ("teamId", "scoreboardTeamId", 256),
            ("scoreboardPrefix", "scoreboardPrefix", 4096),
            ("scoreboardSuffix", "scoreboardSuffix", 4096),
        ):
            if target in player or raw.get(source) is None:
                continue
            player[target] = self._optional_text(raw.get(source), limit)

        color = raw.get("scoreboardColorRgb")
        if isinstance(color, (int, float)) and not isinstance(color, bool):
            player["scoreboardColorRgb"] = int(color) & 0xFFFFFF

        for source, target, plain_key in (
            ("formattedDisplayName", "formattedDisplayName", "displayName"),
            ("formattedScoreboardPrefix", "formattedScoreboardPrefix", "scoreboardPrefix"),
            ("formattedScoreboardSuffix", "formattedScoreboardSuffix", "scoreboardSuffix"),
        ):
            formatted = self._sanitize_formatted_text(raw.get(source), player.get(plain_key))
            if formatted is not None:
                player[target] = formatted

        # During the 0.7 compatibility window, the legacy field remains the
        # plain scoreboard prefix when no explicit replacement was reported.
        if "scoreboardPrefix" not in player and player.get("prefixedName") is not None:
            player["scoreboardPrefix"] = player["prefixedName"]
        if "prefixedName" not in player and player.get("scoreboardPrefix") is not None:
            player["prefixedName"] = player["scoreboardPrefix"]
        return player

    @staticmethod
    def _canonical_json(value: Any) -> str:
        return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))

    @classmethod
    def _label_signature(cls, player: dict[str, Any]) -> str:
        return hashlib.sha256(cls._canonical_json(player).encode("utf-8")).hexdigest()

    @staticmethod
    def _display_label_quality(player: dict[str, Any]) -> int:
        """Rank resolved Tab displays above a legacy prefix copied into displayName."""
        display_name = str(player.get("displayName") or "").strip()
        if not display_name:
            return 0
        prefix = str(player.get("scoreboardPrefix") or player.get("prefixedName") or "").strip()
        return 1 if prefix and display_name == prefix else 2

    @classmethod
    def _entry_etag(cls, player: dict[str, Any], first_at: int, last_at: int) -> bytes:
        payload = {"player": player, "labelFirstObservedAtUtcMs": first_at, "lastObservedAtUtcMs": last_at}
        return hashlib.sha256(cls._canonical_json(payload).encode("utf-8")).digest()

    async def upsert_players(
        self,
        room_code: str,
        players: list[tuple[Any, Any]],
        *,
        observed_at_ms: int | None = None,
    ) -> bool:
        stamp = int(observed_at_ms if observed_at_ms is not None else time.time() * 1000)
        normalized: dict[str, dict[str, Any]] = {}
        for fallback_uuid, raw in players:
            player = self.sanitize_player(raw, fallback_uuid=fallback_uuid)
            if player is not None:
                normalized[player["uuid"]] = player
        if not normalized:
            return False

        async with self._lock:
            return await asyncio.to_thread(
                self._upsert_players_sync,
                room_code,
                normalized,
                stamp,
            )

    def _upsert_players_sync(
        self,
        room_code: str,
        normalized: dict[str, dict[str, Any]],
        stamp: int,
    ) -> bool:
        db = self._require_db()
        current_head = self._head_row(room_code)
        next_revision = int(current_head["revision"] if current_head is not None else 0) + 1
        changed: list[dict[str, Any]] = []
        for player_uuid, player in normalized.items():
            row = db.execute(
                "SELECT * FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?",
                (room_code, player_uuid),
            ).fetchone()
            if row is not None:
                persisted_player = json.loads(row["player_json"])
                if self._display_label_quality(player) < self._display_label_quality(persisted_player):
                    continue
            signature = self._label_signature(player)
            label_changed = row is None or str(row["label_signature"]) != signature
            if row is not None and not label_changed:
                minimum_interval_ms = max(0, self.config.observation_update_interval_sec) * 1000
                if stamp - int(row["last_observed_at"]) < minimum_interval_ms:
                    continue
            first_at = stamp if label_changed else int(row["label_first_observed_at"])
            etag = self._entry_etag(player, first_at, stamp)
            entry = {
                "player": player,
                "labelFirstObservedAtUtcMs": first_at,
                "lastObservedAtUtcMs": stamp,
                "revision": next_revision,
                "etagSha256": etag,
            }
            changed.append(entry)
            db.execute(
                """
                INSERT INTO tab_history_entries (
                    room_code, player_uuid, normalized_name, player_json, label_signature,
                    label_first_observed_at, last_observed_at, revision, etag_sha256
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(room_code, player_uuid) DO UPDATE SET
                    normalized_name = excluded.normalized_name,
                    player_json = excluded.player_json,
                    label_signature = excluded.label_signature,
                    label_first_observed_at = excluded.label_first_observed_at,
                    last_observed_at = excluded.last_observed_at,
                    revision = excluded.revision,
                    etag_sha256 = excluded.etag_sha256
                """,
                (
                    room_code,
                    player_uuid,
                    self.normalize_name(player.get("name")) or None,
                    self._canonical_json(player),
                    signature,
                    first_at,
                    stamp,
                    next_revision,
                    etag,
                ),
            )
            db.execute(
                "INSERT OR REPLACE INTO tab_history_deltas VALUES (?, ?, ?, 'upsert', ?, ?)",
                (room_code, next_revision, player_uuid, self._entry_json(entry), stamp),
            )

        if not changed:
            return False
        self._write_head(room_code, next_revision, stamp)
        db.commit()
        return True

    async def cleanup_retention(self, *, now_ms: int | None = None) -> dict[str, int]:
        stamp = int(now_ms if now_ms is not None else time.time() * 1000)
        expired_entries = 0
        expired_deltas = 0
        async with self._lock:
            db = self._require_db()
            if self.config.retention_days > 0:
                cutoff = stamp - self.config.retention_days * 86400 * 1000
                rooms = db.execute(
                    "SELECT DISTINCT room_code FROM tab_history_entries WHERE last_observed_at < ?", (cutoff,)
                ).fetchall()
                for room_row in rooms:
                    room_code = str(room_row["room_code"])
                    rows = db.execute(
                        "SELECT player_uuid FROM tab_history_entries WHERE room_code = ? AND last_observed_at < ?",
                        (room_code, cutoff),
                    ).fetchall()
                    if not rows:
                        continue
                    head = self._head_row(room_code)
                    revision = int(head["revision"] if head is not None else 0) + 1
                    for row in rows:
                        player_uuid = str(row["player_uuid"])
                        db.execute(
                            "DELETE FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?",
                            (room_code, player_uuid),
                        )
                        db.execute(
                            "INSERT OR REPLACE INTO tab_history_deltas VALUES (?, ?, ?, 'delete', NULL, ?)",
                            (room_code, revision, player_uuid, stamp),
                        )
                    expired_entries += len(rows)
                    self._write_head(room_code, revision, stamp)

            if self.config.delta_retention_days > 0:
                cutoff = stamp - self.config.delta_retention_days * 86400 * 1000
                cursor = db.execute("DELETE FROM tab_history_deltas WHERE occurred_at < ?", (cutoff,))
                expired_deltas = max(0, int(cursor.rowcount))
                db.execute(
                    """
                    DELETE FROM tab_history_revisions
                    WHERE created_at < ? AND NOT EXISTS (
                        SELECT 1 FROM tab_history_heads h
                        WHERE h.room_code = tab_history_revisions.room_code
                          AND h.revision = tab_history_revisions.revision
                    )
                    """,
                    (cutoff,),
                )
            db.commit()
        return {"expiredEntries": expired_entries, "expiredDeltas": expired_deltas}

    async def list_entries(
        self,
        *,
        room_code: str | None = None,
        search: str | None = None,
        page: int = 1,
        page_size: int = 50,
    ) -> dict[str, Any]:
        """Return a paginated admin view of the latest label mirror."""
        normalized_room = str(room_code or "").strip() or None
        normalized_search = str(search or "").strip()
        page = max(1, int(page))
        page_size = max(1, min(int(page_size), 200))

        clauses: list[str] = []
        params: list[Any] = []
        if normalized_room is not None:
            clauses.append("room_code = ?")
            params.append(normalized_room)
        if normalized_search:
            wildcard = f"%{normalized_search.casefold()}%"
            clauses.append(
                "(LOWER(player_uuid) LIKE ? OR normalized_name LIKE ? OR LOWER(player_json) LIKE ?)"
            )
            params.extend((wildcard, wildcard, wildcard))

        where_clause = f"WHERE {' AND '.join(clauses)}" if clauses else ""
        async with self._lock:
            db = self._require_db()
            total_row = db.execute(
                f"SELECT COUNT(*) AS count FROM tab_history_entries {where_clause}", tuple(params)
            ).fetchone()
            rows = db.execute(
                f"""
                SELECT *
                FROM tab_history_entries
                {where_clause}
                ORDER BY last_observed_at DESC, room_code ASC, player_uuid ASC
                LIMIT ? OFFSET ?
                """,
                (*params, page_size, (page - 1) * page_size),
            ).fetchall()
            rooms = db.execute(
                "SELECT DISTINCT room_code FROM tab_history_entries ORDER BY room_code ASC"
            ).fetchall()

        return {
            "items": [self._admin_entry_from_row(row) for row in rows],
            "total": int(total_row["count"] if total_row is not None else 0),
            "page": page,
            "pageSize": page_size,
            "availableRooms": [str(row["room_code"]) for row in rooms if row["room_code"]],
        }

    async def delete_entries(self, room_code: str, player_uuids: list[Any], *, occurred_at_ms: int | None = None) -> list[str]:
        """Delete latest entries and publish tombstones through one room revision."""
        normalized_room = str(room_code or "").strip()
        if not normalized_room:
            return []
        normalized_uuids = sorted({
            player_uuid
            for value in player_uuids
            if (player_uuid := self.normalize_uuid(value)) is not None
        })
        if not normalized_uuids:
            return []

        stamp = int(occurred_at_ms if occurred_at_ms is not None else time.time() * 1000)
        placeholders = ", ".join("?" for _ in normalized_uuids)
        async with self._lock:
            db = self._require_db()
            rows = db.execute(
                f"""
                SELECT player_uuid
                FROM tab_history_entries
                WHERE room_code = ? AND player_uuid IN ({placeholders})
                """,
                (normalized_room, *normalized_uuids),
            ).fetchall()
            deleted = [str(row["player_uuid"]) for row in rows]
            if not deleted:
                return []

            head = self._head_row(normalized_room)
            revision = int(head["revision"] if head is not None else 0) + 1
            for player_uuid in deleted:
                db.execute(
                    "DELETE FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?",
                    (normalized_room, player_uuid),
                )
                db.execute(
                    "INSERT OR REPLACE INTO tab_history_deltas VALUES (?, ?, ?, 'delete', NULL, ?)",
                    (normalized_room, revision, player_uuid, stamp),
                )
            self._write_head(normalized_room, revision, stamp)
            db.commit()
            return deleted

    async def head(self, room_code: str) -> dict[str, Any]:
        async with self._lock:
            return await asyncio.to_thread(self._head_sync, room_code)

    def _head_sync(self, room_code: str) -> dict[str, Any]:
        return self._serialize_head(room_code, self._head_row(room_code))

    async def sync(
        self,
        room_code: str,
        *,
        preferred_mode: str,
        base_revision: int | None,
        base_digest: bytes | None,
        allow_full_fallback: bool,
    ) -> dict[str, Any]:
        async with self._lock:
            return await asyncio.to_thread(
                self._sync_sync,
                room_code,
                preferred_mode,
                base_revision,
                base_digest,
                allow_full_fallback,
            )

    def _sync_sync(
        self,
        room_code: str,
        preferred_mode: str,
        base_revision: int | None,
        base_digest: bytes | None,
        allow_full_fallback: bool,
    ) -> dict[str, Any]:
        db = self._require_db()
        head_row = self._head_row(room_code)
        head = self._serialize_head(room_code, head_row)
        mode = "TAB_HISTORY_SYNC_MODE_DELTA" if preferred_mode.endswith("DELTA") else "TAB_HISTORY_SYNC_MODE_FULL"
        reset_reason: str | None = None

        if mode == "TAB_HISTORY_SYNC_MODE_DELTA":
            revision_row = None
            if base_revision is not None:
                revision_row = db.execute(
                    "SELECT digest_sha256 FROM tab_history_revisions WHERE room_code = ? AND revision = ?",
                    (room_code, int(base_revision)),
                ).fetchone()
                if int(base_revision) == 0 and revision_row is None:
                    revision_row = {"digest_sha256": hashlib.sha256(b"").digest()}
            if base_revision is None or revision_row is None:
                reset_reason = "TAB_HISTORY_RESET_REASON_BASE_UNKNOWN"
            elif base_digest is not None and bytes(revision_row["digest_sha256"]) != bytes(base_digest):
                reset_reason = "TAB_HISTORY_RESET_REASON_DIGEST_MISMATCH"
            elif int(base_revision) < int(head["oldestAvailableDeltaRevision"]):
                reset_reason = "TAB_HISTORY_RESET_REASON_DELTA_EXPIRED"

            if reset_reason is not None and allow_full_fallback:
                mode = "TAB_HISTORY_SYNC_MODE_FULL"
                reset_reason = None

        if mode == "TAB_HISTORY_SYNC_MODE_FULL":
            rows = db.execute(
                "SELECT * FROM tab_history_entries WHERE room_code = ? ORDER BY player_uuid", (room_code,)
            ).fetchall()
            upsert = [self._entry_from_row(row) for row in rows]
            deletes: list[str] = []
        elif reset_reason is None:
            rows = db.execute(
                "SELECT * FROM tab_history_deltas WHERE room_code = ? AND revision > ? ORDER BY revision, player_uuid",
                (room_code, int(base_revision or 0)),
            ).fetchall()
            latest: dict[str, sqlite3.Row] = {str(row["player_uuid"]): row for row in rows}
            upsert = [
                self._entry_from_json(row["entry_json"])
                for row in latest.values()
                if row["operation"] == "upsert"
            ]
            deletes = [player_uuid for player_uuid, row in latest.items() if row["operation"] == "delete"]
        else:
            upsert = []
            deletes = []
        return {
            "mode": mode,
            "head": head,
            "upsert": upsert,
            "deleteUuids": deletes,
            "resetReason": reset_reason,
        }

    async def lookup(self, room_code: str, selectors: list[dict[str, Any]]) -> tuple[dict[str, Any], list[dict[str, Any]]]:
        async with self._lock:
            db = self._require_db()
            head = self._serialize_head(room_code, self._head_row(room_code))
            results: list[dict[str, Any]] = []
            for index, selector in enumerate(selectors):
                rows: list[sqlite3.Row]
                if isinstance(selector, dict) and selector.get("uuid") is not None:
                    player_uuid = self.normalize_uuid(selector.get("uuid"))
                    rows = [] if player_uuid is None else db.execute(
                        "SELECT * FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?",
                        (room_code, player_uuid),
                    ).fetchall()
                elif isinstance(selector, dict) and selector.get("name") is not None:
                    name = self.normalize_name(selector.get("name"))
                    rows = [] if not name else db.execute(
                        "SELECT * FROM tab_history_entries WHERE room_code = ? AND normalized_name = ? ORDER BY player_uuid",
                        (room_code, name),
                    ).fetchall()
                else:
                    rows = []
                results.append({"selectorIndex": index, "entries": [self._entry_from_row(row) for row in rows]})
            return head, results

    def _head_row(self, room_code: str) -> sqlite3.Row | None:
        return self._require_db().execute(
            "SELECT * FROM tab_history_heads WHERE room_code = ?", (room_code,)
        ).fetchone()

    def _write_head(self, room_code: str, revision: int, stamp: int) -> None:
        db = self._require_db()
        rows = db.execute(
            "SELECT player_uuid, etag_sha256 FROM tab_history_entries WHERE room_code = ? ORDER BY player_uuid",
            (room_code,),
        ).fetchall()
        digest = hashlib.sha256()
        for row in rows:
            digest.update(uuid_lib.UUID(str(row["player_uuid"])).bytes)
            digest.update(bytes(row["etag_sha256"]))
        digest_bytes = digest.digest()
        db.execute(
            "INSERT OR REPLACE INTO tab_history_heads VALUES (?, ?, ?, ?, ?)",
            (room_code, revision, digest_bytes, len(rows), stamp),
        )
        db.execute(
            "INSERT OR REPLACE INTO tab_history_revisions VALUES (?, ?, ?, ?, ?)",
            (room_code, revision, digest_bytes, len(rows), stamp),
        )

    def _serialize_head(self, room_code: str, row: sqlite3.Row | None) -> dict[str, Any]:
        db = self._require_db()
        delta = db.execute(
            "SELECT MIN(revision) AS minimum_revision FROM tab_history_deltas WHERE room_code = ?", (room_code,)
        ).fetchone()
        minimum = int(delta["minimum_revision"] or 0) if delta is not None else 0
        if row is None:
            return {
                "revision": 0,
                "digestSha256": hashlib.sha256(b"").digest(),
                "recordCount": 0,
                "generatedAtUtcMs": int(time.time() * 1000),
                "oldestAvailableDeltaRevision": 0,
            }
        return {
            "revision": int(row["revision"]),
            "digestSha256": bytes(row["digest_sha256"]),
            "recordCount": int(row["record_count"]),
            "generatedAtUtcMs": int(row["updated_at"]),
            "oldestAvailableDeltaRevision": max(0, minimum - 1) if minimum else int(row["revision"]),
        }

    @staticmethod
    def _entry_from_row(row: sqlite3.Row) -> dict[str, Any]:
        return {
            "player": json.loads(row["player_json"]),
            "labelFirstObservedAtUtcMs": int(row["label_first_observed_at"]),
            "lastObservedAtUtcMs": int(row["last_observed_at"]),
            "revision": int(row["revision"]),
            "etagSha256": bytes(row["etag_sha256"]),
        }

    @classmethod
    def _admin_entry_from_row(cls, row: sqlite3.Row) -> dict[str, Any]:
        entry = cls._entry_from_row(row)
        return {
            "roomCode": str(row["room_code"]),
            "playerUuid": str(row["player_uuid"]),
            "player": entry["player"],
            "labelFirstObservedAtUtcMs": entry["labelFirstObservedAtUtcMs"],
            "lastObservedAtUtcMs": entry["lastObservedAtUtcMs"],
            "revision": entry["revision"],
        }

    def _entry_json(self, entry: dict[str, Any]) -> str:
        serializable = dict(entry)
        serializable["etagSha256"] = bytes(entry.get("etagSha256") or b"").hex()
        return self._canonical_json(serializable)

    @staticmethod
    def _entry_from_json(value: str) -> dict[str, Any]:
        entry = json.loads(value)
        raw_etag = entry.get("etagSha256")
        entry["etagSha256"] = bytes.fromhex(raw_etag) if isinstance(raw_etag, str) else b""
        return entry

    def _require_db(self) -> sqlite3.Connection:
        if self._db is None:
            raise RuntimeError("TabHistoryStore is not initialized")
        return self._db
