use std::{net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use metra_proto::RESUME_CHUNK_SIZE_BYTES;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum QuicProfile {
    Lan,
    Wan,
    HighBdp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RuntimeProfile {
    Balanced,
    Throughput,
    LowCpu,
}

impl RuntimeProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Throughput => "throughput",
            Self::LowCpu => "low-cpu",
        }
    }
}

impl QuicProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Wan => "wan",
            Self::HighBdp => "high-bdp",
        }
    }
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
    Tui(TuiArgs),
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
    MatrixProfiles(MatrixProfilesArgs),
    TuneRuntime(TuneRuntimeArgs),
    Compare(CompareArgs),
    CompareSeries(CompareSeriesArgs),
    TuneLanes(TuneLanesArgs),
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

#[derive(Debug, Clone, clap::Args)]
pub struct TuiArgs {
    #[arg(long, default_value_t = 1)]
    pub bench_size_gib: u64,
    #[arg(long, default_value_t = 2)]
    pub bench_lanes: u32,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub bench_io_chunk_bytes: usize,
    #[arg(long, default_value_t = true)]
    pub bench_no_disk: bool,
    #[arg(long, default_value = "/tmp/metra-tui-bench.bin")]
    pub bench_file_path: PathBuf,
    #[arg(long)]
    pub auto_runtime_report: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy: Option<PathBuf>,
}

