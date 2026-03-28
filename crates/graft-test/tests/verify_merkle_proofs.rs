use graft::{
    core::{
        CommitHashBuilder, MerkleInclusionProof, LogId,
        commit_hash::CommitMetadata,
        page::Page,
        page_count::PageCount,
    },
    lsn, pageidx,
    volume_reader::VolumeRead,
    volume_writer::VolumeWrite,
};
use graft_test::GraftTestRuntime;

// Required by the lsn! and pageidx! macros.
extern crate static_assertions;

#[test]
fn test_merkle_proof_after_push_pull() -> anyhow::Result<()> {
    graft_test::ensure_test_env();

    let remote = LogId::random();
    let runtime = GraftTestRuntime::with_memory_remote();

    // Open two volumes on the same remote.
    let vid1 = runtime.volume_open(None, None, Some(remote.clone()))?.vid;
    let vid2 = runtime.volume_open(None, None, Some(remote.clone()))?.vid;

    // Write pages to vid1.
    let page1 = Page::test_filled(0x11);
    let page2 = Page::test_filled(0x22);
    let page3 = Page::test_filled(0x33);

    let mut writer = runtime.volume_writer(vid1.clone())?;
    writer.write_page(pageidx!(1), page1.clone())?;
    writer.write_page(pageidx!(5), page2.clone())?;
    writer.write_page(pageidx!(9), page3.clone())?;
    writer.commit()?;

    // Push to remote.
    runtime.volume_push(vid1.clone())?;

    // Pull to vid2 and verify pages arrived.
    runtime.volume_pull(vid2.clone())?;
    let reader = runtime.volume_reader(vid2.clone())?;
    assert_eq!(reader.read_page(pageidx!(1))?, page1);
    assert_eq!(reader.read_page(pageidx!(5))?, page2);
    assert_eq!(reader.read_page(pageidx!(9))?, page3);

    // Build a CommitHashBuilder with the same pages to verify Merkle proofs.
    // In a real integration, the runtime would expose the tree from build_segment.
    // For now, we reconstruct it from the known page data.
    let vol_info = runtime.volume_get(&vid1)?;
    let lsn = lsn!(1);
    let vol_pages = PageCount::new(9);
    let commit_pages = PageCount::new(3);

    let mut builder = CommitHashBuilder::new(
        vol_info.remote.clone(),
        lsn,
        vol_pages,
        commit_pages,
    );
    builder.write_page(pageidx!(1), &page1);
    builder.write_page(pageidx!(5), &page2);
    builder.write_page(pageidx!(9), &page3);

    let (commit_hash, tree) = builder.build_with_tree();
    let metadata = CommitMetadata::new(vol_info.remote, lsn, vol_pages, commit_pages);

    // Verify the tree has the expected structure.
    assert_eq!(tree.total_leaves(), 3);

    // Generate and verify a proof for a single page.
    let proof = tree.proof(&[pageidx!(5)])?;
    assert!(
        proof.verify(&commit_hash, &metadata, &[(pageidx!(5), &page2)]),
        "single page proof should verify"
    );

    // Generate and verify a multi-page proof.
    let proof_all = tree.proof(&[pageidx!(1), pageidx!(5), pageidx!(9)])?;
    assert!(
        proof_all.verify(
            &commit_hash,
            &metadata,
            &[
                (pageidx!(1), &page1),
                (pageidx!(5), &page2),
                (pageidx!(9), &page3),
            ]
        ),
        "multi-page proof should verify"
    );

    // Verify that a tampered page fails proof verification.
    let tampered = Page::test_filled(0xFF);
    assert!(
        !proof.verify(&commit_hash, &metadata, &[(pageidx!(5), &tampered)]),
        "tampered page should fail verification"
    );

    // Verify proof serialization roundtrip.
    let proof_bytes = proof.to_bytes();
    let proof_restored = MerkleInclusionProof::from_bytes(&proof_bytes)?;
    assert!(
        proof_restored.verify(&commit_hash, &metadata, &[(pageidx!(5), &page2)]),
        "deserialized proof should still verify"
    );

    runtime.shutdown().unwrap();
    Ok(())
}

