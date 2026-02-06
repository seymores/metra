use anyhow::{Context, Result};
use metra_proto::{
    CreateTransferRequest, CreateTransferResponse, HealthResponse, QuicCertificateResponse,
    TransferSummary,
};
use reqwest::Client;
use uuid::Uuid;

pub async fn fetch_health(http: &Client, server: &str) -> Result<HealthResponse> {
    let response = http
        .get(format!("{server}/health"))
        .send()
        .await
        .context("health request failed")?
        .error_for_status()
        .context("health endpoint returned non-success response")?;
    response
        .json::<HealthResponse>()
        .await
        .context("failed parsing health response")
}

pub async fn create_transfer(
    http: &Client,
    server: &str,
    request: &CreateTransferRequest,
) -> Result<CreateTransferResponse> {
    let response = http
        .post(format!("{server}/v1/transfers"))
        .json(request)
        .send()
        .await
        .context("create transfer request failed")?
        .error_for_status()
        .context("create transfer returned non-success response")?;
    response
        .json::<CreateTransferResponse>()
        .await
        .context("failed parsing create transfer response")
}

pub async fn fetch_transfer_status(
    http: &Client,
    server: &str,
    transfer_id: Uuid,
) -> Result<TransferSummary> {
    let response = http
        .get(format!("{server}/v1/transfers/{transfer_id}"))
        .send()
        .await
        .context("transfer status request failed")?
        .error_for_status()
        .context("transfer status returned non-success response")?;
    response
        .json::<TransferSummary>()
        .await
        .context("failed parsing transfer status response")
}

pub async fn fetch_quic_certificate(
    http: &Client,
    server: &str,
) -> Result<QuicCertificateResponse> {
    let response = http
        .get(format!("{server}/v1/quic/certificate"))
        .send()
        .await
        .context("quic certificate request failed")?
        .error_for_status()
        .context("quic certificate endpoint returned non-success response")?;
    response
        .json::<QuicCertificateResponse>()
        .await
        .context("failed parsing quic certificate response")
}
