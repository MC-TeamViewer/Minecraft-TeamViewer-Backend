use std::{collections::HashMap, convert::Infallible, env, net::IpAddr};

use axum::{
    Json,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response, Sse, sse::Event},
};
use chrono::{Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, Timelike};
use futures_util::{StreamExt, stream};
use ipnet::IpNet;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{QueryBuilder, Row, Sqlite};
use uuid::Uuid;

use crate::{transport::TransportConnectInfo, web::AppState};

const COOKIE_NAME: &str = "teamviewer_admin_session";

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(transport): ConnectInfo<TransportConnectInfo>,
    headers: HeaderMap,
    Json(login): Json<LoginRequest>,
) -> Response {
    let expected_user = env::var("TEAMVIEWER_ADMIN_USERNAME").unwrap_or_else(|_| "admin".into());
    let expected_password =
        env::var("TEAMVIEWER_ADMIN_PASSWORD").unwrap_or_else(|_| "admin".into());
    let remote_addr = request_remote_addr(&headers, &transport);
    if expected_user.is_empty() || expected_password.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail": "admin_not_configured"})),
        )
            .into_response();
    }
    if !constant_time_equal(&login.username, &expected_user)
        || !constant_time_equal(&login.password, &expected_password)
    {
        record_audit(
            &state,
            "admin_auth_failed",
            "admin",
            Some(&login.username),
            false,
            Some(&remote_addr),
            json!({"reason":"invalid_credentials"}),
        )
        .await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"detail": "invalid_admin_credentials"})),
        )
            .into_response();
    }
    let raw_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let token_hash = token_hash(&raw_token);
    let session_id = Uuid::new_v4().simple().to_string();
    let now = unix_millis();
    let ttl = session_ttl_sec();
    let expires = now + ttl * 1_000;
    if sqlx::query(
        "INSERT INTO admin_sessions (session_id, token_hash, actor_id, remote_addr, created_at, last_seen_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&session_id)
    .bind(token_hash)
    .bind(&login.username)
    .bind(&remote_addr)
    .bind(now)
    .bind(now)
    .bind(expires)
    .execute(&state.db)
    .await
    .is_err()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let session = session_json(
        &session_id,
        &login.username,
        Some(&remote_addr),
        now,
        now,
        expires,
    );
    record_audit(
        &state,
        "admin_session_started",
        "admin",
        Some(&login.username),
        true,
        Some(&remote_addr),
        json!({"sessionId":session_id}),
    )
    .await;
    let secure = request_is_secure(&headers, &transport);
    let cookie = format!(
        "{COOKIE_NAME}={raw_token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl}{}",
        if secure { "; Secure" } else { "" }
    );
    ([(header::SET_COOKIE, cookie)], Json(session)).into_response()
}

pub async fn current(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match authenticate(&state, &headers).await {
        Some(session) => Json(session).into_response(),
        None => unauthorized(),
    }
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = cookie_token(&headers) else {
        return unauthorized();
    };
    let now = unix_millis();
    let _ = sqlx::query(
        "UPDATE admin_sessions SET ended_at = ?, end_reason = 'logout' WHERE token_hash = ?",
    )
    .bind(now)
    .bind(token_hash(&token))
    .execute(&state.db)
    .await;
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE_NAME}=; Path=/; HttpOnly; Max-Age=0"),
        )],
        Json(json!({"ok": true})),
    )
        .into_response()
}

pub async fn overview(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    match state.relay.snapshot(None).await {
        Ok(snapshot) => Json(overview_value(&snapshot)).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let snapshot = state
        .relay
        .snapshot(None)
        .await
        .unwrap_or_else(|_| json!({}));
    let overview = overview_value(&snapshot);
    let daily_metrics = default_daily_metrics(&state.db).await;
    let hourly_metrics = default_hourly_metrics(&state.db).await;
    let traffic_history = default_traffic_history(&state.db).await;
    let bootstrap = json!({
        "serverTime": unix_seconds(),
        "overview": overview,
        "dailyMetrics": daily_metrics,
        "hourlyMetrics": hourly_metrics,
        "liveTraffic": state.metrics.live_traffic_json(),
        "protobufTraffic": state.metrics.protobuf_json(),
        "trafficHistory": traffic_history,
        "audit": {"items": [], "playerIdentityMappings": [], "nextBeforeId": null, "limit": 100, "availableEventTypes": []},
    });
    let bootstrap_event = Event::default()
        .event("bootstrap")
        .json_data(bootstrap)
        .expect("JSON event");
    let updates = stream::unfold((state, 0_usize), |(state, index)| async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let event_names = [
            "overview",
            "daily_metrics",
            "hourly_metrics",
            "traffic_live",
            "protobuf_traffic",
            "traffic_history",
            "audit",
            "last_seen_history",
            "tab_history",
            "runtime_state",
            "heartbeat",
        ];
        let event_name = event_names[index % event_names.len()];
        let payload = match event_name {
            "overview" => {
                let snapshot = state
                    .relay
                    .snapshot(None)
                    .await
                    .unwrap_or_else(|_| json!({}));
                let mut payload = overview_value(&snapshot);
                payload["serverTime"] = json!(unix_seconds());
                payload
            }
            "daily_metrics" => default_daily_metrics(&state.db).await,
            "hourly_metrics" => default_hourly_metrics(&state.db).await,
            "traffic_live" => state.metrics.live_traffic_json(),
            "protobuf_traffic" => state.metrics.protobuf_json(),
            "traffic_history" => default_traffic_history(&state.db).await,
            "audit" => json!({
                "serverTime":unix_seconds(),"items":[],"playerIdentityMappings":[],
                "nextBeforeId":null,"limit":100,"availableEventTypes":[],
            }),
            _ => json!({"serverTime":unix_seconds()}),
        };
        let event = Event::default()
            .event(event_name)
            .json_data(payload)
            .expect("JSON event");
        Some((Ok::<_, Infallible>(event), (state, index + 1)))
    });
    Sse::new(stream::once(async move { Ok::<_, Infallible>(bootstrap_event) }).chain(updates))
        .into_response()
}

