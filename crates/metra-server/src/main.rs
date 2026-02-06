use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use clap::Parser;
use metra_proto::{
    CreateTransferRequest, CreateTransferResponse, ErrorResponse, HealthResponse,
    QUIC_CONTROL_FRAME_MAX_BYTES, QUIC_PROTOCOL_VERSION, QuicCertificateResponse,
    QuicTransferCompleteAck, QuicTransferOpen, QuicTransferOpenAck, TransferStatus,
    TransferSummary,
};
use opentelemetry::trace::TracerProvider as _;
use quinn::crypto::rustls::QuicServerConfig;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt as TokioAsyncWriteExt, SeekFrom},
    sync::RwLock,
};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

const PROGRESS_UPDATE_INTERVAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "metra-server",
    about = "Metra control-plane API and QUIC transfer listener"
)]
struct Args {
    #[arg(long, env = "METRA_REST_ADDR", default_value = "127.0.0.1:8080")]
    rest_addr: SocketAddr,
    #[arg(long, env = "METRA_QUIC_ADDR", default_value = "127.0.0.1:8443")]
    quic_addr: SocketAddr,
    #[arg(long, env = "METRA_DATA_DIR", default_value = "./var/data")]
    data_dir: PathBuf,
    #[arg(long, env = "METRA_QUIC_SERVER_NAME", default_value = "localhost")]
    quic_server_name: String,
}

#[derive(Clone)]
struct AppState {
    transfers: Arc<RwLock<HashMap<Uuid, TransferSummary>>>,
    quic_addr: SocketAddr,
    data_dir: Arc<PathBuf>,
    quic_server_name: Arc<String>,
    quic_cert_der_b64: Arc<String>,
}

impl AppState {
    fn new(
        quic_addr: SocketAddr,
        data_dir: PathBuf,
        quic_server_name: String,
        quic_cert_der_b64: String,
    ) -> Self {
        Self {
            transfers: Arc::new(RwLock::new(HashMap::new())),
            quic_addr,
            data_dir: Arc::new(data_dir),
            quic_server_name: Arc::new(quic_server_name),
            quic_cert_der_b64: Arc::new(quic_cert_der_b64),
        }
    }
}

type ApiResult<T> = std::result::Result<T, (StatusCode, Json<ErrorResponse>)>;

