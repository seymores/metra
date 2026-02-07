use std::{cmp::Ordering, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;

const RUNTIME_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeWorkloadProfile {
    pub size_gib: u64,
    pub lanes: u32,
    pub io_chunk_bytes: usize,
    pub no_disk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicyEntry {
    pub profile: RuntimeWorkloadProfile,
    pub recommended_profile: String,
    pub effective_io_chunk_bytes: usize,
    pub file_read_pipeline_depth: usize,
    pub source: String,
    pub tuned_at: DateTime<Utc>,
    pub throughput_p50_gbps: f64,
    pub throughput_p95_gbps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePolicyStore {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub entries: Vec<RuntimePolicyEntry>,
}

#[derive(Debug, Clone)]
pub struct RuntimePolicySelection {
    pub entry: RuntimePolicyEntry,
    pub exact_profile_match: bool,
}

fn default_schema_version() -> u32 {
    RUNTIME_POLICY_SCHEMA_VERSION
}

impl Default for RuntimePolicyStore {
    fn default() -> Self {
        Self {
            schema_version: RUNTIME_POLICY_SCHEMA_VERSION,
            updated_at: Utc::now(),
            entries: Vec::new(),
        }
    }
}

pub async fn read_runtime_policy(path: &Path) -> Result<RuntimePolicyStore> {
    let payload = fs::read(path)
        .await
        .with_context(|| format!("failed reading runtime policy {}", path.display()))?;
    serde_json::from_slice::<RuntimePolicyStore>(&payload)
        .with_context(|| format!("failed parsing runtime policy JSON {}", path.display()))
}

pub async fn upsert_runtime_policy_entry(path: &Path, entry: RuntimePolicyEntry) -> Result<()> {
    if entry.recommended_profile.trim().is_empty() {
        anyhow::bail!("runtime policy entry recommended_profile must not be empty");
    }
    if entry.effective_io_chunk_bytes == 0 {
        anyhow::bail!("runtime policy entry effective_io_chunk_bytes must be > 0");
    }
    if entry.file_read_pipeline_depth == 0 {
        anyhow::bail!("runtime policy entry file_read_pipeline_depth must be > 0");
    }

    let mut store = match fs::read(path).await {
        Ok(payload) => serde_json::from_slice::<RuntimePolicyStore>(&payload)
            .with_context(|| format!("failed parsing runtime policy JSON {}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => RuntimePolicyStore::default(),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed reading runtime policy {}", path.display()));
        }
    };

    store.schema_version = RUNTIME_POLICY_SCHEMA_VERSION;
    store.updated_at = Utc::now();

    if let Some(existing) = store
        .entries
        .iter_mut()
        .find(|existing| existing.profile == entry.profile)
    {
        *existing = entry;
    } else {
        store.entries.push(entry);
    }

    store.entries.sort_by(|left, right| {
        left.profile
            .no_disk
            .cmp(&right.profile.no_disk)
            .then(left.profile.lanes.cmp(&right.profile.lanes))
            .then(left.profile.size_gib.cmp(&right.profile.size_gib))
            .then(
                left.profile
                    .io_chunk_bytes
                    .cmp(&right.profile.io_chunk_bytes),
            )
            .then(right.tuned_at.cmp(&left.tuned_at))
    });

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }

    let payload =
        serde_json::to_vec_pretty(&store).context("failed serializing runtime policy JSON")?;
    fs::write(path, payload)
        .await
        .with_context(|| format!("failed writing runtime policy {}", path.display()))?;
    Ok(())
}

pub fn select_runtime_policy(
    store: &RuntimePolicyStore,
    requested: &RuntimeWorkloadProfile,
) -> Option<RuntimePolicySelection> {
    let best = store
        .entries
        .iter()
        .filter(|entry| {
            !entry.recommended_profile.trim().is_empty()
                && entry.effective_io_chunk_bytes > 0
                && entry.file_read_pipeline_depth > 0
        })
        .min_by(|left, right| compare_entry_match(left, right, requested))?
        .clone();

    let exact_profile_match = best.profile == *requested;
    Some(RuntimePolicySelection {
        entry: best,
        exact_profile_match,
    })
}

fn compare_entry_match(
    left: &RuntimePolicyEntry,
    right: &RuntimePolicyEntry,
    requested: &RuntimeWorkloadProfile,
) -> Ordering {
    let left_key = match_key(left, requested);
    let right_key = match_key(right, requested);

    left_key
        .cmp(&right_key)
        .then(right.tuned_at.cmp(&left.tuned_at))
}

fn match_key(
    entry: &RuntimePolicyEntry,
    requested: &RuntimeWorkloadProfile,
) -> (u8, u32, u64, usize) {
    (
        u8::from(entry.profile.no_disk != requested.no_disk),
        entry.profile.lanes.abs_diff(requested.lanes),
        entry.profile.size_gib.abs_diff(requested.size_gib),
        entry
            .profile
            .io_chunk_bytes
            .abs_diff(requested.io_chunk_bytes),
    )
}

#[cfg(test)]
mod tests {
    use std::{env, path::PathBuf};

    use chrono::Duration;
    use uuid::Uuid;

    use super::*;

    fn make_entry(
        size_gib: u64,
        lanes: u32,
        io_chunk_bytes: usize,
        no_disk: bool,
        recommended_profile: &str,
        tuned_at: DateTime<Utc>,
    ) -> RuntimePolicyEntry {
        RuntimePolicyEntry {
            profile: RuntimeWorkloadProfile {
                size_gib,
                lanes,
                io_chunk_bytes,
                no_disk,
            },
            recommended_profile: recommended_profile.to_owned(),
            effective_io_chunk_bytes: 16 * 1024 * 1024,
            file_read_pipeline_depth: 8,
            source: "test".to_owned(),
            tuned_at,
            throughput_p50_gbps: 1.0,
            throughput_p95_gbps: 1.1,
        }
    }

    #[test]
    fn selects_exact_profile_match_first() {
        let now = Utc::now();
        let store = RuntimePolicyStore {
            schema_version: 1,
            updated_at: now,
            entries: vec![
                make_entry(2, 2, 16 * 1024 * 1024, true, "balanced", now),
                make_entry(4, 2, 16 * 1024 * 1024, true, "throughput", now),
            ],
        };
        let requested = RuntimeWorkloadProfile {
            size_gib: 4,
            lanes: 2,
            io_chunk_bytes: 16 * 1024 * 1024,
            no_disk: true,
        };

        let selected =
            select_runtime_policy(&store, &requested).expect("expected runtime selection");
        assert!(selected.exact_profile_match);
        assert_eq!(selected.entry.recommended_profile, "throughput");
    }

    #[test]
    fn falls_back_to_nearest_profile() {
        let now = Utc::now();
        let store = RuntimePolicyStore {
            schema_version: 1,
            updated_at: now,
            entries: vec![
                make_entry(1, 2, 16 * 1024 * 1024, true, "balanced", now),
                make_entry(8, 4, 16 * 1024 * 1024, true, "low-cpu", now),
            ],
        };
        let requested = RuntimeWorkloadProfile {
            size_gib: 6,
            lanes: 4,
            io_chunk_bytes: 16 * 1024 * 1024,
            no_disk: true,
        };

        let selected =
            select_runtime_policy(&store, &requested).expect("expected runtime selection");
        assert!(!selected.exact_profile_match);
        assert_eq!(selected.entry.profile.size_gib, 8);
        assert_eq!(selected.entry.recommended_profile, "low-cpu");
    }

    #[tokio::test]
    async fn upsert_replaces_matching_profile_entry() {
        let path = temp_policy_path();
        let now = Utc::now();
        let first = make_entry(
            4,
            2,
            16 * 1024 * 1024,
            true,
            "balanced",
            now - Duration::minutes(1),
        );
        let second = make_entry(4, 2, 16 * 1024 * 1024, true, "throughput", now);

        upsert_runtime_policy_entry(&path, first)
            .await
            .expect("first upsert should succeed");
        upsert_runtime_policy_entry(&path, second)
            .await
            .expect("second upsert should succeed");

        let store = read_runtime_policy(&path)
            .await
            .expect("policy should be readable");
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].recommended_profile, "throughput");

        cleanup_temp_file(&path);
    }

    fn temp_policy_path() -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!("metra-runtime-policy-test-{}.json", Uuid::new_v4()));
        path
    }

    fn cleanup_temp_file(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}