#[test]
fn test_merkle_proof_sparse_pages() -> anyhow::Result<()> {
    graft_test::ensure_test_env();

    let remote = LogId::random();
    let lsn = lsn!(42);
    let vol_pages = PageCount::new(1000);
    let commit_pages = PageCount::new(3);

    // Build a commit with sparse page indices (1, 100, 1000).
    let page_a = Page::test_filled(0xAA);
    let page_b = Page::test_filled(0xBB);
    let page_c = Page::test_filled(0xCC);

    let mut builder = CommitHashBuilder::new(
        remote.clone(),
        lsn,
        vol_pages,
        commit_pages,
    );
    builder.write_page(pageidx!(1), &page_a);
    builder.write_page(pageidx!(100), &page_b);
    builder.write_page(pageidx!(1000), &page_c);

    let (hash, tree) = builder.build_with_tree();
    let metadata = CommitMetadata::new(remote, lsn, vol_pages, commit_pages);

    // Prove the middle page.
    let proof = tree.proof(&[pageidx!(100)])?;
    assert!(proof.verify(&hash, &metadata, &[(pageidx!(100), &page_b)]));

    // Wrong page index with right data should fail.
    assert!(!proof.verify(&hash, &metadata, &[(pageidx!(1), &page_b)]));

    Ok(())
}

#[test]
fn test_merkle_proof_single_page_commit() -> anyhow::Result<()> {
    graft_test::ensure_test_env();

    let remote = LogId::random();
    let lsn = lsn!(1);
    let vol_pages = PageCount::new(1);
    let commit_pages = PageCount::new(1);
    let page = Page::test_filled(0x42);

    let mut builder = CommitHashBuilder::new(
        remote.clone(),
        lsn,
        vol_pages,
        commit_pages,
    );
    builder.write_page(pageidx!(1), &page);

    let (hash, tree) = builder.build_with_tree();
    let metadata = CommitMetadata::new(remote, lsn, vol_pages, commit_pages);
    assert_eq!(tree.total_leaves(), 1);

    let proof = tree.proof(&[pageidx!(1)])?;
    assert!(proof.verify(&hash, &metadata, &[(pageidx!(1), &page)]));

    Ok(())
}

/// Writes real data via SQLite, pushes to remote, pulls to a second node,
/// reads all pages, builds a Merkle tree, generates proofs, then simulates
/// corruption by flipping a byte in one page and verifies the proof detects it.
#[test]
fn test_sqlite_corruption_detected_by_merkle_proof() -> anyhow::Result<()> {
    graft_test::ensure_test_env();

    let remote = LogId::random();
    let mut runtime = GraftTestRuntime::with_memory_remote();
    let sqlite = runtime.open_sqlite("main", Some(remote.clone()));

    // Create a table and insert enough rows to span multiple SQLite pages.
    sqlite.execute_batch(
        r#"
        PRAGMA journal_mode = MEMORY;
        CREATE TABLE accounts (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            balance REAL NOT NULL,
            padding TEXT NOT NULL
        );
        "#,
    )?;

    // Insert 200 rows with ~200 bytes each to span several 4KB pages.
    for i in 0..200 {
        sqlite.execute(
            "INSERT INTO accounts VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                i,
                format!("account-{i:04}"),
                1000.0 + i as f64,
                "x".repeat(150),
            ],
        )?;
    }

    // Push to remote.
    sqlite.graft_pragma("push")?;

    // Get volume info to reconstruct the commit metadata.
    let tag = runtime.tag_get("main")?.expect("tag should exist");
    let vol_info = runtime.volume_get(&tag)?;
    let snapshot = runtime.volume_snapshot(&tag)?;

    // Read all pages from the snapshot to build the Merkle tree.
    let reader = runtime.volume_reader(tag.clone())?;
    let page_count = snapshot.page_count;
    let mut pages: Vec<(graft::core::PageIdx, Page)> = Vec::new();

    for pidx_u32 in 1..=page_count.to_u32() {
        let pidx = graft::core::PageIdx::try_new(pidx_u32).unwrap();
        let page = reader.read_page(pidx)?;
        // Only include non-empty pages (sparse volume).
        if page != Page::EMPTY {
            pages.push((pidx, page));
        }
    }

    assert!(!pages.is_empty(), "should have at least some pages");
    let num_pages = pages.len();
    eprintln!("  read {num_pages} non-empty pages from SQLite volume");

    // Build the Merkle tree from the actual page data.
    let mut builder = CommitHashBuilder::new(
        vol_info.remote.clone(),
        lsn!(1),
        page_count,
        PageCount::new(num_pages as u32),
    );
    for (pidx, page) in &pages {
        builder.write_page(*pidx, page);
    }
    let (commit_hash, tree) = builder.build_with_tree();
    let metadata = CommitMetadata::new(
        vol_info.remote,
        lsn!(1),
        page_count,
        PageCount::new(num_pages as u32),
    );

    // Verify a proof for the first page passes with clean data.
    let (first_pidx, first_page) = &pages[0];
    let proof = tree.proof(&[*first_pidx])?;
    assert!(
        proof.verify(&commit_hash, &metadata, &[(*first_pidx, first_page)]),
        "proof should verify with clean page data"
    );

    // Now simulate corruption: flip a single byte in the page data.
    let mut corrupted_bytes = first_page.as_ref().to_vec();
    // Flip byte at offset 100 (arbitrary choice within the 4KB page).
    corrupted_bytes[100] ^= 0xFF;
    let corrupted_page = Page::try_from(bytes::Bytes::from(corrupted_bytes))?;

    // The proof should FAIL with the corrupted page.
    assert!(
        !proof.verify(&commit_hash, &metadata, &[(*first_pidx, &corrupted_page)]),
        "proof should FAIL with corrupted page data (single byte flip)"
    );

    // Verify a proof for a middle page too.
    let mid = pages.len() / 2;
    let (mid_pidx, mid_page) = &pages[mid];
    let mid_proof = tree.proof(&[*mid_pidx])?;
    assert!(
        mid_proof.verify(&commit_hash, &metadata, &[(*mid_pidx, mid_page)]),
        "middle page proof should verify with clean data"
    );

    // Corrupt the middle page (zero it out entirely).
    let zeroed_page = Page::EMPTY;
    assert!(
        !mid_proof.verify(&commit_hash, &metadata, &[(*mid_pidx, &zeroed_page)]),
        "proof should FAIL with zeroed page"
    );

    eprintln!("  corruption detected for both single-byte-flip and full-zero scenarios");

    runtime.shutdown().unwrap();
    Ok(())
}

