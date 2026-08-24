use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::File,
    io::{BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, bail};
use chrono::{Local, SecondsFormat, Utc};
use pprof::protos::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{RwLock, mpsc},
};
use tracing::{info, warn};

use crate::{metrics::Metrics, relay::RelayHandle};

const BUILD_FLAVOR: &str = "memory-debug";

#[derive(Clone, Debug)]
pub struct ResourceDebugConfig {
    pub directory: PathBuf,
    pub sample_interval: Duration,
    pub automatic_cpu_profiles: bool,
    pub cpu_trigger_percent: f64,
    pub cpu_trigger_samples: u32,
    pub cpu_profile_duration: Duration,
    pub memory_growth_bytes: u64,
    pub periodic_profile_interval: Duration,
    pub profile_cooldown: Duration,
    pub startup_profile_delay: Duration,
    pub retention_days: usize,
    pub max_profile_groups: usize,
    pub max_profile_bytes: u64,
}

impl ResourceDebugConfig {
    fn load(db_path: &str) -> Self {
        let default_directory = Path::new(db_path)
            .parent()
            .unwrap_or_else(|| Path::new("./data"))
            .join("memory-debug");
        Self {
            directory: env::var_os("TEAMVIEWER_DEBUG_DIR")
                .map(PathBuf::from)
                .unwrap_or(default_directory),
            sample_interval: Duration::from_secs(env_u64(
                "TEAMVIEWER_DEBUG_SAMPLE_INTERVAL_SEC",
                10,
                2,
                300,
            )),
            automatic_cpu_profiles: env_bool("TEAMVIEWER_DEBUG_AUTO_CPU_PROFILE", false),
            cpu_trigger_percent: env_f64(
                "TEAMVIEWER_DEBUG_CPU_TRIGGER_PERCENT",
                20.0,
                1.0,
                1_600.0,
            ),
            cpu_trigger_samples: env_u64("TEAMVIEWER_DEBUG_CPU_TRIGGER_SAMPLES", 3, 1, 30) as u32,
            cpu_profile_duration: Duration::from_secs(env_u64(
                "TEAMVIEWER_DEBUG_CPU_PROFILE_SEC",
                30,
                5,
                300,
            )),
            memory_growth_bytes: env_u64("TEAMVIEWER_DEBUG_MEMORY_GROWTH_MIB", 8, 1, 4_096)
                * 1024
                * 1024,
            periodic_profile_interval: Duration::from_secs(env_u64(
                "TEAMVIEWER_DEBUG_PERIODIC_PROFILE_SEC",
                3_600,
                300,
                86_400,
            )),
            profile_cooldown: Duration::from_secs(env_u64(
                "TEAMVIEWER_DEBUG_PROFILE_COOLDOWN_SEC",
                1_800,
                60,
                86_400,
            )),
            startup_profile_delay: Duration::from_secs(env_u64(
                "TEAMVIEWER_DEBUG_STARTUP_PROFILE_DELAY_SEC",
                120,
                10,
                3_600,
            )),
            retention_days: env_u64("TEAMVIEWER_DEBUG_RETENTION_DAYS", 7, 1, 90) as usize,
            max_profile_groups: env_u64("TEAMVIEWER_DEBUG_MAX_PROFILES", 48, 2, 1_000) as usize,
            max_profile_bytes: env_u64("TEAMVIEWER_DEBUG_MAX_DISK_MIB", 512, 32, 32_768)
                * 1024
                * 1024,
        }
    }

