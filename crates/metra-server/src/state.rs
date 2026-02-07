use std::{collections::HashMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Instant};

use metra_proto::TransferSummary;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub transfers: Arc<RwLock<HashMap<Uuid, TransferSummary>>>,
    pub transfer_started_at: Arc<RwLock<HashMap<Uuid, Instant>>>,
    pub quic_addr: SocketAddr,
    pub data_dir: Arc<PathBuf>,
    pub quic_server_name: Arc<String>,
    pub quic_profile: Arc<String>,
    pub quic_cert_der_b64: Arc<String>,
    pub finalize_lock: Arc<Mutex<()>>,
    pub checkpoint_lock: Arc<Mutex<()>>,
}

impl AppState {
    pub fn new(
        quic_addr: SocketAddr,
        data_dir: PathBuf,
        quic_server_name: String,
        quic_profile: String,
        quic_cert_der_b64: String,
    ) -> Self {
        Self {
            transfers: Arc::new(RwLock::new(HashMap::new())),
            transfer_started_at: Arc::new(RwLock::new(HashMap::new())),
            quic_addr,
            data_dir: Arc::new(data_dir),
            quic_server_name: Arc::new(quic_server_name),
            quic_profile: Arc::new(quic_profile),
            quic_cert_der_b64: Arc::new(quic_cert_der_b64),
            finalize_lock: Arc::new(Mutex::new(())),
            checkpoint_lock: Arc::new(Mutex::new(())),
        }
    }
}
