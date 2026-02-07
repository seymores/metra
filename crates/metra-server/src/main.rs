mod api;
mod args;
mod quic;
mod quic_metrics;
mod signal;
mod state;
mod telemetry;
mod wire;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use tokio::fs;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::{args::Args, state::AppState};

#[tokio::main]
async fn main() -> Result<()> {
    telemetry::install_crypto_provider();
    telemetry::init_telemetry()?;

    let args = Args::parse();
    fs::create_dir_all(&args.data_dir).await.with_context(|| {
        format!(
            "failed to create data directory {}",
            args.data_dir.display()
        )
    })?;

    let (endpoint, cert_der) = quic::build_quic_endpoint(args.quic_addr, &args.quic_server_name)?;
    let app_state = AppState::new(
        args.quic_addr,
        args.data_dir.clone(),
        args.quic_server_name.clone(),
        BASE64.encode(cert_der),
    );

    let shutdown = CancellationToken::new();
    let quic_shutdown = shutdown.child_token();
    let quic_state = app_state.clone();
    let quic_task = tokio::spawn(async move {
        if let Err(err) = quic::run_quic_listener(endpoint, quic_state, quic_shutdown).await {
            error!(error = %err, "quic listener exited with error");
        }
    });

    let app = api::router(app_state);
    let listener = tokio::net::TcpListener::bind(args.rest_addr)
        .await
        .with_context(|| format!("failed to bind REST listener {}", args.rest_addr))?;
    info!(
        rest_addr = %args.rest_addr,
        quic_addr = %args.quic_addr,
        data_dir = %args.data_dir.display(),
        "metra server started"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(signal::shutdown_signal(shutdown.clone()))
        .await
        .context("REST server exited unexpectedly")?;

    shutdown.cancel();
    if let Err(join_err) = quic_task.await {
        error!(error = %join_err, "quic task join error");
    }

    Ok(())
}
