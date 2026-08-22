use std::collections::BTreeMap;

use anyhow::Context;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};
use tokio::sync::Mutex;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::proto::teamviewer::v1::{
    FormattedText, FormattedTextSpan, TabHistoryEntry, TabHistoryHead, TabHistoryLookupResult,
    TabHistoryLookupSelector, TabHistoryResetReason, TabHistorySyncMode, TabPlayerEntry,
    tab_history_lookup_selector,
};

pub const DEFAULT_CHUNK_ENTRIES: usize = 256;
pub const MAX_CHUNK_ENTRIES: usize = 512;
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_LOOKUP_SELECTORS: usize = 128;

pub struct SyncResult {
    pub mode: i32,
    pub head: TabHistoryHead,
    pub upsert: Vec<TabHistoryEntry>,
    pub delete_uuids: Vec<String>,
    pub reset_reason: Option<i32>,
}

pub struct TabHistoryStore {
    db: SqlitePool,
    writes: Mutex<()>,
}

impl TabHistoryStore {
    pub fn new(db: SqlitePool) -> Self {
        Self {
            db,
            writes: Mutex::new(()),
        }
    }

    pub async fn initialize(&self) -> anyhow::Result<()> {
        for statement in [
            r#"CREATE TABLE IF NOT EXISTS tab_history_heads (
                room_code TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                digest_sha256 BLOB NOT NULL,
                record_count INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )"#,
            r#"CREATE TABLE IF NOT EXISTS tab_history_entries (
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
            )"#,
            r#"CREATE TABLE IF NOT EXISTS tab_history_deltas (
                room_code TEXT NOT NULL,
                revision INTEGER NOT NULL,
                player_uuid TEXT NOT NULL,
                operation TEXT NOT NULL,
                entry_json TEXT,
                occurred_at INTEGER NOT NULL,
                PRIMARY KEY (room_code, revision, player_uuid)
            )"#,
            r#"CREATE TABLE IF NOT EXISTS tab_history_revisions (
                room_code TEXT NOT NULL,
                revision INTEGER NOT NULL,
                digest_sha256 BLOB NOT NULL,
                record_count INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (room_code, revision)
            )"#,
            "CREATE INDEX IF NOT EXISTS idx_tab_history_entries_name ON tab_history_entries (room_code, normalized_name)",
            "CREATE INDEX IF NOT EXISTS idx_tab_history_entries_last_observed ON tab_history_entries (last_observed_at)",
            "CREATE INDEX IF NOT EXISTS idx_tab_history_deltas_time ON tab_history_deltas (occurred_at)",
        ] {
            sqlx::query(statement).execute(&self.db).await?;
        }
        Ok(())
    }

    pub async fn upsert_players(
        &self,
        room: &str,
        players: &[TabPlayerEntry],
        observed_at_ms: i64,
    ) -> anyhow::Result<bool> {
        let mut normalized = BTreeMap::new();
        for player in players {
            if let Some(player) = sanitize_player(player)
                && let Some(uuid) = player.uuid.clone()
            {
                normalized.insert(uuid, player);
            }
        }
        if normalized.is_empty() {
            return Ok(false);
        }

        let _guard = self.writes.lock().await;
        let mut transaction = self.db.begin().await?;
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM tab_history_heads WHERE room_code = ?",
        )
        .bind(room)
        .fetch_optional(&mut *transaction)
        .await?
        .unwrap_or(0)
            + 1;
        let mut changed = false;

        for (player_uuid, player) in normalized {
            let row = sqlx::query(
                "SELECT player_json, label_signature, label_first_observed_at, last_observed_at FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?",
            )
            .bind(room)
            .bind(&player_uuid)
            .fetch_optional(&mut *transaction)
            .await?;
            if let Some(row) = &row {
                let persisted = player_from_json(&row.get::<String, _>("player_json"))?;
                if display_label_quality(&player) < display_label_quality(&persisted) {
                    continue;
                }
            }
            let player_value = player_to_value(&player);
            let player_json = canonical_json(&player_value)?;
            let signature = sha256_hex(player_json.as_bytes());
            let label_changed = row
                .as_ref()
                .is_none_or(|row| row.get::<String, _>("label_signature") != signature);
            if let Some(row) = &row
                && !label_changed
                && observed_at_ms - row.get::<i64, _>("last_observed_at") < 300_000
            {
                continue;
            }
            let first_at = if label_changed {
                observed_at_ms
            } else {
                row.as_ref()
                    .map_or(observed_at_ms, |row| row.get("label_first_observed_at"))
            };
            let etag = entry_etag(&player_value, first_at, observed_at_ms)?;
            let entry = TabHistoryEntry {
                player: Some(player.clone()),
                label_first_observed_at_utc_ms: first_at,
                last_observed_at_utc_ms: observed_at_ms,
                revision: revision as u64,
                etag_sha256: etag.clone(),
            };
            sqlx::query(
                r#"INSERT INTO tab_history_entries (
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
                    etag_sha256 = excluded.etag_sha256"#,
            )
            .bind(room)
            .bind(&player_uuid)
            .bind(player.name.as_deref().map(normalize_name))
            .bind(player_json)
            .bind(signature)
            .bind(first_at)
            .bind(observed_at_ms)
            .bind(revision)
            .bind(etag)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT OR REPLACE INTO tab_history_deltas VALUES (?, ?, ?, 'upsert', ?, ?)",
            )
            .bind(room)
            .bind(revision)
            .bind(&player_uuid)
            .bind(entry_to_json(&entry)?)
            .bind(observed_at_ms)
            .execute(&mut *transaction)
            .await?;
            changed = true;
        }

        if changed {
            write_head(&mut transaction, room, revision, observed_at_ms).await?;
        }
        transaction.commit().await?;
        Ok(changed)
    }

    pub async fn head(&self, room: &str) -> anyhow::Result<TabHistoryHead> {
        read_head(&self.db, room).await
    }

    pub async fn sync(
        &self,
        room: &str,
        preferred_mode: i32,
        base_revision: Option<u64>,
        base_digest: Option<&[u8]>,
        allow_full_fallback: bool,
    ) -> anyhow::Result<SyncResult> {
        let head = self.head(room).await?;
        let mut mode = if preferred_mode == TabHistorySyncMode::Delta as i32 {
            TabHistorySyncMode::Delta
        } else {
            TabHistorySyncMode::Full
        };
        let mut reset_reason = None;
        if mode == TabHistorySyncMode::Delta {
            let known_digest = if base_revision == Some(0) {
                Some(Sha256::digest([]).to_vec())
            } else if let Some(revision) = base_revision {
                sqlx::query_scalar::<_, Vec<u8>>(
                    "SELECT digest_sha256 FROM tab_history_revisions WHERE room_code = ? AND revision = ?",
                )
                .bind(room)
                .bind(revision as i64)
                .fetch_optional(&self.db)
                .await?
            } else {
                None
            };
            reset_reason = if base_revision.is_none() || known_digest.is_none() {
                Some(TabHistoryResetReason::BaseUnknown)
            } else if base_digest.is_some_and(|digest| Some(digest) != known_digest.as_deref()) {
                Some(TabHistoryResetReason::DigestMismatch)
            } else if base_revision
                .is_some_and(|revision| revision < head.oldest_available_delta_revision)
            {
                Some(TabHistoryResetReason::DeltaExpired)
            } else {
                None
            };
            if reset_reason.is_some() && allow_full_fallback {
                mode = TabHistorySyncMode::Full;
                reset_reason = None;
            }
        }

        let (upsert, delete_uuids) = if mode == TabHistorySyncMode::Full {
            let rows = sqlx::query(
                "SELECT * FROM tab_history_entries WHERE room_code = ? ORDER BY player_uuid",
            )
            .bind(room)
            .fetch_all(&self.db)
            .await?;
            let entries = rows
                .iter()
                .map(entry_from_row)
                .collect::<anyhow::Result<_>>()?;
            (entries, Vec::new())
        } else if reset_reason.is_none() {
            let rows = sqlx::query(
                "SELECT player_uuid, operation, entry_json FROM tab_history_deltas WHERE room_code = ? AND revision > ? ORDER BY revision, player_uuid",
            )
            .bind(room)
            .bind(base_revision.unwrap_or(0) as i64)
            .fetch_all(&self.db)
            .await?;
            let mut latest = BTreeMap::new();
            for row in rows {
                latest.insert(
                    row.get::<String, _>("player_uuid"),
                    (
                        row.get::<String, _>("operation"),
                        row.try_get::<String, _>("entry_json").ok(),
                    ),
                );
            }
            let mut upsert = Vec::new();
            let mut deletes = Vec::new();
            for (uuid, (operation, entry_json)) in latest {
                if operation == "upsert" {
                    if let Some(entry_json) = entry_json {
                        upsert.push(entry_from_json(&entry_json)?);
                    }
                } else {
                    deletes.push(uuid);
                }
            }
            (upsert, deletes)
        } else {
            (Vec::new(), Vec::new())
        };
        Ok(SyncResult {
            mode: mode as i32,
            head,
            upsert,
            delete_uuids,
            reset_reason: reset_reason.map(|reason| reason as i32),
        })
    }

    pub async fn lookup(
        &self,
        room: &str,
        selectors: &[TabHistoryLookupSelector],
    ) -> anyhow::Result<(TabHistoryHead, Vec<TabHistoryLookupResult>)> {
        let head = self.head(room).await?;
        let mut results = Vec::with_capacity(selectors.len());
        for (index, selector) in selectors.iter().enumerate() {
            let rows = match selector.selector.as_ref() {
                Some(tab_history_lookup_selector::Selector::Uuid(value)) => {
                    if let Some(value) = normalize_uuid(value) {
                        sqlx::query("SELECT * FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?")
                            .bind(room).bind(value).fetch_all(&self.db).await?
                    } else {
                        Vec::new()
                    }
                }
                Some(tab_history_lookup_selector::Selector::Name(value)) => {
                    let value = normalize_name(value);
                    if value.is_empty() {
                        Vec::new()
                    } else {
                        sqlx::query("SELECT * FROM tab_history_entries WHERE room_code = ? AND normalized_name = ? ORDER BY player_uuid")
                            .bind(room).bind(value).fetch_all(&self.db).await?
                    }
                }
                None => Vec::new(),
            };
            results.push(TabHistoryLookupResult {
                selector_index: index as u32,
                entries: rows
                    .iter()
                    .map(entry_from_row)
                    .collect::<anyhow::Result<_>>()?,
            });
        }
        Ok((head, results))
    }

    pub async fn admin_list(
        &self,
        room: Option<&str>,
        search: Option<&str>,
        page: u32,
        page_size: u32,
    ) -> anyhow::Result<Value> {
        let room = room.map(str::trim).filter(|value| !value.is_empty());
        let search = search.map(str::trim).filter(|value| !value.is_empty());
        let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM tab_history_entries");
        append_admin_filters(&mut count, room, search);
        let total = count
            .build_query_scalar::<i64>()
            .fetch_one(&self.db)
            .await?;

        let mut query = QueryBuilder::<Sqlite>::new("SELECT * FROM tab_history_entries");
        append_admin_filters(&mut query, room, search);
        query.push(" ORDER BY last_observed_at DESC, room_code ASC, player_uuid ASC LIMIT ");
        query.push_bind(i64::from(page_size));
        query.push(" OFFSET ");
        query.push_bind(i64::from((page - 1) * page_size));
        let rows = query.build().fetch_all(&self.db).await?;
        let items = rows
            .iter()
            .map(|row| {
                let player =
                    player_to_value(&player_from_json(&row.get::<String, _>("player_json"))?);
                Ok(json!({
                    "roomCode": row.get::<String, _>("room_code"),
                    "playerUuid": row.get::<String, _>("player_uuid"),
                    "player": player,
                    "labelFirstObservedAtUtcMs": row.get::<i64, _>("label_first_observed_at"),
                    "lastObservedAtUtcMs": row.get::<i64, _>("last_observed_at"),
                    "revision": row.get::<i64, _>("revision"),
                }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let available_rooms = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT room_code FROM tab_history_entries ORDER BY room_code",
        )
        .fetch_all(&self.db)
        .await?;
        Ok(json!({
            "items": items,
            "total": total,
            "page": page,
            "pageSize": page_size,
            "availableRooms": available_rooms,
        }))
    }

    pub async fn admin_delete(
        &self,
        room: &str,
        player_uuids: &[String],
    ) -> anyhow::Result<Vec<String>> {
        let room = room.trim();
        if room.is_empty() {
            return Ok(Vec::new());
        }
        let mut normalized = player_uuids
            .iter()
            .filter_map(|value| normalize_uuid(value))
            .collect::<Vec<_>>();
        normalized.sort();
        normalized.dedup();
        if normalized.is_empty() {
            return Ok(Vec::new());
        }

        let _guard = self.writes.lock().await;
        let mut transaction = self.db.begin().await?;
        let revision = sqlx::query_scalar::<_, i64>(
            "SELECT revision FROM tab_history_heads WHERE room_code = ?",
        )
        .bind(room)
        .fetch_optional(&mut *transaction)
        .await?
        .unwrap_or(0)
            + 1;
        let stamp = unix_millis();
        let mut deleted = Vec::new();
        for player_uuid in normalized {
            let result = sqlx::query(
                "DELETE FROM tab_history_entries WHERE room_code = ? AND player_uuid = ?",
            )
            .bind(room)
            .bind(&player_uuid)
            .execute(&mut *transaction)
            .await?;
            if result.rows_affected() == 0 {
                continue;
            }
            sqlx::query(
                "INSERT OR REPLACE INTO tab_history_deltas VALUES (?, ?, ?, 'delete', NULL, ?)",
            )
            .bind(room)
            .bind(revision)
            .bind(&player_uuid)
            .bind(stamp)
            .execute(&mut *transaction)
            .await?;
            deleted.push(player_uuid);
        }
        if !deleted.is_empty() {
            write_head(&mut transaction, room, revision, stamp).await?;
        }
        transaction.commit().await?;
        Ok(deleted)
    }
}

fn append_admin_filters(
    query: &mut QueryBuilder<'_, Sqlite>,
    room: Option<&str>,
    search: Option<&str>,
) {
    if room.is_none() && search.is_none() {
        return;
    }
    query.push(" WHERE ");
    let mut separated = query.separated(" AND ");
    if let Some(room) = room {
        separated
            .push("room_code = ")
            .push_bind_unseparated(room.to_owned());
    }
    if let Some(search) = search {
        let wildcard = format!("%{}%", normalize_name(search));
        separated
            .push("(LOWER(player_uuid) LIKE ")
            .push_bind_unseparated(wildcard.clone())
            .push_unseparated(" OR normalized_name LIKE ")
            .push_bind_unseparated(wildcard.clone())
            .push_unseparated(" OR LOWER(player_json) LIKE ")
            .push_bind_unseparated(wildcard)
            .push_unseparated(")");
    }
}

async fn write_head(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    room: &str,
    revision: i64,
    stamp: i64,
) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT player_uuid, etag_sha256 FROM tab_history_entries WHERE room_code = ? ORDER BY player_uuid",
    )
    .bind(room)
    .fetch_all(&mut **transaction)
    .await?;
    let mut digest = Sha256::new();
    for row in &rows {
        let player_uuid = Uuid::parse_str(&row.get::<String, _>("player_uuid"))?;
        digest.update(player_uuid.as_bytes());
        digest.update(row.get::<Vec<u8>, _>("etag_sha256"));
    }
    let digest = digest.finalize().to_vec();
    sqlx::query("INSERT OR REPLACE INTO tab_history_heads VALUES (?, ?, ?, ?, ?)")
        .bind(room)
        .bind(revision)
        .bind(&digest)
        .bind(rows.len() as i64)
        .bind(stamp)
        .execute(&mut **transaction)
        .await?;
    sqlx::query("INSERT OR REPLACE INTO tab_history_revisions VALUES (?, ?, ?, ?, ?)")
        .bind(room)
        .bind(revision)
        .bind(digest)
        .bind(rows.len() as i64)
        .bind(stamp)
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

async fn read_head(db: &SqlitePool, room: &str) -> anyhow::Result<TabHistoryHead> {
    let row = sqlx::query("SELECT * FROM tab_history_heads WHERE room_code = ?")
        .bind(room)
        .fetch_optional(db)
        .await?;
    let minimum = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(revision) FROM tab_history_deltas WHERE room_code = ?",
    )
    .bind(room)
    .fetch_one(db)
    .await?
    .unwrap_or(0);
    Ok(if let Some(row) = row {
        let revision = row.get::<i64, _>("revision") as u64;
        TabHistoryHead {
            revision,
            digest_sha256: row.get("digest_sha256"),
            record_count: row.get::<i64, _>("record_count") as u32,
            generated_at_utc_ms: row.get("updated_at"),
            oldest_available_delta_revision: if minimum > 0 {
                (minimum - 1) as u64
            } else {
                revision
            },
        }
    } else {
        TabHistoryHead {
            revision: 0,
            digest_sha256: Sha256::digest([]).to_vec(),
            record_count: 0,
            generated_at_utc_ms: unix_millis(),
            oldest_available_delta_revision: 0,
        }
    })
}

