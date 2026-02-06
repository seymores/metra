use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use metra_proto::{
    CreateTransferRequest, CreateTransferResponse, HealthResponse, QUIC_CONTROL_FRAME_MAX_BYTES,
    QUIC_PROTOCOL_VERSION, QuicCertificateResponse, QuicTransferCompleteAck, QuicTransferOpen,
    QuicTransferOpenAck, RESUME_CHUNK_SIZE_BYTES, TransferSummary,
};
use quinn::crypto::rustls::QuicClientConfig;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use reqwest::Client;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    fs::{self, OpenOptions},
    io::{AsyncReadExt as TokioAsyncReadExt, AsyncSeekExt, SeekFrom},
};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Parser)]
#[command(name = "metra-client", about = "Metra TUI + scriptable CLI client")]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:8080")]
    server: String,
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    output: OutputFormat,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Tui,
    Health,
    Transfer {
        #[command(subcommand)]
        action: TransferAction,
    },
}

#[derive(Debug, Subcommand)]
enum TransferAction {
    Create(CreateArgs),
    Status(StatusArgs),
    Send(SendArgs),
    Bench(BenchArgs),
}

#[derive(Debug, clap::Args)]
struct CreateArgs {
    #[arg(long)]
    tenant_id: String,
    #[arg(long)]
    user_id: String,
    #[arg(long)]
    source_uri: String,
    #[arg(long)]
    destination_uri: String,
    #[arg(long)]
    file_name: String,
    #[arg(long)]
    file_size_bytes: u64,
    #[arg(long, default_value_t = RESUME_CHUNK_SIZE_BYTES)]
    resume_chunk_size_bytes: u64,
    #[arg(long, default_value_t = false)]
    overwrite: bool,
    #[arg(long, default_value_t = false)]
    immutable_destination: bool,
}

#[derive(Debug, clap::Args)]
struct StatusArgs {
    #[arg(long)]
    transfer_id: Uuid,
}

#[derive(Debug, clap::Args)]
struct SendArgs {
    #[arg(long)]
    transfer_id: Uuid,
    #[arg(long)]
    file_path: PathBuf,
    #[arg(long)]
    quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    io_chunk_bytes: usize,
    #[arg(long, default_value_t = 1)]
    progress_interval_secs: u64,
}

#[derive(Debug, clap::Args)]
struct BenchArgs {
    #[arg(long, default_value_t = 2)]
    size_gib: u64,
    #[arg(long, default_value = "/tmp/metra-bench.bin")]
    file_path: PathBuf,
    #[arg(long, default_value = "bench-tenant")]
    tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    user_id: String,
    #[arg(long, default_value = "local://benchmark/metra-bench.bin")]
    destination_uri: String,
    #[arg(long)]
    quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    io_chunk_bytes: usize,
}

