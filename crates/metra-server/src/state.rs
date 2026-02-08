use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::Instant,
};

use metra_proto::TransferSummary;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use crate::transfer_store::TransferStore;

#[derive(Clone)]
pub struct AppState {
    pub transfers: Arc<RwLock<HashMap<Uuid, TransferSummary>>>,
    pub transfer_started_at: Arc<RwLock<HashMap<Uuid, Instant>>>,
    pub active_lane_writers: Arc<StdMutex<HashSet<(Uuid, u32)>>>,
    pub transfer_store: Arc<TransferStore>,
    pub quic_addr: SocketAddr,
    pub data_dir: Arc<PathBuf>,
    pub quic_server_name: Arc<String>,
    pub quic_profile: Arc<String>,
    pub quic_cert_der_b64: Arc<String>,
    finalize_locks: Arc<StdMutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
    checkpoint_locks: Arc<StdMutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
}

impl AppState {
    pub fn new(
        quic_addr: SocketAddr,
        data_dir: PathBuf,
        quic_server_name: String,
        quic_profile: String,
        quic_cert_der_b64: String,
        transfer_store: TransferStore,
        persisted_transfers: Vec<TransferSummary>,
    ) -> Self {
        let transfers = persisted_transfers
            .into_iter()
            .map(|summary| (summary.transfer_id, summary))
            .collect();
        Self {
            transfers: Arc::new(RwLock::new(transfers)),
            transfer_started_at: Arc::new(RwLock::new(HashMap::new())),
            active_lane_writers: Arc::new(StdMutex::new(HashSet::new())),
            transfer_store: Arc::new(transfer_store),
            quic_addr,
            data_dir: Arc::new(data_dir),
            quic_server_name: Arc::new(quic_server_name),
            quic_profile: Arc::new(quic_profile),
            quic_cert_der_b64: Arc::new(quic_cert_der_b64),
            finalize_locks: Arc::new(StdMutex::new(HashMap::new())),
            checkpoint_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub fn try_acquire_lane_writer(&self, transfer_id: Uuid, lane_index: u32) -> bool {
        let mut active_writers = self
            .active_lane_writers
            .lock()
            .expect("active_lane_writers mutex poisoned");
        active_writers.insert((transfer_id, lane_index))
    }

    pub fn release_lane_writer(&self, transfer_id: Uuid, lane_index: u32) {
        let mut active_writers = self
            .active_lane_writers
            .lock()
            .expect("active_lane_writers mutex poisoned");
        active_writers.remove(&(transfer_id, lane_index));
    }

    pub fn finalize_lock_for(&self, transfer_id: Uuid) -> Arc<Mutex<()>> {
        let mut locks = self
            .finalize_locks
            .lock()
            .expect("finalize_locks mutex poisoned");
        locks
            .entry(transfer_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn checkpoint_lock_for(&self, transfer_id: Uuid) -> Arc<Mutex<()>> {
        let mut locks = self
            .checkpoint_locks
            .lock()
            .expect("checkpoint_locks mutex poisoned");
        locks
            .entry(transfer_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn clear_transfer_locks(&self, transfer_id: Uuid) {
        self.finalize_locks
            .lock()
            .expect("finalize_locks mutex poisoned")
            .remove(&transfer_id);
        self.checkpoint_locks
            .lock()
            .expect("checkpoint_locks mutex poisoned")
            .remove(&transfer_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::Arc,
    };

    fn test_state() -> AppState {
        let db_path =
            std::env::temp_dir().join(format!("metra-state-test-{}.sqlite", Uuid::new_v4()));
        let store = TransferStore::open(db_path).expect("create transfer store");
        AppState::new(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8443),
            PathBuf::from("./var/test-data"),
            "localhost".to_owned(),
            "lan".to_owned(),
            "cert".to_owned(),
            store,
            Vec::new(),
        )
    }

    #[test]
    fn lane_writer_acquire_is_exclusive_per_lane() {
        let state = test_state();
        let transfer_id = Uuid::new_v4();

        assert!(state.try_acquire_lane_writer(transfer_id, 0));
        assert!(!state.try_acquire_lane_writer(transfer_id, 0));

        state.release_lane_writer(transfer_id, 0);
        assert!(state.try_acquire_lane_writer(transfer_id, 0));
    }

    #[test]
    fn lane_writer_allows_different_lanes_and_transfers() {
        let state = test_state();
        let transfer_a = Uuid::new_v4();
        let transfer_b = Uuid::new_v4();

        assert!(state.try_acquire_lane_writer(transfer_a, 0));
        assert!(state.try_acquire_lane_writer(transfer_a, 1));
        assert!(state.try_acquire_lane_writer(transfer_b, 0));
    }

    #[test]
    fn per_transfer_locks_are_scoped_by_transfer_id() {
        let state = test_state();
        let transfer_a = Uuid::new_v4();
        let transfer_b = Uuid::new_v4();

        let finalize_a_1 = state.finalize_lock_for(transfer_a);
        let finalize_a_2 = state.finalize_lock_for(transfer_a);
        let finalize_b = state.finalize_lock_for(transfer_b);
        assert!(Arc::ptr_eq(&finalize_a_1, &finalize_a_2));
        assert!(!Arc::ptr_eq(&finalize_a_1, &finalize_b));

        let checkpoint_a_1 = state.checkpoint_lock_for(transfer_a);
        let checkpoint_a_2 = state.checkpoint_lock_for(transfer_a);
        let checkpoint_b = state.checkpoint_lock_for(transfer_b);
        assert!(Arc::ptr_eq(&checkpoint_a_1, &checkpoint_a_2));
        assert!(!Arc::ptr_eq(&checkpoint_a_1, &checkpoint_b));
    }
}
