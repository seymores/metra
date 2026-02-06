use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc};

use metra_proto::TransferSummary;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub transfers: Arc<RwLock<HashMap<Uuid, TransferSummary>>>,
    pub quic_addr: SocketAddr,
    pub data_dir: Arc<PathBuf>,
    pub quic_server_name: Arc<String>,
    pub quic_cert_der_b64: Arc<String>,
}

impl AppState {
    pub fn new(
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
