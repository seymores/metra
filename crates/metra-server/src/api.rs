use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as AxumPath, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::Utc;
use metra_proto::{
    CreateTransferRequest, CreateTransferResponse, ErrorResponse, HealthResponse,
    QUIC_PROTOCOL_VERSION, QuicCertificateResponse, TransferStatus, TransferSummary,
};
use tower_http::trace::TraceLayer;
use tracing::info;
use uuid::Uuid;

use crate::state::AppState;

type ApiResult<T> = std::result::Result<T, (StatusCode, Json<ErrorResponse>)>;
const CONTROL_PLANE_BODY_MAX_BYTES: usize = 64 * 1024;
const MAX_TRACKED_TRANSFERS: usize = 1_024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/quic/certificate", get(quic_certificate))
        .route("/v1/transfers", post(create_transfer))
        .route("/v1/transfers/{transfer_id}", get(get_transfer))
        .with_state(state)
        .layer(DefaultBodyLimit::max(CONTROL_PLANE_BODY_MAX_BYTES))
        .layer(TraceLayer::new_for_http())
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        quic_listener: state.quic_addr.to_string(),
        quic_profile: state.quic_profile.to_string(),
        timestamp: Utc::now(),
    })
}

async fn quic_certificate(State(state): State<AppState>) -> Json<QuicCertificateResponse> {
    Json(QuicCertificateResponse {
        server_name: state.quic_server_name.to_string(),
        quic_addr: state.quic_addr.to_string(),
        der_base64: state.quic_cert_der_b64.to_string(),
        protocol_version: QUIC_PROTOCOL_VERSION.to_owned(),
    })
}

async fn create_transfer(
    State(state): State<AppState>,
    Json(payload): Json<CreateTransferRequest>,
) -> ApiResult<(StatusCode, Json<CreateTransferResponse>)> {
    if let Err(message) = payload.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                code: "validation_error".to_owned(),
                message,
            }),
        ));
    }

    let now = Utc::now();
    let transfer_id = Uuid::new_v4();
    let summary = TransferSummary {
        transfer_id,
        tenant_id: payload.tenant_id,
        user_id: payload.user_id,
        source_uri: payload.source_uri,
        destination_uri: payload.destination_uri,
        file_name: sanitize_file_name(&payload.file_name),
        file_size_bytes: payload.file_size_bytes,
        overwrite: payload.overwrite,
        immutable_destination: payload.immutable_destination,
        status: TransferStatus::Queued,
        resume_chunk_size_bytes: payload.resume_chunk_size_bytes,
        bytes_transferred: 0,
        created_at: now,
        updated_at: now,
    };

    {
        let mut transfers = state.transfers.write().await;
        let active_transfers = transfers
            .values()
            .filter(|transfer| {
                matches!(
                    transfer.status,
                    TransferStatus::Queued | TransferStatus::Running
                )
            })
            .count();
        if active_transfers >= MAX_TRACKED_TRANSFERS {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorResponse {
                    code: "transfer_limit_reached".to_owned(),
                    message: format!(
                        "server is at capacity (max {} active transfers)",
                        MAX_TRACKED_TRANSFERS
                    ),
                }),
            ));
        }
        transfers.insert(transfer_id, summary.clone());
    }
    if let Err(err) = state.transfer_store.upsert(&summary).await {
        state.transfers.write().await.remove(&transfer_id);
        return Err(internal_error(format!(
            "failed persisting transfer {}: {}",
            transfer_id, err
        )));
    }
    info!(transfer_id = %transfer_id, "transfer accepted");

    Ok((
        StatusCode::CREATED,
        Json(CreateTransferResponse {
            transfer_id,
            status: TransferStatus::Queued,
            accepted_at: now,
            resume_chunk_size_bytes: payload.resume_chunk_size_bytes,
        }),
    ))
}

fn internal_error(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            code: "internal_error".to_owned(),
            message,
        }),
    )
}

async fn get_transfer(
    State(state): State<AppState>,
    AxumPath(transfer_id): AxumPath<Uuid>,
) -> ApiResult<Json<TransferSummary>> {
    match state.transfers.read().await.get(&transfer_id).cloned() {
        Some(transfer) => Ok(Json(transfer)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                code: "not_found".to_owned(),
                message: format!("transfer {transfer_id} was not found"),
            }),
        )),
    }
}

fn sanitize_file_name(file_name: &str) -> String {
    let mut output = String::with_capacity(file_name.len());
    for ch in file_name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "transfer.bin".to_owned()
    } else {
        output
    }
}
