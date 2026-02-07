use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use metra_proto::{
    CreateTransferRequest, QUIC_PROTOCOL_VERSION, QuicTransferCompleteAck, QuicTransferOpen,
    QuicTransferOpenAck, RESUME_CHUNK_SIZE_BYTES,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt as TokioAsyncReadExt, AsyncSeekExt, SeekFrom},
    sync::mpsc,
    task::JoinSet,
    time::sleep,
};
use uuid::Uuid;

use crate::{
    cli::{
        BenchArgs, CompareArgs, CompareSeriesArgs, CreateArgs, MatrixArgs, SendArgs, TuneLanesArgs,
    },
    lane_policy::{
        LanePolicyEntry, WorkloadProfile, read_lane_policy, select_lane_policy,
        upsert_lane_policy_entry,
    },
    quic::{connect_quic, read_json_frame, write_json_frame},
    rest::{create_transfer, fetch_quic_certificate, fetch_transfer_status},
};

#[derive(Debug, Clone, Serialize)]
pub struct SendTransferReport {
    transfer_id: Uuid,
    file_path: String,
    file_size_bytes: u64,
    effective_lanes: u32,
    lane_selection: Option<String>,
    resumed_from_bytes: u64,
    bytes_streamed_this_session: u64,
    total_streamed_bytes: u64,
    elapsed_ms: u128,
    average_gbps: f64,
    final_status: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchmarkMatrixRun {
    size_gib: u64,
    lanes: u32,
    effective_lanes: Option<u32>,
    lane_selection: Option<String>,
    io_chunk_bytes: usize,
    file_path: String,
    success: bool,
    transfer_id: Option<Uuid>,
    elapsed_ms: Option<u128>,
    average_gbps: Option<f64>,
    total_streamed_bytes: Option<u64>,
    final_status: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkMatrixReport {
    server: String,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    total_runs: usize,
    successful_runs: usize,
    failed_runs: usize,
    best_run: Option<BenchmarkMatrixRun>,
    runs: Vec<BenchmarkMatrixRun>,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkCompareReport {
    server: String,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    size_gib: u64,
    lanes: u32,
    io_chunk_bytes: usize,
    iterations: u32,
    disk_backed: ThroughputStats,
    no_disk: ThroughputStats,
    delta_gbps: ThroughputStats,
    delta_percent_over_disk: ThroughputStats,
    disk_fraction_of_no_disk: ThroughputStats,
    runs: Vec<BenchmarkCompareIteration>,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkCompareSeriesReport {
    server: String,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    sizes_gib: Vec<u64>,
    lanes: u32,
    io_chunk_bytes: usize,
    iterations: u32,
    total_sizes: usize,
    best_disk_p50_size_gib: Option<u64>,
    best_no_disk_p50_size_gib: Option<u64>,
    largest_delta_p50_size_gib: Option<u64>,
    rows: Vec<BenchmarkCompareSeriesRow>,
    reports: Vec<BenchmarkCompareReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneLanesReport {
    server: String,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    size_gib: u64,
    concurrency: u32,
    iterations: u32,
    io_chunk_bytes: usize,
    no_disk: bool,
    lane_candidates: Vec<u32>,
    recommended_lanes: Option<u32>,
    candidates: Vec<TuneLanesCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneLanesCandidate {
    lanes: u32,
    aggregate_gbps: ThroughputStats,
    transfer_gbps: ThroughputStats,
    successful_runs: u32,
    failed_runs: u32,
    runs: Vec<TuneLanesRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneLanesRun {
    iteration: u32,
    lanes: u32,
    elapsed_ms: u128,
    aggregate_gbps: f64,
    transfer_gbps: ThroughputStats,
    successful_jobs: u32,
    failed_jobs: u32,
    host_start: HostTelemetrySnapshot,
    host_end: HostTelemetrySnapshot,
    host_delta: HostTelemetryDelta,
    errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkCompareSeriesRow {
    size_gib: u64,
    disk_p50_gbps: f64,
    no_disk_p50_gbps: f64,
    delta_p50_gbps: f64,
    delta_percent_p50: f64,
    disk_fraction_p50: f64,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkCompareIteration {
    iteration: u32,
    disk_backed: SendTransferReport,
    no_disk: SendTransferReport,
    delta_gbps: f64,
    delta_percent_over_disk: f64,
    disk_fraction_of_no_disk: f64,
    host_start: HostTelemetrySnapshot,
    host_after_disk: HostTelemetrySnapshot,
    host_after_no_disk: HostTelemetrySnapshot,
    host_total_delta: HostTelemetryDelta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThroughputStats {
    min: f64,
    p50: f64,
    p95: f64,
    max: f64,
    mean: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostTelemetrySnapshot {
    captured_at: DateTime<Utc>,
    global_cpu_percent: f64,
    used_memory_bytes: u64,
    total_memory_bytes: u64,
    process_cpu_percent: Option<f64>,
    process_memory_bytes: Option<u64>,
    process_virtual_memory_bytes: Option<u64>,
    load_avg_one: f64,
    load_avg_five: f64,
    load_avg_fifteen: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostTelemetryDelta {
    global_cpu_percent_delta: f64,
    used_memory_bytes_delta: i64,
    process_cpu_percent_delta: Option<f64>,
    process_memory_bytes_delta: Option<i64>,
    process_virtual_memory_bytes_delta: Option<i64>,
}

#[derive(Clone)]
struct LaneConfig {
    transfer_id: Uuid,
    file_size_bytes: u64,
    file_name: String,
    resume_chunk_size_bytes: u64,
    source: PayloadSource,
    io_chunk_bytes: usize,
    lane_index: u32,
    total_lanes: u32,
    range_start: u64,
    range_end_exclusive: u64,
}

#[derive(Clone)]
enum PayloadSource {
    File(PathBuf),
    GeneratedZeros { label: String },
}

impl PayloadSource {
    fn label(&self) -> String {
        match self {
            Self::File(path) => path.display().to_string(),
            Self::GeneratedZeros { label } => label.clone(),
        }
    }
}

struct LaneTransferResult {
    bytes_streamed: u64,
    resumed_from_bytes: u64,
    complete_ack: QuicTransferCompleteAck,
}

const FILE_READ_PIPELINE_DEPTH: usize = 4;

pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn create_transfer_request(args: &CreateArgs) -> Result<CreateTransferRequest> {
    let request = CreateTransferRequest {
        tenant_id: args.tenant_id.clone(),
        user_id: args.user_id.clone(),
        source_uri: args.source_uri.clone(),
        destination_uri: args.destination_uri.clone(),
        file_name: args.file_name.clone(),
        file_size_bytes: args.file_size_bytes,
        resume_chunk_size_bytes: args.resume_chunk_size_bytes,
        overwrite: args.overwrite,
        immutable_destination: args.immutable_destination,
    };
    request
        .validate()
        .map_err(|err| anyhow::anyhow!("invalid transfer request: {err}"))?;
    Ok(request)
}

pub async fn run_benchmark(
    http: &Client,
    server: &str,
    args: BenchArgs,
) -> Result<SendTransferReport> {
    run_benchmark_with_progress(http, server, args, 1).await
}

pub async fn run_benchmark_matrix(
    http: &Client,
    server: &str,
    args: MatrixArgs,
) -> Result<BenchmarkMatrixReport> {
    if args.sizes_gib.is_empty() {
        anyhow::bail!("sizes_gib must contain at least one value");
    }
    if args.lanes.is_empty() {
        anyhow::bail!("lanes must contain at least one value");
    }
    if args.io_chunk_bytes.is_empty() {
        anyhow::bail!("io_chunk_bytes must contain at least one value");
    }
    if args.sizes_gib.iter().any(|size| *size == 0) {
        anyhow::bail!("sizes_gib values must be > 0");
    }
    if args.lanes.iter().any(|lanes| *lanes == 0) {
        anyhow::bail!("lanes values must be > 0");
    }
    if args.io_chunk_bytes.iter().any(|chunk| *chunk == 0) {
        anyhow::bail!("io_chunk_bytes values must be > 0");
    }

    fs::create_dir_all(&args.file_dir).await.with_context(|| {
        format!(
            "failed creating benchmark directory {}",
            args.file_dir.display()
        )
    })?;

    let started_at = Utc::now();
    let mut runs = Vec::new();
    let destination_prefix = if args.no_disk {
        "null://benchmark".to_owned()
    } else {
        args.destination_prefix.trim_end_matches('/').to_owned()
    };

    for size_gib in &args.sizes_gib {
        for lanes in &args.lanes {
            for io_chunk_bytes in &args.io_chunk_bytes {
                let file_name = format!("metra-bench-{size_gib}g-l{lanes}-c{io_chunk_bytes}.bin");
                let file_path = args.file_dir.join(&file_name);
                let bench_args = BenchArgs {
                    size_gib: *size_gib,
                    file_path: file_path.clone(),
                    tenant_id: args.tenant_id.clone(),
                    user_id: args.user_id.clone(),
                    destination_uri: format!("{destination_prefix}/{file_name}"),
                    quic_addr: args.quic_addr,
                    io_chunk_bytes: *io_chunk_bytes,
                    lanes: *lanes,
                    no_disk: args.no_disk,
                    auto_lanes_report: args.auto_lanes_report.clone(),
                    lane_policy: args.lane_policy.clone(),
                };

                let result = run_benchmark_with_progress(http, server, bench_args, 0).await;
                match result {
                    Ok(report) => runs.push(BenchmarkMatrixRun {
                        size_gib: *size_gib,
                        lanes: *lanes,
                        effective_lanes: Some(report.effective_lanes),
                        lane_selection: report.lane_selection.clone(),
                        io_chunk_bytes: *io_chunk_bytes,
                        file_path: report.file_path.clone(),
                        success: true,
                        transfer_id: Some(report.transfer_id),
                        elapsed_ms: Some(report.elapsed_ms),
                        average_gbps: Some(report.average_gbps),
                        total_streamed_bytes: Some(report.total_streamed_bytes),
                        final_status: Some(report.final_status),
                        error: None,
                    }),
                    Err(err) => runs.push(BenchmarkMatrixRun {
                        size_gib: *size_gib,
                        lanes: *lanes,
                        effective_lanes: None,
                        lane_selection: None,
                        io_chunk_bytes: *io_chunk_bytes,
                        file_path: file_path.display().to_string(),
                        success: false,
                        transfer_id: None,
                        elapsed_ms: None,
                        average_gbps: None,
                        total_streamed_bytes: None,
                        final_status: None,
                        error: Some(err.to_string()),
                    }),
                }

                if args.cleanup_files {
                    match fs::remove_file(&file_path).await {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                        Err(err) => {
                            eprintln!(
                                "failed to clean benchmark file {}: {}",
                                file_path.display(),
                                err
                            );
                        }
                    }
                }
            }
        }
    }

    let successful_runs = runs.iter().filter(|run| run.success).count();
    let failed_runs = runs.len().saturating_sub(successful_runs);
    let best_run = runs
        .iter()
        .filter(|run| run.success)
        .max_by(|left, right| {
            left.average_gbps
                .unwrap_or(0.0)
                .total_cmp(&right.average_gbps.unwrap_or(0.0))
        })
        .cloned();

    Ok(BenchmarkMatrixReport {
        server: server.to_owned(),
        started_at,
        completed_at: Utc::now(),
        total_runs: runs.len(),
        successful_runs,
        failed_runs,
        best_run,
        runs,
    })
}

pub async fn run_benchmark_compare(
    http: &Client,
    server: &str,
    args: CompareArgs,
) -> Result<BenchmarkCompareReport> {
    if args.size_gib == 0 {
        anyhow::bail!("size_gib must be > 0");
    }
    if args.lanes == 0 {
        anyhow::bail!("lanes must be > 0");
    }
    if args.io_chunk_bytes == 0 {
        anyhow::bail!("io_chunk_bytes must be > 0");
    }
    if args.iterations == 0 {
        anyhow::bail!("iterations must be > 0");
    }

    let json_out = args.json_out.clone();
    let started_at = Utc::now();
    let mut telemetry = HostTelemetryCollector::new();
    let mut runs = Vec::with_capacity(args.iterations as usize);
    for iteration in 1..=args.iterations {
        let host_start = telemetry.sample();

        let disk_args = BenchArgs {
            size_gib: args.size_gib,
            file_path: args.file_path.clone(),
            tenant_id: args.tenant_id.clone(),
            user_id: args.user_id.clone(),
            destination_uri: args.destination_uri.clone(),
            quic_addr: args.quic_addr,
            io_chunk_bytes: args.io_chunk_bytes,
            lanes: args.lanes,
            no_disk: false,
            auto_lanes_report: None,
            lane_policy: None,
        };
        let disk_backed = run_benchmark_with_progress(http, server, disk_args, 0).await?;
        let host_after_disk = telemetry.sample();

        let no_disk_args = BenchArgs {
            size_gib: args.size_gib,
            file_path: args.file_path.clone(),
            tenant_id: args.tenant_id.clone(),
            user_id: args.user_id.clone(),
            destination_uri: args.destination_uri.clone(),
            quic_addr: args.quic_addr,
            io_chunk_bytes: args.io_chunk_bytes,
            lanes: args.lanes,
            no_disk: true,
            auto_lanes_report: None,
            lane_policy: None,
        };
        let no_disk = run_benchmark_with_progress(http, server, no_disk_args, 0).await?;
        let host_after_no_disk = telemetry.sample();

        let delta_gbps = no_disk.average_gbps - disk_backed.average_gbps;
        let delta_percent_over_disk = if disk_backed.average_gbps > 0.0 {
            (delta_gbps / disk_backed.average_gbps) * 100.0
        } else {
            0.0
        };
        let disk_fraction_of_no_disk = if no_disk.average_gbps > 0.0 {
            disk_backed.average_gbps / no_disk.average_gbps
        } else {
            0.0
        };

        runs.push(BenchmarkCompareIteration {
            iteration,
            disk_backed,
            no_disk,
            delta_gbps,
            delta_percent_over_disk,
            disk_fraction_of_no_disk,
            host_start: host_start.clone(),
            host_after_disk: host_after_disk.clone(),
            host_after_no_disk: host_after_no_disk.clone(),
            host_total_delta: host_delta(&host_start, &host_after_no_disk),
        });
    }

    if args.cleanup_file {
        match fs::remove_file(&args.file_path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!(
                    "failed to clean compare benchmark file {}: {}",
                    args.file_path.display(),
                    err
                );
            }
        }
    }

    let disk_values = runs
        .iter()
        .map(|run| run.disk_backed.average_gbps)
        .collect::<Vec<_>>();
    let no_disk_values = runs
        .iter()
        .map(|run| run.no_disk.average_gbps)
        .collect::<Vec<_>>();
    let delta_values = runs.iter().map(|run| run.delta_gbps).collect::<Vec<_>>();
    let delta_percent_values = runs
        .iter()
        .map(|run| run.delta_percent_over_disk)
        .collect::<Vec<_>>();
    let fraction_values = runs
        .iter()
        .map(|run| run.disk_fraction_of_no_disk)
        .collect::<Vec<_>>();

    let report = BenchmarkCompareReport {
        server: server.to_owned(),
        started_at,
        completed_at: Utc::now(),
        size_gib: args.size_gib,
        lanes: args.lanes,
        io_chunk_bytes: args.io_chunk_bytes,
        iterations: args.iterations,
        disk_backed: summarize_values(&disk_values),
        no_disk: summarize_values(&no_disk_values),
        delta_gbps: summarize_values(&delta_values),
        delta_percent_over_disk: summarize_values(&delta_percent_values),
        disk_fraction_of_no_disk: summarize_values(&fraction_values),
        runs,
    };
    if let Some(path) = json_out {
        write_json_file(&path, &report).await?;
    }
    Ok(report)
}

pub async fn run_benchmark_compare_series(
    http: &Client,
    server: &str,
    args: CompareSeriesArgs,
) -> Result<BenchmarkCompareSeriesReport> {
    if args.sizes_gib.is_empty() {
        anyhow::bail!("sizes_gib must contain at least one value");
    }
    if args.sizes_gib.iter().any(|size| *size == 0) {
        anyhow::bail!("sizes_gib values must be > 0");
    }
    if args.lanes == 0 {
        anyhow::bail!("lanes must be > 0");
    }
    if args.io_chunk_bytes == 0 {
        anyhow::bail!("io_chunk_bytes must be > 0");
    }
    if args.iterations == 0 {
        anyhow::bail!("iterations must be > 0");
    }

    let json_out = args.json_out.clone();
    fs::create_dir_all(&args.file_dir).await.with_context(|| {
        format!(
            "failed creating benchmark compare-series directory {}",
            args.file_dir.display()
        )
    })?;

    let started_at = Utc::now();
    let mut rows = Vec::with_capacity(args.sizes_gib.len());
    let mut reports = Vec::with_capacity(args.sizes_gib.len());
    let destination_prefix = args.destination_prefix.trim_end_matches('/').to_owned();

    for size_gib in &args.sizes_gib {
        let file_name = format!("{}-{}g.bin", args.file_prefix, size_gib);
        let file_path = args.file_dir.join(&file_name);
        let compare_args = CompareArgs {
            size_gib: *size_gib,
            file_path,
            tenant_id: args.tenant_id.clone(),
            user_id: args.user_id.clone(),
            destination_uri: format!("{destination_prefix}/{file_name}"),
            quic_addr: args.quic_addr,
            io_chunk_bytes: args.io_chunk_bytes,
            lanes: args.lanes,
            iterations: args.iterations,
            cleanup_file: args.cleanup_files,
            json_out: None,
        };
        let report = run_benchmark_compare(http, server, compare_args).await?;
        rows.push(BenchmarkCompareSeriesRow {
            size_gib: *size_gib,
            disk_p50_gbps: report.disk_backed.p50,
            no_disk_p50_gbps: report.no_disk.p50,
            delta_p50_gbps: report.delta_gbps.p50,
            delta_percent_p50: report.delta_percent_over_disk.p50,
            disk_fraction_p50: report.disk_fraction_of_no_disk.p50,
        });
        reports.push(report);
    }

    let series = BenchmarkCompareSeriesReport {
        server: server.to_owned(),
        started_at,
        completed_at: Utc::now(),
        sizes_gib: args.sizes_gib.clone(),
        lanes: args.lanes,
        io_chunk_bytes: args.io_chunk_bytes,
        iterations: args.iterations,
        total_sizes: rows.len(),
        best_disk_p50_size_gib: rows
            .iter()
            .max_by(|left, right| left.disk_p50_gbps.total_cmp(&right.disk_p50_gbps))
            .map(|row| row.size_gib),
        best_no_disk_p50_size_gib: rows
            .iter()
            .max_by(|left, right| left.no_disk_p50_gbps.total_cmp(&right.no_disk_p50_gbps))
            .map(|row| row.size_gib),
        largest_delta_p50_size_gib: rows
            .iter()
            .max_by(|left, right| left.delta_p50_gbps.total_cmp(&right.delta_p50_gbps))
            .map(|row| row.size_gib),
        rows,
        reports,
    };

    if let Some(path) = json_out {
        write_json_file(&path, &series).await?;
    }
    Ok(series)
}

pub async fn run_tune_lanes_under_load(
    http: &Client,
    server: &str,
    args: TuneLanesArgs,
) -> Result<TuneLanesReport> {
    if args.size_gib == 0 {
        anyhow::bail!("size_gib must be > 0");
    }
    if args.concurrency == 0 {
        anyhow::bail!("concurrency must be > 0");
    }
    if args.iterations == 0 {
        anyhow::bail!("iterations must be > 0");
    }
    if args.io_chunk_bytes == 0 {
        anyhow::bail!("io_chunk_bytes must be > 0");
    }
    if args.lanes.is_empty() {
        anyhow::bail!("lanes must contain at least one value");
    }

    let mut lane_candidates = args
        .lanes
        .iter()
        .copied()
        .filter(|lanes| *lanes > 0)
        .collect::<Vec<_>>();
    if lane_candidates.is_empty() {
        anyhow::bail!("lanes must contain values > 0");
    }
    lane_candidates.sort_unstable();
    lane_candidates.dedup();

    let destination_prefix = if args.no_disk {
        "null://benchmark/tune-lanes".to_owned()
    } else {
        args.destination_prefix.trim_end_matches('/').to_owned()
    };
    let file_size_bytes = args.size_gib * 1024 * 1024 * 1024;
    if !args.no_disk {
        prepare_sparse_file(&args.file_path, file_size_bytes).await?;
    }

    let started_at = Utc::now();
    let mut telemetry = HostTelemetryCollector::new();
    let mut candidates = Vec::with_capacity(lane_candidates.len());

    for lanes in &lane_candidates {
        let lanes = *lanes;
        let mut runs = Vec::with_capacity(args.iterations as usize);
        let mut aggregate_values = Vec::with_capacity(args.iterations as usize);
        let mut transfer_values = Vec::new();
        let mut successful_runs = 0u32;
        let mut failed_runs = 0u32;

        for iteration in 1..=args.iterations {
            let host_start = telemetry.sample();
            let iteration_started = Instant::now();
            let mut join_set = JoinSet::new();

            for job in 0..args.concurrency {
                let http = http.clone();
                let server = server.to_owned();
                let file_name = format!(
                    "{}-{}g-l{}-it{}-job{}.bin",
                    args.file_prefix, args.size_gib, lanes, iteration, job
                );
                let source_file_path = args.file_path.clone();
                let destination_uri = format!("{destination_prefix}/{file_name}");
                let tenant_id = args.tenant_id.clone();
                let user_id = args.user_id.clone();
                let quic_addr = args.quic_addr;
                let io_chunk_bytes = args.io_chunk_bytes;
                let no_disk = args.no_disk;

                join_set.spawn(async move {
                    let source_uri = if no_disk {
                        format!("generated://zeros/{file_name}")
                    } else {
                        format!("file://{}", source_file_path.display())
                    };
                    let create = CreateTransferRequest {
                        tenant_id,
                        user_id,
                        source_uri,
                        destination_uri,
                        file_name,
                        file_size_bytes,
                        resume_chunk_size_bytes: RESUME_CHUNK_SIZE_BYTES,
                        overwrite: true,
                        immutable_destination: false,
                    };
                    create.validate().map_err(|err| {
                        anyhow::anyhow!("invalid load tuning transfer request: {err}")
                    })?;
                    let created = create_transfer(&http, &server, &create).await?;

                    let source = if no_disk {
                        PayloadSource::GeneratedZeros {
                            label: format!("generated://zeros/{file_size_bytes}"),
                        }
                    } else {
                        PayloadSource::File(source_file_path)
                    };
                    send_transfer_with_source(
                        &http,
                        &server,
                        created.transfer_id,
                        source,
                        quic_addr,
                        io_chunk_bytes,
                        0,
                        lanes,
                        None,
                    )
                    .await
                });
            }

            let mut successful_reports = Vec::new();
            let mut errors = Vec::new();
            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok(Ok(report)) => successful_reports.push(report),
                    Ok(Err(err)) => errors.push(err.to_string()),
                    Err(err) => errors.push(format!("join error: {err}")),
                }
            }

            let elapsed_ms = iteration_started.elapsed().as_millis();
            let streamed_bytes = successful_reports
                .iter()
                .map(|report| report.bytes_streamed_this_session)
                .sum::<u64>();
            let aggregate_gbps = if elapsed_ms == 0 {
                0.0
            } else {
                (streamed_bytes as f64 * 8.0) / ((elapsed_ms as f64 / 1000.0) * 1_000_000_000.0)
            };
            let per_transfer_values = successful_reports
                .iter()
                .map(|report| report.average_gbps)
                .collect::<Vec<_>>();
            transfer_values.extend(per_transfer_values.iter().copied());
            let transfer_gbps = summarize_values(&per_transfer_values);

            let successful_jobs = successful_reports.len() as u32;
            let failed_jobs = args.concurrency.saturating_sub(successful_jobs);
            if failed_jobs == 0 {
                successful_runs += 1;
                aggregate_values.push(aggregate_gbps);
            } else {
                failed_runs += 1;
            }

            let host_end = telemetry.sample();
            runs.push(TuneLanesRun {
                iteration,
                lanes,
                elapsed_ms,
                aggregate_gbps,
                transfer_gbps,
                successful_jobs,
                failed_jobs,
                host_start: host_start.clone(),
                host_end: host_end.clone(),
                host_delta: host_delta(&host_start, &host_end),
                errors,
            });
        }

        candidates.push(TuneLanesCandidate {
            lanes,
            aggregate_gbps: summarize_values(&aggregate_values),
            transfer_gbps: summarize_values(&transfer_values),
            successful_runs,
            failed_runs,
            runs,
        });
    }

    if args.cleanup_file && !args.no_disk {
        match fs::remove_file(&args.file_path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!(
                    "failed to clean tune-lanes source file {}: {}",
                    args.file_path.display(),
                    err
                );
            }
        }
    }

    let recommended_lanes = recommended_lanes_from_candidates(&candidates);
    let report = TuneLanesReport {
        server: server.to_owned(),
        started_at,
        completed_at: Utc::now(),
        size_gib: args.size_gib,
        concurrency: args.concurrency,
        iterations: args.iterations,
        io_chunk_bytes: args.io_chunk_bytes,
        no_disk: args.no_disk,
        lane_candidates: lane_candidates.clone(),
        recommended_lanes,
        candidates,
    };

    if let Some(path) = args.json_out.as_ref() {
        write_json_file(path, &report).await?;
    }
    if let Some(path) = args.lane_policy_out.as_ref() {
        persist_tune_lanes_policy(path, args.json_out.as_deref(), &report).await?;
    }
    Ok(report)
}

fn recommended_lanes_from_candidates(candidates: &[TuneLanesCandidate]) -> Option<u32> {
    candidates
        .iter()
        .filter(|candidate| candidate.successful_runs > 0)
        .max_by(|left, right| left.aggregate_gbps.p50.total_cmp(&right.aggregate_gbps.p50))
        .map(|candidate| candidate.lanes)
}

fn recommended_lanes_from_report(report: &TuneLanesReport) -> Option<u32> {
    report
        .recommended_lanes
        .or_else(|| recommended_lanes_from_candidates(&report.candidates))
}

async fn persist_tune_lanes_policy(
    policy_path: &Path,
    json_out: Option<&Path>,
    report: &TuneLanesReport,
) -> Result<()> {
    let recommended_lanes = recommended_lanes_from_report(report)
        .filter(|lanes| *lanes > 0)
        .context("cannot persist lane policy: tune-lanes report has no recommended lanes")?;
    let selected = report
        .candidates
        .iter()
        .find(|candidate| candidate.lanes == recommended_lanes);

    let entry = LanePolicyEntry {
        profile: WorkloadProfile {
            size_gib: report.size_gib,
            concurrency: report.concurrency,
            io_chunk_bytes: report.io_chunk_bytes,
            no_disk: report.no_disk,
        },
        recommended_lanes,
        source: json_out
            .map(|path| format!("tune-lanes report {}", path.display()))
            .unwrap_or_else(|| "transfer tune-lanes".to_owned()),
        tuned_at: report.completed_at,
        aggregate_p50_gbps: selected
            .map(|candidate| candidate.aggregate_gbps.p50)
            .unwrap_or(0.0),
        aggregate_p95_gbps: selected
            .map(|candidate| candidate.aggregate_gbps.p95)
            .unwrap_or(0.0),
    };
    upsert_lane_policy_entry(policy_path, entry).await
}

async fn resolve_benchmark_lanes(
    configured_lanes: u32,
    auto_lanes_report: Option<&Path>,
    lane_policy_path: Option<&Path>,
    requested_profile: &WorkloadProfile,
) -> Result<(u32, Option<String>)> {
    if configured_lanes == 0 {
        anyhow::bail!("lanes must be > 0");
    }

    if let Some(report_path) = auto_lanes_report {
        let payload = fs::read(report_path).await.with_context(|| {
            format!("failed reading auto lanes report {}", report_path.display())
        })?;
        let report = serde_json::from_slice::<TuneLanesReport>(&payload).with_context(|| {
            format!(
                "failed parsing auto lanes report JSON {}",
                report_path.display()
            )
        })?;

        if let Some(lanes) = recommended_lanes_from_report(&report).filter(|lanes| *lanes > 0) {
            let exact = report.size_gib == requested_profile.size_gib
                && report.concurrency == requested_profile.concurrency
                && report.io_chunk_bytes == requested_profile.io_chunk_bytes
                && report.no_disk == requested_profile.no_disk;
            let selection = if exact {
                format!(
                    "auto-selected lanes={} from {}",
                    lanes,
                    report_path.display()
                )
            } else {
                format!(
                    "auto-selected lanes={} from {} (report profile size_gib={} concurrency={} io_chunk_bytes={} no_disk={})",
                    lanes,
                    report_path.display(),
                    report.size_gib,
                    report.concurrency,
                    report.io_chunk_bytes,
                    report.no_disk
                )
            };
            return Ok((lanes, Some(selection)));
        }
    }

    if let Some(policy_path) = lane_policy_path {
        let policy = read_lane_policy(policy_path).await?;
        if let Some(selection) = select_lane_policy(&policy, requested_profile) {
            let lanes = selection.entry.recommended_lanes;
            if lanes > 0 {
                let selection_note = if selection.exact_profile_match {
                    format!(
                        "auto-selected lanes={} from lane policy {}",
                        lanes,
                        policy_path.display()
                    )
                } else {
                    format!(
                        "auto-selected lanes={} from lane policy {} fallback profile size_gib={} concurrency={} io_chunk_bytes={} no_disk={}",
                        lanes,
                        policy_path.display(),
                        selection.entry.profile.size_gib,
                        selection.entry.profile.concurrency,
                        selection.entry.profile.io_chunk_bytes,
                        selection.entry.profile.no_disk
                    )
                };
                return Ok((lanes, Some(selection_note)));
            }
        }
    }

    Ok((configured_lanes, None))
}

async fn run_benchmark_with_progress(
    http: &Client,
    server: &str,
    args: BenchArgs,
    progress_interval_secs: u64,
) -> Result<SendTransferReport> {
    let workload_profile = WorkloadProfile {
        size_gib: args.size_gib,
        concurrency: 1,
        io_chunk_bytes: args.io_chunk_bytes,
        no_disk: args.no_disk,
    };
    let (selected_lanes, lane_selection) = resolve_benchmark_lanes(
        args.lanes,
        args.auto_lanes_report.as_deref(),
        args.lane_policy.as_deref(),
        &workload_profile,
    )
    .await?;

    let file_size_bytes = args.size_gib * 1024 * 1024 * 1024;
    let file_name = args
        .file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("metra-bench.bin")
        .to_owned();
    let source_uri = if args.no_disk {
        format!("generated://zeros/{file_name}")
    } else {
        prepare_sparse_file(&args.file_path, file_size_bytes).await?;
        format!("file://{}", args.file_path.display())
    };
    let destination_uri = if args.no_disk {
        format!("null://benchmark/{file_name}")
    } else {
        args.destination_uri.clone()
    };

    let create = CreateTransferRequest {
        tenant_id: args.tenant_id.clone(),
        user_id: args.user_id.clone(),
        source_uri,
        destination_uri,
        file_name,
        file_size_bytes,
        resume_chunk_size_bytes: RESUME_CHUNK_SIZE_BYTES,
        overwrite: true,
        immutable_destination: false,
    };
    create
        .validate()
        .map_err(|err| anyhow::anyhow!("invalid benchmark transfer request: {err}"))?;
    let created = create_transfer(http, server, &create).await?;

    let source = if args.no_disk {
        PayloadSource::GeneratedZeros {
            label: format!("generated://zeros/{file_size_bytes}"),
        }
    } else {
        PayloadSource::File(args.file_path.clone())
    };
    send_transfer_with_source(
        http,
        server,
        created.transfer_id,
        source,
        args.quic_addr,
        args.io_chunk_bytes,
        progress_interval_secs,
        selected_lanes,
        lane_selection,
    )
    .await
}

pub async fn send_transfer(
    http: &Client,
    server: &str,
    args: SendArgs,
) -> Result<SendTransferReport> {
    send_transfer_with_source(
        http,
        server,
        args.transfer_id,
        PayloadSource::File(args.file_path),
        args.quic_addr,
        args.io_chunk_bytes,
        args.progress_interval_secs,
        args.lanes,
        None,
    )
    .await
}

async fn send_transfer_with_source(
    http: &Client,
    server: &str,
    transfer_id: Uuid,
    source: PayloadSource,
    quic_addr_override: Option<std::net::SocketAddr>,
    io_chunk_bytes: usize,
    progress_interval_secs: u64,
    lanes: u32,
    lane_selection: Option<String>,
) -> Result<SendTransferReport> {
    if io_chunk_bytes == 0 {
        anyhow::bail!("io_chunk_bytes must be > 0");
    }
    if lanes == 0 {
        anyhow::bail!("lanes must be > 0");
    }

    let transfer = fetch_transfer_status(http, server, transfer_id).await?;
    if let PayloadSource::File(file_path) = &source {
        let file_metadata = fs::metadata(file_path)
            .await
            .with_context(|| format!("failed reading file metadata {}", file_path.display()))?;
        if file_metadata.len() != transfer.file_size_bytes {
            anyhow::bail!(
                "local file size {} does not match transfer size {}",
                file_metadata.len(),
                transfer.file_size_bytes
            );
        }
    }

    let cert_response = fetch_quic_certificate(http, server).await?;
    if cert_response.protocol_version != QUIC_PROTOCOL_VERSION {
        anyhow::bail!(
            "server protocol version mismatch: got {}, expected {}",
            cert_response.protocol_version,
            QUIC_PROTOCOL_VERSION
        );
    }
    let quic_addr =
        quic_addr_override.unwrap_or(cert_response.quic_addr.parse().with_context(|| {
            format!("invalid quic_addr from server: {}", cert_response.quic_addr)
        })?);

    let (_endpoint, connection) = connect_quic(&cert_response, quic_addr).await?;
    let lanes = normalized_lane_count(lanes, transfer.file_size_bytes);
    let ranges = split_ranges(transfer.file_size_bytes, lanes);

    let started_at = Instant::now();
    let progress_bytes = Arc::new(AtomicU64::new(0));
    let stop_progress = Arc::new(AtomicBool::new(false));
    let progress_task = if progress_interval_secs > 0 {
        Some(tokio::spawn(report_progress(
            transfer.transfer_id,
            progress_bytes.clone(),
            stop_progress.clone(),
            started_at,
            progress_interval_secs,
        )))
    } else {
        None
    };

    let mut join_set = JoinSet::new();
    for (lane_index, (range_start, range_end_exclusive)) in ranges.into_iter().enumerate() {
        let lane = LaneConfig {
            transfer_id: transfer.transfer_id,
            file_size_bytes: transfer.file_size_bytes,
            file_name: transfer.file_name.clone(),
            resume_chunk_size_bytes: transfer.resume_chunk_size_bytes,
            source: source.clone(),
            io_chunk_bytes,
            lane_index: lane_index as u32,
            total_lanes: lanes,
            range_start,
            range_end_exclusive,
        };
        let progress = progress_bytes.clone();
        let connection = connection.clone();
        join_set.spawn(async move { send_lane(connection, lane, progress).await });
    }

    let lane_results = async {
        let mut bytes_streamed_this_session = 0u64;
        let mut resumed_from_bytes = 0u64;
        let mut best_ack: Option<QuicTransferCompleteAck> = None;

        while let Some(joined) = join_set.join_next().await {
            let lane_result = joined.context("lane task panicked")??;
            bytes_streamed_this_session += lane_result.bytes_streamed;
            resumed_from_bytes += lane_result.resumed_from_bytes;
            if best_ack
                .as_ref()
                .is_none_or(|ack| lane_result.complete_ack.bytes_received > ack.bytes_received)
            {
                best_ack = Some(lane_result.complete_ack);
            }
        }

        let complete_ack = best_ack.context("no completion ack received from server")?;
        Ok::<_, anyhow::Error>((
            bytes_streamed_this_session,
            resumed_from_bytes,
            complete_ack,
        ))
    }
    .await;

    stop_progress.store(true, Ordering::Relaxed);
    if let Some(task) = progress_task {
        let _ = task.await;
    }

    let (bytes_streamed_this_session, resumed_from_bytes, complete_ack) = lane_results?;
    let elapsed_ms = started_at.elapsed().as_millis();
    let avg_gbps = if elapsed_ms == 0 {
        0.0
    } else {
        (bytes_streamed_this_session as f64 * 8.0)
            / ((elapsed_ms as f64 / 1000.0) * 1_000_000_000.0)
    };

    Ok(SendTransferReport {
        transfer_id: transfer.transfer_id,
        file_path: source.label(),
        file_size_bytes: transfer.file_size_bytes,
        effective_lanes: lanes,
        lane_selection,
        resumed_from_bytes,
        bytes_streamed_this_session,
        total_streamed_bytes: complete_ack.bytes_received,
        elapsed_ms,
        average_gbps: avg_gbps,
        final_status: format!("{:?}", complete_ack.status),
        message: complete_ack.message,
    })
}

async fn send_lane(
    connection: quinn::Connection,
    lane: LaneConfig,
    progress_bytes: Arc<AtomicU64>,
) -> Result<LaneTransferResult> {
    let (mut send_stream, mut recv_stream) = connection
        .open_bi()
        .await
        .context("failed opening bidirectional QUIC stream")?;

    let open = QuicTransferOpen {
        transfer_id: lane.transfer_id,
        file_size_bytes: lane.file_size_bytes,
        file_name: lane.file_name,
        resume_chunk_size_bytes: lane.resume_chunk_size_bytes,
        lane_index: lane.lane_index,
        total_lanes: lane.total_lanes,
        range_start: lane.range_start,
        range_end_exclusive: lane.range_end_exclusive,
    };
    write_json_frame(&mut send_stream, &open).await?;
    let open_ack = read_json_frame::<QuicTransferOpenAck>(&mut recv_stream).await?;
    if !open_ack.ok {
        anyhow::bail!(
            "server rejected transfer open for lane {}: {}",
            lane.lane_index,
            open_ack.message
        );
    }
    if open_ack.resume_offset_bytes < lane.range_start
        || open_ack.resume_offset_bytes > lane.range_end_exclusive
    {
        anyhow::bail!(
            "invalid resume offset {} for lane {} range {}..{}",
            open_ack.resume_offset_bytes,
            lane.lane_index,
            lane.range_start,
            lane.range_end_exclusive
        );
    }

    let bytes_streamed = match &lane.source {
        PayloadSource::File(file_path) => {
            send_file_lane_pipelined(
                &mut send_stream,
                file_path,
                open_ack.resume_offset_bytes,
                lane.range_end_exclusive,
                lane.io_chunk_bytes,
                lane.lane_index,
                progress_bytes.clone(),
            )
            .await?
        }
        PayloadSource::GeneratedZeros { .. } => {
            send_generated_lane(
                &mut send_stream,
                open_ack.resume_offset_bytes,
                lane.range_end_exclusive,
                lane.io_chunk_bytes,
                progress_bytes.clone(),
            )
            .await?
        }
    };

    send_stream.finish()?;
    let complete_ack = read_json_frame::<QuicTransferCompleteAck>(&mut recv_stream).await?;
    Ok(LaneTransferResult {
        bytes_streamed,
        resumed_from_bytes: open_ack.resume_offset_bytes - lane.range_start,
        complete_ack,
    })
}

async fn send_file_lane_pipelined(
    send_stream: &mut quinn::SendStream,
    file_path: &Path,
    resume_offset: u64,
    range_end_exclusive: u64,
    io_chunk_bytes: usize,
    lane_index: u32,
    progress_bytes: Arc<AtomicU64>,
) -> Result<u64> {
    let total_bytes = range_end_exclusive - resume_offset;
    if total_bytes == 0 {
        return Ok(0);
    }

    let (buffer_tx, mut buffer_rx) = mpsc::channel::<Vec<u8>>(FILE_READ_PIPELINE_DEPTH);
    for _ in 0..FILE_READ_PIPELINE_DEPTH {
        buffer_tx
            .send(vec![0u8; io_chunk_bytes])
            .await
            .map_err(|_| anyhow::anyhow!("failed seeding file read pipeline buffers"))?;
    }

    let (chunk_tx, mut chunk_rx) = mpsc::channel::<(Vec<u8>, usize)>(FILE_READ_PIPELINE_DEPTH);
    let file_path = file_path.to_path_buf();
    let producer = tokio::spawn(async move {
        let mut file = fs::File::open(&file_path)
            .await
            .with_context(|| format!("failed opening file {}", file_path.display()))?;
        file.seek(SeekFrom::Start(resume_offset))
            .await
            .context("failed seeking local file for resume")?;

        let mut remaining = total_bytes;
        while remaining > 0 {
            let mut buffer = buffer_rx
                .recv()
                .await
                .context("file read pipeline buffer pool unexpectedly closed")?;
            let read_len = remaining.min(buffer.len() as u64) as usize;
            let bytes_read = file
                .read(&mut buffer[..read_len])
                .await
                .context("failed reading local file")?;
            if bytes_read == 0 {
                anyhow::bail!(
                    "unexpected EOF while sending lane {} (remaining {} bytes)",
                    lane_index,
                    remaining
                );
            }
            chunk_tx.send((buffer, bytes_read)).await.map_err(|_| {
                anyhow::anyhow!("file read pipeline chunk channel unexpectedly closed")
            })?;
            remaining -= bytes_read as u64;
        }
        Ok::<_, anyhow::Error>(())
    });

    let consume_result = async {
        let mut remaining = total_bytes;
        let mut bytes_streamed = 0u64;

        while remaining > 0 {
            let (buffer, bytes_read) = chunk_rx
                .recv()
                .await
                .context("file read pipeline ended before lane completion")?;
            send_stream
                .write_all(&buffer[..bytes_read])
                .await
                .context("failed writing stream payload")?;
            let written = bytes_read as u64;
            remaining = remaining.saturating_sub(written);
            bytes_streamed += written;
            progress_bytes.fetch_add(written, Ordering::Relaxed);
            let _ = buffer_tx.send(buffer).await;
        }

        if bytes_streamed != total_bytes {
            anyhow::bail!(
                "lane {} streamed {} bytes but expected {}",
                lane_index,
                bytes_streamed,
                total_bytes
            );
        }
        Ok::<_, anyhow::Error>(bytes_streamed)
    }
    .await;

    match consume_result {
        Ok(bytes_streamed) => {
            let producer_result = producer.await.context("file read pipeline task panicked")?;
            producer_result?;
            Ok(bytes_streamed)
        }
        Err(err) => {
            producer.abort();
            let _ = producer.await;
            Err(err)
        }
    }
}

async fn send_generated_lane(
    send_stream: &mut quinn::SendStream,
    resume_offset: u64,
    range_end_exclusive: u64,
    io_chunk_bytes: usize,
    progress_bytes: Arc<AtomicU64>,
) -> Result<u64> {
    let buffer = vec![0u8; io_chunk_bytes];
    let mut remaining = range_end_exclusive - resume_offset;
    let mut bytes_streamed = 0u64;

    while remaining > 0 {
        let to_send = remaining.min(buffer.len() as u64) as usize;
        send_stream
            .write_all(&buffer[..to_send])
            .await
            .context("failed writing generated payload")?;
        let written = to_send as u64;
        remaining -= written;
        bytes_streamed += written;
        progress_bytes.fetch_add(written, Ordering::Relaxed);
    }

    Ok(bytes_streamed)
}

fn normalized_lane_count(lanes: u32, file_size_bytes: u64) -> u32 {
    let upper = u32::try_from(file_size_bytes).unwrap_or(u32::MAX).max(1);
    lanes.max(1).min(upper)
}

fn split_ranges(file_size_bytes: u64, lanes: u32) -> Vec<(u64, u64)> {
    let base = file_size_bytes / lanes as u64;
    let remainder = file_size_bytes % lanes as u64;

    let mut ranges = Vec::with_capacity(lanes as usize);
    let mut start = 0u64;
    for lane in 0..lanes {
        let lane_len = base + u64::from((lane as u64) < remainder);
        let end = start + lane_len;
        ranges.push((start, end));
        start = end;
    }
    ranges
}

async fn report_progress(
    transfer_id: Uuid,
    progress_bytes: Arc<AtomicU64>,
    stop_progress: Arc<AtomicBool>,
    started_at: Instant,
    interval_secs: u64,
) {
    let interval = Duration::from_secs(interval_secs.max(1));
    loop {
        sleep(interval).await;
        let streamed = progress_bytes.load(Ordering::Relaxed);
        let elapsed = started_at.elapsed().as_secs_f64();
        let gbps = if elapsed > 0.0 {
            (streamed as f64 * 8.0) / (elapsed * 1_000_000_000.0)
        } else {
            0.0
        };
        eprintln!(
            "transfer_id={} streamed={} bytes avg={:.3} Gbps",
            transfer_id, streamed, gbps
        );

        if stop_progress.load(Ordering::Relaxed) {
            break;
        }
    }
}

struct HostTelemetryCollector {
    system: System,
    pid: Option<Pid>,
}

impl HostTelemetryCollector {
    fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_all();
        let pid = sysinfo::get_current_pid().ok();
        Self { system, pid }
    }

    fn sample(&mut self) -> HostTelemetrySnapshot {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        if let Some(pid) = self.pid {
            self.system
                .refresh_processes(ProcessesToUpdate::Some(&[pid]));
        }

        let process = self.pid.and_then(|pid| self.system.process(pid));
        let load = System::load_average();
        HostTelemetrySnapshot {
            captured_at: Utc::now(),
            global_cpu_percent: self.system.global_cpu_usage() as f64,
            used_memory_bytes: self.system.used_memory(),
            total_memory_bytes: self.system.total_memory(),
            process_cpu_percent: process.map(|proc| proc.cpu_usage() as f64),
            process_memory_bytes: process.map(|proc| proc.memory()),
            process_virtual_memory_bytes: process.map(|proc| proc.virtual_memory()),
            load_avg_one: load.one,
            load_avg_five: load.five,
            load_avg_fifteen: load.fifteen,
        }
    }
}

fn host_delta(start: &HostTelemetrySnapshot, end: &HostTelemetrySnapshot) -> HostTelemetryDelta {
    HostTelemetryDelta {
        global_cpu_percent_delta: end.global_cpu_percent - start.global_cpu_percent,
        used_memory_bytes_delta: signed_delta_u64(end.used_memory_bytes, start.used_memory_bytes),
        process_cpu_percent_delta: end
            .process_cpu_percent
            .zip(start.process_cpu_percent)
            .map(|(end, start)| end - start),
        process_memory_bytes_delta: end
            .process_memory_bytes
            .zip(start.process_memory_bytes)
            .map(|(end, start)| signed_delta_u64(end, start)),
        process_virtual_memory_bytes_delta: end
            .process_virtual_memory_bytes
            .zip(start.process_virtual_memory_bytes)
            .map(|(end, start)| signed_delta_u64(end, start)),
    }
}

fn signed_delta_u64(end: u64, start: u64) -> i64 {
    let delta = end as i128 - start as i128;
    delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn summarize_values(values: &[f64]) -> ThroughputStats {
    let mut sorted = values.to_vec();
    if sorted.is_empty() {
        return ThroughputStats {
            min: 0.0,
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
            mean: 0.0,
        };
    }

    sorted.sort_by(|left, right| left.total_cmp(right));
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;

    ThroughputStats {
        min: *sorted.first().unwrap_or(&0.0),
        p50: percentile(&sorted, 0.50),
        p95: percentile(&sorted, 0.95),
        max: *sorted.last().unwrap_or(&0.0),
        mean,
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }

    let rank = quantile.clamp(0.0, 1.0) * (sorted.len() as f64 - 1.0);
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }

    let weight = rank - lower as f64;
    sorted[lower] + (sorted[upper] - sorted[lower]) * weight
}

async fn prepare_sparse_file(path: &Path, size: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .with_context(|| format!("failed creating benchmark file {}", path.display()))?;
    file.set_len(size)
        .await
        .with_context(|| format!("failed sizing benchmark file {}", path.display()))?;
    Ok(())
}

async fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let payload = serde_json::to_vec_pretty(value).context("failed serializing JSON report")?;
    fs::write(path, payload)
        .await
        .with_context(|| format!("failed writing {}", path.display()))?;
    Ok(())
}
