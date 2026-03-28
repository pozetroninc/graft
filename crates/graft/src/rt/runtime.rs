use std::{sync::Arc, time::Duration};

use crate::core::{
    LogId, PageCount, PageIdx, VolumeId, checksum::Checksum, commit::Commit,
    commit_hash::{CommitHashBuilder, compute_leaf_hash}, logref::LogRef, lsn::LSN, page::Page,
    pageset::PageSet,
};
use bytestring::ByteString;
use tracing::Instrument;
use tryiter::TryIteratorExt;

use crate::{
    GraftErr, LogicalErr,
    remote::Remote,
    rt::{
        action::{Action, FetchLog, FetchSegment, HydrateSnapshot, RemoteCommit},
        task::{autosync::AutosyncTask, supervise},
    },
    snapshot::Snapshot,
    volume::{Volume, VolumeStatus},
    volume_reader::VolumeReader,
    volume_writer::VolumeWriter,
};

use crate::local::fjall_storage::FjallStorage;

type Result<T> = std::result::Result<T, GraftErr>;

#[derive(Clone, Debug)]
pub struct Runtime {
    inner: Arc<RuntimeInner>,
}

#[derive(Debug)]
struct RuntimeInner {
    tokio: tokio::runtime::Handle,
    storage: Arc<FjallStorage>,
    remote: Arc<Remote>,
    require_leaf_hashes: bool,
}

impl Runtime {
    /// Create a Graft `Runtime` wrapping the provided Tokio runtime handle.
    pub fn new(
        tokio_rt: tokio::runtime::Handle,
        remote: Arc<Remote>,
        storage: Arc<FjallStorage>,
        autosync: Option<Duration>,
        require_leaf_hashes: bool,
    ) -> Runtime {
        // spin up background tasks as needed
        if let Some(interval) = autosync {
            let _guard = tokio_rt.enter();
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tokio_rt.spawn(supervise(
                storage.clone(),
                remote.clone(),
                AutosyncTask::new(ticker, require_leaf_hashes),
            ));
        }
        Runtime {
            inner: Arc::new(RuntimeInner { tokio: tokio_rt, storage, remote, require_leaf_hashes }),
        }
    }

    pub(crate) fn storage(&self) -> &FjallStorage {
        &self.inner.storage
    }

    /// Test-only: expose storage for page corruption tests.
    #[cfg(feature = "testutil")]
    pub fn storage_for_test(&self) -> &FjallStorage {
        &self.inner.storage
    }

    pub(crate) fn read_page(&self, snapshot: &Snapshot, pageidx: PageIdx) -> Result<Page> {
        let reader = self.storage().read();
        if let Some(commit) = reader.search_page(snapshot, pageidx)? {
            let idx = commit
                .segment_idx()
                .expect("BUG: commit claims to contain pageidx");

            if let Some(page) = reader.read_page(idx.sid().clone(), pageidx)? {
                if self.inner.require_leaf_hashes && commit.leaf_hashes.is_empty() {
                    return Err(LogicalErr::MissingLeafHashes {
                        log: commit.log.clone(),
                        lsn: commit.lsn,
                        min_lsn: LSN::FIRST,
                    }
                    .into());
                }
                if !commit.leaf_hashes.is_empty() {
                    let expected =
                        commit.leaf_hashes.get(pageidx).ok_or_else(|| LogicalErr::MissingLeafHash {
                            sid: idx.sid().clone(),
                            pageidx,
                        })?;
                    let actual = compute_leaf_hash(pageidx, &page);
                    if actual != expected {
                        return Err(LogicalErr::PageIntegrity {
                            sid: idx.sid().clone(),
                            pageidx,
                            expected,
                            actual,
                        }
                        .into());
                    }
                }
                return Ok(page);
            }

            // fallthrough to loading the page from the remote
            let range = idx
                .frame_for_pageidx(pageidx)
                .expect("BUG: no frame for pageidx");

            // fetch the segment frame containing the page
            self.run_action(FetchSegment {
                range,
                leaf_hashes: commit.leaf_hashes.clone(),
            })?;

            // now that we've fetched the segment, read the page again using a
            // fresh storage reader
            Ok(self
                .storage()
                .read()
                .read_page(idx.sid.clone(), pageidx)?
                .expect("BUG: page not found after fetching"))
        } else {
            Ok(Page::EMPTY)
        }
    }

    fn run_action<A: Action>(&self, action: A) -> Result<()> {
        let span = tracing::debug_span!("Action::run", ?action);

        self.inner.tokio.block_on(
            action
                .run(self.inner.storage.clone(), self.inner.remote.clone())
                .instrument(span),
        )
    }
}

