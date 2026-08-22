CREATE TABLE daily_player_activity (
    local_date TEXT NOT NULL,
    player_id TEXT NOT NULL,
    room_code TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (local_date, player_id, room_code)
);

CREATE TABLE hourly_player_activity (
    local_hour TEXT NOT NULL,
    player_id TEXT NOT NULL,
    room_code TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    PRIMARY KEY (local_hour, player_id, room_code)
);

CREATE TABLE audit_events (
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

CREATE TABLE player_identity_mappings (
    player_id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE admin_sessions (
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

CREATE TABLE hourly_traffic_bytes (
    local_hour TEXT NOT NULL,
    channel TEXT NOT NULL,
    direction TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (local_hour, channel, direction)
);

CREATE TABLE minute_traffic_bytes (
    local_minute TEXT NOT NULL,
    channel TEXT NOT NULL,
    direction TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (local_minute, channel, direction)
);

CREATE TABLE minute_wire_traffic_bytes (
    local_minute TEXT NOT NULL,
    channel TEXT NOT NULL,
    direction TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (local_minute, channel, direction)
);

CREATE TABLE daily_traffic_bytes (
    local_date TEXT NOT NULL,
    channel TEXT NOT NULL,
    direction TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (local_date, channel, direction)
);

CREATE TABLE hourly_wire_traffic_bytes (
    local_hour TEXT NOT NULL,
    channel TEXT NOT NULL,
    direction TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (local_hour, channel, direction)
);

CREATE TABLE daily_wire_traffic_bytes (
    local_date TEXT NOT NULL,
    channel TEXT NOT NULL,
    direction TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (local_date, channel, direction)
);

CREATE INDEX idx_daily_player_activity_date ON daily_player_activity (local_date);
CREATE INDEX idx_daily_player_activity_room_date ON daily_player_activity (room_code, local_date);
CREATE INDEX idx_hourly_player_activity_hour ON hourly_player_activity (local_hour);
CREATE INDEX idx_hourly_player_activity_room_hour ON hourly_player_activity (room_code, local_hour);
CREATE INDEX idx_audit_events_occurred_at ON audit_events (occurred_at DESC);
CREATE INDEX idx_audit_events_filters ON audit_events (event_type, actor_type, success, id DESC);
CREATE INDEX idx_player_identity_mappings_updated_at ON player_identity_mappings (updated_at DESC, player_id ASC);
CREATE INDEX idx_admin_sessions_token_hash ON admin_sessions (token_hash);
CREATE INDEX idx_admin_sessions_expiry ON admin_sessions (expires_at, ended_at);
