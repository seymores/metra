use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "metra-server",
    about = "Metra control-plane API and QUIC transfer listener"
)]
pub struct Args {
    #[arg(long, env = "METRA_REST_ADDR", default_value = "127.0.0.1:8080")]
    pub rest_addr: SocketAddr,
    #[arg(long, env = "METRA_QUIC_ADDR", default_value = "127.0.0.1:8443")]
    pub quic_addr: SocketAddr,
    #[arg(long, env = "METRA_DATA_DIR", default_value = "./var/data")]
    pub data_dir: PathBuf,
    #[arg(long, env = "METRA_QUIC_SERVER_NAME", default_value = "localhost")]
    pub quic_server_name: String,
}
