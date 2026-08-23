use std::{collections::HashSet, env, net::SocketAddr, sync::Arc};

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use teamviewrelay_rust::{
    config::RuntimeConfig,
    metrics::{Direction, Layer, Metrics, TrafficChannel, TrafficIncrement},
    relay::RelayHandle,
    tab_history::TabHistoryStore,
    transport::{CountingListener, TransportConnectInfo},
    web::{AppState, router},
};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let db_path =
        env::var("TEAMVIEWER_DB_PATH").unwrap_or_else(|_| "./data/teamviewer-admin.db".to_owned());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let options: SqliteConnectOptions = format!("sqlite://{db_path}")
        .parse::<SqliteConnectOptions>()?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let db = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&db).await?;

    let config = Arc::new(RuntimeConfig::load());
    let tab_history = Arc::new(TabHistoryStore::new(db.clone()));
    tab_history.initialize().await?;
    let metrics = Arc::new(Metrics::default());
    tokio::spawn(flush_traffic_loop(db.clone(), metrics.clone()));
    let state = AppState {
        relay: RelayHandle::spawn(config.clone()),
        db,
        tab_history,
        config,
        metrics,
        maintenance_rooms: Arc::new(tokio::sync::RwLock::new(HashSet::new())),
    };
    let port = env::var("TEAMVIEWER_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8765);
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = CountingListener::new(tokio::net::TcpListener::bind(address).await?);
    info!(%address, "TeamViewRelay Rust backend listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<TransportConnectInfo>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP server failed")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}

async fn flush_traffic_loop(db: sqlx::SqlitePool, metrics: Arc<Metrics>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        interval.tick().await;
        let increments = metrics.drain_pending();
        if increments.is_empty() {
            continue;
        }
        if let Err(error) = flush_traffic(&db, &increments).await {
            tracing::warn!(%error, "failed to flush traffic metrics");
            metrics.requeue(increments);
        }
    }
}

async fn flush_traffic(
    db: &sqlx::SqlitePool,
    increments: &[TrafficIncrement],
) -> anyhow::Result<()> {
    let mut transaction = db.begin().await?;
    for increment in increments {
        let (minute_table, hourly_table, daily_table) = match increment.layer {
            Layer::Application => (
                "minute_traffic_bytes",
                "hourly_traffic_bytes",
                "daily_traffic_bytes",
            ),
            Layer::Wire => (
                "minute_wire_traffic_bytes",
                "hourly_wire_traffic_bytes",
                "daily_wire_traffic_bytes",
            ),
        };
        let channel = match increment.channel {
            TrafficChannel::Player => "player",
            TrafficChannel::WebMap => "web_map",
        };
        let direction = match increment.direction {
            Direction::Ingress => "ingress",
            Direction::Egress => "egress",
        };
        for (table, column, format) in [
            (minute_table, "local_minute", "%Y-%m-%dT%H:%M:00"),
            (hourly_table, "local_hour", "%Y-%m-%dT%H:00:00"),
            (daily_table, "local_date", "%Y-%m-%d"),
        ] {
            let query = format!(
                "INSERT INTO {table} ({column}, channel, direction, bytes) VALUES (strftime('{format}', ?, 'unixepoch', 'localtime'), ?, ?, ?) ON CONFLICT({column}, channel, direction) DO UPDATE SET bytes = bytes + excluded.bytes"
            );
            sqlx::query(&query)
                .bind(increment.second)
                .bind(channel)
                .bind(direction)
                .bind(i64::try_from(increment.bytes).unwrap_or(i64::MAX))
                .execute(&mut *transaction)
                .await?;
        }
    }
    transaction.commit().await?;
    Ok(())
}