    fn public_json(&self) -> Value {
        json!({
            "directory": self.directory,
            "sampleIntervalSec": self.sample_interval.as_secs(),
            "automaticCpuProfiles": self.automatic_cpu_profiles,
            "cpuTriggerPercent": self.cpu_trigger_percent,
            "cpuTriggerSamples": self.cpu_trigger_samples,
            "cpuProfileSec": self.cpu_profile_duration.as_secs(),
            "memoryGrowthMiB": self.memory_growth_bytes / 1024 / 1024,
            "periodicProfileSec": self.periodic_profile_interval.as_secs(),
            "profileCooldownSec": self.profile_cooldown.as_secs(),
            "retentionDays": self.retention_days,
            "maxProfileGroups": self.max_profile_groups,
            "maxDiskMiB": self.max_profile_bytes / 1024 / 1024,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    Cpu,
    Heap,
}

impl ProfileKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cpu" => Some(Self::Cpu),
            "heap" => Some(Self::Heap),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Heap => "heap",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileStatus {
    pub busy_kind: Option<String>,
    pub busy_reason: Option<String>,
    pub last_completed_at: Option<String>,
    pub last_completed_kind: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileFile {
    pub filename: String,
    pub size_bytes: u64,
    pub modified_at: Option<String>,
    pub content_type: &'static str,
}

struct ProfileCommand {
    kind: ProfileKind,
    reason: String,
}

struct ResourceDebugInner {
    config: ResourceDebugConfig,
    latest: RwLock<Option<Value>>,
    profile_status: RwLock<ProfileStatus>,
    profile_tx: mpsc::Sender<ProfileCommand>,
    cpu_pending: AtomicBool,
    heap_pending: AtomicBool,
}

#[derive(Clone)]
pub struct ResourceDebugHandle {
    inner: Arc<ResourceDebugInner>,
}

impl ResourceDebugHandle {
    pub async fn start(
        relay: RelayHandle,
        db: SqlitePool,
        metrics: Arc<Metrics>,
        db_path: String,
    ) -> anyhow::Result<Self> {
        let config = ResourceDebugConfig::load(&db_path);
        fs::create_dir_all(config.directory.join("profiles"))
            .await
            .with_context(|| {
                format!(
                    "failed to create memory debug directory {}",
                    config.directory.display()
                )
            })?;
        jemalloc_pprof::activate_jemalloc_profiling().await;
        let (profile_tx, profile_rx) = mpsc::channel(8);
        let inner = Arc::new(ResourceDebugInner {
            config,
            latest: RwLock::new(None),
            profile_status: RwLock::new(ProfileStatus::default()),
            profile_tx,
            cpu_pending: AtomicBool::new(false),
            heap_pending: AtomicBool::new(false),
        });
        tokio::spawn(profile_worker(inner.clone(), profile_rx));
        tokio::spawn(sample_loop(inner.clone(), relay, db, metrics, db_path));
        info!(
            directory = %inner.config.directory.display(),
            interval_sec = inner.config.sample_interval.as_secs(),
            automatic_cpu_profiles = inner.config.automatic_cpu_profiles,
            "memory/CPU debug monitoring enabled"
        );
        Ok(Self { inner })
    }

    pub async fn current(&self) -> Value {
        json!({
            "enabled": true,
            "buildFlavor": BUILD_FLAVOR,
            "config": self.inner.config.public_json(),
            "profileStatus": self.inner.profile_status.read().await.clone(),
            "sample": self.inner.latest.read().await.clone(),
        })
    }

    pub fn request_profile(&self, kind: ProfileKind, reason: impl Into<String>) -> bool {
        request_profile(&self.inner, kind, reason.into())
    }

    pub async fn profiles(&self) -> anyhow::Result<Vec<ProfileFile>> {
        list_profiles(&self.inner.config.directory.join("profiles")).await
    }

    pub async fn read_profile(&self, filename: &str) -> anyhow::Result<(Vec<u8>, &'static str)> {
        if !valid_profile_filename(filename) {
            bail!("invalid profile filename");
        }
        let content_type = profile_content_type(filename).context("unsupported profile file")?;
        let bytes = fs::read(self.inner.config.directory.join("profiles").join(filename))
            .await
            .context("profile not found")?;
        Ok((bytes, content_type))
    }
}

fn request_profile(inner: &Arc<ResourceDebugInner>, kind: ProfileKind, reason: String) -> bool {
    let pending = match kind {
        ProfileKind::Cpu => &inner.cpu_pending,
        ProfileKind::Heap => &inner.heap_pending,
    };
    if pending.swap(true, Ordering::AcqRel) {
        return false;
    }
    if inner
        .profile_tx
        .try_send(ProfileCommand {
            kind,
            reason: sanitize_reason(&reason),
        })
        .is_err()
    {
        pending.store(false, Ordering::Release);
        return false;
    }
    true
}

async fn profile_worker(
    inner: Arc<ResourceDebugInner>,
    mut profile_rx: mpsc::Receiver<ProfileCommand>,
) {
    while let Some(command) = profile_rx.recv().await {
        {
            let mut status = inner.profile_status.write().await;
            status.busy_kind = Some(command.kind.as_str().to_owned());
            status.busy_reason = Some(command.reason.clone());
            status.last_error = None;
        }
        let result = match command.kind {
            ProfileKind::Cpu => capture_cpu_profile(&inner.config, &command.reason).await,
            ProfileKind::Heap => capture_heap_profile(&inner.config, &command.reason).await,
        };
        let finished_at = timestamp();
        {
            let mut status = inner.profile_status.write().await;
            status.busy_kind = None;
            status.busy_reason = None;
            match result {
                Ok(()) => {
                    status.last_completed_at = Some(finished_at);
                    status.last_completed_kind = Some(command.kind.as_str().to_owned());
                    status.last_error = None;
                }
                Err(error) => {
                    warn!(
                        kind = command.kind.as_str(),
                        reason = command.reason,
                        %error,
                        "resource profile failed"
                    );
                    status.last_error = Some(error.to_string());
                }
            }
        }
        match command.kind {
            ProfileKind::Cpu => inner.cpu_pending.store(false, Ordering::Release),
            ProfileKind::Heap => inner.heap_pending.store(false, Ordering::Release),
        }
        if let Err(error) = prune_profiles(&inner.config).await {
            warn!(%error, "failed to prune resource profiles");
        }
    }
}

async fn capture_heap_profile(config: &ResourceDebugConfig, reason: &str) -> anyhow::Result<()> {
    let prefix = profile_prefix(ProfileKind::Heap, reason);
    let directory = config.directory.join("profiles");
    let profiler = jemalloc_pprof::PROF_CTL
        .as_ref()
        .context("jemalloc profiling is unavailable")?;
    let mut profiler = profiler.lock().await;
    if !profiler.activated() {
        profiler
            .activate()
            .context("failed to activate jemalloc profiler")?;
    }
    let mut dump = profiler.dump().context("failed to dump heap profile")?;
    drop(profiler);

    let raw_path = directory.join(format!("{prefix}.jeheap"));
    let mappings_path = directory.join(format!("{prefix}.maps.json"));
    let output_prefix = directory.join(&prefix);
    let mappings = mappings::MAPPINGS
        .as_deref()
        .context("failed to read process mappings")?
        .iter()
        .map(StoredMapping::from)
        .collect::<Vec<_>>();
    let raw_path_for_write = raw_path.clone();
    let mappings_path_for_write = mappings_path.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        dump.seek(SeekFrom::Start(0))?;
        let mut output = File::create(raw_path_for_write)?;
        std::io::copy(&mut dump, &mut output)?;
        serde_json::to_writer(File::create(mappings_path_for_write)?, &mappings)?;
        Ok(())
    })
    .await
    .context("heap dump writer task failed")??;

    let status = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--render-heap-profile")
        .arg(&raw_path)
        .arg(&mappings_path)
        .arg(&output_prefix)
        .status()
        .await
        .context("failed to start isolated heap renderer")?;
    if !status.success() {
        bail!("isolated heap renderer exited with {status}");
    }
    info!(profile = prefix, "heap profile captured");
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct StoredMapping {
    memory_start: usize,
    memory_end: usize,
    memory_offset: usize,
    file_offset: u64,
    pathname: PathBuf,
    build_id: Option<Vec<u8>>,
}

impl From<&pprof_util::Mapping> for StoredMapping {
    fn from(value: &pprof_util::Mapping) -> Self {
        Self {
            memory_start: value.memory_start,
            memory_end: value.memory_end,
            memory_offset: value.memory_offset,
            file_offset: value.file_offset,
            pathname: value.pathname.clone(),
            build_id: value.build_id.as_ref().map(|value| value.0.clone()),
        }
    }
}

impl From<StoredMapping> for pprof_util::Mapping {
    fn from(value: StoredMapping) -> Self {
        Self {
            memory_start: value.memory_start,
            memory_end: value.memory_end,
            memory_offset: value.memory_offset,
            file_offset: value.file_offset,
            pathname: value.pathname,
            build_id: value.build_id.map(pprof_util::BuildId),
        }
    }
}

pub fn render_heap_profile(
    raw_path: &Path,
    mappings_path: &Path,
    output_prefix: &Path,
) -> anyhow::Result<()> {
    let mappings = serde_json::from_reader::<_, Vec<StoredMapping>>(File::open(mappings_path)?)?
        .into_iter()
        .map(pprof_util::Mapping::from)
        .collect::<Vec<_>>();
    let profile = pprof_util::parse_jeheap(
        BufReader::new(File::open(raw_path)?),
        Some(mappings.as_slice()),
    )?;
    let pprof = profile.to_pprof(("inuse_space", "bytes"), ("space", "bytes"), None);
    let mut flamegraph_options = pprof_util::FlamegraphOptions::default();
    flamegraph_options.title = "inuse_space".to_owned();
    flamegraph_options.count_name = "bytes".to_owned();
    let flamegraph = profile
        .to_flamegraph(&mut flamegraph_options)
        .unwrap_or_else(|_| empty_heap_flamegraph().as_bytes().to_vec());
    let mut pprof_path = output_prefix.as_os_str().to_os_string();
    pprof_path.push(".pb.gz");
    let mut flamegraph_path = output_prefix.as_os_str().to_os_string();
    flamegraph_path.push(".svg");
    std::fs::write(PathBuf::from(pprof_path), pprof)?;
    std::fs::write(PathBuf::from(flamegraph_path), flamegraph)?;
    Ok(())
}

async fn capture_cpu_profile(config: &ResourceDebugConfig, reason: &str) -> anyhow::Result<()> {
    let duration = config.cpu_profile_duration;
    let prefix = profile_prefix(ProfileKind::Cpu, reason);
    let directory = config.directory.join("profiles");
    warn!(
        duration_sec = duration.as_secs(),
        "CPU profile is intrusive and may permanently warm symbol caches; resource samples remain enabled"
    );
    let (pprof, flamegraph) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(99)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .context("failed to start CPU profiler")?;
        std::thread::sleep(duration);
        let report = guard
            .report()
            .build()
            .context("failed to build CPU profile")?;
        drop(guard);
        let mut flamegraph = Vec::new();
        report
            .flamegraph(&mut flamegraph)
            .context("failed to render CPU flamegraph")?;
        if flamegraph.is_empty() {
            flamegraph.extend_from_slice(empty_cpu_flamegraph().as_bytes());
        }
        let profile = report.pprof().context("failed to encode CPU pprof")?;
        let mut pprof = Vec::new();
        profile
            .write_to_vec(&mut pprof)
            .context("failed to serialize CPU pprof")?;
        Ok((pprof, flamegraph))
    })
    .await
    .context("CPU profiler task failed")??;
    fs::write(directory.join(format!("{prefix}.pb")), pprof).await?;
    fs::write(directory.join(format!("{prefix}.svg")), flamegraph).await?;
    info!(
        profile = prefix,
        duration_sec = duration.as_secs(),
        "CPU profile captured"
    );
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct CpuPoint {
    process_ticks: u64,
    cgroup_usage_usec: Option<u64>,
    sampled_at: Option<Instant>,
}

async fn sample_loop(
    inner: Arc<ResourceDebugInner>,
    relay: RelayHandle,
    db: SqlitePool,
    metrics: Arc<Metrics>,
    db_path: String,
) {
    let started = Instant::now();
    let clock_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let host_cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let cgroup = CgroupPaths::discover().await;
    let mut previous_cpu = CpuPoint::default();
    let mut previous_threads = HashMap::new();
    let mut cpu_trigger_count = 0_u32;
    let mut startup_profiles_requested = false;
    let mut last_cpu_request: Option<Instant> = None;
    let mut last_heap_request: Option<Instant> = None;
    let mut last_periodic_request = started;
    let mut heap_anchor: Option<(u64, u64)> = None;
    let mut last_sample_day = String::new();
    let mut previous_relay_stats = None;
    let mut previous_metrics_stats = None;
    let mut interval = tokio::time::interval(inner.config.sample_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let sample_started = Instant::now();
        let smaps = read_smaps_rollup().await.unwrap_or_default();
        let proc_status = read_key_value_kib("/proc/self/status")
            .await
            .unwrap_or_default();
        let process_ticks = read_process_ticks("/proc/self/stat").await.unwrap_or(0);
        let cgroup_stats = cgroup.read().await;
        let now = Instant::now();
        let wall_seconds = previous_cpu
            .sampled_at
            .map(|previous| now.duration_since(previous).as_secs_f64());
        let process_cpu_percent = wall_seconds.and_then(|wall| {
            (wall > 0.0).then(|| {
                process_ticks.saturating_sub(previous_cpu.process_ticks) as f64 / clock_ticks / wall
                    * 100.0
            })
        });
        let cgroup_cpu_percent = wall_seconds.and_then(|wall| {
            previous_cpu
                .cgroup_usage_usec
                .zip(cgroup_stats.cpu_usage_usec)
                .filter(|_| wall > 0.0)
                .map(|(previous, current)| {
                    current.saturating_sub(previous) as f64 / 1_000_000.0 / wall * 100.0
                })
        });
        previous_cpu = CpuPoint {
            process_ticks,
            cgroup_usage_usec: cgroup_stats.cpu_usage_usec,
            sampled_at: Some(now),
        };
        let top_threads = read_thread_cpu(&mut previous_threads, wall_seconds, clock_ticks)
            .await
            .unwrap_or_default();
        let jemalloc = jemalloc_stats();
        let rss_bytes = smaps.get("Rss").copied().unwrap_or(0);
        let allocated_bytes = jemalloc.get("allocated").copied().unwrap_or(0);
        let relay_stats = relay.debug_stats().await.ok();
        let metrics_stats = metrics.debug_stats();
        let runtime_metrics = tokio::runtime::Handle::current().metrics();
        let sqlite = sqlite_json(&db, &db_path).await;
        let rates = debug_rates(
            relay_stats.as_ref(),
            previous_relay_stats.as_ref(),
            &metrics_stats,
            previous_metrics_stats.as_ref(),
            wall_seconds,
        );
        previous_relay_stats = relay_stats.clone();
        previous_metrics_stats = Some(metrics_stats.clone());
        let sample_collection_ms = sample_started.elapsed().as_secs_f64() * 1_000.0;
        let sample = json!({
            "timestamp": timestamp(),
            "buildFlavor": BUILD_FLAVOR,
            "uptimeSec": started.elapsed().as_secs_f64(),
            "sampleCollectionMs": sample_collection_ms,
            "process": {
                "cpuPercentOneCore": process_cpu_percent,
                "cpuPercentHost": process_cpu_percent.map(|value| value / host_cpus as f64),
                "rssBytes": rss_bytes,
                "pssBytes": smaps.get("Pss").copied(),
                "anonymousBytes": smaps.get("Anonymous").copied(),
                "privateDirtyBytes": smaps.get("Private_Dirty").copied(),
                "swapBytes": smaps.get("Swap").copied(),
                "threads": proc_status.get("Threads").map(|value| value / 1024),
                "voluntaryContextSwitches": proc_status.get("voluntary_ctxt_switches").map(|value| value / 1024),
                "nonvoluntaryContextSwitches": proc_status.get("nonvoluntary_ctxt_switches").map(|value| value / 1024),
                "topThreads": top_threads,
                "hostLogicalCpus": host_cpus,
            },
            "cgroup": cgroup_stats.to_json(cgroup_cpu_percent),
            "jemalloc": jemalloc,
            "sqlite": sqlite,
            "relay": relay_stats,
            "traffic": metrics_stats,
            "rates": rates,
            "tokio": {
                "workerThreads": runtime_metrics.num_workers(),
                "aliveTasks": runtime_metrics.num_alive_tasks(),
                "globalQueueDepth": runtime_metrics.global_queue_depth(),
            },
        });
        *inner.latest.write().await = Some(sample.clone());
        let day = Local::now().format("%Y-%m-%d").to_string();
        if let Err(error) = append_sample(&inner.config.directory, &day, &sample).await {
            warn!(%error, "failed to write resource sample");
        }
        if day != last_sample_day {
            last_sample_day = day;
            if let Err(error) = prune_samples(&inner.config).await {
                warn!(%error, "failed to prune resource samples");
            }
        }

        let elapsed = started.elapsed();
        if !startup_profiles_requested && elapsed >= inner.config.startup_profile_delay {
            startup_profiles_requested = true;
            if request_profile(&inner, ProfileKind::Heap, "startup-baseline".to_owned()) {
                last_heap_request = Some(now);
                heap_anchor = Some((rss_bytes, allocated_bytes));
            }
            if inner.config.automatic_cpu_profiles
                && request_profile(&inner, ProfileKind::Cpu, "startup-baseline".to_owned())
            {
                last_cpu_request = Some(now);
            }
            last_periodic_request = now;
        }

        if inner.config.automatic_cpu_profiles
            && process_cpu_percent.is_some_and(|cpu| cpu >= inner.config.cpu_trigger_percent)
        {
            cpu_trigger_count = cpu_trigger_count.saturating_add(1);
        } else {
            cpu_trigger_count = 0;
        }
        if inner.config.automatic_cpu_profiles
            && cpu_trigger_count >= inner.config.cpu_trigger_samples
            && cooldown_elapsed(last_cpu_request, now, inner.config.profile_cooldown)
            && request_profile(&inner, ProfileKind::Cpu, "high-cpu".to_owned())
        {
            cpu_trigger_count = 0;
            last_cpu_request = Some(now);
        }

        let growth_triggered = heap_anchor.is_some_and(|(rss_anchor, allocated_anchor)| {
            rss_bytes.saturating_sub(rss_anchor) >= inner.config.memory_growth_bytes
                || allocated_bytes.saturating_sub(allocated_anchor)
                    >= inner.config.memory_growth_bytes
        });
        if growth_triggered
            && cooldown_elapsed(last_heap_request, now, inner.config.profile_cooldown)
            && request_profile(&inner, ProfileKind::Heap, "memory-growth".to_owned())
        {
            last_heap_request = Some(now);
            heap_anchor = Some((rss_bytes, allocated_bytes));
        }

        if startup_profiles_requested
            && now.duration_since(last_periodic_request) >= inner.config.periodic_profile_interval
        {
            if request_profile(&inner, ProfileKind::Heap, "periodic".to_owned()) {
                last_heap_request = Some(now);
                heap_anchor = Some((rss_bytes, allocated_bytes));
            }
            if inner.config.automatic_cpu_profiles
                && request_profile(&inner, ProfileKind::Cpu, "periodic".to_owned())
            {
                last_cpu_request = Some(now);
            }
            last_periodic_request = now;
        }
    }
}

fn debug_rates(
    relay: Option<&crate::relay::RelayDebugStats>,
    previous_relay: Option<&crate::relay::RelayDebugStats>,
    metrics: &crate::metrics::MetricsDebugStats,
    previous_metrics: Option<&crate::metrics::MetricsDebugStats>,
    wall_seconds: Option<f64>,
) -> Value {
    let Some(wall) = wall_seconds.filter(|wall| *wall > 0.0) else {
        return Value::Null;
    };
    let relay_rates = relay.zip(previous_relay).map(|(current, previous)| {
        let broadcasts = current
            .broadcasts_total
            .saturating_sub(previous.broadcasts_total);
        let broadcast_nanoseconds = current
            .broadcast_nanoseconds
            .saturating_sub(previous.broadcast_nanoseconds);
        let digests = current.digests_total.saturating_sub(previous.digests_total);
        let payload_builds = current
            .payload_builds_total
            .saturating_sub(previous.payload_builds_total);
        let payload_cache_hits = current
            .payload_cache_hits
            .saturating_sub(previous.payload_cache_hits);
        let digest_cache_hits = current
            .digest_cache_hits
            .saturating_sub(previous.digest_cache_hits);
        let digest_nanoseconds = current
            .digest_nanoseconds
            .saturating_sub(previous.digest_nanoseconds);
        json!({
            "eventsPerSec": per_second(current.events_total.saturating_sub(previous.events_total), wall),
            "playerReportsPerSec": per_second(current.player_reports.saturating_sub(previous.player_reports), wall),
            "deliveredPerSec": per_second(current.delivered_events.saturating_sub(previous.delivered_events), wall),
            "ticksPerSec": per_second(current.ticks_total.saturating_sub(previous.ticks_total), wall),
            "broadcastsPerSec": per_second(broadcasts, wall),
            "broadcastAverageMs": average_nanoseconds(broadcast_nanoseconds, broadcasts),
            "cleanupMsPerSec": broadcast_duration_rate(
                current.cleanup_nanoseconds.saturating_sub(previous.cleanup_nanoseconds),
                wall,
            ),
            "recipientsPerSec": per_second(current.broadcast_recipients.saturating_sub(previous.broadcast_recipients), wall),
            "encodedPayloadBytesPerSec": per_second(current.encoded_payload_bytes.saturating_sub(previous.encoded_payload_bytes), wall),
            "payloadBuildsPerSec": per_second(payload_builds, wall),
            "payloadCacheHitsPerSec": per_second(payload_cache_hits, wall),
            "payloadCacheHitRatio": ratio(payload_cache_hits, payload_builds.saturating_add(payload_cache_hits)),
            "digestsPerSec": per_second(digests, wall),
            "digestCacheHitsPerSec": per_second(digest_cache_hits, wall),
            "digestCacheHitRatio": ratio(digest_cache_hits, digests.saturating_add(digest_cache_hits)),
            "digestAverageMs": average_nanoseconds(digest_nanoseconds, digests),
        })
    });
    let traffic_rates = previous_metrics.map(|previous| {
        let writer_sends = metrics
            .writer_send_count
            .saturating_sub(previous.writer_send_count);
        let writer_nanoseconds = metrics
            .writer_send_nanoseconds
            .saturating_sub(previous.writer_send_nanoseconds);
        json!({
            "protobufMessagesPerSec": per_second(metrics.protobuf_messages_total.saturating_sub(previous.protobuf_messages_total), wall),
            "protobufBytesPerSec": per_second(metrics.protobuf_bytes_total.saturating_sub(previous.protobuf_bytes_total), wall),
            "writerSendsPerSec": per_second(writer_sends, wall),
            "writerSendAverageMs": average_nanoseconds(writer_nanoseconds, writer_sends),
            "writerFailuresPerSec": per_second(metrics.writer_send_failures.saturating_sub(previous.writer_send_failures), wall),
        })
    });
    json!({"relay": relay_rates, "traffic": traffic_rates})
}

fn per_second(value: u64, wall_seconds: f64) -> f64 {
    value as f64 / wall_seconds
}

fn average_nanoseconds(nanoseconds: u64, operations: u64) -> Option<f64> {
    (operations > 0).then(|| nanoseconds as f64 / operations as f64 / 1_000_000.0)
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn broadcast_duration_rate(nanoseconds: u64, wall_seconds: f64) -> f64 {
    nanoseconds as f64 / 1_000_000.0 / wall_seconds
}

fn cooldown_elapsed(previous: Option<Instant>, now: Instant, cooldown: Duration) -> bool {
    previous.is_none_or(|previous| now.duration_since(previous) >= cooldown)
}

async fn append_sample(directory: &Path, day: &str, sample: &Value) -> anyhow::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(format!("samples-{day}.jsonl")))
        .await?;
    file.write_all(serde_json::to_string(sample)?.as_bytes())
        .await?;
    file.write_all(b"\n").await?;
    file.flush().await?;
    Ok(())
}

async fn prune_samples(config: &ResourceDebugConfig) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(&config.directory).await?;
    let mut samples = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("samples-") && name.ends_with(".jsonl") {
            samples.push((name, entry.path()));
        }
    }
    samples.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in samples.into_iter().skip(config.retention_days) {
        fs::remove_file(path).await?;
    }
    Ok(())
}

