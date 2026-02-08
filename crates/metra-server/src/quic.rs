use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use metra_proto::{
    APP_PAYLOAD_FRAME_HEADER_BYTES, AeadLaneCodec, PayloadCodec, QUIC_PROTOCOL_VERSION,
    QuicTransferCompleteAck, QuicTransferOpen, QuicTransferOpenAck, RESUME_CHUNK_SIZE_BYTES,
    TRANSFER_LANES_MAX, TransferStatus, TransferSummary, app_payload_wire_len,
    decode_payload_frame, decode_payload_frame_header, is_valid_storage_id_segment,
    normalize_receive_write_pipeline_depth,
};
use quinn::crypto::rustls::QuicServerConfig;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt as TokioAsyncWriteExt, SeekFrom},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    args::QuicTransportProfile,
    quic_metrics,
    state::AppState,
    wire::{read_json_frame, write_json_frame},
};

const PROGRESS_UPDATE_INTERVAL_BYTES: u64 = RESUME_CHUNK_SIZE_BYTES;
const MAX_CONCURRENT_BIDI_STREAMS: u32 = 128;
const MAX_CONCURRENT_UNI_STREAMS: u32 = 32;

fn is_null_sink_uri(uri: &str) -> bool {
    uri.starts_with("null://") || uri.starts_with("memory://") || uri.starts_with("mem://")
}

struct LaneStreamMetricGuard {
    lane_index: u32,
    total_lanes: u32,
    striped: bool,
    no_disk: bool,
    started_at: Instant,
    bytes_received: u64,
    finished: bool,
}

impl LaneStreamMetricGuard {
    fn new(lane_index: u32, total_lanes: u32, striped: bool, no_disk: bool) -> Self {
        quic_metrics::record_lane_stream_started(lane_index, total_lanes, striped, no_disk);
        Self {
            lane_index,
            total_lanes,
            striped,
            no_disk,
            started_at: Instant::now(),
            bytes_received: 0,
            finished: false,
        }
    }

    fn add_bytes(&mut self, bytes: u64) {
        self.bytes_received = self.bytes_received.saturating_add(bytes);
    }

    fn finish(&mut self, status: TransferStatus) {
        if self.finished {
            return;
        }
        self.finished = true;
        quic_metrics::record_lane_stream_finished(
            self.lane_index,
            self.total_lanes,
            self.striped,
            self.no_disk,
            status,
            self.bytes_received,
            self.started_at.elapsed(),
        );
    }
}

impl Drop for LaneStreamMetricGuard {
    fn drop(&mut self) {
        if !self.finished {
            quic_metrics::record_lane_stream_finished(
                self.lane_index,
                self.total_lanes,
                self.striped,
                self.no_disk,
                TransferStatus::Failed,
                self.bytes_received,
                self.started_at.elapsed(),
            );
        }
    }
}

struct LaneWriterLeaseGuard {
    state: AppState,
    transfer_id: Uuid,
    lane_index: u32,
}

impl LaneWriterLeaseGuard {
    fn acquire(state: &AppState, transfer_id: Uuid, lane_index: u32) -> Result<Self> {
        if !state.try_acquire_lane_writer(transfer_id, lane_index) {
            bail!(
                "lane {} for transfer {} already has an active writer",
                lane_index,
                transfer_id
            );
        }
        Ok(Self {
            state: state.clone(),
            transfer_id,
            lane_index,
        })
    }
}

impl Drop for LaneWriterLeaseGuard {
    fn drop(&mut self) {
        self.state
            .release_lane_writer(self.transfer_id, self.lane_index);
    }
}

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
    profile: QuicTransportProfile,
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
    apply_transport_profile(transport_config, profile)?;

    let endpoint = quinn::Endpoint::server(server_config, bind_addr)
        .with_context(|| format!("failed binding QUIC endpoint on {bind_addr}"))?;

    Ok((endpoint, cert_der))
}

