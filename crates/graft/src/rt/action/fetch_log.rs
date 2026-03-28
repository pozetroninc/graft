use std::{collections::HashSet, sync::Arc};

use crate::core::{
    LogId,
    lsn::{LSN, LSNRangeExt},
};
use range_set_blaze::RangeOnce;
use tokio_stream::StreamExt;

use crate::{
    err::LogicalErr,
    local::fjall_storage::FjallStorage,
    remote::Remote,
    rt::action::{Action, Result},
};

/// Fetches new commits and metadata from a remote.
#[derive(Debug)]
pub struct FetchLog {
    pub log: LogId,
    pub max_lsn: Option<LSN>,
    /// Trust-on-first-use boundary: the first LSN on this log where leaf
    /// hashes were present. Commits at or above this LSN without leaf hashes
    /// are rejected. `None` means no boundary established yet.
    pub leaf_hash_min_lsn: Option<LSN>,
}

impl Action for FetchLog {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        let reader = storage.read();
        let mut batch = storage.batch();

        // calculate the lsn range to retrieve
        let start = reader
            .latest_lsn(&self.log)?
            .map_or(LSN::FIRST, |lsn| lsn.next());
        let end = self.max_lsn.unwrap_or(LSN::LAST);
        let lsns = start..=end;

        tracing::debug!(log = ?self.log, lsns = %lsns.to_string(), "fetching log");

        // figure out which lsns we are missing
        let existing_lsns = storage.read().lsns(&self.log, &lsns)?;
        let missing_lsns =
            (RangeOnce::new(lsns) - existing_lsns.into_ranges()).flat_map(|r| r.iter());

        let mut seen_lsns = HashSet::new();
        let mut checkpoints = HashSet::new();

        // Enforce the leaf_hash trust-on-first-use boundary. Once we've seen
        // a commit with leaf hashes on this log, all subsequent commits must
        // also have them — a commit without leaf hashes above the boundary is
        // treated as potentially tampered.
        let mut leaf_hash_min = self.leaf_hash_min_lsn;

        let mut commits = remote.stream_commits_ordered(&self.log, missing_lsns);
        while let Some(commit) = commits.try_next().await? {
            // Check trust boundary
            if let Some(min_lsn) = leaf_hash_min {
                if commit.lsn >= min_lsn && commit.leaf_hashes.is_empty() {
                    return Err(LogicalErr::MissingLeafHashes {
                        log: self.log.clone(),
                        lsn: commit.lsn,
                        min_lsn,
                    }
                    .into());
                }
            }

            // Establish boundary on first commit with leaf hashes
            if leaf_hash_min.is_none() && !commit.leaf_hashes.is_empty() {
                leaf_hash_min = Some(commit.lsn);
            }

            // In-band enforcement: if any commit on this log has set
            // leaf_hashes_required, establish the boundary permanently.
            if commit.leaf_hashes_required && leaf_hash_min.is_none() {
                leaf_hash_min = Some(commit.lsn);
            }

            seen_lsns.insert(commit.lsn);
            // keep track of checkpoints that we need to re-fetch
            checkpoints.extend(
                commit
                    .checkpoints
                    .iter()
                    .copied()
                    .filter(|lsn| !seen_lsns.contains(lsn)),
            );
            batch.write_commit(commit);
        }

        // fetch missing checkpoints
        if !checkpoints.is_empty() {
            let mut commits = remote.stream_commits_ordered(&self.log, checkpoints);
            while let Some(commit) = commits.try_next().await? {
                // Apply same trust boundary check as the main loop
                if let Some(min_lsn) = leaf_hash_min {
                    if commit.lsn >= min_lsn && commit.leaf_hashes.is_empty() {
                        return Err(LogicalErr::MissingLeafHashes {
                            log: self.log.clone(),
                            lsn: commit.lsn,
                            min_lsn,
                        }
                        .into());
                    }
                }

                // Establish boundary on first commit with leaf hashes
                if leaf_hash_min.is_none() && !commit.leaf_hashes.is_empty() {
                    leaf_hash_min = Some(commit.lsn);
                }

                // In-band enforcement
                if commit.leaf_hashes_required && leaf_hash_min.is_none() {
                    leaf_hash_min = Some(commit.lsn);
                }

                batch.write_commit(commit);
            }
        }

        Ok(batch.commit()?)
    }
}