async fn prune_profiles(config: &ResourceDebugConfig) -> anyhow::Result<()> {
    let directory = config.directory.join("profiles");
    let mut entries = fs::read_dir(&directory).await?;
    let mut groups: BTreeMap<String, Vec<(PathBuf, u64)>> = BTreeMap::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_profile_filename(&name) {
            continue;
        }
        let key = profile_group_key(&name);
        let size = entry.metadata().await.map_or(0, |metadata| metadata.len());
        groups.entry(key).or_default().push((entry.path(), size));
    }
    let mut total_bytes = groups
        .values()
        .flatten()
        .map(|(_, size)| *size)
        .sum::<u64>();
    while groups.len() > config.max_profile_groups || total_bytes > config.max_profile_bytes {
        let Some(oldest) = groups
            .keys()
            .min_by(|left, right| profile_sort_key(left).cmp(profile_sort_key(right)))
            .cloned()
        else {
            break;
        };
        if let Some(files) = groups.remove(&oldest) {
            for (path, size) in files {
                total_bytes = total_bytes.saturating_sub(size);
                fs::remove_file(path).await?;
            }
        }
    }
    Ok(())
}

async fn list_profiles(directory: &Path) -> anyhow::Result<Vec<ProfileFile>> {
    let mut entries = fs::read_dir(directory).await?;
    let mut profiles = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let filename = entry.file_name().to_string_lossy().into_owned();
        if !valid_profile_filename(&filename) {
            continue;
        }
        let metadata = entry.metadata().await?;
        let modified_at = metadata.modified().ok().map(system_time_string);
        profiles.push(ProfileFile {
            content_type: profile_content_type(&filename).unwrap_or("application/octet-stream"),
            filename,
            size_bytes: metadata.len(),
            modified_at,
        });
    }
    profiles.sort_by(|left, right| right.filename.cmp(&left.filename));
    Ok(profiles)
}