#[derive(Default, Deserialize)]
pub struct DailyMetricsQuery {
    days: Option<u32>,
    #[serde(rename = "roomCode")]
    room_code: Option<String>,
    #[serde(rename = "startDate")]
    start_date: Option<String>,
}

pub async fn metrics_daily(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DailyMetricsQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let days = query.days.unwrap_or(30).clamp(1, 400);
    let start = match query.start_date {
        Some(value) => match NaiveDate::parse_from_str(&value, "%Y-%m-%d") {
            Ok(value) => value,
            Err(_) => return invalid_parameter("invalid_start_date"),
        },
        None => Local::now().date_naive() - ChronoDuration::days(i64::from(days - 1)),
    };
    let rows = sqlx::query("SELECT local_date AS bucket, COUNT(DISTINCT player_id) AS active_players FROM daily_player_activity WHERE local_date >= ? AND local_date < ? AND (? IS NULL OR room_code = ?) GROUP BY local_date")
        .bind(start.format("%Y-%m-%d").to_string())
        .bind((start + ChronoDuration::days(i64::from(days))).format("%Y-%m-%d").to_string())
        .bind(query.room_code.as_deref()).bind(query.room_code.as_deref())
        .fetch_all(&state.db).await.unwrap_or_default();
    let counts = rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("bucket"),
                row.get::<i64, _>("active_players"),
            )
        })
        .collect::<HashMap<_, _>>();
    let items = (0..days)
        .map(|offset| {
            let bucket = (start + ChronoDuration::days(i64::from(offset)))
                .format("%Y-%m-%d")
                .to_string();
            json!({"bucket":bucket,"activePlayers":counts.get(&bucket).copied().unwrap_or(0)})
        })
        .collect::<Vec<_>>();
    Json(json!({"timezone":timezone_name(),"roomCode":query.room_code,"days":days,"startDate":start.format("%Y-%m-%d").to_string(),"items":items,"serverTime":unix_seconds()})).into_response()
}

#[derive(Default, Deserialize)]
pub struct HourlyMetricsQuery {
    hours: Option<u32>,
    #[serde(rename = "roomCode")]
    room_code: Option<String>,
    #[serde(rename = "startAt")]
    start_at: Option<String>,
}

pub async fn metrics_hourly(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HourlyMetricsQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let hours = query.hours.unwrap_or(48).clamp(1, 24 * 90);
    let start = match query.start_at {
        Some(value) => match parse_local_datetime(&value) {
            Some(value) => value,
            None => return invalid_parameter("invalid_start_at"),
        },
        None => {
            let now = Local::now().naive_local();
            now.date()
                .and_hms_opt(now.hour(), 0, 0)
                .expect("valid hour")
                - ChronoDuration::hours(i64::from(hours - 1))
        }
    };
    let end = start + ChronoDuration::hours(i64::from(hours));
    let rows = sqlx::query("SELECT local_hour AS bucket, COUNT(DISTINCT player_id) AS active_players FROM hourly_player_activity WHERE local_hour >= ? AND local_hour < ? AND (? IS NULL OR room_code = ?) GROUP BY local_hour")
        .bind(format_local_hour(start)).bind(format_local_hour(end))
        .bind(query.room_code.as_deref()).bind(query.room_code.as_deref())
        .fetch_all(&state.db).await.unwrap_or_default();
    let counts = rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("bucket"),
                row.get::<i64, _>("active_players"),
            )
        })
        .collect::<HashMap<_, _>>();
    let items = (0..hours)
        .map(|offset| {
            let bucket = format_local_hour(start + ChronoDuration::hours(i64::from(offset)));
            json!({"bucket":bucket,"activePlayers":counts.get(&bucket).copied().unwrap_or(0)})
        })
        .collect::<Vec<_>>();
    Json(json!({"timezone":timezone_name(),"roomCode":query.room_code,"hours":hours,"startAt":format_local_hour(start),"items":items,"serverTime":unix_seconds()})).into_response()
}

pub async fn traffic_live(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let value = state.metrics.live_traffic_json();
    authenticated_json(&state, &headers, value).await
}

pub async fn protobuf_live(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let value = state.metrics.protobuf_json();
    authenticated_json(&state, &headers, value).await
}