#[tokio::main]
async fn main() -> Result<()> {
    install_crypto_provider();
    init_telemetry()?;
    let args = Args::parse();
    fs::create_dir_all(&args.data_dir).await.with_context(|| {
        format!(
            "failed to create data directory {}",
            args.data_dir.display()
        )
    })?;

    let (endpoint, cert_der) = build_quic_endpoint(args.quic_addr, &args.quic_server_name)?;
    let app_state = AppState::new(
        args.quic_addr,
        args.data_dir.clone(),
        args.quic_server_name.clone(),
        BASE64.encode(cert_der),
    );

    let shutdown = CancellationToken::new();
    let quic_shutdown = shutdown.child_token();
    let quic_state = app_state.clone();
    let quic_task = tokio::spawn(async move {
        if let Err(err) = run_quic_listener(endpoint, quic_state, quic_shutdown).await {
            error!(error = %err, "quic listener exited with error");
        }
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/quic/certificate", get(quic_certificate))
        .route("/v1/transfers", post(create_transfer))
        .route("/v1/transfers/{transfer_id}", get(get_transfer))
        .with_state(app_state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(args.rest_addr)
        .await
        .with_context(|| format!("failed to bind REST listener {}", args.rest_addr))?;
    info!(
        rest_addr = %args.rest_addr,
        quic_addr = %args.quic_addr,
        data_dir = %args.data_dir.display(),
        "metra server started"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await
        .context("REST server exited unexpectedly")?;

    shutdown.cancel();
    if let Err(join_err) = quic_task.await {
        error!(error = %join_err, "quic task join error");
    }

    Ok(())
}

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        quic_listener: state.quic_addr.to_string(),
        timestamp: Utc::now(),
    })
}

async fn quic_certificate(State(state): State<AppState>) -> Json<QuicCertificateResponse> {
    Json(QuicCertificateResponse {
        server_name: state.quic_server_name.to_string(),
        quic_addr: state.quic_addr.to_string(),
        der_base64: state.quic_cert_der_b64.to_string(),
        protocol_version: QUIC_PROTOCOL_VERSION.to_owned(),
    })
}

async fn create_transfer(
    State(state): State<AppState>,
    Json(payload): Json<CreateTransferRequest>,
) -> ApiResult<(StatusCode, Json<CreateTransferResponse>)> {
    if let Err(message) = payload.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                code: "validation_error".to_owned(),
                message,
            }),
        ));
    }

    let now = Utc::now();
    let transfer_id = Uuid::new_v4();
    let summary = TransferSummary {
        transfer_id,
        tenant_id: payload.tenant_id,
        user_id: payload.user_id,
        source_uri: payload.source_uri,
        destination_uri: payload.destination_uri,
        file_name: sanitize_file_name(&payload.file_name),
        file_size_bytes: payload.file_size_bytes,
        overwrite: payload.overwrite,
        immutable_destination: payload.immutable_destination,
        status: TransferStatus::Queued,
        resume_chunk_size_bytes: payload.resume_chunk_size_bytes,
        bytes_transferred: 0,
        created_at: now,
        updated_at: now,
    };

    state.transfers.write().await.insert(transfer_id, summary);
    info!(transfer_id = %transfer_id, "transfer accepted");

    Ok((
        StatusCode::CREATED,
        Json(CreateTransferResponse {
            transfer_id,
            status: TransferStatus::Queued,
            accepted_at: now,
            resume_chunk_size_bytes: payload.resume_chunk_size_bytes,
        }),
    ))
}

async fn get_transfer(
    State(state): State<AppState>,
    AxumPath(transfer_id): AxumPath<Uuid>,
) -> ApiResult<Json<TransferSummary>> {
    match state.transfers.read().await.get(&transfer_id).cloned() {
        Some(transfer) => Ok(Json(transfer)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                code: "not_found".to_owned(),
                message: format!("transfer {transfer_id} was not found"),
            }),
        )),
    }
}

async fn run_quic_listener(
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
        let resume_offset = match fs::metadata(&staging_path).await {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err).context("failed reading staging file metadata"),
        };

        if resume_offset > transfer.file_size_bytes {
            bail!(
                "staging file larger than expected size: {} > {}",
                resume_offset,
                transfer.file_size_bytes
            );
        }

        transfer.status = TransferStatus::Running;
        transfer.bytes_transferred = resume_offset;
        transfer.updated_at = Utc::now();
        (staging_path, final_path, resume_offset)
    };

    let open_ack = QuicTransferOpenAck {
        ok: true,
        resume_offset_bytes: resume_offset,
        message: "transfer stream accepted".to_owned(),
    };
    write_json_frame(&mut send_stream, &open_ack).await?;
    info!(
        peer = %peer,
        transfer_id = %open.transfer_id,
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

    let mut stream_bytes_written: u64 = 0;
    let mut since_update: u64 = 0;

    loop {
        let Some(chunk) = recv_stream.read_chunk(8 * 1024 * 1024, true).await? else {
            break;
        };
        file.write_all(&chunk.bytes)
            .await
            .context("failed writing transfer chunk to disk")?;
        let chunk_size = chunk.bytes.len() as u64;
        stream_bytes_written += chunk_size;
        since_update += chunk_size;

        if since_update >= PROGRESS_UPDATE_INTERVAL_BYTES {
            since_update = 0;
            let mut transfers = state.transfers.write().await;
            if let Some(transfer) = transfers.get_mut(&open.transfer_id) {
                transfer.bytes_transferred =
                    (resume_offset + stream_bytes_written).min(transfer.file_size_bytes);
                transfer.updated_at = Utc::now();
            }
        }
    }

    file.flush()
        .await
        .context("failed flushing transfer file")?;
    let bytes_received = resume_offset + stream_bytes_written;
    let transfer_status = finalize_transfer(
        &state,
        open.transfer_id,
        bytes_received,
        &staging_path,
        &final_path,
    )
    .await?;

    let complete_ack = QuicTransferCompleteAck {
        ok: transfer_status.status == TransferStatus::Completed,
        status: transfer_status.status,
        bytes_received: transfer_status.bytes_transferred,
        message: transfer_status.message,
        updated_at: Utc::now(),
    };
    write_json_frame(&mut send_stream, &complete_ack).await?;
    send_stream.finish()?;
    info!(
        peer = %peer,
        transfer_id = %open.transfer_id,
        bytes_received = complete_ack.bytes_received,
        status = ?complete_ack.status,
        "transfer stream completed"
    );

    Ok(())
}