fn valid_profile_filename(filename: &str) -> bool {
    !filename.contains('/')
        && !filename.contains('\\')
        && !filename.contains("..")
        && (filename.starts_with("cpu-") || filename.starts_with("heap-"))
        && profile_content_type(filename).is_some()
}

fn profile_content_type(filename: &str) -> Option<&'static str> {
    if filename.ends_with(".svg") {
        Some("image/svg+xml")
    } else if filename.ends_with(".pb.gz") {
        Some("application/gzip")
    } else if filename.ends_with(".pb") || filename.ends_with(".jeheap") {
        Some("application/octet-stream")
    } else if filename.ends_with(".maps.json") {
        Some("application/json")
    } else {
        None
    }
}

fn profile_group_key(filename: &str) -> String {
    filename
        .strip_suffix(".pb.gz")
        .or_else(|| filename.strip_suffix(".svg"))
        .or_else(|| filename.strip_suffix(".pb"))
        .or_else(|| filename.strip_suffix(".jeheap"))
        .or_else(|| filename.strip_suffix(".maps.json"))
        .unwrap_or(filename)
        .to_owned()
}

fn profile_sort_key(group: &str) -> &str {
    group.split_once('-').map_or(group, |(_, suffix)| suffix)
}

fn profile_prefix(kind: ProfileKind, reason: &str) -> String {
    format!(
        "{}-{}-{}",
        kind.as_str(),
        Local::now().format("%Y%m%dT%H%M%S%.3f"),
        sanitize_reason(reason)
    )
}