fn entry_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TabHistoryEntry> {
    Ok(TabHistoryEntry {
        player: Some(player_from_json(&row.get::<String, _>("player_json"))?),
        label_first_observed_at_utc_ms: row.get("label_first_observed_at"),
        last_observed_at_utc_ms: row.get("last_observed_at"),
        revision: row.get::<i64, _>("revision") as u64,
        etag_sha256: row.get("etag_sha256"),
    })
}

fn entry_to_json(entry: &TabHistoryEntry) -> anyhow::Result<String> {
    canonical_json(&json!({
        "player": entry.player.as_ref().map(player_to_value),
        "labelFirstObservedAtUtcMs": entry.label_first_observed_at_utc_ms,
        "lastObservedAtUtcMs": entry.last_observed_at_utc_ms,
        "revision": entry.revision,
        "etagSha256": hex(&entry.etag_sha256),
    }))
}

fn entry_from_json(raw: &str) -> anyhow::Result<TabHistoryEntry> {
    let value: Value = serde_json::from_str(raw)?;
    Ok(TabHistoryEntry {
        player: value.get("player").map(player_from_value).transpose()?,
        label_first_observed_at_utc_ms: value["labelFirstObservedAtUtcMs"].as_i64().unwrap_or(0),
        last_observed_at_utc_ms: value["lastObservedAtUtcMs"].as_i64().unwrap_or(0),
        revision: value["revision"].as_u64().unwrap_or(0),
        etag_sha256: unhex(value["etagSha256"].as_str().unwrap_or(""))?,
    })
}