// tag methods
impl Runtime {
    pub fn tag_iter(&self) -> impl Iterator<Item = Result<(ByteString, VolumeId)>> {
        self.storage().read().iter_tags().map_err(GraftErr::from)
    }

    pub fn tag_exists(&self, name: &str) -> Result<bool> {
        Ok(self.storage().read().tag_exists(name)?)
    }

    pub fn tag_get(&self, tag: &str) -> Result<Option<VolumeId>> {
        Ok(self.storage().read().get_tag(tag)?)
    }

    /// retrieves the `VolumeId` for a tag, replacing it with the provided `VolumeId`
    pub fn tag_replace(&self, tag: &str, vid: VolumeId) -> Result<Option<VolumeId>> {
        Ok(self.storage().read_write().tag_replace(tag, vid)?)
    }

    pub fn tag_delete(&self, tag: &str) -> Result<()> {
        Ok(self.storage().tag_delete(tag)?)
    }
}

// volume methods
impl Runtime {
    pub fn volume_iter(&self) -> impl Iterator<Item = Result<Volume>> {
        self.storage().read().iter_volumes().map_err(GraftErr::from)
    }

    pub fn volume_exists(&self, vid: &VolumeId) -> Result<bool> {
        Ok(self.storage().read().volume_exists(vid)?)
    }

    /// opens a volume. if any id is missing, it will be randomly
    /// generated. If the volume already exists, this function will fail if its
    /// remote Log doesn't match.
    pub fn volume_open(
        &self,
        vid: Option<VolumeId>,
        local: Option<LogId>,
        remote: Option<LogId>,
    ) -> Result<Volume> {
        Ok(self
            .storage()
            .read_write()
            .volume_open(vid, local, remote)?)
    }

    /// creates a new volume by forking an existing logref
    pub fn volume_from_logref(&self, logref: LogRef) -> Result<Option<Volume>> {
        Ok(self.storage().volume_from_logref(logref)?)
    }

    /// creates a new volume by forking an existing snapshot
    pub fn volume_from_snapshot(&self, snapshot: &Snapshot) -> Result<Volume> {
        Ok(self.storage().volume_from_snapshot(snapshot)?)
    }

    /// retrieves an existing volume. returns `LogicalErr::VolumeNotFound` if missing
    pub fn volume_get(&self, vid: &VolumeId) -> Result<Volume> {
        Ok(self.storage().read().volume(vid)?)
    }

    /// removes a volume but leaves the underlying logs in place
    pub fn volume_delete(&self, vid: &VolumeId) -> Result<()> {
        Ok(self.storage().volume_delete(vid)?)
    }

    /// fetches the latest changes to the remote and then pulls them into the volume
    pub fn volume_pull(&self, vid: VolumeId) -> Result<()> {
        let volume = self.inner.storage.read().volume(&vid)?;
        self.fetch_log(volume.remote.clone(), None, volume.leaf_hash_min_lsn)?;

        // Persist the trust-on-first-use boundary if not already set.
        // Scan the remote log for the first commit with leaf hashes.
        if volume.leaf_hash_min_lsn.is_none() {
            let reader = self.storage().read();
            if let Some(first_lsn) = reader.first_lsn_with_leaf_hashes(&volume.remote)? {
                self.storage()
                    .read_write()
                    .set_leaf_hash_min_lsn(&vid, first_lsn)?;
            }
        }

        if volume.pending_commit.is_some() {
            self.storage().read_write().recover_pending_commit(&vid)?;
        }
        Ok(self
            .storage()
            .read_write()
            .sync_remote_to_local(volume.vid)?)
    }

    pub fn volume_push(&self, vid: VolumeId) -> Result<()> {
        self.run_action(RemoteCommit { vid })
    }

    pub fn volume_status(&self, vid: &VolumeId) -> Result<VolumeStatus> {
        let reader = self.storage().read();
        let volume = reader.volume(vid)?;
        let latest_local = reader.latest_lsn(&volume.local)?;
        let latest_remote = reader.latest_lsn(&volume.remote)?;
        Ok(volume.status(latest_local, latest_remote))
    }

    pub fn volume_snapshot(&self, vid: &VolumeId) -> Result<Snapshot> {
        Ok(self.storage().read().snapshot(vid)?)
    }

    pub fn volume_reader(&self, vid: VolumeId) -> Result<VolumeReader> {
        let snapshot = self.volume_snapshot(&vid)?;
        Ok(VolumeReader::new(self.clone(), vid, snapshot))
    }

