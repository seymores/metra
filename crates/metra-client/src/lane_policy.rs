use std::{cmp::Ordering, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::fs;

const LANE_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadProfile {
    pub size_gib: u64,
    pub concurrency: u32,
    pub io_chunk_bytes: usize,
    pub no_disk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanePolicyEntry {
    pub profile: WorkloadProfile,
    pub recommended_lanes: u32,
    pub source: String,
    pub tuned_at: DateTime<Utc>,
    pub aggregate_p50_gbps: f64,
    pub aggregate_p95_gbps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanePolicyStore {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub entries: Vec<LanePolicyEntry>,
}

#[derive(Debug, Clone)]
pub struct LanePolicySelection {
    pub entry: LanePolicyEntry,
    pub exact_profile_match: bool,
}

fn default_schema_version() -> u32 {
    LANE_POLICY_SCHEMA_VERSION
}

impl Default for LanePolicyStore {
    fn default() -> Self {
        Self {
            schema_version: LANE_POLICY_SCHEMA_VERSION,
            updated_at: Utc::now(),
            entries: Vec::new(),
        }
    }
}

pub async fn read_lane_policy(path: &Path) -> Result<LanePolicyStore> {
    let payload = fs::read(path)
        .await
        .with_context(|| format!("failed reading lane policy {}", path.display()))?;
    serde_json::from_slice::<LanePolicyStore>(&payload)
        .with_context(|| format!("failed parsing lane policy JSON {}", path.display()))
}

pub async fn upsert_lane_policy_entry(path: &Path, entry: LanePolicyEntry) -> Result<()> {
    if entry.recommended_lanes == 0 {
        anyhow::bail!("policy entry recommended_lanes must be > 0");
    }

    let mut store = match fs::read(path).await {
        Ok(payload) => serde_json::from_slice::<LanePolicyStore>(&payload)
            .with_context(|| format!("failed parsing lane policy JSON {}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LanePolicyStore::default(),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed reading lane policy {}", path.display()));
        }
    };

    store.schema_version = LANE_POLICY_SCHEMA_VERSION;
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
            .then(left.profile.concurrency.cmp(&right.profile.concurrency))
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
        serde_json::to_vec_pretty(&store).context("failed serializing lane policy JSON")?;
    fs::write(path, payload)
        .await
        .with_context(|| format!("failed writing lane policy {}", path.display()))?;
    Ok(())
}

pub fn select_lane_policy(
    store: &LanePolicyStore,
    requested: &WorkloadProfile,
) -> Option<LanePolicySelection> {
    let best = store
        .entries
        .iter()
        .filter(|entry| entry.recommended_lanes > 0)
        .min_by(|left, right| compare_entry_match(left, right, requested))?
        .clone();

    let exact_profile_match = best.profile == *requested;
    Some(LanePolicySelection {
        entry: best,
        exact_profile_match,
    })
}

fn compare_entry_match(
    left: &LanePolicyEntry,
    right: &LanePolicyEntry,
    requested: &WorkloadProfile,
) -> Ordering {
    let left_key = match_key(left, requested);
    let right_key = match_key(right, requested);

    left_key
        .cmp(&right_key)
        .then(right.tuned_at.cmp(&left.tuned_at))
}

fn match_key(entry: &LanePolicyEntry, requested: &WorkloadProfile) -> (u8, u32, u64, usize) {
    (
        u8::from(entry.profile.no_disk != requested.no_disk),
        entry.profile.concurrency.abs_diff(requested.concurrency),
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
        concurrency: u32,
        io_chunk_bytes: usize,
        no_disk: bool,
        lanes: u32,
        tuned_at: DateTime<Utc>,
    ) -> LanePolicyEntry {
        LanePolicyEntry {
            profile: WorkloadProfile {
                size_gib,
                concurrency,
                io_chunk_bytes,
                no_disk,
            },
            recommended_lanes: lanes,
            source: "test".to_owned(),
            tuned_at,
            aggregate_p50_gbps: 0.0,
            aggregate_p95_gbps: 0.0,
        }
    }

    #[test]
    fn selects_exact_profile_match_first() {
        let now = Utc::now();
        let store = LanePolicyStore {
            schema_version: 1,
            updated_at: now,
            entries: vec![
                make_entry(2, 2, 16 * 1024 * 1024, true, 2, now),
                make_entry(4, 2, 16 * 1024 * 1024, true, 4, now),
            ],
        };
        let requested = WorkloadProfile {
            size_gib: 4,
            concurrency: 2,
            io_chunk_bytes: 16 * 1024 * 1024,
            no_disk: true,
        };

        let selected = select_lane_policy(&store, &requested).expect("expected policy selection");
        assert!(selected.exact_profile_match);
        assert_eq!(selected.entry.recommended_lanes, 4);
    }

    #[test]
    fn falls_back_to_nearest_profile() {
        let now = Utc::now();
        let store = LanePolicyStore {
            schema_version: 1,
            updated_at: now,
            entries: vec![
                make_entry(1, 2, 16 * 1024 * 1024, true, 2, now),
                make_entry(8, 4, 16 * 1024 * 1024, true, 8, now),
            ],
        };
        let requested = WorkloadProfile {
            size_gib: 6,
            concurrency: 4,
            io_chunk_bytes: 16 * 1024 * 1024,
            no_disk: true,
        };

        let selected = select_lane_policy(&store, &requested).expect("expected policy selection");
        assert!(!selected.exact_profile_match);
        assert_eq!(selected.entry.profile.size_gib, 8);
        assert_eq!(selected.entry.recommended_lanes, 8);
    }

    #[test]
    fn prefers_same_disk_mode_before_other_fallbacks() {
        let now = Utc::now();
        let store = LanePolicyStore {
            schema_version: 1,
            updated_at: now,
            entries: vec![
                make_entry(4, 2, 16 * 1024 * 1024, false, 2, now),
                make_entry(4, 2, 16 * 1024 * 1024, true, 4, now),
            ],
        };
        let requested = WorkloadProfile {
            size_gib: 4,
            concurrency: 2,
            io_chunk_bytes: 16 * 1024 * 1024,
            no_disk: false,
        };

        let selected = select_lane_policy(&store, &requested).expect("expected policy selection");
        assert_eq!(selected.entry.recommended_lanes, 2);
    }

    #[tokio::test]
    async fn upsert_replaces_matching_profile_entry() {
        let path = temp_policy_path();
        let now = Utc::now();
        let first = make_entry(4, 2, 16 * 1024 * 1024, true, 2, now - Duration::minutes(1));
        let second = make_entry(4, 2, 16 * 1024 * 1024, true, 4, now);

        upsert_lane_policy_entry(&path, first)
            .await
            .expect("first upsert should succeed");
        upsert_lane_policy_entry(&path, second)
            .await
            .expect("second upsert should succeed");

        let store = read_lane_policy(&path)
            .await
            .expect("policy should be readable");
        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].recommended_lanes, 4);

        cleanup_temp_file(&path);
    }

    fn temp_policy_path() -> PathBuf {
        let mut path = env::temp_dir();
        path.push(format!("metra-lane-policy-test-{}.json", Uuid::new_v4()));
        path
    }

    fn cleanup_temp_file(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}