fn sanitize_player(raw: &TabPlayerEntry) -> Option<TabPlayerEntry> {
    let uuid = normalize_uuid(raw.uuid.as_deref()?)?;
    let mut player = raw.clone();
    player.uuid = Some(uuid);
    player.name = truncate(player.name.as_deref(), 256);
    player.display_name = truncate(player.display_name.as_deref(), 4096);
    player.prefixed_name = truncate(player.prefixed_name.as_deref(), 4096);
    player.scoreboard_team_id = truncate(player.scoreboard_team_id.as_deref(), 256);
    player.scoreboard_prefix = truncate(player.scoreboard_prefix.as_deref(), 4096);
    player.scoreboard_suffix = truncate(player.scoreboard_suffix.as_deref(), 4096);
    if player.scoreboard_prefix.is_none() {
        player.scoreboard_prefix.clone_from(&player.prefixed_name);
    }
    if player.prefixed_name.is_none() {
        player.prefixed_name.clone_from(&player.scoreboard_prefix);
    }
    player.formatted_display_name = sanitize_formatted(
        player.formatted_display_name.as_ref(),
        player.display_name.as_deref(),
    );
    player.formatted_scoreboard_prefix = sanitize_formatted(
        player.formatted_scoreboard_prefix.as_ref(),
        player.scoreboard_prefix.as_deref(),
    );
    player.formatted_scoreboard_suffix = sanitize_formatted(
        player.formatted_scoreboard_suffix.as_ref(),
        player.scoreboard_suffix.as_deref(),
    );
    Some(player)
}

