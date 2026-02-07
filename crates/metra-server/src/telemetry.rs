use anyhow::{Context, Result};
use opentelemetry::{global, trace::TracerProvider as _};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn init_telemetry() -> Result<()> {
    let tracer_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let tracer = tracer_provider.tracer("metra-server");
    global::set_tracer_provider(tracer_provider);

    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
    global::set_meter_provider(meter_provider);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "metra_server=info,tower_http=info,axum=info,quinn=info".into());

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .with(tracing_opentelemetry::layer().with_tracer(tracer))
        .try_init()
        .context("failed to initialize tracing")?;

    Ok(())
}