struct FinalizeResult {
    status: TransferStatus,
    bytes_transferred: u64,
    message: String,
}

async fn finalize_transfer(
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

    if transfer.immutable_destination && fs::try_exists(final_path).await? {
        transfer.status = TransferStatus::Failed;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred: transfer.bytes_transferred,
            message: format!(
                "immutable destination rejected existing file {}",
                final_path.display()
            ),
        });
    }

    if !transfer.overwrite && fs::try_exists(final_path).await? {
        transfer.status = TransferStatus::Failed;
        return Ok(FinalizeResult {
            status: TransferStatus::Failed,
            bytes_transferred: transfer.bytes_transferred,
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

    transfer.status = TransferStatus::Completed;
    transfer.updated_at = Utc::now();
    Ok(FinalizeResult {
        status: TransferStatus::Completed,
        bytes_transferred: transfer.bytes_transferred,
        message: format!("transfer finalized at {}", final_path.display()),
    })
}

fn build_quic_endpoint(
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

async fn read_json_frame<T>(recv_stream: &mut quinn::RecvStream) -> Result<T>
where
    T: DeserializeOwned,
{
    let mut frame_len = [0u8; 4];
    recv_stream
        .read_exact(&mut frame_len)
        .await
        .context("failed reading frame length")?;
    let frame_len = u32::from_be_bytes(frame_len) as usize;
    if frame_len == 0 || frame_len > QUIC_CONTROL_FRAME_MAX_BYTES {
        bail!("invalid frame length: {frame_len}");
    }

    let mut data = vec![0u8; frame_len];
    recv_stream
        .read_exact(&mut data)
        .await
        .context("failed reading frame payload")?;
    let frame = serde_json::from_slice::<T>(&data).context("failed deserializing frame JSON")?;
    Ok(frame)
}

async fn write_json_frame<T>(send_stream: &mut quinn::SendStream, payload: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(payload).context("failed serializing frame JSON")?;
    if bytes.len() > QUIC_CONTROL_FRAME_MAX_BYTES {
        bail!("outbound frame too large: {}", bytes.len());
    }
    send_stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .context("failed writing frame length")?;
    send_stream
        .write_all(&bytes)
        .await
        .context("failed writing frame payload")?;
    Ok(())
}

fn sanitize_file_name(file_name: &str) -> String {
    let mut output = String::with_capacity(file_name.len());
    for ch in file_name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "transfer.bin".to_owned()
    } else {
        output
    }
}

fn init_telemetry() -> Result<()> {
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let tracer = tracer_provider.tracer("metra-server");
    opentelemetry::global::set_tracer_provider(tracer_provider);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "metra_server=info,tower_http=info,axum=info,quinn=info".into());

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()
        .context("failed to initialize tracing")?;

    Ok(())
}

async fn shutdown_signal(shutdown: CancellationToken) {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            warn!("failed to listen for ctrl-c");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(_) => return,
            };
        let _ = sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    info!("shutdown signal received");
    shutdown.cancel();
}