    pub fn volume_writer(&self, vid: VolumeId) -> Result<VolumeWriter> {
        let snapshot = self.volume_snapshot(&vid)?;
        Ok(VolumeWriter::new(self.clone(), vid, snapshot))
    }
}

// log methods
impl Runtime {
    pub fn fetch_log(
        &self,
        log: LogId,
        max_lsn: Option<LSN>,
        leaf_hash_min_lsn: Option<LSN>,
    ) -> Result<()> {
        let effective_min = if self.inner.require_leaf_hashes {
            Some(LSN::FIRST)
        } else {
            leaf_hash_min_lsn
        };
        self.run_action(FetchLog {
            log,
            max_lsn,
            leaf_hash_min_lsn: effective_min,
        })
    }

    pub fn get_commit(&self, log: &LogId, lsn: LSN) -> Result<Option<Commit>> {
        Ok(self.storage().read().get_commit(log, lsn)?)
    }
}

// snapshot methods
impl Runtime {
    /// returns the total number of pages in the snapshot
    pub fn snapshot_pages(&self, snapshot: &Snapshot) -> Result<PageCount> {
        if let Some((log, lsn)) = snapshot.head() {
            Ok(self
                .storage()
                .read()
                .page_count(log, lsn)?
                .expect("BUG: missing head commit for snapshot"))
        } else {
            Ok(PageCount::ZERO)
        }
    }

    pub fn snapshot_is_latest(&self, vid: &VolumeId, snapshot: &Snapshot) -> Result<bool> {
        Ok(self.storage().read().is_latest_snapshot(vid, snapshot)?)
    }

    /// returns the checksum of the snapshot
    pub fn snapshot_checksum(&self, snapshot: &Snapshot) -> Result<Checksum> {
        Ok(self.storage().read().checksum(snapshot)?)
    }

    pub fn snapshot_missing_pages(&self, snapshot: &Snapshot) -> Result<PageSet> {
        let missing_frames = self.storage().read().find_missing_frames(snapshot)?;
        // merge missing_frames into a single PageSet
        Ok(missing_frames
            .into_iter()
            .fold(PageSet::EMPTY, |mut pageset, frame| {
                pageset |= frame.pageset;
                pageset
            }))
    }

    pub fn snapshot_hydrate(&self, snapshot: Snapshot) -> Result<()> {
        self.run_action(HydrateSnapshot {
            snapshot: snapshot.clone(),
        })?;

        // After hydration, verify commit hashes for all commits in the snapshot.
        // This catches any tampering with the integrity chain by a compromised remote.
        self.verify_snapshot_commit_hashes(&snapshot)?;
        Ok(())
    }

