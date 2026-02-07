use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const RESUME_CHUNK_SIZE_BYTES: u64 = 1_048_576;
pub const QUIC_CONTROL_FRAME_MAX_BYTES: usize = 64 * 1024;
pub const QUIC_PROTOCOL_VERSION: &str = "metra-quic-v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub quic_listener: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTransferRequest {
    pub tenant_id: String,
    pub user_id: String,
    pub source_uri: String,
    pub destination_uri: String,
    pub file_name: String,
    pub file_size_bytes: u64,
    #[serde(default = "default_resume_chunk_size")]
    pub resume_chunk_size_bytes: u64,
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub immutable_destination: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTransferResponse {
    pub transfer_id: Uuid,
    pub status: TransferStatus,
    pub accepted_at: DateTime<Utc>,
    pub resume_chunk_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSummary {
    pub transfer_id: Uuid,
    pub tenant_id: String,
    pub user_id: String,
    pub source_uri: String,
    pub destination_uri: String,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub overwrite: bool,
    pub immutable_destination: bool,
    pub status: TransferStatus,
    pub resume_chunk_size_bytes: u64,
    pub bytes_transferred: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuicCertificateResponse {
    pub server_name: String,
    pub quic_addr: String,
    pub der_base64: String,
    pub protocol_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuicTransferOpen {
    pub transfer_id: Uuid,
    pub file_size_bytes: u64,
    pub file_name: String,
    pub resume_chunk_size_bytes: u64,
    #[serde(default)]
    pub lane_index: u32,
    #[serde(default = "default_total_lanes")]
    pub total_lanes: u32,
    #[serde(default)]
    pub range_start: u64,
    #[serde(default)]
    pub range_end_exclusive: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuicTransferOpenAck {
    pub ok: bool,
    pub resume_offset_bytes: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuicTransferCompleteAck {
    pub ok: bool,
    pub status: TransferStatus,
    pub bytes_received: u64,
    pub message: String,
    pub updated_at: DateTime<Utc>,
}

impl CreateTransferRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.tenant_id.trim().is_empty() {
            return Err("tenant_id must not be empty".to_owned());
        }
        if self.user_id.trim().is_empty() {
            return Err("user_id must not be empty".to_owned());
        }
        if self.source_uri.trim().is_empty() {
            return Err("source_uri must not be empty".to_owned());
        }
        if self.destination_uri.trim().is_empty() {
            return Err("destination_uri must not be empty".to_owned());
        }
        if self.file_name.trim().is_empty() {
            return Err("file_name must not be empty".to_owned());
        }
        if self.file_size_bytes == 0 {
            return Err("file_size_bytes must be > 0".to_owned());
        }
        if self.resume_chunk_size_bytes != RESUME_CHUNK_SIZE_BYTES {
            return Err(format!(
                "resume_chunk_size_bytes must be exactly {} bytes (1 MiB) in v1",
                RESUME_CHUNK_SIZE_BYTES
            ));
        }
        Ok(())
    }
}

fn default_resume_chunk_size() -> u64 {
    RESUME_CHUNK_SIZE_BYTES
}

fn default_total_lanes() -> u32 {
    1
}
