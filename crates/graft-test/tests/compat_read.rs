use graft::core::LogId;
use graft::core::page::Page;
use graft::remote::RemoteConfig;
use graft::volume_reader::VolumeRead;
use graft::{lsn, pageidx};
use graft_test::GraftTestRuntime;
use std::sync::Arc;

extern crate static_assertions;

#[test]
fn read_upstream_data_from_fs_remote() {
    graft_test::ensure_test_env();

    let remote = Arc::new(
        RemoteConfig::Fs { root: "/tmp/fs-remote".to_string() }
            .build()
            .unwrap(),
    );
    let runtime = GraftTestRuntime::with_remote(remote);

    // Same log ID that upstream pushed to
    let log_id: LogId = "74ggbzxuMf-2uAmM7FwXntwW".parse().unwrap();
    let volume = runtime
        .volume_open(None, None, Some(log_id.clone()))
        .unwrap();

    // Pull from the shared filesystem remote
    runtime.volume_pull(volume.vid.clone()).unwrap();
    eprintln!("BRANCH: pulled from /tmp/fs-remote via log {log_id}");

    // Verify the commit exists and check its properties
    let commit = runtime
        .get_commit(&log_id, lsn!(1))
        .unwrap()
        .expect("commit at LSN 1 should exist");
    eprintln!("  commit_hash: {:?}", commit.commit_hash);
    eprintln!("  leaf_hashes empty: {}", commit.leaf_hashes.is_empty());
    eprintln!("  leaf_hashes_required: {}", commit.leaf_hashes_required);

    // Upstream commits should NOT have leaf hashes (upstream predates the feature)
    assert!(
        commit.leaf_hashes.is_empty(),
        "upstream commit should have empty leaf_hashes (field didn't exist in upstream)"
    );
    assert!(
        !commit.leaf_hashes_required,
        "upstream commit should have leaf_hashes_required=false (field didn't exist)"
    );

    // Read pages and verify data integrity
    let reader = runtime.volume_reader(volume.vid.clone()).unwrap();
    let page1 = reader.read_page(pageidx!(1)).unwrap();
    let page2 = reader.read_page(pageidx!(2)).unwrap();
    let page3 = reader.read_page(pageidx!(3)).unwrap();

    assert_eq!(page1, Page::test_filled(0xAA), "page 1 data mismatch");
    assert_eq!(page2, Page::test_filled(0xBB), "page 2 data mismatch");
    assert_eq!(page3, Page::test_filled(0xCC), "page 3 data mismatch");
    eprintln!("  all 3 pages verified: 0xAA, 0xBB, 0xCC");

    // The TOFU boundary should NOT be established (upstream had no leaf hashes)
    let vol = runtime.volume_get(&volume.vid).unwrap();
    assert!(
        vol.leaf_hash_min_lsn.is_none(),
        "TOFU boundary should not be set for upstream-only data"
    );
    eprintln!("  leaf_hash_min_lsn: None (correct — no leaf hashes seen)");

    eprintln!("BRANCH: cross-compatibility verified!");
    runtime.shutdown().unwrap();
}
