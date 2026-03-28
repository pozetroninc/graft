use crate::core::{LogId, PageIdx, SegmentId, VolumeId};
use crate::core::lsn::LSN;
use crate::{local::fjall_storage::FjallStorageErr, remote::RemoteErr};

#[derive(Debug, thiserror::Error)]
pub enum GraftErr {
    #[error(transparent)]
    Storage(FjallStorageErr),

    #[error(transparent)]
    Remote(#[from] RemoteErr),

    #[error(transparent)]
    Logical(#[from] LogicalErr),
}

impl From<FjallStorageErr> for GraftErr {
    fn from(value: FjallStorageErr) -> Self {
        match value {
            FjallStorageErr::LogicalErr(verr) => GraftErr::Logical(verr),
            other => GraftErr::Storage(other),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LogicalErr {
    #[error("Concurrent write to Volume {0}")]
    VolumeConcurrentWrite(VolumeId),

    #[error("Volume {0} not found")]
    VolumeNotFound(VolumeId),

    #[error("Volume {0} has a pending commit and needs recovery")]
    VolumeNeedsRecovery(VolumeId),

    #[error("Volume {0} has diverged from the remote")]
    VolumeDiverged(VolumeId),

    #[error(
        "Volume `{vid}` has a different remote Log than expected; expected={expected}, actual={actual}"
    )]
    VolumeRemoteMismatch {
        vid: VolumeId,
        expected: LogId,
        actual: LogId,
    },

    #[error(
        "Page integrity check failed for segment {sid} page {pageidx}: expected {expected:02x?}, got {actual:02x?}"
    )]
    PageIntegrity {
        sid: SegmentId,
        pageidx: PageIdx,
        expected: [u8; 32],
        actual: [u8; 32],
    },

    #[error(
        "Missing leaf hashes on log {log} at LSN {lsn}; leaf hashes required since LSN {min_lsn}"
    )]
    MissingLeafHashes {
        log: LogId,
        lsn: LSN,
        min_lsn: LSN,
    },
}