#[derive(Default, Deserialize)]
pub struct TrafficHistoryQuery {
    range: Option<String>,
    granularity: Option<String>,
    #[serde(rename = "startAt")]
    start_at: Option<String>,
    #[serde(rename = "selectedLayer")]
    selected_layer: Option<String>,
}

pub async fn traffic_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TrafficHistoryQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let range = query.range.as_deref().unwrap_or("48h");
    let range_seconds = match range {
        "1h" => 3_600,
        "6h" => 21_600,
        "24h" => 86_400,
        "48h" => 172_800,
        "7d" => 604_800,
        "30d" => 2_592_000,
        _ => return invalid_parameter("invalid_traffic_range"),
    };
    let default_granularity = match range {
        "1h" => "1m",
        "6h" => "5m",
        "24h" => "15m",
        "48h" | "7d" => "1h",
        _ => "1d",
    };
    let granularity = query.granularity.as_deref().unwrap_or(default_granularity);
    let valid = match range {
        "1h" => matches!(granularity, "1m" | "5m"),
        "6h" => matches!(granularity, "1m" | "5m" | "15m"),
        "24h" => matches!(granularity, "5m" | "15m" | "1h"),
        "48h" => matches!(granularity, "15m" | "1h"),
        "7d" => matches!(granularity, "1h" | "1d"),
        "30d" => granularity == "1d",
        _ => false,
    };
    if !valid {
        return invalid_parameter("invalid_traffic_granularity");
    }
    let bucket_seconds = match granularity {
        "1m" => 60,
        "5m" => 300,
        "15m" => 900,
        "1h" => 3_600,
        "1d" => 86_400,
        _ => unreachable!(),
    };
    let count = range_seconds / bucket_seconds;
    let start = match query.start_at {
        Some(value) => match parse_local_datetime(&value) {
            Some(value) => align_datetime(value, bucket_seconds),
            None => return invalid_parameter("invalid_start_at"),
        },
        None => {
            align_datetime(Local::now().naive_local(), bucket_seconds)
                - ChronoDuration::seconds(i64::from((count - 1) * bucket_seconds))
        }
    };
    let (application_table, wire_table, bucket_column) = if bucket_seconds >= 86_400 {
        (
            "daily_traffic_bytes",
            "daily_wire_traffic_bytes",
            "local_date",
        )
    } else if bucket_seconds >= 3_600 {
        (
            "hourly_traffic_bytes",
            "hourly_wire_traffic_bytes",
            "local_hour",
        )
    } else {
        (
            "minute_traffic_bytes",
            "minute_wire_traffic_bytes",
            "local_minute",
        )
    };
    let application = traffic_series(
        &state.db,
        application_table,
        bucket_column,
        start,
        count,
        bucket_seconds,
    )
    .await;
    let wire = traffic_series(
        &state.db,
        wire_table,
        bucket_column,
        start,
        count,
        bucket_seconds,
    )
    .await;
    let selected_layer = query
        .selected_layer
        .filter(|value| matches!(value.as_str(), "application" | "wire"))
        .unwrap_or_else(|| "application".to_owned());
    Json(json!({
        "timezone":timezone_name(),"range":range,"granularity":granularity,
        "bucketSeconds":bucket_seconds,"startAt":format_local_second(start),
        "selectedLayer":selected_layer,"application":application,"wire":wire,
        "serverTime":unix_seconds(),
    }))
    .into_response()
}

#[derive(Default, Deserialize)]
pub struct TrafficPeriodQuery {
    hours: Option<u32>,
    days: Option<u32>,
    #[serde(rename = "startAt")]
    start_at: Option<String>,
    #[serde(rename = "startDate")]
    start_date: Option<String>,
}

pub async fn traffic_hourly(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TrafficPeriodQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let hours = query.hours.unwrap_or(48).clamp(1, 24 * 90);
    let start = match query.start_at {
        Some(value) => match parse_local_datetime(&value) {
            Some(value) => align_datetime(value, 3_600),
            None => return invalid_parameter("invalid_start_at"),
        },
        None => {
            align_datetime(Local::now().naive_local(), 3_600)
                - ChronoDuration::hours(i64::from(hours - 1))
        }
    };
    let value = traffic_series(
        &state.db,
        "hourly_traffic_bytes",
        "local_hour",
        start,
        hours,
        3_600,
    )
    .await;
    Json(merge_traffic_metadata(
        value,
        json!({"timezone":timezone_name(),"hours":hours,"startAt":format_local_second(start),"serverTime":unix_seconds()}),
    ))
    .into_response()
}

pub async fn traffic_daily(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TrafficPeriodQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let days = query.days.unwrap_or(30).clamp(1, 400);
    let start_date = match query.start_date {
        Some(value) => match NaiveDate::parse_from_str(&value, "%Y-%m-%d") {
            Ok(value) => value,
            Err(_) => return invalid_parameter("invalid_start_date"),
        },
        None => Local::now().date_naive() - ChronoDuration::days(i64::from(days - 1)),
    };
    let start = start_date.and_hms_opt(0, 0, 0).expect("valid midnight");
    let value = traffic_series(
        &state.db,
        "daily_traffic_bytes",
        "local_date",
        start,
        days,
        86_400,
    )
    .await;
    Json(merge_traffic_metadata(
        value,
        json!({"timezone":timezone_name(),"days":days,"startDate":start_date.format("%Y-%m-%d").to_string(),"serverTime":unix_seconds()}),
    ))
    .into_response()
}

