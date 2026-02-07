use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use metra_proto::{
    CreateTransferRequest, QUIC_PROTOCOL_VERSION, QuicTransferCompleteAck, QuicTransferOpen,
    QuicTransferOpenAck, RESUME_CHUNK_SIZE_BYTES,
};
use reqwest::Client;
use serde::Serialize;
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt as TokioAsyncReadExt, AsyncSeekExt, SeekFrom},
    task::JoinSet,
    time::sleep,
};
use uuid::Uuid;

use crate::{
    cli::{BenchArgs, CreateArgs, SendArgs},
    quic::{connect_quic, read_json_frame, write_json_frame},
    rest::{create_transfer, fetch_quic_certificate, fetch_transfer_status},
};

#[derive(Debug, Serialize)]
pub struct SendTransferReport {
    transfer_id: Uuid,
    file_path: String,
    file_size_bytes: u64,
    resumed_from_bytes: u64,
    bytes_streamed_this_session: u64,
    total_streamed_bytes: u64,
    elapsed_ms: u128,
    average_gbps: f64,
    final_status: String,
    message: String,
}

#[derive(Clone)]
struct LaneConfig {
    transfer_id: Uuid,
    file_size_bytes: u64,
    file_name: String,
    resume_chunk_size_bytes: u64,
    file_path: PathBuf,
    io_chunk_bytes: usize,
    lane_index: u32,
    total_lanes: u32,
    range_start: u64,
    range_end_exclusive: u64,
}

struct LaneTransferResult {
    bytes_streamed: u64,
    resumed_from_bytes: u64,
    complete_ack: QuicTransferCompleteAck,
}

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
    let file_size_bytes = args.size_gib * 1024 * 1024 * 1024;
    prepare_sparse_file(&args.file_path, file_size_bytes).await?;

    let file_name = args
        .file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("metra-bench.bin")
        .to_owned();
    let source_uri = format!("file://{}", args.file_path.display());

    let create = CreateTransferRequest {
        tenant_id: args.tenant_id,
        user_id: args.user_id,
        source_uri,
        destination_uri: args.destination_uri,
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

    let send_args = SendArgs {
        transfer_id: created.transfer_id,
        file_path: args.file_path,
        quic_addr: args.quic_addr,
        io_chunk_bytes: args.io_chunk_bytes,
        progress_interval_secs: 1,
        lanes: args.lanes,
    };
    send_transfer(http, server, send_args).await
}

pub async fn send_transfer(
    http: &Client,
    server: &str,
    args: SendArgs,
) -> Result<SendTransferReport> {
    if args.io_chunk_bytes == 0 {
        anyhow::bail!("io_chunk_bytes must be > 0");
    }
    if args.lanes == 0 {
        anyhow::bail!("lanes must be > 0");
    }

    let transfer = fetch_transfer_status(http, server, args.transfer_id).await?;
    let file_metadata = fs::metadata(&args.file_path)
        .await
        .with_context(|| format!("failed reading file metadata {}", args.file_path.display()))?;
    if file_metadata.len() != transfer.file_size_bytes {
        anyhow::bail!(
            "local file size {} does not match transfer size {}",
            file_metadata.len(),
            transfer.file_size_bytes
        );
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
        args.quic_addr
            .unwrap_or(cert_response.quic_addr.parse().with_context(|| {
                format!("invalid quic_addr from server: {}", cert_response.quic_addr)
            })?);

    let (_endpoint, connection) = connect_quic(&cert_response, quic_addr).await?;
    let lanes = normalized_lane_count(args.lanes, transfer.file_size_bytes);
    let ranges = split_ranges(transfer.file_size_bytes, lanes);

    let started_at = Instant::now();
    let progress_bytes = Arc::new(AtomicU64::new(0));
    let stop_progress = Arc::new(AtomicBool::new(false));
    let progress_task = if args.progress_interval_secs > 0 {
        Some(tokio::spawn(report_progress(
            transfer.transfer_id,
            progress_bytes.clone(),
            stop_progress.clone(),
            started_at,
            args.progress_interval_secs,
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
            file_path: args.file_path.clone(),
            io_chunk_bytes: args.io_chunk_bytes,
            lane_index: lane_index as u32,
            total_lanes: lanes,
            range_start,
            range_end_exclusive,
        };
        let progress = progress_bytes.clone();
        let connection = connection.clone();
        join_set.spawn(async move { send_lane(connection, lane, progress).await });
    }

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

    stop_progress.store(true, Ordering::Relaxed);
    if let Some(task) = progress_task {
        let _ = task.await;
    }

    let complete_ack = best_ack.context("no completion ack received from server")?;
    let elapsed_ms = started_at.elapsed().as_millis();
    let avg_gbps = if elapsed_ms == 0 {
        0.0
    } else {
        (bytes_streamed_this_session as f64 * 8.0)
            / ((elapsed_ms as f64 / 1000.0) * 1_000_000_000.0)
    };

    Ok(SendTransferReport {
        transfer_id: transfer.transfer_id,
        file_path: args.file_path.display().to_string(),
        file_size_bytes: transfer.file_size_bytes,
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

    let mut file = fs::File::open(&lane.file_path)
        .await
        .with_context(|| format!("failed opening file {}", lane.file_path.display()))?;
    file.seek(SeekFrom::Start(open_ack.resume_offset_bytes))
        .await
        .context("failed seeking local file for resume")?;

    let mut buffer = vec![0u8; lane.io_chunk_bytes];
    let mut remaining = lane.range_end_exclusive - open_ack.resume_offset_bytes;
    let mut bytes_streamed = 0u64;

    while remaining > 0 {
        let read_len = remaining.min(buffer.len() as u64) as usize;
        let bytes_read = file
            .read(&mut buffer[..read_len])
            .await
            .context("failed reading local file")?;
        if bytes_read == 0 {
            anyhow::bail!(
                "unexpected EOF while sending lane {} (remaining {} bytes)",
                lane.lane_index,
                remaining
            );
        }
        send_stream
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed writing stream payload")?;
        let written = bytes_read as u64;
        remaining -= written;
        bytes_streamed += written;
        progress_bytes.fetch_add(written, Ordering::Relaxed);
    }

    send_stream.finish()?;
    let complete_ack = read_json_frame::<QuicTransferCompleteAck>(&mut recv_stream).await?;
    Ok(LaneTransferResult {
        bytes_streamed,
        resumed_from_bytes: open_ack.resume_offset_bytes - lane.range_start,
        complete_ack,
    })
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