fn sanitize_formatted(
    value: Option<&FormattedText>,
    fallback: Option<&str>,
) -> Option<FormattedText> {
    let value = value?;
    let mut spans: Vec<FormattedTextSpan> = Vec::new();
    let mut bytes = 0;
    for span in &value.spans {
        if span.text.is_empty() {
            continue;
        }
        bytes += span.text.len();
        if bytes > 4096 {
            return fallback.map(plain_formatted);
        }
        if spans.len() >= 64 {
            return fallback.map(plain_formatted);
        }
        let mut style = span.clone();
        style.text.clear();
        if let Some(previous) = spans.last_mut() {
            let mut previous_style = previous.clone();
            previous_style.text.clear();
            if previous_style == style {
                previous.text.push_str(&span.text);
                continue;
            }
        }
        spans.push(span.clone());
    }
    let plain_text: String = spans.iter().map(|span| span.text.as_str()).collect();
    let expected = if value.plain_text.is_empty() {
        fallback.unwrap_or_default()
    } else {
        &value.plain_text
    };
    if plain_text != expected {
        return fallback.or(Some(expected)).map(plain_formatted);
    }
    Some(FormattedText { plain_text, spans })
}

fn plain_formatted(value: &str) -> FormattedText {
    FormattedText {
        plain_text: value.to_owned(),
        spans: if value.is_empty() {
            Vec::new()
        } else {
            vec![FormattedTextSpan {
                text: value.to_owned(),
                ..Default::default()
            }]
        },
    }
}