fn apply_transport_profile(
    transport_config: &mut quinn::TransportConfig,
    profile: QuicTransportProfile,
) -> Result<()> {
    transport_config.max_concurrent_bidi_streams(MAX_CONCURRENT_BIDI_STREAMS.into());
    transport_config.max_concurrent_uni_streams(MAX_CONCURRENT_UNI_STREAMS.into());
    match profile {
        QuicTransportProfile::Lan => {
            transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
            transport_config.max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
            transport_config.send_window(2 * 1024 * 1024 * 1024);
            transport_config.stream_receive_window((64 * 1024 * 1024u32).into());
            transport_config.receive_window((512 * 1024 * 1024u32).into());
        }
        QuicTransportProfile::Wan => {
            transport_config.keep_alive_interval(Some(Duration::from_secs(5)));
            transport_config.max_idle_timeout(Some(Duration::from_secs(180).try_into()?));
            transport_config.send_window(1024 * 1024 * 1024);
            transport_config.stream_receive_window((32 * 1024 * 1024u32).into());
            transport_config.receive_window((256 * 1024 * 1024u32).into());
        }
        QuicTransportProfile::HighBdp => {
            transport_config.keep_alive_interval(Some(Duration::from_secs(2)));
            transport_config.max_idle_timeout(Some(Duration::from_secs(240).try_into()?));
            transport_config.send_window(4 * 1024 * 1024 * 1024);
            transport_config.stream_receive_window((128 * 1024 * 1024u32).into());
            transport_config.receive_window((1024 * 1024 * 1024u32).into());
        }
    }
    Ok(())
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
    if total_lanes > TRANSFER_LANES_MAX {
        bail!(
            "total_lanes {} exceeds max {}",
            total_lanes,
            TRANSFER_LANES_MAX
        );
    }
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
    let payload_codec = PayloadCodec::from_wire(&open.payload_codec).ok_or_else(|| {
        anyhow::anyhow!(
            "unsupported payload codec '{}' for transfer {} lane {}",
            open.payload_codec,
            open.transfer_id,
            open.lane_index
        )
    })?;
    let receive_write_pipeline_depth =
        normalize_receive_write_pipeline_depth(open.receive_write_pipeline_depth);

    let (
        transfer_id,
        transfer_file_size,
        transfer_resume_chunk_size,
        tenant_id,
        user_id,
        transfer_file_name,
        destination_uri,
        transfer_bytes_transferred,
    ) = {
        let transfers = state.transfers.read().await;
        let transfer = transfers
            .get(&open.transfer_id)
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

        (
            transfer.transfer_id,
            transfer.file_size_bytes,
            transfer.resume_chunk_size_bytes,
            transfer.tenant_id.clone(),
            transfer.user_id.clone(),
            transfer.file_name.clone(),
            transfer.destination_uri.clone(),
            transfer.bytes_transferred,
        )
    };
    if !is_valid_storage_id_segment(&tenant_id) {
        bail!("transfer has invalid tenant_id '{}'", tenant_id);
    }
    if !is_valid_storage_id_segment(&user_id) {
        bail!("transfer has invalid user_id '{}'", user_id);
    }
    let _lane_writer_lease = LaneWriterLeaseGuard::acquire(&state, transfer_id, open.lane_index)?;

    let tenant_dir = state.data_dir.join(&tenant_id).join(&user_id);
    fs::create_dir_all(&tenant_dir)
        .await
        .with_context(|| format!("failed creating tenant directory {}", tenant_dir.display()))?;

    let staging_path = tenant_dir.join(format!("{}.part", transfer_id));
    let final_path = tenant_dir.join(&transfer_file_name);
    let checkpoint_path = checkpoint_path_for(&tenant_dir, transfer_id);
    let discard_payload = is_null_sink_uri(&destination_uri);

    let (resume_offset, bytes_transferred_now) = if striped {
        let (lane_resume, total_transferred) = load_or_init_lane_resume(
            &state,
            &checkpoint_path,
            transfer_id,
            transfer_file_size,
            total_lanes,
            open.lane_index,
            range_start,
            range_end_exclusive,
        )
        .await?;
        (lane_resume, total_transferred.min(transfer_file_size))
    } else if discard_payload {
        let bytes = transfer_bytes_transferred.min(transfer_file_size);
        (bytes, bytes)
    } else {
        let resume = match fs::metadata(&staging_path).await {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err).context("failed reading staging file metadata"),
        };
        if resume > transfer_file_size {
            bail!(
                "staging file larger than expected size: {} > {}",
                resume,
                transfer_file_size
            );
        }
        (resume, resume)
    };

    {
        let mut transfers = state.transfers.write().await;
        let transfer = transfers
            .get_mut(&open.transfer_id)
            .with_context(|| format!("unknown transfer_id {}", open.transfer_id))?;
        if transfer.file_size_bytes != transfer_file_size
            || transfer.resume_chunk_size_bytes != transfer_resume_chunk_size
        {
            bail!("transfer metadata changed while opening stream");
        }
        transfer.bytes_transferred = bytes_transferred_now;
        transfer.status = TransferStatus::Running;
        transfer.updated_at = Utc::now();
    }
    persist_transfer_summary(&state, open.transfer_id).await?;
    mark_transfer_started_if_absent(&state, open.transfer_id).await;

    let stream_result: Result<()> = async {
        if striped && !discard_payload {
            let staging_file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&staging_path)
                .await
                .with_context(|| {
                    format!("failed opening staging file {}", staging_path.display())
                })?;
            staging_file
                .set_len(open.file_size_bytes)
                .await
                .context("failed pre-sizing striped staging file")?;
        }

        let open_ack = QuicTransferOpenAck {
            ok: true,
            resume_offset_bytes: resume_offset,
            message: "transfer stream accepted".to_owned(),
            payload_codec: payload_codec.as_wire().to_owned(),
            receive_write_pipeline_depth,
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
            payload_codec = payload_codec.as_wire(),
            receive_write_pipeline_depth = receive_write_pipeline_depth,
            "accepted upload stream"
        );
        let mut lane_metrics =
            LaneStreamMetricGuard::new(open.lane_index, total_lanes, striped, discard_payload);

        let expected_bytes = range_end_exclusive - resume_offset;
        let stream_bytes_written = if discard_payload {
            receive_lane_payload_no_disk(
                &mut recv_stream,
                expected_bytes,
                open.transfer_id,
                open.lane_index,
                payload_codec,
            )
            .await?
        } else {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .truncate(false)
                .open(&staging_path)
                .await
                .with_context(|| {
                    format!("failed opening staging file {}", staging_path.display())
                })?;
            file.seek(SeekFrom::Start(resume_offset))
                .await
                .context("failed seeking staging file")?;
            receive_lane_payload_to_file_pipelined(
                &state,
                &mut recv_stream,
                file,
                ReceiveLanePipelineArgs {
                    transfer_id: open.transfer_id,
                    striped,
                    checkpoint_path: checkpoint_path.clone(),
                    lane_index: open.lane_index,
                    resume_offset,
                    expected_bytes,
                    payload_codec,
                    receive_write_pipeline_depth,
                },
            )
            .await?
        };
        lane_metrics.add_bytes(stream_bytes_written);

        if striped && discard_payload {
            persist_progress(
                &state,
                open.transfer_id,
                true,
                &checkpoint_path,
                open.lane_index,
                resume_offset + stream_bytes_written,
            )
            .await?;
        }

        let complete = if striped {
            finalize_if_ready(
                &state,
                open.transfer_id,
                &staging_path,
                &final_path,
                &checkpoint_path,
                discard_payload,
            )
            .await?
        } else if discard_payload {
            let bytes_received = resume_offset + stream_bytes_written;
            finalize_single_lane_transfer_memory(&state, open.transfer_id, bytes_received).await?
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
        lane_metrics.finish(complete.status);
        if matches!(
            complete.status,
            TransferStatus::Completed | TransferStatus::Failed
        ) {
            if let Some(transfer_started_at) =
                take_transfer_started_at(&state, open.transfer_id).await
            {
                quic_metrics::record_transfer_finished(
                    total_lanes,
                    striped,
                    discard_payload,
                    complete.status,
                    complete.bytes_transferred,
                    transfer_started_at.elapsed(),
                );
            }
        }

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
    .await;

    if let Err(err) = stream_result {
        if let Err(mark_err) = fail_running_transfer(&state, open.transfer_id).await {
            warn!(
                transfer_id = %open.transfer_id,
                error = %mark_err,
                "failed marking transfer as failed after stream error"
            );
        }
        return Err(err);
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct ReceiveLanePipelineArgs {
    transfer_id: Uuid,
    striped: bool,
    checkpoint_path: PathBuf,
    lane_index: u32,
    resume_offset: u64,
    expected_bytes: u64,
    payload_codec: PayloadCodec,
    receive_write_pipeline_depth: usize,
}

async fn receive_lane_payload_no_disk(
    recv_stream: &mut quinn::RecvStream,
    expected_bytes: u64,
    transfer_id: Uuid,
    lane_index: u32,
    payload_codec: PayloadCodec,
) -> Result<u64> {
    async fn read_wire_payload_frame(
        recv_stream: &mut quinn::RecvStream,
        payload_codec: PayloadCodec,
        transfer_id: Uuid,
        lane_index: u32,
    ) -> Result<(usize, [u8; 12], Vec<u8>)> {
        let mut header = [0u8; APP_PAYLOAD_FRAME_HEADER_BYTES];
        recv_stream.read_exact(&mut header).await.with_context(|| {
            format!(
                "failed reading payload frame header for transfer {} lane {}",
                transfer_id, lane_index
            )
        })?;
        let (plaintext_len, nonce) = decode_payload_frame_header(header)
            .map_err(|msg| anyhow::anyhow!(msg))
            .with_context(|| {
                format!(
                    "invalid payload frame header for transfer {} lane {}",
                    transfer_id, lane_index
                )
            })?;
        let wire_len = app_payload_wire_len(payload_codec, plaintext_len)
            .map_err(|msg| anyhow::anyhow!(msg))
            .with_context(|| {
                format!(
                    "invalid payload frame length for transfer {} lane {}",
                    transfer_id, lane_index
                )
            })?;
        let mut wire_payload = vec![0u8; wire_len];
        recv_stream
            .read_exact(&mut wire_payload)
            .await
            .with_context(|| {
                format!(
                    "failed reading payload frame body for transfer {} lane {}",
                    transfer_id, lane_index
                )
            })?;
        Ok((plaintext_len, nonce, wire_payload))
    }

    let aead_codec = match payload_codec {
        PayloadCodec::RawV1 => None,
        PayloadCodec::AeadV1 => Some(AeadLaneCodec::new(transfer_id, lane_index)),
    };
    let mut stream_bytes_written = 0u64;
    while stream_bytes_written < expected_bytes {
        let (plaintext_len, nonce, wire_payload) =
            read_wire_payload_frame(recv_stream, payload_codec, transfer_id, lane_index).await?;
        let remaining = expected_bytes - stream_bytes_written;
        if plaintext_len as u64 > remaining {
            bail!(
                "received lane payload overflow for transfer {} lane {}",
                transfer_id,
                lane_index
            );
        }
        let plaintext = decode_payload_frame(
            payload_codec,
            plaintext_len,
            nonce,
            &wire_payload,
            aead_codec.as_ref(),
        )
        .map_err(|msg| anyhow::anyhow!(msg))
        .with_context(|| {
            format!(
                "failed decoding payload frame for transfer {} lane {}",
                transfer_id, lane_index
            )
        })?;
        stream_bytes_written += plaintext.len() as u64;
    }
    if stream_bytes_written != expected_bytes {
        bail!(
            "incomplete lane stream for transfer {} lane {}: got {} expected {}",
            transfer_id,
            lane_index,
            stream_bytes_written,
            expected_bytes
        );
    }
    Ok(stream_bytes_written)
}

async fn receive_lane_payload_to_file_pipelined(
    state: &AppState,
    recv_stream: &mut quinn::RecvStream,
    mut file: fs::File,
    args: ReceiveLanePipelineArgs,
) -> Result<u64> {
    #[derive(Debug)]
    struct ReceiveWireFrame {
        plaintext_len: usize,
        nonce: [u8; 12],
        wire_payload: Vec<u8>,
    }

    #[derive(Debug)]
    struct ReceivePipelineChunk {
        buffer: Vec<u8>,
        bytes_read: usize,
    }

    async fn read_wire_payload_frame(
        recv_stream: &mut quinn::RecvStream,
        payload_codec: PayloadCodec,
        transfer_id: Uuid,
        lane_index: u32,
    ) -> Result<(usize, [u8; 12], Vec<u8>)> {
        let mut header = [0u8; APP_PAYLOAD_FRAME_HEADER_BYTES];
        recv_stream.read_exact(&mut header).await.with_context(|| {
            format!(
                "failed reading payload frame header for transfer {} lane {}",
                transfer_id, lane_index
            )
        })?;
        let (plaintext_len, nonce) = decode_payload_frame_header(header)
            .map_err(|msg| anyhow::anyhow!(msg))
            .with_context(|| {
                format!(
                    "invalid payload frame header for transfer {} lane {}",
                    transfer_id, lane_index
                )
            })?;
        let wire_len = app_payload_wire_len(payload_codec, plaintext_len)
            .map_err(|msg| anyhow::anyhow!(msg))
            .with_context(|| {
                format!(
                    "invalid payload frame length for transfer {} lane {}",
                    transfer_id, lane_index
                )
            })?;
        let mut wire_payload = vec![0u8; wire_len];
        recv_stream
            .read_exact(&mut wire_payload)
            .await
            .with_context(|| {
                format!(
                    "failed reading payload frame body for transfer {} lane {}",
                    transfer_id, lane_index
                )
            })?;
        Ok((plaintext_len, nonce, wire_payload))
    }

    let pipeline_depth = normalize_receive_write_pipeline_depth(args.receive_write_pipeline_depth);
    let (decrypt_tx, mut decrypt_rx) = mpsc::channel::<ReceiveWireFrame>(pipeline_depth);
    let (writer_tx, mut writer_rx) = mpsc::channel::<ReceivePipelineChunk>(pipeline_depth);

    let state_for_writer = state.clone();
    let writer_args = args.clone();
    let decrypt_args = args.clone();
    let writer_task = tokio::spawn(async move {
        let mut bytes_written = 0u64;
        let mut since_update = 0u64;
        let mut last_durable_offset = writer_args.resume_offset;
        while let Some(chunk) = writer_rx.recv().await {
            file.write_all(&chunk.buffer[..chunk.bytes_read])
                .await
                .context("failed writing transfer chunk to disk")?;
            let chunk_len = chunk.bytes_read as u64;
            bytes_written += chunk_len;
            since_update += chunk_len;

            if since_update >= PROGRESS_UPDATE_INTERVAL_BYTES {
                file.sync_data()
                    .await
                    .context("failed syncing transfer file before checkpoint update")?;
                persist_progress(
                    &state_for_writer,
                    writer_args.transfer_id,
                    writer_args.striped,
                    &writer_args.checkpoint_path,
                    writer_args.lane_index,
                    writer_args.resume_offset + bytes_written,
                )
                .await?;
                last_durable_offset = writer_args.resume_offset + bytes_written;
                since_update = 0;
            }
        }
        let final_offset = writer_args.resume_offset + bytes_written;
        if final_offset > last_durable_offset {
            file.sync_data()
                .await
                .context("failed syncing transfer file before final checkpoint update")?;
            persist_progress(
                &state_for_writer,
                writer_args.transfer_id,
                writer_args.striped,
                &writer_args.checkpoint_path,
                writer_args.lane_index,
                final_offset,
            )
            .await?;
        }
        file.flush()
            .await
            .context("failed flushing transfer file")?;
        Ok::<u64, anyhow::Error>(bytes_written)
    });

    let payload_codec = args.payload_codec;
    let decrypt_task = tokio::spawn(async move {
        let aead_codec = match payload_codec {
            PayloadCodec::RawV1 => None,
            PayloadCodec::AeadV1 => Some(AeadLaneCodec::new(
                decrypt_args.transfer_id,
                decrypt_args.lane_index,
            )),
        };
        while let Some(frame) = decrypt_rx.recv().await {
            let plaintext = decode_payload_frame(
                payload_codec,
                frame.plaintext_len,
                frame.nonce,
                &frame.wire_payload,
                aead_codec.as_ref(),
            )
            .map_err(|msg| anyhow::anyhow!(msg))
            .with_context(|| {
                format!(
                    "failed decoding payload frame for transfer {} lane {}",
                    decrypt_args.transfer_id, decrypt_args.lane_index
                )
            })?;
            writer_tx
                .send(ReceivePipelineChunk {
                    bytes_read: plaintext.len(),
                    buffer: plaintext,
                })
                .await
                .map_err(|_| anyhow::anyhow!("writer stage channel unexpectedly closed"))?;
        }
        Ok::<_, anyhow::Error>(())
    });

    let mut bytes_received = 0u64;
    let mut read_error: Option<anyhow::Error> = None;
    while bytes_received < args.expected_bytes {
        let (plaintext_len, nonce, wire_payload) = match read_wire_payload_frame(
            recv_stream,
            args.payload_codec,
            args.transfer_id,
            args.lane_index,
        )
        .await
        {
            Ok(frame) => frame,
            Err(err) => {
                read_error = Some(err);
                break;
            }
        };
        let remaining = args.expected_bytes - bytes_received;
        if plaintext_len as u64 > remaining {
            read_error = Some(anyhow::anyhow!(
                "received lane payload overflow for transfer {} lane {}",
                args.transfer_id,
                args.lane_index
            ));
            break;
        }

        if decrypt_tx
            .send(ReceiveWireFrame {
                plaintext_len,
                nonce,
                wire_payload,
            })
            .await
            .is_err()
        {
            read_error = Some(anyhow::anyhow!(
                "decrypt pipeline closed unexpectedly for transfer {} lane {}",
                args.transfer_id,
                args.lane_index
            ));
            break;
        }
        bytes_received += plaintext_len as u64;
    }

    drop(decrypt_tx);
    let decrypt_joined = decrypt_task
        .await
        .context("decrypt pipeline task panicked")?;
    decrypt_joined?;
    let writer_joined = writer_task.await.context("writer pipeline task panicked")?;
    let bytes_written = writer_joined?;

    if let Some(err) = read_error {
        return Err(err);
    }
    if bytes_received != args.expected_bytes {
        bail!(
            "incomplete lane stream for transfer {} lane {}: got {} expected {}",
            args.transfer_id,
            args.lane_index,
            bytes_received,
            args.expected_bytes
        );
    }
    if bytes_written != bytes_received {
        bail!(
            "lane {} writer mismatch: written {} != received {}",
            args.lane_index,
            bytes_written,
            bytes_received
        );
    }
    Ok(bytes_written)
}

async fn mark_transfer_started_if_absent(state: &AppState, transfer_id: Uuid) {
    let mut started_at = state.transfer_started_at.write().await;
    started_at.entry(transfer_id).or_insert_with(Instant::now);
}

async fn take_transfer_started_at(state: &AppState, transfer_id: Uuid) -> Option<Instant> {
    let mut started_at = state.transfer_started_at.write().await;
    started_at.remove(&transfer_id)
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
        update_checkpoint_offset(state, checkpoint_path, transfer_id, lane_index, new_offset)
            .await?
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

async fn persist_transfer_summary(state: &AppState, transfer_id: Uuid) -> Result<()> {
    let snapshot: Option<TransferSummary> = {
        let transfers = state.transfers.read().await;
        transfers.get(&transfer_id).cloned()
    };
    if let Some(summary) = snapshot {
        state.transfer_store.upsert(&summary).await?;
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
    let file_size = {
        let mut transfers = state.transfers.write().await;
        let transfer = transfers
            .get_mut(&transfer_id)
            .with_context(|| format!("unknown transfer_id {transfer_id}"))?;
        transfer.bytes_transferred = bytes_received.min(transfer.file_size_bytes);
        transfer.updated_at = Utc::now();
        transfer.file_size_bytes
    };

    if bytes_received != file_size {
        set_transfer_status(
            state,
            transfer_id,
            TransferStatus::Failed,
            bytes_received.min(file_size),
        )
        .await?;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred: bytes_received.min(file_size),
            message: format!(
                "incomplete stream: received {bytes_received} bytes, expected {}",
                file_size
            ),
        });
    }

    finalize_transfer_on_disk(state, transfer_id, staging_path, final_path).await
}

async fn finalize_single_lane_transfer_memory(
    state: &AppState,
    transfer_id: Uuid,
    bytes_received: u64,
) -> Result<FinalizeResult> {
    let file_size = {
        let mut transfers = state.transfers.write().await;
        let transfer = transfers
            .get_mut(&transfer_id)
            .with_context(|| format!("unknown transfer_id {transfer_id}"))?;
        transfer.bytes_transferred = bytes_received.min(transfer.file_size_bytes);
        transfer.updated_at = Utc::now();
        transfer.file_size_bytes
    };

    if bytes_received != file_size {
        set_transfer_status(
            state,
            transfer_id,
            TransferStatus::Failed,
            bytes_received.min(file_size),
        )
        .await?;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred: bytes_received.min(file_size),
            message: format!(
                "incomplete stream: received {bytes_received} bytes, expected {}",
                file_size
            ),
        });
    }

    set_transfer_status(
        state,
        transfer_id,
        TransferStatus::Completed,
        bytes_received.min(file_size),
    )
    .await?;
    Ok(FinalizeResult {
        status: TransferStatus::Completed,
        bytes_transferred: bytes_received.min(file_size),
        message: "transfer finalized in null sink mode".to_owned(),
    })
}

