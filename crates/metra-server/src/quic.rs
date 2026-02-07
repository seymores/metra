use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use metra_proto::{
    QUIC_PROTOCOL_VERSION, QuicTransferCompleteAck, QuicTransferOpen, QuicTransferOpenAck,
    TransferStatus,
};
use quinn::crypto::rustls::QuicServerConfig;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt as TokioAsyncWriteExt, SeekFrom},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    state::AppState,
    wire::{read_json_frame, write_json_frame},
};

const PROGRESS_UPDATE_INTERVAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHUNK_READ_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaneCheckpoint {
    lane_index: u32,
    range_start: u64,
    range_end_exclusive: u64,
    offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransferCheckpoint {
    transfer_id: Uuid,
    file_size_bytes: u64,
    total_lanes: u32,
    lanes: Vec<LaneCheckpoint>,
}

impl TransferCheckpoint {
    fn total_transferred(&self) -> u64 {
        self.lanes
            .iter()
            .map(|lane| lane.offset.saturating_sub(lane.range_start))
            .sum()
    }

    fn all_lanes_complete(&self) -> bool {
        self.lanes
            .iter()
            .all(|lane| lane.offset >= lane.range_end_exclusive)
    }

    fn lane_mut(&mut self, lane_index: u32) -> Option<&mut LaneCheckpoint> {
        self.lanes
            .iter_mut()
            .find(|lane| lane.lane_index == lane_index)
    }

    fn validate_layout(
        &self,
        file_size_bytes: u64,
        total_lanes: u32,
        range_start: u64,
        range_end_exclusive: u64,
        lane_index: u32,
    ) -> Result<()> {
        if self.file_size_bytes != file_size_bytes {
            bail!(
                "checkpoint file size mismatch: {} != {}",
                self.file_size_bytes,
                file_size_bytes
            );
        }
        if self.total_lanes != total_lanes {
            bail!(
                "checkpoint lane count mismatch: {} != {}",
                self.total_lanes,
                total_lanes
            );
        }
        let lane = self
            .lanes
            .iter()
            .find(|lane| lane.lane_index == lane_index)
            .with_context(|| format!("checkpoint missing lane {lane_index}"))?;
        if lane.range_start != range_start || lane.range_end_exclusive != range_end_exclusive {
            bail!(
                "checkpoint lane range mismatch: lane {} expected {}..{} found {}..{}",
                lane_index,
                range_start,
                range_end_exclusive,
                lane.range_start,
                lane.range_end_exclusive
            );
        }
        Ok(())
    }
}

pub async fn run_quic_listener(
    endpoint: quinn::Endpoint,
    state: AppState,
    shutdown: CancellationToken,
) -> Result<()> {
    info!(quic_addr = %state.quic_addr, "quic listener started");
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("quic listener received shutdown signal");
                break;
            }
            connecting = endpoint.accept() => {
                let Some(connecting) = connecting else {
                    warn!("quic endpoint accept loop ended");
                    break;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    match connecting.await {
                        Ok(connection) => {
                            info!(peer = %connection.remote_address(), "accepted quic connection");
                            if let Err(err) = handle_quic_connection(state, connection).await {
                                warn!(error = %err, "quic connection closed with error");
                            }
                        }
                        Err(err) => warn!(error = %err, "failed quic handshake"),
                    }
                });
            }
        }
    }

    endpoint.close(0u32.into(), b"metra server shutdown");
    endpoint.wait_idle().await;
    Ok(())
}

pub fn build_quic_endpoint(
    bind_addr: SocketAddr,
    server_name: &str,
) -> Result<(quinn::Endpoint, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec![server_name.to_owned()])
        .context("failed generating self-signed certificate")?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.signing_key.serialize_der();
    let key = rustls::pki_types::PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(
        key_der,
    ));

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .context("failed building rustls server config")?;
    tls_config.alpn_protocols = vec![QUIC_PROTOCOL_VERSION.as_bytes().to_vec()];
    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls_config)?));

    let transport_config = Arc::get_mut(&mut server_config.transport)
        .context("failed accessing QUIC transport config")?;
    transport_config.max_concurrent_bidi_streams(4_096u32.into());
    transport_config.max_concurrent_uni_streams(1_024u32.into());
    transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
    transport_config.max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
    transport_config.send_window(2 * 1024 * 1024 * 1024);

    let endpoint = quinn::Endpoint::server(server_config, bind_addr)
        .with_context(|| format!("failed binding QUIC endpoint on {bind_addr}"))?;

    Ok((endpoint, cert_der))
}

