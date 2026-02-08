use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const RESUME_CHUNK_SIZE_BYTES: u64 = 1_048_576;
pub const TRANSFER_FILE_SIZE_MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024 * 1024;
pub const TRANSFER_LANES_MAX: u32 = 64;
pub const STORAGE_ID_SEGMENT_MAX_LEN: usize = 64;
pub const QUIC_CONTROL_FRAME_MAX_BYTES: usize = 64 * 1024;
pub const QUIC_PROTOCOL_VERSION: &str = "metra-quic-v1";
pub const APP_PAYLOAD_CODEC_RAW_V1: &str = "raw-v1";
pub const APP_PAYLOAD_CODEC_AEAD_V1: &str = "aead-chacha20poly1305-v1";
pub const APP_PAYLOAD_CODEC_DEFAULT: &str = APP_PAYLOAD_CODEC_AEAD_V1;
pub const APP_PAYLOAD_FRAME_HEADER_BYTES: usize = 16;
pub const APP_PAYLOAD_AEAD_TAG_BYTES: usize = 16;
pub const APP_PAYLOAD_MAX_PLAINTEXT_BYTES: usize = 64 * 1024 * 1024;
pub const APP_RECEIVE_WRITE_PIPELINE_DEPTH_DEFAULT: usize = 4;
pub const APP_RECEIVE_WRITE_PIPELINE_DEPTH_MIN: usize = 1;
pub const APP_RECEIVE_WRITE_PIPELINE_DEPTH_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadCodec {
    RawV1,
    AeadV1,
}

impl PayloadCodec {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::RawV1 => APP_PAYLOAD_CODEC_RAW_V1,
            Self::AeadV1 => APP_PAYLOAD_CODEC_AEAD_V1,
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            APP_PAYLOAD_CODEC_RAW_V1 => Some(Self::RawV1),
            APP_PAYLOAD_CODEC_AEAD_V1 => Some(Self::AeadV1),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct AeadLaneCodec {
    transfer_id: Uuid,
    lane_index: u32,
    cipher: ChaCha20Poly1305,
}

impl AeadLaneCodec {
    pub fn new(transfer_id: Uuid, lane_index: u32) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"metra-app-aead-v1");
        hasher.update(transfer_id.as_bytes());
        hasher.update(&lane_index.to_be_bytes());
        let digest = hasher.finalize();
        let key = chacha20poly1305::Key::from_slice(digest.as_bytes());
        let cipher = ChaCha20Poly1305::new(key);
        Self {
            transfer_id,
            lane_index,
            cipher,
        }
    }

    fn aad(&self) -> [u8; 20] {
        let mut aad = [0u8; 20];
        aad[..16].copy_from_slice(self.transfer_id.as_bytes());
        aad[16..20].copy_from_slice(&self.lane_index.to_be_bytes());
        aad
    }

    pub fn seal(&self, nonce_bytes: [u8; 12], plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = self.aad();
        self.cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| "app-level AEAD encryption failed".to_owned())
    }

    pub fn open(&self, nonce_bytes: [u8; 12], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad = self.aad();
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| "app-level AEAD decryption failed".to_owned())
    }
}

pub fn normalize_receive_write_pipeline_depth(depth: usize) -> usize {
    depth.clamp(
        APP_RECEIVE_WRITE_PIPELINE_DEPTH_MIN,
        APP_RECEIVE_WRITE_PIPELINE_DEPTH_MAX,
    )
}

pub fn app_payload_wire_len(codec: PayloadCodec, plaintext_len: usize) -> Result<usize, String> {
    if plaintext_len == 0 {
        return Err("plaintext frame length must be > 0".to_owned());
    }
    if plaintext_len > APP_PAYLOAD_MAX_PLAINTEXT_BYTES {
        return Err(format!(
            "plaintext frame length {} exceeds max {}",
            plaintext_len, APP_PAYLOAD_MAX_PLAINTEXT_BYTES
        ));
    }
    match codec {
        PayloadCodec::RawV1 => Ok(plaintext_len),
        PayloadCodec::AeadV1 => plaintext_len
            .checked_add(APP_PAYLOAD_AEAD_TAG_BYTES)
            .ok_or_else(|| "AEAD wire length overflow".to_owned()),
    }
}

pub fn encode_payload_frame_header(
    plaintext_len: usize,
    nonce: [u8; 12],
) -> Result<[u8; APP_PAYLOAD_FRAME_HEADER_BYTES], String> {
    if plaintext_len == 0 {
        return Err("plaintext frame length must be > 0".to_owned());
    }
    if plaintext_len > APP_PAYLOAD_MAX_PLAINTEXT_BYTES {
        return Err(format!(
            "plaintext frame length {} exceeds max {}",
            plaintext_len, APP_PAYLOAD_MAX_PLAINTEXT_BYTES
        ));
    }
    let payload_len_u32 = u32::try_from(plaintext_len)
        .map_err(|_| "frame plaintext length exceeds u32".to_owned())?;
    let mut header = [0u8; APP_PAYLOAD_FRAME_HEADER_BYTES];
    header[..4].copy_from_slice(&payload_len_u32.to_le_bytes());
    header[4..16].copy_from_slice(&nonce);
    Ok(header)
}

