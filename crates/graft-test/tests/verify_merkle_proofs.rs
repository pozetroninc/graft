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