async fn handle_quic_connection(state: AppState, connection: quinn::Connection) -> Result<()> {
    loop {
        let (send_stream, recv_stream) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(err) => {
                debug!(peer = %connection.remote_address(), error = %err, "connection closed");
                break;
            }
        };

        let state = state.clone();
        let peer = connection.remote_address();
        tokio::spawn(async move {
            if let Err(err) = handle_quic_stream(state, peer, send_stream, recv_stream).await {
                warn!(peer = %peer, error = %err, "failed handling transfer stream");
            }
        });
    }

    Ok(())
}

async fn handle_quic_stream(
    state: AppState,
    peer: SocketAddr,
    mut send_stream: quinn::SendStream,
    mut recv_stream: quinn::RecvStream,
) -> Result<()> {
    let open = read_json_frame::<QuicTransferOpen>(&mut recv_stream).await?;

    let total_lanes = open.total_lanes.max(1);
    if open.lane_index >= total_lanes {
        bail!("invalid lane index {} of {}", open.lane_index, total_lanes);
    }

    let range_start = open.range_start;
    let range_end_exclusive = if open.range_end_exclusive == 0 {
        open.file_size_bytes
    } else {
        open.range_end_exclusive
    };
    if range_start >= range_end_exclusive || range_end_exclusive > open.file_size_bytes {
        bail!(
            "invalid range {}..{} for file size {}",
            range_start,
            range_end_exclusive,
            open.file_size_bytes
        );
    }
    let striped =
        total_lanes > 1 || range_start != 0 || range_end_exclusive != open.file_size_bytes;

    let (staging_path, final_path, checkpoint_path, resume_offset) = {
        let mut transfers = state.transfers.write().await;
        let transfer = transfers
            .get_mut(&open.transfer_id)
            .with_context(|| format!("unknown transfer_id {}", open.transfer_id))?;

        if open.file_size_bytes != transfer.file_size_bytes {
            bail!(
                "file_size mismatch: open={} transfer={}",
                open.file_size_bytes,
                transfer.file_size_bytes
            );
        }
        if open.resume_chunk_size_bytes != transfer.resume_chunk_size_bytes {
            bail!(
                "resume_chunk_size mismatch: open={} transfer={}",
                open.resume_chunk_size_bytes,
                transfer.resume_chunk_size_bytes
            );
        }

        let tenant_dir = state
            .data_dir
            .join(&transfer.tenant_id)
            .join(&transfer.user_id);
        fs::create_dir_all(&tenant_dir).await.with_context(|| {
            format!("failed creating tenant directory {}", tenant_dir.display())
        })?;

        let staging_path = tenant_dir.join(format!("{}.part", transfer.transfer_id));
        let final_path = tenant_dir.join(&transfer.file_name);
        let checkpoint_path = checkpoint_path_for(&tenant_dir, transfer.transfer_id);

        let resume_offset = if striped {
            let (lane_resume, total_transferred) = load_or_init_lane_resume(
                &state,
                &checkpoint_path,
                transfer.transfer_id,
                transfer.file_size_bytes,
                total_lanes,
                open.lane_index,
                range_start,
                range_end_exclusive,
            )
            .await?;
            transfer.bytes_transferred = total_transferred.min(transfer.file_size_bytes);
            lane_resume
        } else {
            let resume = match fs::metadata(&staging_path).await {
                Ok(meta) => meta.len(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
                Err(err) => return Err(err).context("failed reading staging file metadata"),
            };
            if resume > transfer.file_size_bytes {
                bail!(
                    "staging file larger than expected size: {} > {}",
                    resume,
                    transfer.file_size_bytes
                );
            }
            transfer.bytes_transferred = resume;
            resume
        };

        transfer.status = TransferStatus::Running;
        transfer.updated_at = Utc::now();
        (staging_path, final_path, checkpoint_path, resume_offset)
    };

    if striped {
        let staging_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&staging_path)
            .await
            .with_context(|| format!("failed opening staging file {}", staging_path.display()))?;
        staging_file
            .set_len(open.file_size_bytes)
            .await
            .context("failed pre-sizing striped staging file")?;
    }

    let open_ack = QuicTransferOpenAck {
        ok: true,
        resume_offset_bytes: resume_offset,
        message: "transfer stream accepted".to_owned(),
    };
    write_json_frame(&mut send_stream, &open_ack).await?;
    info!(
        peer = %peer,
        transfer_id = %open.transfer_id,
        lane = open.lane_index,
        total_lanes = total_lanes,
        range_start = range_start,
        range_end = range_end_exclusive,
        resume_offset = resume_offset,
        "accepted upload stream"
    );

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(&staging_path)
        .await
        .with_context(|| format!("failed opening staging file {}", staging_path.display()))?;
    file.seek(SeekFrom::Start(resume_offset))
        .await
        .context("failed seeking staging file")?;

    let expected_bytes = range_end_exclusive - resume_offset;
    let mut stream_bytes_written: u64 = 0;
    let mut since_update: u64 = 0;

    loop {
        if stream_bytes_written >= expected_bytes {
            break;
        }

        let next = match recv_stream.read_chunk(MAX_CHUNK_READ_BYTES, true).await {
            Ok(next) => next,
            Err(err) => {
                if since_update > 0 {
                    persist_progress(
                        &state,
                        open.transfer_id,
                        striped,
                        &checkpoint_path,
                        open.lane_index,
                        resume_offset + stream_bytes_written,
                    )
                    .await?;
                }
                return Err(err).context("failed reading stream chunk");
            }
        };

        let Some(chunk) = next else {
            break;
        };

        let remaining = expected_bytes - stream_bytes_written;
        let to_write = remaining.min(chunk.bytes.len() as u64) as usize;
        file.write_all(&chunk.bytes[..to_write])
            .await
            .context("failed writing transfer chunk to disk")?;
        if to_write < chunk.bytes.len() {
            bail!(
                "received lane payload overflow for transfer {} lane {}",
                open.transfer_id,
                open.lane_index
            );
        }

        let chunk_size = to_write as u64;
        stream_bytes_written += chunk_size;
        since_update += chunk_size;

        if since_update >= PROGRESS_UPDATE_INTERVAL_BYTES {
            persist_progress(
                &state,
                open.transfer_id,
                striped,
                &checkpoint_path,
                open.lane_index,
                resume_offset + stream_bytes_written,
            )
            .await?;
            since_update = 0;
        }
    }

    if since_update > 0 {
        persist_progress(
            &state,
            open.transfer_id,
            striped,
            &checkpoint_path,
            open.lane_index,
            resume_offset + stream_bytes_written,
        )
        .await?;
    }

    if stream_bytes_written != expected_bytes {
        bail!(
            "incomplete lane stream for transfer {} lane {}: got {} expected {}",
            open.transfer_id,
            open.lane_index,
            stream_bytes_written,
            expected_bytes
        );
    }

    file.flush()
        .await
        .context("failed flushing transfer file")?;

    let complete = if striped {
        finalize_if_ready(
            &state,
            open.transfer_id,
            &staging_path,
            &final_path,
            &checkpoint_path,
        )
        .await?
    } else {
        let bytes_received = resume_offset + stream_bytes_written;
        finalize_single_lane_transfer(
            &state,
            open.transfer_id,
            bytes_received,
            &staging_path,
            &final_path,
        )
        .await?
    };

    let complete_ack = QuicTransferCompleteAck {
        ok: complete.status == TransferStatus::Completed,
        status: complete.status,
        bytes_received: complete.bytes_transferred,
        message: complete.message,
        updated_at: Utc::now(),
    };
    write_json_frame(&mut send_stream, &complete_ack).await?;
    send_stream.finish()?;
    info!(
        peer = %peer,
        transfer_id = %open.transfer_id,
        lane = open.lane_index,
        bytes_received = complete_ack.bytes_received,
        status = ?complete_ack.status,
        "transfer stream completed"
    );

    Ok(())
}