async fn finalize_if_ready(
    state: &AppState,
    transfer_id: Uuid,
    staging_path: &Path,
    final_path: &Path,
    checkpoint_path: &Path,
    discard_payload: bool,
) -> Result<FinalizeResult> {
    let finalize_lock = state.finalize_lock_for(transfer_id);
    let _finalize_guard = finalize_lock.lock().await;

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

    let checkpoint = load_checkpoint(state, checkpoint_path, transfer_id).await?;
    let total = checkpoint.total_transferred();
    if !checkpoint.all_lanes_complete() {
        set_transfer_status(state, transfer_id, TransferStatus::Running, total).await?;
        return Ok(FinalizeResult {
            status: TransferStatus::Running,
            bytes_transferred: total,
            message: "lane complete; waiting for remaining lanes".to_owned(),
        });
    }

    let finalized = if discard_payload {
        set_transfer_status(state, transfer_id, TransferStatus::Completed, total).await?;
        FinalizeResult {
            status: TransferStatus::Completed,
            bytes_transferred: total,
            message: "transfer finalized in null sink mode".to_owned(),
        }
    } else {
        finalize_transfer_on_disk(state, transfer_id, staging_path, final_path).await?
    };
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
        .await?;
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
        .await?;
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
    .await?;

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
) -> Result<()> {
    let mut terminal = false;
    let mut transfers = state.transfers.write().await;
    if let Some(transfer) = transfers.get_mut(&transfer_id) {
        transfer.status = status;
        transfer.bytes_transferred = bytes_transferred.min(transfer.file_size_bytes);
        transfer.updated_at = Utc::now();
        terminal = matches!(status, TransferStatus::Completed | TransferStatus::Failed);
    }
    drop(transfers);
    persist_transfer_summary(state, transfer_id).await?;
    if terminal {
        state.clear_transfer_locks(transfer_id);
    }
    Ok(())
}