fn empty_cpu_flamegraph() -> &'static str {
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="900" height="120" viewBox="0 0 900 120"><rect width="100%" height="100%" fill="#fafafa"/><text x="24" y="55" font-family="sans-serif" font-size="20">No CPU samples were collected during this profile.</text><text x="24" y="86" font-family="sans-serif" font-size="14">The service was idle; use the high-CPU or manual capture while the load is present.</text></svg>"##
}

fn empty_heap_flamegraph() -> &'static str {
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="900" height="120" viewBox="0 0 900 120"><rect width="100%" height="100%" fill="#fafafa"/><text x="24" y="55" font-family="sans-serif" font-size="20">No sampled heap allocations were present.</text><text x="24" y="86" font-family="sans-serif" font-size="14">The raw jemalloc profile and pprof file are valid; capture again while the service is under load.</text></svg>"##
}

fn sanitize_reason(reason: &str) -> String {
    let value = reason
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .take(48)
        .collect::<String>();
    if value.is_empty() {
        "manual".to_owned()
    } else {
        value
    }
}

async fn read_smaps_rollup() -> anyhow::Result<BTreeMap<String, u64>> {
    read_key_value_kib("/proc/self/smaps_rollup").await
}

async fn read_key_value_kib(path: &str) -> anyhow::Result<BTreeMap<String, u64>> {
    let text = fs::read_to_string(path).await?;
    Ok(parse_key_value_kib(&text))
}

