use std::{net::SocketAddr, path::PathBuf};

use clap::{Parser, ValueEnum};
use std::fmt;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum QuicTransportProfile {
    Lan,
    Wan,
    HighBdp,
}

impl QuicTransportProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lan => "lan",
            Self::Wan => "wan",
            Self::HighBdp => "high-bdp",
        }
    }
}

impl fmt::Display for QuicTransportProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

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
    #[arg(
        long,
        env = "METRA_QUIC_PROFILE",
        value_enum,
        default_value_t = QuicTransportProfile::Lan
    )]
    pub quic_profile: QuicTransportProfile,
}