#[derive(Default)]
struct TuiState {
    health: Option<HealthResponse>,
    error: Option<String>,
    refreshed_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendTransferReport {
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

#[tokio::main]
async fn main() -> Result<()> {
    install_crypto_provider();
    let cli = Cli::parse();
    let http = Client::new();

    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(&http, &cli.server).await,
        Command::Health => {
            let health = fetch_health(&http, &cli.server).await?;
            print_output(&health, cli.output)?;
            Ok(())
        }
        Command::Transfer { action } => match action {
            TransferAction::Create(args) => {
                let request = CreateTransferRequest {
                    tenant_id: args.tenant_id,
                    user_id: args.user_id,
                    source_uri: args.source_uri,
                    destination_uri: args.destination_uri,
                    file_name: args.file_name,
                    file_size_bytes: args.file_size_bytes,
                    resume_chunk_size_bytes: args.resume_chunk_size_bytes,
                    overwrite: args.overwrite,
                    immutable_destination: args.immutable_destination,
                };
                request
                    .validate()
                    .map_err(|err| anyhow::anyhow!("invalid transfer request: {err}"))?;
                let created = create_transfer(&http, &cli.server, &request).await?;
                print_output(&created, cli.output)?;
                Ok(())
            }
            TransferAction::Status(args) => {
                let transfer = fetch_transfer_status(&http, &cli.server, args.transfer_id).await?;
                print_output(&transfer, cli.output)?;
                Ok(())
            }
            TransferAction::Send(args) => {
                let report = send_transfer(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
            TransferAction::Bench(args) => {
                let report = run_benchmark(&http, &cli.server, args).await?;
                print_output(&report, cli.output)?;
                Ok(())
            }
        },
    }
}

fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

async fn fetch_health(http: &Client, server: &str) -> Result<HealthResponse> {
    let response = http
        .get(format!("{server}/health"))
        .send()
        .await
        .context("health request failed")?
        .error_for_status()
        .context("health endpoint returned non-success response")?;
    response
        .json::<HealthResponse>()
        .await
        .context("failed parsing health response")
}

async fn create_transfer(
    http: &Client,
    server: &str,
    request: &CreateTransferRequest,
) -> Result<CreateTransferResponse> {
    let response = http
        .post(format!("{server}/v1/transfers"))
        .json(request)
        .send()
        .await
        .context("create transfer request failed")?
        .error_for_status()
        .context("create transfer returned non-success response")?;
    response
        .json::<CreateTransferResponse>()
        .await
        .context("failed parsing create transfer response")
}

async fn fetch_transfer_status(
    http: &Client,
    server: &str,
    transfer_id: Uuid,
) -> Result<TransferSummary> {
    let response = http
        .get(format!("{server}/v1/transfers/{transfer_id}"))
        .send()
        .await
        .context("transfer status request failed")?
        .error_for_status()
        .context("transfer status returned non-success response")?;
    response
        .json::<TransferSummary>()
        .await
        .context("failed parsing transfer status response")
}

async fn fetch_quic_certificate(http: &Client, server: &str) -> Result<QuicCertificateResponse> {
    let response = http
        .get(format!("{server}/v1/quic/certificate"))
        .send()
        .await
        .context("quic certificate request failed")?
        .error_for_status()
        .context("quic certificate endpoint returned non-success response")?;
    response
        .json::<QuicCertificateResponse>()
        .await
        .context("failed parsing quic certificate response")
}

async fn run_benchmark(http: &Client, server: &str, args: BenchArgs) -> Result<SendTransferReport> {
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

async fn send_transfer(http: &Client, server: &str, args: SendArgs) -> Result<SendTransferReport> {
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

async fn connect_quic(
    cert: &QuicCertificateResponse,
    quic_addr: SocketAddr,
) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let cert_der = BASE64
        .decode(&cert.der_base64)
        .context("failed decoding server certificate")?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der))
        .context("failed adding server certificate to root store")?;

    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![QUIC_PROTOCOL_VERSION.as_bytes().to_vec()];

    let client_crypto =
        QuicClientConfig::try_from(tls).context("failed building QUIC TLS config")?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
    transport.send_window(2 * 1024 * 1024 * 1024);
    client_config.transport_config(Arc::new(transport));

    let bind_addr = if quic_addr.is_ipv4() {
        "0.0.0.0:0".parse()?
    } else {
        "[::]:0".parse()?
    };
    let mut endpoint =
        quinn::Endpoint::client(bind_addr).context("failed creating QUIC endpoint")?;
    endpoint.set_default_client_config(client_config);

    let connection = endpoint
        .connect(quic_addr, &cert.server_name)
        .context("failed to begin QUIC connect")?
        .await
        .context("quic handshake failed")?;
    Ok((endpoint, connection))
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
        anyhow::bail!("invalid frame length: {frame_len}");
    }

    let mut data = vec![0u8; frame_len];
    recv_stream
        .read_exact(&mut data)
        .await
        .context("failed reading frame payload")?;
    serde_json::from_slice::<T>(&data).context("failed deserializing frame JSON")
}

async fn write_json_frame<T>(send_stream: &mut quinn::SendStream, payload: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(payload).context("failed serializing frame JSON")?;
    if bytes.len() > QUIC_CONTROL_FRAME_MAX_BYTES {
        anyhow::bail!("outbound frame too large: {}", bytes.len());
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

fn print_output<T: Serialize>(value: &T, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json | OutputFormat::Text => {
            println!("{}", serde_json::to_string_pretty(value)?);
        }
    }
    Ok(())
}

async fn run_tui(http: &Client, server: &str) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;
    let result = run_tui_loop(&mut terminal, http, server).await;

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    http: &Client,
    server: &str,
) -> Result<()> {
    let mut state = TuiState::default();
    refresh_health_in_state(&mut state, http, server).await;

    loop {
        terminal.draw(|frame| render_tui(frame, &state, server))?;
        if event::poll(Duration::from_millis(200))? {
            let event = event::read()?;
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('r') => refresh_health_in_state(&mut state, http, server).await,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn refresh_health_in_state(state: &mut TuiState, http: &Client, server: &str) {
    match fetch_health(http, server).await {
        Ok(health) => {
            state.health = Some(health);
            state.error = None;
            state.refreshed_at = Some(Utc::now().to_rfc3339());
        }
        Err(err) => {
            state.error = Some(err.to_string());
            state.refreshed_at = Some(Utc::now().to_rfc3339());
        }
    }
}

fn render_tui(frame: &mut ratatui::Frame<'_>, state: &TuiState, server: &str) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Min(8),
        ])
        .split(frame.area());

    let header = Paragraph::new(Text::from(vec![
        Line::from("Metra Client TUI"),
        Line::from(format!("Server: {server}")),
        Line::from("Keys: r=refresh health, q=quit"),
    ]))
    .block(Block::default().title("Session").borders(Borders::ALL));
    frame.render_widget(header, chunks[0]);

    let refreshed_text = state
        .refreshed_at
        .as_deref()
        .map_or_else(|| "never".to_owned(), ToOwned::to_owned);
    let refresh_box = Paragraph::new(format!("Last refresh: {refreshed_text}"))
        .block(Block::default().title("Refresh").borders(Borders::ALL));
    frame.render_widget(refresh_box, chunks[1]);

    let body_lines = if let Some(health) = &state.health {
        vec![
            Line::from(format!("status: {}", health.status)),
            Line::from(format!("version: {}", health.version)),
            Line::from(format!("quic_listener: {}", health.quic_listener)),
            Line::from(format!("timestamp: {}", health.timestamp)),
        ]
    } else if let Some(error) = &state.error {
        vec![Line::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red),
        )]
    } else {
        vec![Line::from("no data yet")]
    };

    let body = Paragraph::new(Text::from(body_lines))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Health").borders(Borders::ALL));
    frame.render_widget(body, chunks[2]);
}