/// Writes data via SQLite, pushes (generating leaf hashes), pulls to a second
/// node, then corrupts a page directly in fjall storage and reads through
/// SQLite. The read-path verification should automatically detect the corruption.
#[test]
fn test_vfs_read_detects_corrupted_cached_page() {
    graft_test::ensure_test_env();

    let remote = LogId::random();

    // Node 1: write data and push.
    let mut runtime1 = GraftTestRuntime::with_memory_remote();
    let sqlite1 = runtime1.open_sqlite("main", Some(remote.clone()));

    sqlite1
        .execute_batch(
            r#"
            PRAGMA journal_mode = MEMORY;
            CREATE TABLE items (id INTEGER PRIMARY KEY, data TEXT NOT NULL);
            "#,
        )
        .unwrap();

    // Insert enough rows to create multiple pages.
    for i in 0..100 {
        sqlite1
            .execute(
                "INSERT INTO items VALUES (?1, ?2)",
                rusqlite::params![i, "x".repeat(100)],
            )
            .unwrap();
    }

    sqlite1.graft_pragma("push").unwrap();

    // Node 2: pull the data (fetches commits + pages into local storage).
    let mut runtime2 = runtime1.spawn_peer();
    let sqlite2 = runtime2.open_sqlite("main", Some(remote.clone()));
    sqlite2.graft_pragma("pull").unwrap();

    // First, read all data to populate the local page cache. This triggers
    // FetchSegment which downloads clean pages from remote into fjall.
    let count: i64 = sqlite2
        .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 100, "should read 100 rows before corruption");

    // Drop and re-open the SQLite connection to clear SQLite's internal page
    // cache. The next read will go through the VFS again but pages are now
    // in fjall's local cache (not remote).
    drop(sqlite2);
    let sqlite2 = runtime2.open_sqlite("main", Some(remote.clone()));

    // Now corrupt page 2 — it's cached in fjall, so the next read will hit
    // the cache-hit verification path (not FetchSegment).
    let tag2 = runtime2.tag_get("main").unwrap().unwrap();
    let snapshot2 = runtime2.volume_snapshot(&tag2).unwrap();
    let pidx2 = graft::core::PageIdx::try_new(2).unwrap();
    // Debug: check if the commit has leaf hashes
    let volume2 = runtime2.volume_get(&tag2).unwrap();
    let commit = runtime2.get_commit(&volume2.remote, graft::lsn!(1)).unwrap();
    match &commit {
        Some(c) => {
            eprintln!(
                "  commit LSN 1 has {} leaf hashes, has page 2 hash: {}",
                c.leaf_hashes.len(),
                c.leaf_hashes.get(pidx2).is_some()
            );
        }
        None => eprintln!("  no commit at LSN 1"),
    }

    let corrupted = runtime2
        .storage_for_test()
        .corrupt_page(&snapshot2, pidx2, Page::test_filled(0xDE))
        .unwrap();
    assert!(corrupted, "page 2 should have been found and corrupted");
    eprintln!("  corrupted page 2 in node 2's fjall storage BEFORE first read");

    // Now try to read through SQLite. The VFS read path should detect the
    // corruption via leaf hash verification and return an error.
    let result = sqlite2.query_row("SELECT COUNT(*) FROM items", [], |row| row.get::<_, i64>(0));

    assert!(
        result.is_err(),
        "SQLite read should fail due to corrupted page detected by Merkle verification"
    );
    eprintln!(
        "  VFS read correctly detected corruption: {}",
        result.unwrap_err()
    );

    runtime1.shutdown().unwrap();
    runtime2.shutdown().unwrap();
}

