use anyhow::{Context, Result, bail};
use metra_proto::QUIC_CONTROL_FRAME_MAX_BYTES;
use serde::{Serialize, de::DeserializeOwned};

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
        bail!("invalid frame length: {frame_len}");
    }

    let mut data = vec![0u8; frame_len];
    recv_stream
        .read_exact(&mut data)
        .await
        .context("failed reading frame payload")?;
    let frame = serde_json::from_slice::<T>(&data).context("failed deserializing frame JSON")?;
    Ok(frame)
}

pub async fn write_json_frame<T>(send_stream: &mut quinn::SendStream, payload: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(payload).context("failed serializing frame JSON")?;
    if bytes.len() > QUIC_CONTROL_FRAME_MAX_BYTES {
        bail!("outbound frame too large: {}", bytes.len());
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