impl Default for TuiArgs {
    fn default() -> Self {
        Self {
            bench_size_gib: 1,
            bench_lanes: 2,
            bench_io_chunk_bytes: 8 * 1024 * 1024,
            bench_no_disk: true,
            bench_file_path: PathBuf::from("/tmp/metra-tui-bench.bin"),
            auto_runtime_report: None,
            runtime_policy: None,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct SendArgs {
    #[arg(value_name = "FILE_PATH")]
    pub file_path: PathBuf,
    #[arg(value_name = "DESTINATION", required_unless_present = "transfer_id")]
    pub destination: Option<String>,
    #[arg(long)]
    pub transfer_id: Option<Uuid>,
    #[arg(long)]
    pub tenant_id: Option<String>,
    #[arg(long)]
    pub user_id: Option<String>,
    #[arg(long, default_value_t = false)]
    pub overwrite: bool,
    #[arg(long, default_value_t = false)]
    pub immutable_destination: bool,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = 1)]
    pub progress_interval_secs: u64,
    #[arg(long, default_value_t = 2)]
    pub lanes: u32,
    #[arg(long)]
    pub auto_lanes_report: Option<PathBuf>,
    #[arg(long)]
    pub lane_policy: Option<PathBuf>,
    #[arg(long)]
    pub auto_runtime_report: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy_out: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub runtime_profile: Option<RuntimeProfile>,
    #[arg(long)]
    pub file_read_pipeline_depth: Option<usize>,
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
    #[arg(long, default_value_t = false)]
    pub no_disk: bool,
    #[arg(long)]
    pub auto_lanes_report: Option<PathBuf>,
    #[arg(long)]
    pub lane_policy: Option<PathBuf>,
    #[arg(long)]
    pub auto_runtime_report: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy_out: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub runtime_profile: Option<RuntimeProfile>,
    #[arg(long)]
    pub file_read_pipeline_depth: Option<usize>,
}

#[derive(Debug, Clone, clap::Args)]
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
    #[arg(long, default_value_t = false)]
    pub no_disk: bool,
    #[arg(long)]
    pub auto_lanes_report: Option<PathBuf>,
    #[arg(long)]
    pub lane_policy: Option<PathBuf>,
    #[arg(long)]
    pub auto_runtime_report: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub runtime_profile: Option<RuntimeProfile>,
    #[arg(long)]
    pub file_read_pipeline_depth: Option<usize>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct MatrixProfilesArgs {
    #[arg(long, value_delimiter = ',', default_value = "lan,wan,high-bdp")]
    pub profiles: Vec<QuicProfile>,
    #[arg(long, value_delimiter = ',')]
    pub servers: Vec<String>,
    #[command(flatten)]
    pub matrix: MatrixArgs,
}

#[derive(Debug, clap::Args)]
pub struct TuneRuntimeArgs {
    #[arg(long, default_value_t = 1)]
    pub size_gib: u64,
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "balanced,throughput,low-cpu"
    )]
    pub profiles: Vec<RuntimeProfile>,
    #[arg(long, default_value_t = 2)]
    pub lanes: u32,
    #[arg(long, default_value_t = 2)]
    pub iterations: u32,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = false)]
    pub no_disk: bool,
    #[arg(long, default_value = "/tmp/metra-tune-runtime.bin")]
    pub file_path: PathBuf,
    #[arg(long, default_value = "metra-tune-runtime")]
    pub file_prefix: String,
    #[arg(long, default_value = "bench-tenant")]
    pub tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    pub user_id: String,
    #[arg(long, default_value = "local://benchmark/tune-runtime")]
    pub destination_prefix: String,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long)]
    pub file_read_pipeline_depth: Option<usize>,
    #[arg(long, default_value_t = true)]
    pub cleanup_file: bool,
    #[arg(long)]
    pub json_out: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy_out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct CompareArgs {
    #[arg(long, default_value_t = 2)]
    pub size_gib: u64,
    #[arg(long, default_value = "/tmp/metra-bench-compare.bin")]
    pub file_path: PathBuf,
    #[arg(long, default_value = "bench-tenant")]
    pub tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    pub user_id: String,
    #[arg(long, default_value = "local://benchmark/metra-bench-compare.bin")]
    pub destination_uri: String,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = 1)]
    pub lanes: u32,
    #[arg(long, default_value_t = 3)]
    pub iterations: u32,
    #[arg(long)]
    pub auto_runtime_report: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy_out: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub runtime_profile: Option<RuntimeProfile>,
    #[arg(long)]
    pub file_read_pipeline_depth: Option<usize>,
    #[arg(long, default_value_t = true)]
    pub cleanup_file: bool,
    #[arg(long)]
    pub json_out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct CompareSeriesArgs {
    #[arg(long, value_delimiter = ',', default_value = "1,2,4")]
    pub sizes_gib: Vec<u64>,
    #[arg(long, default_value = "/tmp")]
    pub file_dir: PathBuf,
    #[arg(long, default_value = "metra-bench-compare-series")]
    pub file_prefix: String,
    #[arg(long, default_value = "bench-tenant")]
    pub tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    pub user_id: String,
    #[arg(long, default_value = "local://benchmark")]
    pub destination_prefix: String,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = 1)]
    pub lanes: u32,
    #[arg(long, default_value_t = 3)]
    pub iterations: u32,
    #[arg(long)]
    pub auto_runtime_report: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy: Option<PathBuf>,
    #[arg(long)]
    pub runtime_policy_out: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub runtime_profile: Option<RuntimeProfile>,
    #[arg(long)]
    pub file_read_pipeline_depth: Option<usize>,
    #[arg(long, default_value_t = true)]
    pub cleanup_files: bool,
    #[arg(long)]
    pub json_out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct TuneLanesArgs {
    #[arg(long, default_value_t = 2)]
    pub size_gib: u64,
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8")]
    pub lanes: Vec<u32>,
    #[arg(long, default_value_t = 2)]
    pub concurrency: u32,
    #[arg(long, default_value_t = 2)]
    pub iterations: u32,
    #[arg(long, default_value_t = 16 * 1024 * 1024)]
    pub io_chunk_bytes: usize,
    #[arg(long, default_value_t = false)]
    pub no_disk: bool,
    #[arg(long, default_value = "/tmp/metra-tune-lanes.bin")]
    pub file_path: PathBuf,
    #[arg(long, default_value = "metra-tune-lanes")]
    pub file_prefix: String,
    #[arg(long, default_value = "bench-tenant")]
    pub tenant_id: String,
    #[arg(long, default_value = "bench-user")]
    pub user_id: String,
    #[arg(long, default_value = "local://benchmark/tune-lanes")]
    pub destination_prefix: String,
    #[arg(long)]
    pub quic_addr: Option<SocketAddr>,
    #[arg(long, default_value_t = true)]
    pub cleanup_file: bool,
    #[arg(long)]
    pub json_out: Option<PathBuf>,
    #[arg(long)]
    pub lane_policy_out: Option<PathBuf>,
}
