use std::fmt::Debug;
use std::sync::Arc;

use crate::core::commit::LeafHashIndex;
use crate::core::commit::SegmentRangeRef;
use crate::core::commit_hash::compute_leaf_hash;

use crate::{
    LogicalErr,
    local::fjall_storage::FjallStorage,
    remote::{Remote, segment::segment_frame_iter},
    rt::action::{Action, Result},
};

/// Fetches one or more Segment frames and loads the pages into Storage.
#[derive(Debug)]
pub struct FetchSegment {
    pub range: SegmentRangeRef,
    pub leaf_hashes: LeafHashIndex,
}

impl Action for FetchSegment {
    async fn run(self, storage: Arc<FjallStorage>, remote: Arc<Remote>) -> Result<()> {
        let bytes = remote
            .get_segment_range(&self.range.sid, self.range.bytes)
            .await?;
        let pageidxs = self.range.pageset.iter();
        let pages = segment_frame_iter(&bytes);
        let mut batch = storage.batch();
        for (pageidx, page) in pageidxs.zip(pages) {
            if let Some(expected) = self.leaf_hashes.get(pageidx) {
                let actual = compute_leaf_hash(pageidx, &page);
                if actual != expected {
                    return Err(LogicalErr::PageIntegrity {
                        sid: self.range.sid.clone(),
                        pageidx,
                        expected,
                        actual,
                    }
                    .into());
                }
            }
            batch.write_page(self.range.sid.clone(), pageidx, page);
        }
        batch.commit()?;
        Ok(())
    }
}