#[derive(Default, Deserialize)]
pub struct AuditQuery {
    limit: Option<u32>,
    #[serde(rename = "beforeId")]
    before_id: Option<i64>,
    #[serde(rename = "eventType")]
    event_type: Option<String>,
    #[serde(rename = "actorType")]
    actor_type: Option<String>,
    #[serde(default, rename = "actorTypes")]
    actor_types: Vec<String>,
    success: Option<bool>,
}

pub async fn audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let mut actor_types = query
        .actor_types
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if actor_types.is_empty()
        && let Some(actor_type) = query.actor_type.map(|value| value.trim().to_owned())
        && !actor_type.is_empty()
    {
        actor_types.push(actor_type);
    }
    actor_types.sort();
    actor_types.dedup();
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT id, occurred_at, event_type, actor_type, actor_id, room_code, success, remote_addr, detail_json FROM audit_events",
    );
    let has_filters = query.before_id.is_some()
        || query
            .event_type
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || !actor_types.is_empty()
        || query.success.is_some();
    if has_filters {
        builder.push(" WHERE ");
        let mut clauses = builder.separated(" AND ");
        if let Some(before_id) = query.before_id {
            clauses.push("id < ").push_bind_unseparated(before_id);
        }
        if let Some(event_type) = query.event_type.map(|value| value.trim().to_owned())
            && !event_type.is_empty()
        {
            clauses
                .push("event_type = ")
                .push_bind_unseparated(event_type);
        }
        if !actor_types.is_empty() {
            clauses.push("actor_type IN (");
            for (index, actor_type) in actor_types.into_iter().enumerate() {
                if index > 0 {
                    clauses.push_unseparated(", ");
                }
                clauses.push_bind_unseparated(actor_type);
            }
            clauses.push_unseparated(")");
        }
        if let Some(success) = query.success {
            clauses
                .push("success = ")
                .push_bind_unseparated(i64::from(success));
        }
    }
    builder.push(" ORDER BY id DESC LIMIT ").push_bind(limit);
    let rows = builder
        .build()
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let items: Vec<Value> = rows.into_iter().map(|row| json!({
        "id": row.get::<i64,_>("id"), "occurredAt": row.get::<i64,_>("occurred_at"),
        "eventType": row.get::<String,_>("event_type"), "actorType": row.get::<String,_>("actor_type"),
        "actorId": row.try_get::<Option<String>,_>("actor_id").ok().flatten(), "roomCode": row.try_get::<Option<String>,_>("room_code").ok().flatten(),
        "remoteAddr": row.try_get::<Option<String>,_>("remote_addr").ok().flatten(),
        "success": row.get::<i64,_>("success") != 0,
        "detail": row.try_get::<Option<String>,_>("detail_json").ok().flatten().and_then(|value| serde_json::from_str::<Value>(&value).ok()).unwrap_or_else(|| json!({})),
    })).collect();
    let next_before_id = (items.len() == limit as usize)
        .then(|| items.last().and_then(|item| item["id"].as_i64()))
        .flatten();
    let identities = sqlx::query("SELECT player_id, username, updated_at FROM player_identity_mappings ORDER BY updated_at DESC, player_id ASC")
        .fetch_all(&state.db).await.unwrap_or_default().into_iter().map(|row| json!({
            "playerId":row.get::<String,_>("player_id"),"username":row.get::<String,_>("username"),"updatedAt":row.get::<i64,_>("updated_at")
        })).collect::<Vec<_>>();
    let available_event_types =
        sqlx::query("SELECT DISTINCT event_type FROM audit_events ORDER BY event_type")
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.get::<String, _>("event_type"))
            .collect::<Vec<_>>();
    Json(json!({"items": items, "playerIdentityMappings": identities, "nextBeforeId": next_before_id, "limit": limit, "availableEventTypes": available_event_types})).into_response()
}

pub async fn empty_history(State(state): State<AppState>, headers: HeaderMap) -> Response {
    authenticated_json(&state, &headers, json!({"items": [], "total": 0, "page": 1, "pageSize": 50, "availableRooms": [], "serverTime": unix_seconds()})).await
}

#[derive(Default, Deserialize)]
pub struct HistoryQuery {
    #[serde(rename = "roomCode")]
    room_code: Option<String>,
    search: Option<String>,
    page: Option<u32>,
    #[serde(rename = "pageSize")]
    page_size: Option<u32>,
}