fn display_label_quality(player: &TabPlayerEntry) -> u8 {
    let display = player.display_name.as_deref().unwrap_or_default().trim();
    if display.is_empty() {
        return 0;
    }
    let prefix = player
        .scoreboard_prefix
        .as_deref()
        .or(player.prefixed_name.as_deref())
        .unwrap_or_default()
        .trim();
    if !prefix.is_empty() && display == prefix {
        1
    } else {
        2
    }
}

fn player_to_value(player: &TabPlayerEntry) -> Value {
    let mut map = Map::new();
    macro_rules! text {
        ($field:ident, $name:literal) => {
            if let Some(value) = &player.$field {
                map.insert($name.to_owned(), Value::String(value.clone()));
            }
        };
    }
    text!(uuid, "uuid");
    text!(name, "name");
    text!(display_name, "displayName");
    text!(prefixed_name, "prefixedName");
    text!(scoreboard_team_id, "scoreboardTeamId");
    text!(scoreboard_prefix, "scoreboardPrefix");
    text!(scoreboard_suffix, "scoreboardSuffix");
    if let Some(value) = player.scoreboard_color_rgb {
        map.insert("scoreboardColorRgb".to_owned(), json!(value & 0x00ff_ffff));
    }
    for (name, value) in [
        ("formattedDisplayName", &player.formatted_display_name),
        (
            "formattedScoreboardPrefix",
            &player.formatted_scoreboard_prefix,
        ),
        (
            "formattedScoreboardSuffix",
            &player.formatted_scoreboard_suffix,
        ),
    ] {
        if let Some(value) = value {
            map.insert(name.to_owned(), formatted_to_value(value));
        }
    }
    Value::Object(map)
}