async fn persist_progress(
    state: &AppState,
    transfer_id: Uuid,
    striped: bool,
    checkpoint_path: &Path,
    lane_index: u32,
    new_offset: u64,
) -> Result<()> {
    let bytes = if striped {
        update_checkpoint_offset(state, checkpoint_path, lane_index, new_offset).await?
    } else {
        new_offset
    };
    let mut transfers = state.transfers.write().await;
    if let Some(transfer) = transfers.get_mut(&transfer_id) {
        transfer.bytes_transferred = bytes.min(transfer.file_size_bytes);
        transfer.updated_at = Utc::now();
    }
    Ok(())
}

struct FinalizeResult {
    status: TransferStatus,
    bytes_transferred: u64,
    message: String,
}

async fn finalize_single_lane_transfer(
    state: &AppState,
    transfer_id: Uuid,
    bytes_received: u64,
    staging_path: &Path,
    final_path: &Path,
) -> Result<FinalizeResult> {
    let mut transfers = state.transfers.write().await;
    let transfer = transfers
        .get_mut(&transfer_id)
        .with_context(|| format!("unknown transfer_id {transfer_id}"))?;

    transfer.bytes_transferred = bytes_received.min(transfer.file_size_bytes);
    transfer.updated_at = Utc::now();

    if bytes_received != transfer.file_size_bytes {
        transfer.status = TransferStatus::Failed;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred: transfer.bytes_transferred,
            message: format!(
                "incomplete stream: received {bytes_received} bytes, expected {}",
                transfer.file_size_bytes
            ),
        });
    }

    finalize_transfer_on_disk(state, transfer_id, staging_path, final_path).await
}

