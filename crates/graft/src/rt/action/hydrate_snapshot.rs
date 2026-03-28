use std::collections::HashMap;
use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use itertools::Itertools;
use tryiter::TryIteratorExt;

use crate::{
    GraftErr,
    core::{SegmentId, commit::LeafHashIndex},
    local::fjall_storage::FjallStorage,
    remote::Remote,
    rt::action::{Action, fetch_segment::FetchSegment},
    snapshot::Snapshot,
};

const HYDRATE_CONCURRENCY: usize = 5;

/// Downloads all missing pages for a Snapshot.
#[derive(Debug)]
pub struct HydrateSnapshot {
    pub snapshot: Snapshot,
}

impl Action for HydrateSnapshot {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<(), GraftErr> {
        let reader = storage.read();
        let missing_frames = reader.find_missing_frames(&self.snapshot)?;

        // Build a map from SegmentId -> LeafHashIndex by iterating commits
        // in the snapshot so we can pass leaf hashes to each FetchSegment.
        let mut leaf_hash_map: HashMap<SegmentId, LeafHashIndex> = HashMap::new();
        let mut commits = reader.commits(&self.snapshot);
        while let Some(commit) = commits.try_next()? {
            if !commit.leaf_hashes.is_empty() {
                if let Some(idx) = &commit.segment_idx {
                    leaf_hash_map.insert(idx.sid.clone(), commit.leaf_hashes.clone());
                }
            }
        }

        futures::stream::iter(
            missing_frames
                .into_iter()
                // coalesce adjacent frames to minimize requests
                .coalesce(|a, b| a.coalesce(b)),
        )
        .map(Ok)
        .try_for_each_concurrent(HYDRATE_CONCURRENCY, |range| {
            let leaf_hashes = leaf_hash_map
                .get(&range.sid)
                .cloned()
                .unwrap_or_default();
            FetchSegment { range, leaf_hashes }.run(storage.clone(), remote.clone())
        })
        .await
    }
}