fn formatted_to_value(value: &FormattedText) -> Value {
    json!({
        "plainText": value.plain_text,
        "spans": value.spans.iter().map(span_to_value).collect::<Vec<_>>(),
    })
}

fn span_to_value(span: &FormattedTextSpan) -> Value {
    let mut map = Map::new();
    map.insert("text".to_owned(), Value::String(span.text.clone()));
    macro_rules! optional {
        ($field:ident, $name:literal) => {
            if let Some(value) = &span.$field {
                map.insert($name.to_owned(), json!(value));
            }
        };
    }
    optional!(color_argb, "colorArgb");
    optional!(shadow_color_argb, "shadowColorArgb");
    optional!(bold, "bold");
    optional!(italic, "italic");
    optional!(underlined, "underlined");
    optional!(strikethrough, "strikethrough");
    optional!(obfuscated, "obfuscated");
    optional!(font_id, "fontId");
    Value::Object(map)
}

fn player_from_json(raw: &str) -> anyhow::Result<TabPlayerEntry> {
    player_from_value(&serde_json::from_str(raw)?)
}

fn player_from_value(value: &Value) -> anyhow::Result<TabPlayerEntry> {
    let formatted = |name: &str| value.get(name).map(formatted_from_value).transpose();
    Ok(TabPlayerEntry {
        uuid: string(value, "uuid"),
        name: string(value, "name"),
        display_name: string(value, "displayName"),
        prefixed_name: string(value, "prefixedName"),
        scoreboard_team_id: string(value, "scoreboardTeamId"),
        scoreboard_prefix: string(value, "scoreboardPrefix"),
        scoreboard_suffix: string(value, "scoreboardSuffix"),
        scoreboard_color_rgb: value["scoreboardColorRgb"]
            .as_u64()
            .map(|value| value as u32),
        formatted_display_name: formatted("formattedDisplayName")?,
        formatted_scoreboard_prefix: formatted("formattedScoreboardPrefix")?,
        formatted_scoreboard_suffix: formatted("formattedScoreboardSuffix")?,
    })
}

