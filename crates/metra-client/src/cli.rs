use std::{net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use metra_proto::RESUME_CHUNK_SIZE_BYTES;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Parser)]
#[command(name = "metra-client", about = "Metra TUI + scriptable CLI client")]
pub struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:8080")]
    pub server: String,
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Tui,
    Health,
    Transfer {
        #[command(subcommand)]
        action: TransferAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum TransferAction {
    Create(CreateArgs),
    Status(StatusArgs),
    Send(SendArgs),
    Bench(BenchArgs),
    Matrix(MatrixArgs),
}

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    #[arg(long)]
    pub tenant_id: String,
    #[arg(long)]
    pub user_id: String,
    #[arg(long)]
    pub source_uri: String,
    #[arg(long)]
    pub destination_uri: String,
    #[arg(long)]
    pub file_name: String,
    #[arg(long)]
    pub file_size_bytes: u64,
    #[arg(long, default_value_t = RESUME_CHUNK_SIZE_BYTES)]
    pub resume_chunk_size_bytes: u64,
    #[arg(long, default_value_t = false)]
    pub overwrite: bool,
    #[arg(long, default_value_t = false)]
    pub immutable_destination: bool,
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub transfer_id: Uuid,
}

#[derive(Debug, clap::Args)]
pub struct SendArgs {
    #[arg(long)]
    pub transfer_id: Uuid,
    #[arg(long)]
    pub file_path: PathBuf,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = 1)]
    pub progress_interval_secs: u64,
    #[arg(long, default_value_t = 1)]
    pub lanes: u32,
}

#[derive(Debug, clap::Args)]
pub struct BenchArgs {
    #[arg(long, default_value_t = 2)]
    pub size_gib: u64,
    #[arg(long, default_value = "/tmp/metra-bench.bin")]
    pub file_path: PathBuf,
    #[arg(long, default_value = "bench-tenant")]
    pub tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    pub user_id: String,
    #[arg(long, default_value = "local://benchmark/metra-bench.bin")]
    pub destination_uri: String,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = 1)]
    pub lanes: u32,
}

#[derive(Debug, clap::Args)]
pub struct MatrixArgs {
    #[arg(long, value_delimiter = ',', default_value = "4,16")]
    pub sizes_gib: Vec<u64>,
    #[arg(long, value_delimiter = ',', default_value = "1,2,4")]
    pub lanes: Vec<u32>,
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "4194304,16777216,67108864"
    )]
    pub io_chunk_bytes: Vec<usize>,
    #[arg(long, default_value = "/tmp")]
    pub file_dir: PathBuf,
    #[arg(long, default_value = "bench-tenant")]
    pub tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    pub user_id: String,
    #[arg(long, default_value = "local://benchmark")]
    pub destination_prefix: String,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = true)]
    pub cleanup_files: bool,
}