pub fn decode_payload_frame_header(
    header: [u8; APP_PAYLOAD_FRAME_HEADER_BYTES],
) -> Result<(usize, [u8; 12]), String> {
    let plaintext_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if plaintext_len == 0 {
        return Err("payload frame plaintext length must be > 0".to_owned());
    }
    if plaintext_len > APP_PAYLOAD_MAX_PLAINTEXT_BYTES {
        return Err(format!(
            "payload frame plaintext length {} exceeds max {}",
            plaintext_len, APP_PAYLOAD_MAX_PLAINTEXT_BYTES
        ));
    }
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&header[4..16]);
    Ok((plaintext_len, nonce))
}

pub fn encode_payload_frame(
    codec: PayloadCodec,
    plaintext: &[u8],
    nonce: [u8; 12],
    aead_codec: Option<&AeadLaneCodec>,
) -> Result<Vec<u8>, String> {
    let wire_payload = match codec {
        PayloadCodec::RawV1 => plaintext.to_vec(),
        PayloadCodec::AeadV1 => {
            let codec = aead_codec.ok_or_else(|| "missing AEAD codec".to_owned())?;
            codec.seal(nonce, plaintext)?
        }
    };
    let header = encode_payload_frame_header(plaintext.len(), nonce)?;
    let mut out = Vec::with_capacity(APP_PAYLOAD_FRAME_HEADER_BYTES + wire_payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&wire_payload);
    Ok(out)
}

pub fn decode_payload_frame(
    codec: PayloadCodec,
    plaintext_len: usize,
    nonce: [u8; 12],
    wire_payload: &[u8],
    aead_codec: Option<&AeadLaneCodec>,
) -> Result<Vec<u8>, String> {
    let expected_wire_len = app_payload_wire_len(codec, plaintext_len)?;
    if wire_payload.len() != expected_wire_len {
        return Err(format!(
            "wire payload length mismatch: got {} expected {}",
            wire_payload.len(),
            expected_wire_len
        ));
    }
    let plaintext = match codec {
        PayloadCodec::RawV1 => wire_payload.to_vec(),
        PayloadCodec::AeadV1 => {
            let codec = aead_codec.ok_or_else(|| "missing AEAD codec".to_owned())?;
            codec.open(nonce, wire_payload)?
        }
    };
    if plaintext.len() != plaintext_len {
        return Err(format!(
            "decoded plaintext length mismatch: got {} expected {}",
            plaintext.len(),
            plaintext_len
        ));
    }
    Ok(plaintext)
}

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
    pub quic_profile: String,
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
    pub payload_codec: String,
    pub receive_write_pipeline_depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuicTransferOpenAck {
    pub ok: bool,
    pub resume_offset_bytes: u64,
    pub message: String,
    pub payload_codec: String,
    pub receive_write_pipeline_depth: usize,
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
        if !is_valid_storage_id_segment(&self.tenant_id) {
            return Err(format!(
                "tenant_id must match [a-zA-Z0-9_-] and be 1..={} chars",
                STORAGE_ID_SEGMENT_MAX_LEN
            ));
        }
        if !is_valid_storage_id_segment(&self.user_id) {
            return Err(format!(
                "user_id must match [a-zA-Z0-9_-] and be 1..={} chars",
                STORAGE_ID_SEGMENT_MAX_LEN
            ));
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
        if self.file_size_bytes > TRANSFER_FILE_SIZE_MAX_BYTES {
            return Err(format!(
                "file_size_bytes must be <= {}",
                TRANSFER_FILE_SIZE_MAX_BYTES
            ));
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

pub fn is_valid_storage_id_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= STORAGE_ID_SEGMENT_MAX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> CreateTransferRequest {
        CreateTransferRequest {
            tenant_id: "tenant_01".to_owned(),
            user_id: "user-01".to_owned(),
            source_uri: "file:///tmp/source.bin".to_owned(),
            destination_uri: "file:///tmp/dest.bin".to_owned(),
            file_name: "file.bin".to_owned(),
            file_size_bytes: 1_024,
            resume_chunk_size_bytes: RESUME_CHUNK_SIZE_BYTES,
            overwrite: false,
            immutable_destination: false,
        }
    }

    #[test]
    fn storage_id_segment_accepts_safe_charset() {
        assert!(is_valid_storage_id_segment("tenant-01_user"));
    }

    #[test]
    fn storage_id_segment_rejects_path_escape_chars() {
        assert!(!is_valid_storage_id_segment("../etc"));
        assert!(!is_valid_storage_id_segment("tenant/user"));
    }

    #[test]
    fn create_transfer_rejects_oversized_file() {
        let mut req = base_request();
        req.file_size_bytes = TRANSFER_FILE_SIZE_MAX_BYTES + 1;
        assert!(req.validate().is_err());
    }
}