async fn finalize_if_ready(
    state: &AppState,
    transfer_id: Uuid,
    staging_path: &Path,
    final_path: &Path,
    checkpoint_path: &Path,
) -> Result<FinalizeResult> {
    let _finalize_guard = state.finalize_lock.lock().await;

    let (already_done, bytes_transferred) = {
        let transfers = state.transfers.read().await;
        let transfer = transfers
            .get(&transfer_id)
            .with_context(|| format!("unknown transfer_id {transfer_id}"))?;
        (
            transfer.status == TransferStatus::Completed,
            transfer.bytes_transferred,
        )
    };
    if already_done {
        return Ok(FinalizeResult {
            status: TransferStatus::Completed,
            bytes_transferred,
            message: "transfer already finalized".to_owned(),
        });
    }

    let checkpoint = load_checkpoint(state, checkpoint_path).await?;
    let total = checkpoint.total_transferred();
    if !checkpoint.all_lanes_complete() {
        set_transfer_status(state, transfer_id, TransferStatus::Running, total).await;
        return Ok(FinalizeResult {
            status: TransferStatus::Running,
            bytes_transferred: total,
            message: "lane complete; waiting for remaining lanes".to_owned(),
        });
    }

    let finalized = finalize_transfer_on_disk(state, transfer_id, staging_path, final_path).await?;
    if let Err(err) = fs::remove_file(checkpoint_path).await {
        warn!(
            error = %err,
            checkpoint = %checkpoint_path.display(),
            "failed removing checkpoint file after finalize"
        );
    }
    Ok(finalized)
}

async fn finalize_transfer_on_disk(
    state: &AppState,
    transfer_id: Uuid,
    staging_path: &Path,
    final_path: &Path,
) -> Result<FinalizeResult> {
    let (overwrite, immutable_destination, bytes_transferred) = {
        let transfers = state.transfers.read().await;
        let transfer = transfers
            .get(&transfer_id)
            .with_context(|| format!("unknown transfer_id {transfer_id}"))?;
        (
            transfer.overwrite,
            transfer.immutable_destination,
            transfer.bytes_transferred,
        )
    };

    if immutable_destination && fs::try_exists(final_path).await? {
        set_transfer_status(
            state,
            transfer_id,
            TransferStatus::Failed,
            bytes_transferred,
        )
        .await;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred,
            message: format!(
                "immutable destination rejected existing file {}",
                final_path.display()
            ),
        });
    }

    if !overwrite && fs::try_exists(final_path).await? {
        set_transfer_status(
            state,
            transfer_id,
            TransferStatus::Failed,
            bytes_transferred,
        )
        .await;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred,
            message: format!(
                "destination exists and overwrite is disabled: {}",
                final_path.display()
            ),
        });
    }

    if fs::try_exists(final_path).await? {
        fs::remove_file(final_path)
            .await
            .with_context(|| format!("failed removing existing file {}", final_path.display()))?;
    }

    fs::rename(staging_path, final_path)
        .await
        .with_context(|| {
            format!(
                "failed moving staging file {} to {}",
                staging_path.display(),
                final_path.display()
            )
        })?;

    set_transfer_status(
        state,
        transfer_id,
        TransferStatus::Completed,
        bytes_transferred,
    )
    .await;

    Ok(FinalizeResult {
        status: TransferStatus::Completed,
        bytes_transferred,
        message: format!("transfer finalized at {}", final_path.display()),
    })
}