async fn fail_running_transfer(state: &AppState, transfer_id: Uuid) -> Result<()> {
    let bytes_transferred = {
        let transfers = state.transfers.read().await;
        let Some(transfer) = transfers.get(&transfer_id) else {
            return Ok(());
        };
        if transfer.status != TransferStatus::Running {
            return Ok(());
        }
        transfer.bytes_transferred
    };
    set_transfer_status(
        state,
        transfer_id,
        TransferStatus::Failed,
        bytes_transferred,
    )
    .await
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
    let checkpoint_lock = state.checkpoint_lock_for(transfer_id);
    let _guard = checkpoint_lock.lock().await;
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
    lane.offset = durable_resume_offset(lane.offset, lane.range_start, lane.range_end_exclusive);
    let resume = lane.offset;
    let total = checkpoint.total_transferred();
    persist_checkpoint_unlocked(checkpoint_path, &checkpoint).await?;
    Ok((resume, total))
}

async fn update_checkpoint_offset(
    state: &AppState,
    checkpoint_path: &Path,
    transfer_id: Uuid,
    lane_index: u32,
    new_offset: u64,
) -> Result<u64> {
    let checkpoint_lock = state.checkpoint_lock_for(transfer_id);
    let _guard = checkpoint_lock.lock().await;
    let mut checkpoint = load_checkpoint_unlocked(checkpoint_path).await?;
    let lane = checkpoint
        .lane_mut(lane_index)
        .with_context(|| format!("missing lane {lane_index} in checkpoint"))?;
    let durable_offset =
        durable_resume_offset(new_offset, lane.range_start, lane.range_end_exclusive);
    lane.offset = lane.offset.max(durable_offset);
    let total = checkpoint.total_transferred();
    persist_checkpoint_unlocked(checkpoint_path, &checkpoint).await?;
    Ok(total)
}

