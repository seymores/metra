use std::{path::Path, time::Instant};

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
    let (mut send_stream, mut recv_stream) = connection
        .open_bi()
        .await
        .context("failed opening bidirectional QUIC stream")?;

    let open = QuicTransferOpen {
        transfer_id: transfer.transfer_id,
        file_size_bytes: transfer.file_size_bytes,
        file_name: transfer.file_name.clone(),
        resume_chunk_size_bytes: transfer.resume_chunk_size_bytes,
    };
    write_json_frame(&mut send_stream, &open).await?;
    let open_ack = read_json_frame::<QuicTransferOpenAck>(&mut recv_stream).await?;
    if !open_ack.ok {
        anyhow::bail!("server rejected transfer open: {}", open_ack.message);
    }
    if open_ack.resume_offset_bytes > transfer.file_size_bytes {
        anyhow::bail!(
            "invalid resume offset {} for transfer size {}",
            open_ack.resume_offset_bytes,
            transfer.file_size_bytes
        );
    }

    let mut file = fs::File::open(&args.file_path)
        .await
        .with_context(|| format!("failed opening file {}", args.file_path.display()))?;
    file.seek(SeekFrom::Start(open_ack.resume_offset_bytes))
        .await
        .context("failed seeking local file for resume")?;

    let started_at = Instant::now();
    let mut last_progress = Instant::now();
    let mut buffer = vec![0u8; args.io_chunk_bytes];
    let mut session_bytes: u64 = 0;

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .await
            .context("failed reading local file")?;
        if bytes_read == 0 {
            break;
        }
        send_stream
            .write_all(&buffer[..bytes_read])
            .await
            .context("failed writing stream payload")?;
        session_bytes += bytes_read as u64;

        if last_progress.elapsed().as_secs() >= args.progress_interval_secs {
            let elapsed = started_at.elapsed().as_secs_f64();
            let gbps = if elapsed > 0.0 {
                (session_bytes as f64 * 8.0) / (elapsed * 1_000_000_000.0)
            } else {
                0.0
            };
            eprintln!(
                "transfer_id={} streamed={} bytes avg={:.3} Gbps",
                transfer.transfer_id, session_bytes, gbps
            );
            last_progress = Instant::now();
        }
    }

    send_stream.finish()?;
    let complete_ack = read_json_frame::<QuicTransferCompleteAck>(&mut recv_stream).await?;
    let elapsed_ms = started_at.elapsed().as_millis();
    let avg_gbps = if elapsed_ms == 0 {
        0.0
    } else {
        (session_bytes as f64 * 8.0) / ((elapsed_ms as f64 / 1000.0) * 1_000_000_000.0)
    };

    Ok(SendTransferReport {
        transfer_id: transfer.transfer_id,
        file_path: args.file_path.display().to_string(),
        file_size_bytes: transfer.file_size_bytes,
        resumed_from_bytes: open_ack.resume_offset_bytes,
        bytes_streamed_this_session: session_bytes,
        total_streamed_bytes: complete_ack.bytes_received,
        elapsed_ms,
        average_gbps: avg_gbps,
        final_status: format!("{:?}", complete_ack.status),
        message: complete_ack.message,
    })
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