async fn set_transfer_status(
    state: &AppState,
    transfer_id: Uuid,
    status: TransferStatus,
    bytes_transferred: u64,
) {
    let mut transfers = state.transfers.write().await;
    if let Some(transfer) = transfers.get_mut(&transfer_id) {
        transfer.status = status;
        transfer.bytes_transferred = bytes_transferred.min(transfer.file_size_bytes);
        transfer.updated_at = Utc::now();
    }
}

async fn load_or_init_lane_resume(
    state: &AppState,
    checkpoint_path: &Path,
    transfer_id: Uuid,
    file_size_bytes: u64,
    total_lanes: u32,
    lane_index: u32,
    range_start: u64,
    range_end_exclusive: u64,
) -> Result<(u64, u64)> {
    let _guard = state.checkpoint_lock.lock().await;
    let mut checkpoint = if fs::try_exists(checkpoint_path).await? {
        load_checkpoint_unlocked(checkpoint_path).await?
    } else {
        let fresh = new_checkpoint(transfer_id, file_size_bytes, total_lanes);
        persist_checkpoint_unlocked(checkpoint_path, &fresh).await?;
        fresh
    };

    checkpoint.validate_layout(
        file_size_bytes,
        total_lanes,
        range_start,
        range_end_exclusive,
        lane_index,
    )?;
    let lane = checkpoint
        .lane_mut(lane_index)
        .with_context(|| format!("missing lane {lane_index} in checkpoint"))?;
    lane.offset = lane
        .offset
        .clamp(lane.range_start, lane.range_end_exclusive);
    let resume = lane.offset;
    let total = checkpoint.total_transferred();
    persist_checkpoint_unlocked(checkpoint_path, &checkpoint).await?;
    Ok((resume, total))
}

async fn update_checkpoint_offset(
    state: &AppState,
    checkpoint_path: &Path,
    lane_index: u32,
    new_offset: u64,
) -> Result<u64> {
    let _guard = state.checkpoint_lock.lock().await;
    let mut checkpoint = load_checkpoint_unlocked(checkpoint_path).await?;
    let lane = checkpoint
        .lane_mut(lane_index)
        .with_context(|| format!("missing lane {lane_index} in checkpoint"))?;
    lane.offset = new_offset.clamp(lane.range_start, lane.range_end_exclusive);
    let total = checkpoint.total_transferred();
    persist_checkpoint_unlocked(checkpoint_path, &checkpoint).await?;
    Ok(total)
}

async fn load_checkpoint(state: &AppState, checkpoint_path: &Path) -> Result<TransferCheckpoint> {
    let _guard = state.checkpoint_lock.lock().await;
    load_checkpoint_unlocked(checkpoint_path).await
}

async fn load_checkpoint_unlocked(checkpoint_path: &Path) -> Result<TransferCheckpoint> {
    let bytes = fs::read(checkpoint_path).await.with_context(|| {
        format!(
            "failed reading checkpoint file {}",
            checkpoint_path.display()
        )
    })?;
    serde_json::from_slice::<TransferCheckpoint>(&bytes).with_context(|| {
        format!(
            "failed parsing checkpoint file {}",
            checkpoint_path.display()
        )
    })
}

async fn persist_checkpoint_unlocked(
    checkpoint_path: &Path,
    checkpoint: &TransferCheckpoint,
) -> Result<()> {
    let json =
        serde_json::to_vec_pretty(checkpoint).context("failed serializing checkpoint JSON")?;
    fs::write(checkpoint_path, json).await.with_context(|| {
        format!(
            "failed writing checkpoint file {}",
            checkpoint_path.display()
        )
    })
}

fn checkpoint_path_for(tenant_dir: &Path, transfer_id: Uuid) -> PathBuf {
    tenant_dir.join(format!("{transfer_id}.part.meta.json"))
}

fn new_checkpoint(transfer_id: Uuid, file_size_bytes: u64, total_lanes: u32) -> TransferCheckpoint {
    let lanes = split_ranges(file_size_bytes, total_lanes)
        .into_iter()
        .enumerate()
        .map(
            |(lane_index, (range_start, range_end_exclusive))| LaneCheckpoint {
                lane_index: lane_index as u32,
                range_start,
                range_end_exclusive,
                offset: range_start,
            },
        )
        .collect();
    TransferCheckpoint {
        transfer_id,
        file_size_bytes,
        total_lanes,
        lanes,
    }
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