fn formatted_from_value(value: &Value) -> anyhow::Result<FormattedText> {
    let spans = value["spans"]
        .as_array()
        .into_iter()
        .flatten()
        .map(span_from_value)
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(FormattedText {
        plain_text: value["plainText"].as_str().unwrap_or_default().to_owned(),
        spans,
    })
}

fn span_from_value(value: &Value) -> anyhow::Result<FormattedTextSpan> {
    Ok(FormattedTextSpan {
        text: value["text"].as_str().unwrap_or_default().to_owned(),
        color_argb: value["colorArgb"].as_u64().map(|value| value as u32),
        shadow_color_argb: value["shadowColorArgb"].as_u64().map(|value| value as u32),
        bold: value["bold"].as_bool(),
        italic: value["italic"].as_bool(),
        underlined: value["underlined"].as_bool(),
        strikethrough: value["strikethrough"].as_bool(),
        obfuscated: value["obfuscated"].as_bool(),
        font_id: string(value, "fontId"),
    })
}

fn normalize_uuid(value: &str) -> Option<String> {
    Uuid::parse_str(value.trim())
        .ok()
        .map(|uuid| uuid.hyphenated().to_string())
}

fn normalize_name(value: &str) -> String {
    value.trim().nfkc().flat_map(char::to_lowercase).collect()
}

fn truncate(value: Option<&str>, max_bytes: usize) -> Option<String> {
    let value = value?;
    if value.len() <= max_bytes {
        return Some(value.to_owned());
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    Some(value[..boundary].to_owned())
}

fn entry_etag(player: &Value, first_at: i64, last_at: i64) -> anyhow::Result<Vec<u8>> {
    let value = json!({
        "player": player,
        "labelFirstObservedAtUtcMs": first_at,
        "lastObservedAtUtcMs": last_at,
    });
    Ok(Sha256::digest(canonical_json(&value)?.as_bytes()).to_vec())
}

fn canonical_json(value: &Value) -> anyhow::Result<String> {
    serde_json::to_string(value).context("serialize canonical tab history JSON")
}

fn sha256_hex(value: &[u8]) -> String {
    hex(&Sha256::digest(value))
}

fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(value: &str) -> anyhow::Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        anyhow::bail!("invalid hex length");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).map_err(Into::into))
        .collect()
}

fn string(value: &Value, name: &str) -> Option<String> {
    value.get(name)?.as_str().map(ToOwned::to_owned)
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_player_json_matches_python_shape() {
        let player = sanitize_player(&TabPlayerEntry {
            uuid: Some("00000000000000000000000000000001".to_owned()),
            name: Some("Alice".to_owned()),
            scoreboard_prefix: Some("[A]".to_owned()),
            ..Default::default()
        })
        .expect("player");
        assert_eq!(
            canonical_json(&player_to_value(&player)).expect("json"),
            r#"{"name":"Alice","prefixedName":"[A]","scoreboardPrefix":"[A]","uuid":"00000000-0000-0000-0000-000000000001"}"#
        );
    }
}