pub async fn tab_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    match state
        .tab_history
        .admin_list(
            query.room_code.as_deref(),
            query.search.as_deref(),
            page,
            page_size,
        )
        .await
    {
        Ok(mut payload) => {
            payload["serverTime"] = json!(unix_seconds());
            Json(payload).into_response()
        }
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[derive(Deserialize)]
pub struct HistoryDeleteRecord {
    #[serde(rename = "roomCode")]
    room_code: String,
    #[serde(rename = "playerUuid")]
    player_uuid: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum HistoryDeleteRequest {
    Records(Vec<HistoryDeleteRecord>),
    Wrapped { records: Vec<HistoryDeleteRecord> },
}

pub async fn delete_tab_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<HistoryDeleteRequest>,
) -> Response {
    let Some(session) = authenticate(&state, &headers).await else {
        return unauthorized();
    };
    let records = match request {
        HistoryDeleteRequest::Records(records) | HistoryDeleteRequest::Wrapped { records } => {
            records
        }
    };
    let requested = records.len();
    let mut by_room: HashMap<String, Vec<String>> = HashMap::new();
    for record in records {
        let room = record.room_code.trim();
        if room.is_empty() || Uuid::parse_str(record.player_uuid.trim()).is_err() {
            return invalid_parameter("invalid_history_delete_records");
        }
        by_room
            .entry(room.to_owned())
            .or_default()
            .push(record.player_uuid);
    }
    let mut deleted_records = Vec::new();
    for (room, player_uuids) in by_room {
        let deleted = match state.tab_history.admin_delete(&room, &player_uuids).await {
            Ok(deleted) => deleted,
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
        if !deleted.is_empty()
            && let Ok(head) = state.tab_history.head(&room).await
        {
            let _ = state
                .relay
                .send(crate::relay::RelayEvent::TabHistoryChanged {
                    room: room.clone(),
                    head,
                })
                .await;
        }
        deleted_records.extend(
            deleted
                .into_iter()
                .map(|player_uuid| json!({"roomCode":room,"playerUuid":player_uuid})),
        );
    }
    record_audit(
        &state,
        "admin_tab_history_deleted",
        "admin",
        session["actorId"].as_str(),
        true,
        None,
        json!({"requested":requested,"deleted":deleted_records.len(),"records":deleted_records}),
    )
    .await;
    Json(json!({
        "requested": requested,
        "deleted": deleted_records.len(),
        "missing": requested.saturating_sub(deleted_records.len()),
    }))
    .into_response()
}

pub async fn last_seen_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HistoryQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    let snapshot = match state.relay.snapshot(None).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let all_items = snapshot["lastSeenHistory"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut available_rooms = all_items
        .iter()
        .filter_map(|item| item["roomCode"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    available_rooms.sort();
    available_rooms.dedup();
    let search = query
        .search
        .as_deref()
        .map(normalize_search)
        .filter(|value| !value.is_empty());
    let mut items = all_items
        .into_iter()
        .filter(|item| {
            query
                .room_code
                .as_deref()
                .is_none_or(|room| item["roomCode"].as_str() == Some(room.trim()))
                && search.as_ref().is_none_or(|search| {
                    normalize_search(
                        &[
                            item["roomCode"].as_str().unwrap_or_default(),
                            item["sourceId"].as_str().unwrap_or_default(),
                            item["playerUuid"].as_str().unwrap_or_default(),
                            item["playerName"].as_str().unwrap_or_default(),
                            item["dimension"].as_str().unwrap_or_default(),
                        ]
                        .join(" "),
                    )
                    .contains(search)
                })
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        right["offlineDetectedAtUtcMs"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&left["offlineDetectedAtUtcMs"].as_i64().unwrap_or(0))
    });
    let total = items.len();
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    let offset = (page - 1) as usize * page_size as usize;
    let items = items
        .into_iter()
        .skip(offset)
        .take(page_size as usize)
        .collect::<Vec<_>>();
    Json(json!({
        "items":items,"total":total,"page":page,"pageSize":page_size,
        "availableRooms":available_rooms,"serverTime":unix_seconds(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct LastSeenDeleteRecord {
    #[serde(rename = "roomCode")]
    room_code: String,
    #[serde(rename = "sourceId")]
    source_id: String,
    #[serde(rename = "playerUuid")]
    player_uuid: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum LastSeenDeleteRequest {
    Records(Vec<LastSeenDeleteRecord>),
    Wrapped { records: Vec<LastSeenDeleteRecord> },
}

pub async fn delete_last_seen_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LastSeenDeleteRequest>,
) -> Response {
    let Some(session) = authenticate(&state, &headers).await else {
        return unauthorized();
    };
    let records = match request {
        LastSeenDeleteRequest::Records(records) | LastSeenDeleteRequest::Wrapped { records } => {
            records
        }
    };
    let requested = records.len();
    let mut normalized = Vec::with_capacity(requested);
    for record in records {
        let room = record.room_code.trim();
        let source = record.source_id.trim();
        let Ok(player_uuid) = Uuid::parse_str(record.player_uuid.trim()) else {
            return invalid_parameter("invalid_history_delete_records");
        };
        if room.is_empty() || source.is_empty() {
            return invalid_parameter("invalid_history_delete_records");
        }
        normalized.push((
            room.to_owned(),
            source.to_owned(),
            player_uuid.hyphenated().to_string(),
        ));
    }
    let deleted = match state.relay.delete_last_seen(normalized.clone()).await {
        Ok(deleted) => deleted,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    record_audit(
        &state,
        "admin_last_seen_history_deleted",
        "admin",
        session["actorId"].as_str(),
        true,
        None,
        json!({"requested":requested,"deleted":deleted,"records":normalized}),
    )
    .await;
    Json(json!({
        "requested":requested,"deleted":deleted,"missing":requested.saturating_sub(deleted),
    }))
    .into_response()
}

#[derive(Default, Deserialize)]
pub struct RuntimeQuery {
    #[serde(rename = "roomCode")]
    room_code: Option<String>,
    page: Option<u32>,
    #[serde(rename = "pageSize")]
    page_size: Option<u32>,
}

pub async fn runtime_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Query(query): Query<RuntimeQuery>,
) -> Response {
    if authenticate(&state, &headers).await.is_none() {
        return unauthorized();
    }
    if ![
        "tab-reports",
        "players",
        "entities",
        "waypoints",
        "battle-chunks",
        "player-marks",
    ]
    .contains(&kind.as_str())
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail":"invalid_runtime_kind"})),
        )
            .into_response();
    }
    let room = query
        .room_code
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    let snapshot = match state.relay.snapshot(Some(room.to_owned())).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut items = snapshot["runtimeState"][&kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    items.sort_by(|left, right| {
        left["id"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["id"].as_str().unwrap_or_default())
    });
    let total = items.len();
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(50).clamp(1, 200);
    let offset = (page - 1) as usize * page_size as usize;
    let items = items
        .into_iter()
        .skip(offset)
        .take(page_size as usize)
        .collect::<Vec<_>>();
    Json(json!({
        "kind":kind,"roomCode":room,"items":items,"total":total,"page":page,
        "pageSize":page_size,"availableRooms":snapshot["runtimeRooms"],"serverTime":unix_seconds(),
    }))
    .into_response()
}

async fn authenticated_json(state: &AppState, headers: &HeaderMap, value: Value) -> Response {
    if authenticate(state, headers).await.is_none() {
        unauthorized()
    } else {
        Json(value).into_response()
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Option<Value> {
    let token = cookie_token(headers)?;
    let now = unix_millis();
    let hash = token_hash(&token);
    let row = sqlx::query("SELECT session_id, actor_id, remote_addr, created_at, last_seen_at, expires_at, ended_at FROM admin_sessions WHERE token_hash = ?")
        .bind(&hash).fetch_optional(&state.db).await.ok()??;
    if row
        .try_get::<Option<i64>, _>("ended_at")
        .ok()
        .flatten()
        .is_some()
    {
        return None;
    }
    if row.get::<i64, _>("expires_at") <= now {
        let _ = sqlx::query("UPDATE admin_sessions SET ended_at = ?, end_reason = 'expired' WHERE token_hash = ? AND ended_at IS NULL")
            .bind(now).bind(&hash).execute(&state.db).await;
        return None;
    }
    let expires = now + session_ttl_sec() * 1_000;
    let _ = sqlx::query("UPDATE admin_sessions SET last_seen_at = ?, expires_at = ? WHERE token_hash = ? AND ended_at IS NULL")
        .bind(now).bind(expires).bind(&hash).execute(&state.db).await;
    Some(session_json(
        &row.get::<String, _>("session_id"),
        &row.get::<String, _>("actor_id"),
        row.try_get::<Option<String>, _>("remote_addr")
            .ok()
            .flatten()
            .as_deref(),
        row.get("created_at"),
        now,
        expires,
    ))
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == COOKIE_NAME).then(|| value.to_owned())
        })
}

fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn session_ttl_sec() -> i64 {
    env::var("TEAMVIEWER_ADMIN_SESSION_TTL_SEC")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12 * 60 * 60)
        .clamp(300, 7 * 24 * 60 * 60)
}

fn env_bool(name: &str) -> bool {
    env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn trusted_proxy(remote: IpAddr) -> bool {
    if !env_bool("TEAMVIEWER_TRUST_PROXY_HEADERS") {
        return false;
    }
    env::var("TEAMVIEWER_TRUSTED_PROXY_CIDRS")
        .unwrap_or_else(|_| "127.0.0.1/32,::1/128,172.16.0.0/12".to_owned())
        .split(',')
        .filter_map(|value| value.trim().parse::<IpNet>().ok())
        .any(|network| network.contains(&remote))
}

fn request_remote_addr(headers: &HeaderMap, transport: &TransportConnectInfo) -> String {
    if trusted_proxy(transport.remote_addr.ip()) {
        for name in ["x-forwarded-for", "x-real-ip"] {
            if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok())
                && let Some(first) = value.split(',').next().map(str::trim)
                && !first.is_empty()
            {
                return first.to_owned();
            }
        }
    }
    transport.remote_addr.ip().to_string()
}

fn request_is_secure(headers: &HeaderMap, transport: &TransportConnectInfo) -> bool {
    trusted_proxy(transport.remote_addr.ip())
        && headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
}

async fn record_audit(
    state: &AppState,
    event_type: &str,
    actor_type: &str,
    actor_id: Option<&str>,
    success: bool,
    remote_addr: Option<&str>,
    detail: Value,
) {
    let now = unix_millis();
    let _ = sqlx::query(
        "INSERT INTO audit_events (occurred_at, local_date, local_hour, event_type, actor_type, actor_id, success, remote_addr, detail_json) VALUES (?, strftime('%Y-%m-%d', 'now', 'localtime'), strftime('%Y-%m-%dT%H:00:00', 'now', 'localtime'), ?, ?, ?, ?, ?, ?)",
    )
    .bind(now)
    .bind(event_type)
    .bind(actor_type)
    .bind(actor_id)
    .bind(i64::from(success))
    .bind(remote_addr)
    .bind(detail.to_string())
    .execute(&state.db)
    .await;
}
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"detail":"admin_session_required"})),
    )
        .into_response()
}
fn invalid_parameter(detail: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail":detail})),
    )
        .into_response()
}
fn normalize_search(value: &str) -> String {
    value.trim().to_lowercase()
}
fn timezone_name() -> String {
    Local::now().format("%Z (UTC%:z)").to_string()
}
fn parse_local_datetime(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .ok()
}
fn format_local_hour(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%dT%H:00:00").to_string()
}
fn format_local_second(value: NaiveDateTime) -> String {
    value.format("%Y-%m-%dT%H:%M:%S").to_string()
}
fn align_datetime(value: NaiveDateTime, bucket_seconds: u32) -> NaiveDateTime {
    if bucket_seconds >= 86_400 {
        return value.date().and_hms_opt(0, 0, 0).expect("valid midnight");
    }
    let seconds_since_hour = value.minute() * 60 + value.second();
    let aligned = if bucket_seconds >= 3_600 {
        0
    } else {
        seconds_since_hour / bucket_seconds * bucket_seconds
    };
    value
        .date()
        .and_hms_opt(value.hour(), aligned / 60, aligned % 60)
        .expect("aligned time is valid")
}