async fn load_checkpoint(
    state: &AppState,
    checkpoint_path: &Path,
    transfer_id: Uuid,
) -> Result<TransferCheckpoint> {
    let checkpoint_lock = state.checkpoint_lock_for(transfer_id);
    let _guard = checkpoint_lock.lock().await;
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
    let parent = checkpoint_path.parent().with_context(|| {
        format!(
            "checkpoint path missing parent directory: {}",
            checkpoint_path.display()
        )
    })?;
    fs::create_dir_all(parent).await.with_context(|| {
        format!(
            "failed creating checkpoint parent directory {}",
            parent.display()
        )
    })?;

    let file_name = checkpoint_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("checkpoint");
    let tmp_path = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));
    let mut tmp = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp_path)
        .await
        .with_context(|| {
            format!(
                "failed creating checkpoint temp file {}",
                tmp_path.display()
            )
        })?;
    tmp.write_all(&json)
        .await
        .with_context(|| format!("failed writing checkpoint temp file {}", tmp_path.display()))?;
    tmp.flush().await.with_context(|| {
        format!(
            "failed flushing checkpoint temp file {}",
            tmp_path.display()
        )
    })?;
    tmp.sync_data()
        .await
        .with_context(|| format!("failed syncing checkpoint temp file {}", tmp_path.display()))?;
    drop(tmp);

    fs::rename(&tmp_path, checkpoint_path)
        .await
        .with_context(|| {
            format!(
                "failed atomically replacing checkpoint {} from {}",
                checkpoint_path.display(),
                tmp_path.display()
            )
        })?;
    sync_directory(parent).await?;
    Ok(())
}

