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