async fn traffic_series(
    db: &sqlx::SqlitePool,
    table: &str,
    column: &str,
    start: NaiveDateTime,
    count: u32,
    bucket_seconds: u32,
) -> Value {
    let end = start + ChronoDuration::seconds(i64::from(count * bucket_seconds));
    let start_key = format_bucket_for_column(start, column);
    let end_key = format_bucket_for_column(end, column);
    let query = format!(
        "SELECT {column} AS bucket, channel, direction, bytes FROM {table} WHERE {column} >= ? AND {column} < ?"
    );
    let rows = sqlx::query(&query)
        .bind(start_key)
        .bind(end_key)
        .fetch_all(db)
        .await
        .unwrap_or_default();
    let mut buckets = vec![[0_i64; 4]; count as usize];
    for row in rows {
        let Some(timestamp) = parse_bucket(&row.get::<String, _>("bucket")) else {
            continue;
        };
        let elapsed = timestamp.signed_duration_since(start).num_seconds();
        if elapsed < 0 {
            continue;
        }
        let index = usize::try_from(elapsed / i64::from(bucket_seconds)).unwrap_or(usize::MAX);
        let Some(target) = buckets.get_mut(index) else {
            continue;
        };
        let series = match (
            row.get::<String, _>("channel").as_str(),
            row.get::<String, _>("direction").as_str(),
        ) {
            ("player", "ingress") => 0,
            ("player", "egress") => 1,
            ("web_map", "ingress") => 2,
            ("web_map", "egress") => 3,
            _ => continue,
        };
        target[series] = target[series].saturating_add(row.get::<i64, _>("bytes"));
    }
    let totals = buckets.iter().fold([0_i64; 4], |mut total, value| {
        for index in 0..4 {
            total[index] = total[index].saturating_add(value[index]);
        }
        total
    });
    let items = buckets
        .into_iter()
        .enumerate()
        .map(|(index, values)| {
            let timestamp = start
                + ChronoDuration::seconds(
                    i64::try_from(index).unwrap_or(i64::MAX) * i64::from(bucket_seconds),
                );
            traffic_bucket_json(
                if column == "local_date" {
                    timestamp.format("%Y-%m-%d").to_string()
                } else if column == "local_hour" {
                    format_local_hour(timestamp)
                } else {
                    timestamp.format("%Y-%m-%dT%H:%M:00").to_string()
                },
                values,
            )
        })
        .collect::<Vec<_>>();
    json!({
        "items":items,
        "totalIngressBytes":totals[0].saturating_add(totals[2]),
        "totalEgressBytes":totals[1].saturating_add(totals[3]),
        "totalBytes":totals.iter().sum::<i64>(),
    })
}