fn parse_key_value_kib(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let number = value.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((key.to_owned(), number.saturating_mul(1024)))
        })
        .collect()
}

async fn read_process_ticks(path: &str) -> anyhow::Result<u64> {
    let stat = fs::read_to_string(path).await?;
    parse_process_ticks(&stat).context("invalid /proc stat")
}

fn parse_process_ticks(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    let fields = stat
        .get(close + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let user = fields.get(11)?.parse::<u64>().ok()?;
    let system = fields.get(12)?.parse::<u64>().ok()?;
    Some(user.saturating_add(system))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadCpu {
    tid: u32,
    name: String,
    cpu_percent_one_core: f64,
}

async fn read_thread_cpu(
    previous: &mut HashMap<u32, u64>,
    wall_seconds: Option<f64>,
    clock_ticks: f64,
) -> anyhow::Result<Vec<ThreadCpu>> {
    let mut current = HashMap::new();
    let mut values = Vec::new();
    let mut entries = fs::read_dir("/proc/self/task").await?;
    while let Some(entry) = entries.next_entry().await? {
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")).await {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        let Some(ticks) = parse_process_ticks(&stat) else {
            continue;
        };
        current.insert(tid, ticks);
        if let Some((previous_ticks, wall)) = previous.get(&tid).zip(wall_seconds)
            && wall > 0.0
        {
            let open = stat.find('(').unwrap_or(0);
            let close = stat.rfind(')').unwrap_or(open);
            let name = stat.get(open + 1..close).unwrap_or("unknown").to_owned();
            values.push(ThreadCpu {
                tid,
                name,
                cpu_percent_one_core: ticks.saturating_sub(*previous_ticks) as f64
                    / clock_ticks
                    / wall
                    * 100.0,
            });
        }
    }
    *previous = current;
    values.sort_by(|left, right| {
        right
            .cpu_percent_one_core
            .total_cmp(&left.cpu_percent_one_core)
    });
    values.truncate(8);
    Ok(values)
}

fn jemalloc_stats() -> BTreeMap<&'static str, u64> {
    use tikv_jemalloc_ctl::{epoch, stats};
    let _ = epoch::advance();
    BTreeMap::from([
        ("allocated", stats::allocated::read().unwrap_or(0) as u64),
        ("active", stats::active::read().unwrap_or(0) as u64),
        ("resident", stats::resident::read().unwrap_or(0) as u64),
        ("mapped", stats::mapped::read().unwrap_or(0) as u64),
        ("retained", stats::retained::read().unwrap_or(0) as u64),
        ("metadata", stats::metadata::read().unwrap_or(0) as u64),
    ])
}