    /// Verifies all commit hashes in the snapshot by recomputing them from page data.
    ///
    /// For each commit that has a `commit_hash` and a `segment_idx`, rebuilds the
    /// `CommitHashBuilder` from the stored pages and compares the recomputed hash
    /// against the stored one.
    pub fn verify_snapshot_commit_hashes(&self, snapshot: &Snapshot) -> Result<()> {
        let reader = self.storage().read();

        for commit_result in reader.commits(snapshot) {
            let commit = commit_result?;
            let (Some(commit_hash), Some(segment_idx)) =
                (commit.commit_hash.as_ref(), commit.segment_idx.as_ref())
            else {
                continue;
            };

            let commit_pages = segment_idx.pageset().cardinality();
            let mut builder = CommitHashBuilder::new(
                commit.log.clone(),
                commit.lsn,
                commit.page_count,
                commit_pages,
            );

            for pidx in segment_idx.pageset().iter() {
                let page = reader
                    .read_page(segment_idx.sid().clone(), pidx)?
                    .ok_or_else(|| {
                        LogicalErr::PageNotFound {
                            sid: segment_idx.sid().clone(),
                            pageidx: pidx,
                        }
                    })?;
                builder.write_page(pidx, &page);
            }

            let recomputed = builder.build();
            if &recomputed != commit_hash {
                return Err(LogicalErr::CommitHashMismatch {
                    log: commit.log.clone(),
                    lsn: commit.lsn,
                    expected: commit_hash.clone(),
                    actual: recomputed,
                }
                .into());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use crate::core::{LogId, PageIdx, page::Page};
    use test_log::test;
    use tokio::time::sleep;

    use crate::{
        local::fjall_storage::FjallStorage, remote::RemoteConfig, rt::runtime::Runtime,
        volume_reader::VolumeRead, volume_writer::VolumeWrite,
    };

    #[test]
    fn runtime_sanity() {
        let tokio_rt = tokio::runtime::Builder::new_current_thread()
            .start_paused(true)
            .enable_all()
            .build()
            .unwrap();

        let remote = Arc::new(RemoteConfig::Memory.build().unwrap());
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            Some(Duration::from_secs(1)),
            false,
        );

        let remote_log = LogId::random();
        let vid = runtime
            .volume_open(None, None, Some(remote_log.clone()))
            .unwrap()
            .vid;

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "_ r_",);

        // sanity check volume writer semantics
        let mut writer = runtime.volume_writer(vid.clone()).unwrap();
        for i in [1u8, 2, 5, 9] {
            let pageidx = PageIdx::must_new(i as u32);
            let page = Page::test_filled(i);
            writer.write_page(pageidx, page.clone()).unwrap();
            assert_eq!(writer.read_page(pageidx).unwrap(), page);
        }
        writer.commit().unwrap();

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "+1 r_",);

        // sanity check volume reader semantics
        let reader = runtime.volume_reader(vid.clone()).unwrap();
        tracing::info!("got snapshot {:?}", reader.snapshot());
        for i in [1u8, 2, 5, 9] {
            let pageidx = PageIdx::must_new(i as u32);
            let page = Page::test_filled(i);
            assert!(
                reader.read_page(pageidx).unwrap().into_bytes() == page.into_bytes(),
                "pages aren't equal"
            );
        }

        // create a second runtime connected to the same remote
        let storage = Arc::new(FjallStorage::open_temporary().unwrap());
        let runtime_2 = Runtime::new(
            tokio_rt.handle().clone(),
            remote.clone(),
            storage,
            Some(Duration::from_secs(1)),
            false,
        );

        // open the same remote log in the second runtime
        let vid_2 = runtime_2
            .volume_open(None, None, Some(remote_log))
            .unwrap()
            .vid;

        // let both runtimes run for a little while
        tokio_rt.block_on(async {
            // this sleep lets tokio advance time, allowing the runtime to flush all its jobs
            sleep(Duration::from_secs(5)).await;
            let tree = remote.testonly_format_tree().await;
            tracing::info!("remote tree\n{tree}")
        });

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "1 r1",);
        assert_eq!(runtime_2.volume_status(&vid_2).unwrap().to_string(), "_ r1",);

        // sanity check volume reader semantics in the second runtime
        let reader_2 = runtime_2.volume_reader(vid_2.clone()).unwrap();
        let task = tokio_rt.spawn_blocking(move || {
            for i in [1u8, 2, 5, 9] {
                let pageidx = PageIdx::must_new(i as u32);
                tracing::info!("checking page {pageidx}");
                let expected = Page::test_filled(i);
                let actual = reader_2.read_page(pageidx).unwrap();
                assert_eq!(expected, actual, "read unexpected page contents");
            }
        });
        tokio_rt.block_on(task).unwrap();

        // now write to the second volume, and sync back to the first
        let mut writer_2 = runtime_2.volume_writer(vid_2.clone()).unwrap();
        for i in [3u8, 4, 5, 7] {
            let pageidx = PageIdx::must_new(i as u32);
            let page = Page::test_filled(i + 10);
            writer_2.write_page(pageidx, page.clone()).unwrap();
            assert_eq!(writer_2.read_page(pageidx).unwrap(), page);
        }
        writer_2.commit().unwrap();

        // let both runtimes run for a little while
        tokio_rt.block_on(async {
            // this sleep lets tokio advance time, allowing the runtime to flush all its jobs
            sleep(Duration::from_secs(5)).await;
            let tree = remote.testonly_format_tree().await;
            tracing::info!("remote tree\n{tree}")
        });

        assert_eq!(runtime.volume_status(&vid).unwrap().to_string(), "1 r2",);
        assert_eq!(runtime_2.volume_status(&vid_2).unwrap().to_string(), "1 r2",);

        // sanity check volume reader semantics in the first runtime
        let reader = runtime.volume_reader(vid.clone()).unwrap();
        let task = tokio_rt.spawn_blocking(move || {
            for i in [3u8, 4, 5, 7] {
                let pageidx = PageIdx::must_new(i as u32);
                tracing::info!("checking page {pageidx}");
                let expected = Page::test_filled(i + 10);
                let actual = reader.read_page(pageidx).unwrap();
                assert_eq!(expected, actual, "read unexpected page contents");
            }
        });
        tokio_rt.block_on(task).unwrap();
    }
}