fn format_bucket_for_column(value: NaiveDateTime, column: &str) -> String {
    match column {
        "local_date" => value.format("%Y-%m-%d").to_string(),
        "local_hour" => format_local_hour(value),
        _ => value.format("%Y-%m-%dT%H:%M:00").to_string(),
    }
}

fn parse_bucket(value: &str) -> Option<NaiveDateTime> {
    parse_local_datetime(value).or_else(|| {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .and_then(|date| date.and_hms_opt(0, 0, 0))
    })
}

fn traffic_bucket_json(bucket: String, values: [i64; 4]) -> Value {
    json!({
        "bucket":bucket,
        "playerIngressBytes":values[0],"playerEgressBytes":values[1],
        "webMapIngressBytes":values[2],"webMapEgressBytes":values[3],
        "totalIngressBytes":values[0].saturating_add(values[2]),
        "totalEgressBytes":values[1].saturating_add(values[3]),
        "totalBytes":values.iter().sum::<i64>(),
    })
}

fn merge_traffic_metadata(mut series: Value, metadata: Value) -> Value {
    if let (Some(target), Some(source)) = (series.as_object_mut(), metadata.as_object()) {
        target.extend(source.clone());
    }
    series
}
fn session_json(
    id: &str,
    actor: &str,
    remote_addr: Option<&str>,
    created: i64,
    seen: i64,
    expires: i64,
) -> Value {
    json!({"sessionId":id,"actorId":actor,"remoteAddr":remote_addr,"createdAt":created,"lastSeenAt":seen,"expiresAt":expires})
}
fn overview_value(snapshot: &Value) -> Value {
    json!({
        "playerConnections": snapshot["playerConnections"].as_u64().unwrap_or(0),
        "webMapConnections": snapshot["webMapConnections"].as_u64().unwrap_or(0),
        "externalSourceConnections": snapshot["externalSourceConnections"].as_u64().unwrap_or(0),
        "activeRooms": snapshot["activeRooms"].as_u64().unwrap_or(0),
        "rooms": snapshot["rooms"].clone(),
        "connectionDetails": snapshot["connectionDetails"].clone(),
        "timezone":"local","dbPathMasked":".../teamviewer-admin.db",
        "broadcastHz":snapshot["broadcastHz"].as_f64().unwrap_or(20.0),"hourlyPeak24h":0,
        "observability":{"sseSubscribers":0,"lastRetentionCleanup":null,"apiErrors":0,"sseErrors":0,"trustProxyHeaders":env_bool("TEAMVIEWER_TRUST_PROXY_HEADERS")}
    })
}
async fn default_daily_metrics(db: &sqlx::SqlitePool) -> Value {
    let days = 30_u32;
    let start = Local::now().date_naive() - ChronoDuration::days(i64::from(days - 1));
    let rows = sqlx::query("SELECT local_date AS bucket, COUNT(DISTINCT player_id) AS active_players FROM daily_player_activity WHERE local_date >= ? GROUP BY local_date")
        .bind(start.format("%Y-%m-%d").to_string())
        .fetch_all(db).await.unwrap_or_default();
    let counts = rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("bucket"),
                row.get::<i64, _>("active_players"),
            )
        })
        .collect::<HashMap<_, _>>();
    let items = (0..days)
        .map(|offset| {
            let bucket = (start + ChronoDuration::days(i64::from(offset)))
                .format("%Y-%m-%d")
                .to_string();
            json!({"bucket":bucket,"activePlayers":counts.get(&bucket).copied().unwrap_or(0)})
        })
        .collect::<Vec<_>>();
    json!({"timezone":timezone_name(),"roomCode":null,"days":days,"startDate":start.format("%Y-%m-%d").to_string(),"items":items,"serverTime":unix_seconds()})
}