async fn sqlite_json(db: &SqlitePool, db_path: &str) -> Value {
    json!({
        "poolSize": db.size(),
        "poolIdle": db.num_idle(),
        "databaseBytes": file_size(Path::new(db_path)).await,
        "walBytes": file_size(Path::new(&format!("{db_path}-wal"))).await,
        "shmBytes": file_size(Path::new(&format!("{db_path}-shm"))).await,
    })
}

async fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).await.ok().map(|metadata| metadata.len())
}

#[derive(Default)]
struct CgroupPaths {
    directory: Option<PathBuf>,
}

impl CgroupPaths {
    async fn discover() -> Self {
        let path = fs::read_to_string("/proc/self/cgroup")
            .await
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|line| line.strip_prefix("0::"))
                    .map(|path| Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
            })
            .filter(|path| path.exists());
        Self { directory: path }
    }

    async fn read(&self) -> CgroupStats {
        let Some(directory) = &self.directory else {
            return CgroupStats::default();
        };
        let memory_current = read_u64_file(&directory.join("memory.current")).await;
        let memory_max = read_limit_file(&directory.join("memory.max")).await;
        let memory = read_space_map(&directory.join("memory.stat")).await;
        let cpu = read_space_map(&directory.join("cpu.stat")).await;
        let cpu_max = fs::read_to_string(directory.join("cpu.max"))
            .await
            .ok()
            .and_then(|value| parse_cpu_max(&value));
        CgroupStats {
            available: true,
            memory_current,
            memory_max,
            memory_anon: memory.get("anon").copied(),
            memory_file: memory.get("file").copied(),
            memory_kernel: memory.get("kernel").copied(),
            cpu_usage_usec: cpu.get("usage_usec").copied(),
            cpu_user_usec: cpu.get("user_usec").copied(),
            cpu_system_usec: cpu.get("system_usec").copied(),
            cpu_throttled_usec: cpu.get("throttled_usec").copied(),
            cpu_nr_throttled: cpu.get("nr_throttled").copied(),
            cpu_quota_cores: cpu_max,
        }
    }
}

