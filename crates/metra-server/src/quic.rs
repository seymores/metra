use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use metra_proto::{
    QUIC_PROTOCOL_VERSION, QuicTransferCompleteAck, QuicTransferOpen, QuicTransferOpenAck,
    TransferStatus,
};
use quinn::crypto::rustls::QuicServerConfig;
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

    let (staging_path, final_path, resume_offset) = {
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

        let resume_offset = if striped {
            if transfer.status == TransferStatus::Queued {
                transfer.bytes_transferred = 0;
            }
            range_start
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
        (staging_path, final_path, resume_offset)
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

    while stream_bytes_written < expected_bytes {
        let Some(chunk) = recv_stream.read_chunk(MAX_CHUNK_READ_BYTES, true).await? else {
            break;
        };
        let remaining = expected_bytes - stream_bytes_written;
        let to_write = remaining.min(chunk.bytes.len() as u64) as usize;
        file.write_all(&chunk.bytes[..to_write])
            .await
            .context("failed writing transfer chunk to disk")?;
        let chunk_size = to_write as u64;
        stream_bytes_written += chunk_size;
        since_update += chunk_size;

        if to_write < chunk.bytes.len() {
            bail!(
                "received lane payload overflow for transfer {} lane {}",
                open.transfer_id,
                open.lane_index
            );
        }

        if since_update >= PROGRESS_UPDATE_INTERVAL_BYTES {
            apply_progress_update(
                &state,
                open.transfer_id,
                striped,
                resume_offset,
                stream_bytes_written,
                since_update,
            )
            .await;
            since_update = 0;
        }
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

    if since_update > 0 {
        apply_progress_update(
            &state,
            open.transfer_id,
            striped,
            resume_offset,
            stream_bytes_written,
            since_update,
        )
        .await;
    }

    file.flush()
        .await
        .context("failed flushing transfer file")?;

    let complete = if striped {
        finalize_if_ready(&state, open.transfer_id, &staging_path, &final_path).await?
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

async fn apply_progress_update(
    state: &AppState,
    transfer_id: Uuid,
    striped: bool,
    resume_offset: u64,
    stream_bytes_written: u64,
    incremental_bytes: u64,
) {
    let mut transfers = state.transfers.write().await;
    if let Some(transfer) = transfers.get_mut(&transfer_id) {
        if striped {
            transfer.bytes_transferred =
                (transfer.bytes_transferred + incremental_bytes).min(transfer.file_size_bytes);
        } else {
            transfer.bytes_transferred =
                (resume_offset + stream_bytes_written).min(transfer.file_size_bytes);
        }
        transfer.updated_at = Utc::now();
    }
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
) -> Result<FinalizeResult> {
    let _guard = state.finalize_lock.lock().await;
    let (status, bytes_transferred, file_size_bytes) = {
        let transfers = state.transfers.read().await;
        let transfer = transfers
            .get(&transfer_id)
            .with_context(|| format!("unknown transfer_id {transfer_id}"))?;
        (
            transfer.status,
            transfer.bytes_transferred,
            transfer.file_size_bytes,
        )
    };

    if status == TransferStatus::Completed {
        return Ok(FinalizeResult {
            status,
            bytes_transferred,
            message: "transfer already finalized".to_owned(),
        });
    }

    if bytes_transferred < file_size_bytes {
        return Ok(FinalizeResult {
            status: TransferStatus::Running,
            bytes_transferred,
            message: "lane complete; waiting for remaining lanes".to_owned(),
        });
    }

    finalize_transfer_on_disk(state, transfer_id, staging_path, final_path).await
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
