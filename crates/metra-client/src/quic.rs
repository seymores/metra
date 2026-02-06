use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use metra_proto::{QUIC_CONTROL_FRAME_MAX_BYTES, QUIC_PROTOCOL_VERSION, QuicCertificateResponse};
use quinn::crypto::rustls::QuicClientConfig;
use serde::{Serialize, de::DeserializeOwned};

pub async fn connect_quic(
    cert: &QuicCertificateResponse,
    quic_addr: SocketAddr,
) -> Result<(quinn::Endpoint, quinn::Connection)> {
    let cert_der = BASE64
        .decode(&cert.der_base64)
        .context("failed decoding server certificate")?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(cert_der))
        .context("failed adding server certificate to root store")?;

    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![QUIC_PROTOCOL_VERSION.as_bytes().to_vec()];

    let client_crypto =
        QuicClientConfig::try_from(tls).context("failed building QUIC TLS config")?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
    transport.send_window(2 * 1024 * 1024 * 1024);
    client_config.transport_config(Arc::new(transport));

    let bind_addr = if quic_addr.is_ipv4() {
        "0.0.0.0:0".parse()?
    } else {
        "[::]:0".parse()?
    };
    let mut endpoint =
        quinn::Endpoint::client(bind_addr).context("failed creating QUIC endpoint")?;
    endpoint.set_default_client_config(client_config);

    let connection = endpoint
        .connect(quic_addr, &cert.server_name)
        .context("failed to begin QUIC connect")?
        .await
        .context("quic handshake failed")?;
    Ok((endpoint, connection))
}

pub async fn read_json_frame<T>(recv_stream: &mut quinn::RecvStream) -> Result<T>
where
    T: DeserializeOwned,
{
    let mut frame_len = [0u8; 4];
    recv_stream
        .read_exact(&mut frame_len)
        .await
        .context("failed reading frame length")?;
    let frame_len = u32::from_be_bytes(frame_len) as usize;
    if frame_len == 0 || frame_len > QUIC_CONTROL_FRAME_MAX_BYTES {
        anyhow::bail!("invalid frame length: {frame_len}");
    }

    let mut data = vec![0u8; frame_len];
    recv_stream
        .read_exact(&mut data)
        .await
        .context("failed reading frame payload")?;
    serde_json::from_slice::<T>(&data).context("failed deserializing frame JSON")
}

pub async fn write_json_frame<T>(send_stream: &mut quinn::SendStream, payload: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(payload).context("failed serializing frame JSON")?;
    if bytes.len() > QUIC_CONTROL_FRAME_MAX_BYTES {
        anyhow::bail!("outbound frame too large: {}", bytes.len());
    }
    send_stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .context("failed writing frame length")?;
    send_stream
        .write_all(&bytes)
        .await
        .context("failed writing frame payload")?;
    Ok(())
}