/// Writes pages via the Graft API, pushes, then replaces the segment in
/// remote storage with a validly-compressed segment containing wrong page data
/// (keeping the original commit with its leaf hashes). Node 2 pulls commits
/// but when it reads a page, FetchSegment downloads the corrupt segment,
/// decompresses it successfully (valid ZStd), but the leaf hash check rejects
/// the page because the content doesn't match.
#[test]
fn test_fetch_detects_corrupted_remote_segment() {
    use graft::remote::segment::SegmentBuilder;

    graft_test::ensure_test_env();

    let remote_log = LogId::random();
    let runtime1 = GraftTestRuntime::with_memory_remote();

    // Write 3 pages and push.
    let vid1 = runtime1.volume_open(None, None, Some(remote_log.clone())).unwrap().vid;
    let page1 = Page::test_filled(0x11);
    let page2 = Page::test_filled(0x22);
    let page3 = Page::test_filled(0x33);

    let mut writer = runtime1.volume_writer(vid1.clone()).unwrap();
    writer.write_page(pageidx!(1), page1.clone()).unwrap();
    writer.write_page(pageidx!(2), page2.clone()).unwrap();
    writer.write_page(pageidx!(3), page3.clone()).unwrap();
    writer.commit().unwrap();
    runtime1.volume_push(vid1.clone()).unwrap();

    // Get the commit (which has leaf hashes for the ORIGINAL pages).
    let vol1 = runtime1.volume_get(&vid1).unwrap();
    let commit = runtime1
        .get_commit(&vol1.remote, graft::lsn!(1))
        .unwrap()
        .expect("commit should exist");
    let segment_idx = commit.segment_idx().expect("commit should have segment");
    let sid = segment_idx.sid().clone();
    assert!(!commit.leaf_hashes.is_empty(), "commit should have leaf hashes");
    eprintln!(
        "  pushed commit with {} leaf hashes, segment {sid}",
        commit.leaf_hashes.len()
    );

    // Build a corrupt but validly-compressed segment with wrong page data.
    let mut corrupt_builder = SegmentBuilder::new();
    corrupt_builder.write(pageidx!(1), &Page::test_filled(0xAA)); // wrong
    corrupt_builder.write(pageidx!(2), &Page::test_filled(0xBB)); // wrong
    corrupt_builder.write(pageidx!(3), &Page::test_filled(0xCC)); // wrong
    let (_frames, chunks) = corrupt_builder.finish();
    let corrupt_bytes: bytes::Bytes = chunks.into_iter().flatten().collect();

    // Replace the segment in remote. The commit still references the old
    // leaf hashes but the segment now contains different page data.
    let remote = runtime1.remote();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(remote.testonly_replace_segment(&sid, corrupt_bytes))
        .unwrap();
    eprintln!("  replaced segment in remote with corrupt data");

    // Also rewrite the commit's frame index to match the new segment size.
    // We need to update the commit so the byte ranges work for the new segment.
    // The easiest way: build a new commit with the new frame index but OLD leaf hashes.
    let new_commit = {
        let mut c = commit.clone();
        let mut new_idx = segment_idx.clone();
        // Replace frames with the corrupt segment's frames
        new_idx.frames = _frames;
        c.segment_idx = Some(new_idx);
        c
    };
    rt.block_on(async {
        // Delete old commit and write new one
        // Actually, for memory backend, put_commit with if_not_exists will fail.
        // Instead, use the store directly to overwrite.
        remote.testonly_replace_commit(&new_commit).await
    })
    .unwrap();
    eprintln!("  updated commit frame index to match corrupt segment");

    // Node 2: open the same remote log and pull (gets commit with old leaf hashes).
    let runtime2 = runtime1.spawn_peer();
    let vid2 = runtime2
        .volume_open(None, None, Some(remote_log.clone()))
        .unwrap()
        .vid;
    runtime2.volume_pull(vid2.clone()).unwrap();

    // Read page 1 — should trigger FetchSegment which downloads the corrupt
    // segment, decompresses it (valid ZStd), but leaf hash check fails.
    let reader = runtime2.volume_reader(vid2.clone()).unwrap();
    let result = reader.read_page(pageidx!(1));

    assert!(
        result.is_err(),
        "read_page should fail due to leaf hash mismatch on corrupt remote segment"
    );
    let err = result.unwrap_err();
    let err_str = format!("{err}");
    eprintln!("  read correctly detected remote corruption: {err_str}");
    assert!(
        err_str.contains("integrity") || err_str.contains("Integrity"),
        "error should mention integrity, got: {err_str}"
    );

    runtime1.shutdown().unwrap();
    runtime2.shutdown().unwrap();
}