async fn sync_directory(path: &Path) -> Result<()> {
    let dir = OpenOptions::new()
        .read(true)
        .open(path)
        .await
        .with_context(|| format!("failed opening directory for sync {}", path.display()))?;
    dir.sync_data()
        .await
        .with_context(|| format!("failed syncing directory metadata {}", path.display()))
}

fn durable_resume_offset(offset: u64, range_start: u64, range_end_exclusive: u64) -> u64 {
    let clamped = offset.clamp(range_start, range_end_exclusive);
    if clamped >= range_end_exclusive {
        return range_end_exclusive;
    }
    let aligned = clamped - (clamped % RESUME_CHUNK_SIZE_BYTES);
    aligned.clamp(range_start, range_end_exclusive)
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

#[cfg(test)]
mod tests {
    use super::{RESUME_CHUNK_SIZE_BYTES, durable_resume_offset};

    #[test]
    fn durable_resume_offset_aligns_to_resume_chunk_boundary() {
        let offset = (3 * RESUME_CHUNK_SIZE_BYTES) + (RESUME_CHUNK_SIZE_BYTES / 2);
        let aligned = durable_resume_offset(offset, 0, 10 * RESUME_CHUNK_SIZE_BYTES);
        assert_eq!(aligned, 3 * RESUME_CHUNK_SIZE_BYTES);
    }

    #[test]
    fn durable_resume_offset_preserves_lane_end_even_if_unaligned() {
        let range_end = (3 * RESUME_CHUNK_SIZE_BYTES) + 123;
        let aligned = durable_resume_offset(range_end, 0, range_end);
        assert_eq!(aligned, range_end);
    }

    #[test]
    fn durable_resume_offset_clamps_to_lane_start_for_mid_chunk_ranges() {
        let range_start = (2 * RESUME_CHUNK_SIZE_BYTES) + 17;
        let offset = range_start + 512;
        let aligned = durable_resume_offset(offset, range_start, 6 * RESUME_CHUNK_SIZE_BYTES);
        assert_eq!(aligned, range_start);
    }
}
