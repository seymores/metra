use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use anyhow::{Context, Result};
use metra_proto::TransferSummary;
use rusqlite::Connection;

#[derive(Clone)]
pub struct TransferStore {
    path: Arc<PathBuf>,
    conn: Arc<StdMutex<Connection>>,
}

impl TransferStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed creating transfer store directory {}",
                    parent.display()
                )
            })?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed opening transfer store {}", path.display()))?;
        initialize_schema(&conn, &path)?;
        Ok(Self {
            path: Arc::new(path),
            conn: Arc::new(StdMutex::new(conn)),
        })
    }

    pub async fn load_all(&self) -> Result<Vec<TransferSummary>> {
        let conn = self.conn.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .expect("transfer store connection mutex poisoned");
            let mut stmt = conn
                .prepare("SELECT summary_json FROM transfers ORDER BY updated_at ASC")
                .with_context(|| {
                    format!(
                        "failed preparing transfer load statement for {}",
                        path.display()
                    )
                })?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .with_context(|| format!("failed querying transfers from {}", path.display()))?;

            let mut summaries = Vec::new();
            for row in rows {
                let json = row.context("failed reading transfer summary row")?;
                let summary = serde_json::from_str::<TransferSummary>(&json)
                    .context("failed deserializing transfer summary from SQLite")?;
                summaries.push(summary);
            }
            Ok::<Vec<TransferSummary>, anyhow::Error>(summaries)
        })
        .await
        .context("transfer store load task failed")?
    }

    pub async fn upsert(&self, summary: &TransferSummary) -> Result<()> {
        let conn = self.conn.clone();
        let path = self.path.clone();
        let summary = summary.clone();
        tokio::task::spawn_blocking(move || {
            let json =
                serde_json::to_string(&summary).context("failed serializing transfer summary")?;
            let conn = conn
                .lock()
                .expect("transfer store connection mutex poisoned");
            conn.execute(
                "INSERT INTO transfers (transfer_id, summary_json, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(transfer_id) DO UPDATE SET
                   summary_json = excluded.summary_json,
                   updated_at = excluded.updated_at",
                (
                    summary.transfer_id.to_string(),
                    json,
                    summary.updated_at.to_rfc3339(),
                ),
            )
            .with_context(|| format!("failed upserting transfer in {}", path.display()))?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("transfer store upsert task failed")?
    }
}

fn initialize_schema(conn: &Connection, path: &Path) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS transfers (
           transfer_id TEXT PRIMARY KEY,
           summary_json TEXT NOT NULL,
           updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_transfers_updated_at ON transfers(updated_at);",
    )
    .with_context(|| {
        format!(
            "failed initializing transfer store schema {}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use metra_proto::TransferStatus;
    use uuid::Uuid;

    fn sample_transfer() -> TransferSummary {
        let now = Utc::now();
        TransferSummary {
            transfer_id: Uuid::new_v4(),
            tenant_id: "tenant-a".to_owned(),
            user_id: "user-a".to_owned(),
            source_uri: "file:///tmp/source.bin".to_owned(),
            destination_uri: "file:///tmp/destination.bin".to_owned(),
            file_name: "sample.bin".to_owned(),
            file_size_bytes: 1024,
            overwrite: false,
            immutable_destination: false,
            status: TransferStatus::Queued,
            resume_chunk_size_bytes: metra_proto::RESUME_CHUNK_SIZE_BYTES,
            bytes_transferred: 0,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn round_trip_upsert_and_load() {
        let path = std::env::temp_dir().join(format!("metra-store-test-{}.sqlite", Uuid::new_v4()));
        let store = TransferStore::open(path.clone()).expect("open store");
        let summary = sample_transfer();
        store.upsert(&summary).await.expect("persist summary");

        let loaded = store.load_all().await.expect("load summaries");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].transfer_id, summary.transfer_id);

        let _ = std::fs::remove_file(path);
    }
}