async fn default_hourly_metrics(db: &sqlx::SqlitePool) -> Value {
    let hours = 48_u32;
    let now = Local::now().naive_local();
    let start = now
        .date()
        .and_hms_opt(now.hour(), 0, 0)
        .expect("valid hour")
        - ChronoDuration::hours(i64::from(hours - 1));
    let rows = sqlx::query("SELECT local_hour AS bucket, COUNT(DISTINCT player_id) AS active_players FROM hourly_player_activity WHERE local_hour >= ? GROUP BY local_hour")
        .bind(format_local_hour(start)).fetch_all(db).await.unwrap_or_default();
    let counts = rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("bucket"),
                row.get::<i64, _>("active_players"),
            )
        })
        .collect::<HashMap<_, _>>();
    let items = (0..hours)
        .map(|offset| {
            let bucket = format_local_hour(start + ChronoDuration::hours(i64::from(offset)));
            json!({"bucket":bucket,"activePlayers":counts.get(&bucket).copied().unwrap_or(0)})
        })
        .collect::<Vec<_>>();
    json!({"timezone":timezone_name(),"roomCode":null,"hours":hours,"startAt":format_local_hour(start),"items":items,"serverTime":unix_seconds()})
}

async fn default_traffic_history(db: &sqlx::SqlitePool) -> Value {
    let count = 48_u32;
    let start = align_datetime(Local::now().naive_local(), 3_600)
        - ChronoDuration::hours(i64::from(count - 1));
    let application = traffic_series(
        db,
        "hourly_traffic_bytes",
        "local_hour",
        start,
        count,
        3_600,
    )
    .await;
    let wire = traffic_series(
        db,
        "hourly_wire_traffic_bytes",
        "local_hour",
        start,
        count,
        3_600,
    )
    .await;
    json!({"timezone":timezone_name(),"range":"48h","granularity":"1h","bucketSeconds":3600,"startAt":format_local_second(start),"selectedLayer":"application","application":application,"wire":wire,"serverTime":unix_seconds()})
}
fn unix_millis() -> i64 {
    (unix_seconds() * 1000.0) as i64
}
fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