#[derive(Default)]
struct CgroupStats {
    available: bool,
    memory_current: Option<u64>,
    memory_max: Option<u64>,
    memory_anon: Option<u64>,
    memory_file: Option<u64>,
    memory_kernel: Option<u64>,
    cpu_usage_usec: Option<u64>,
    cpu_user_usec: Option<u64>,
    cpu_system_usec: Option<u64>,
    cpu_throttled_usec: Option<u64>,
    cpu_nr_throttled: Option<u64>,
    cpu_quota_cores: Option<f64>,
}

impl CgroupStats {
    fn to_json(&self, cpu_percent_one_core: Option<f64>) -> Value {
        json!({
            "available": self.available,
            "memoryCurrentBytes": self.memory_current,
            "memoryMaxBytes": self.memory_max,
            "memoryAnonBytes": self.memory_anon,
            "memoryFileBytes": self.memory_file,
            "memoryKernelBytes": self.memory_kernel,
            "cpuPercentOneCore": cpu_percent_one_core,
            "cpuPercentOfQuota": cpu_percent_one_core.zip(self.cpu_quota_cores).map(|(cpu, quota)| cpu / quota),
            "cpuUsageUsec": self.cpu_usage_usec,
            "cpuUserUsec": self.cpu_user_usec,
            "cpuSystemUsec": self.cpu_system_usec,
            "cpuThrottledUsec": self.cpu_throttled_usec,
            "cpuNrThrottled": self.cpu_nr_throttled,
            "cpuQuotaCores": self.cpu_quota_cores,
        })
    }
}

async fn read_space_map(path: &Path) -> BTreeMap<String, u64> {
    let Ok(text) = fs::read_to_string(path).await else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.to_owned(), fields.next()?.parse().ok()?))
        })
        .collect()
}

async fn read_u64_file(path: &Path) -> Option<u64> {
    fs::read_to_string(path).await.ok()?.trim().parse().ok()
}

async fn read_limit_file(path: &Path) -> Option<u64> {
    let value = fs::read_to_string(path).await.ok()?;
    let value = value.trim();
    (value != "max").then(|| value.parse().ok()).flatten()
}

fn parse_cpu_max(value: &str) -> Option<f64> {
    let mut fields = value.split_whitespace();
    let quota = fields.next()?;
    let period = fields.next()?.parse::<f64>().ok()?;
    if quota == "max" || period <= 0.0 {
        None
    } else {
        quota.parse::<f64>().ok().map(|quota| quota / period)
    }
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn system_time_string(value: SystemTime) -> String {
    chrono::DateTime::<Utc>::from(value).to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn env_u64(name: &str, default: u64, minimum: u64, maximum: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
        .clamp(minimum, maximum)
}

fn env_f64(name: &str, default: f64, minimum: f64, maximum: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(default)
        .clamp(minimum, maximum)
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name).ok().map_or(default, |value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat_with_spaces_in_name() {
        let stat = "42 (tokio worker 1) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15";
        assert_eq!(parse_process_ticks(stat), Some(23));
    }

    #[test]
    fn parses_kib_and_cpu_quota() {
        let values = parse_key_value_kib("Rss: 12 kB\nThreads: 4\n");
        assert_eq!(values["Rss"], 12 * 1024);
        assert_eq!(values["Threads"], 4 * 1024);
        assert_eq!(parse_cpu_max("200000 100000"), Some(2.0));
        assert_eq!(parse_cpu_max("max 100000"), None);
    }

    #[test]
    fn profile_names_reject_traversal_and_group_pairs() {
        assert!(valid_profile_filename("cpu-20260824-manual.pb"));
        assert!(valid_profile_filename("heap-20260824-manual.pb.gz"));
        assert!(valid_profile_filename("heap-20260824-manual.jeheap"));
        assert!(valid_profile_filename("heap-20260824-manual.maps.json"));
        assert!(!valid_profile_filename("../heap-secret.pb.gz"));
        assert_eq!(
            profile_group_key("heap-20260824-manual.pb.gz"),
            "heap-20260824-manual"
        );
        assert_eq!(
            profile_group_key("heap-20260824-manual.svg"),
            "heap-20260824-manual"
        );
        assert_eq!(
            profile_group_key("heap-20260824-manual.maps.json"),
            "heap-20260824-manual"
        );
    }
}
