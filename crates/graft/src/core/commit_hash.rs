use std::{
    fmt::{Debug, Display},
    str::FromStr,
};

use crate::core::{
    LogId,
    cbe::CBE64,
    lsn::LSN,
    page::Page,
    page_count::PageCount,
    pageidx::PageIdx,
    zerocopy_ext::{self, ZerocopyErr},
};
use crate::derive_zerocopy_encoding;
use rs_merkle::{Hasher as MerkleHasher, MerkleTree};
use thiserror::Error;
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes, Unaligned};

/// The size of a `CommitHash` in bytes.
const COMMIT_HASH_SIZE: usize = 32;

/// The size of the hash portion of the `CommitHash` in bytes.
const HASH_SIZE: usize = 31;

/// Magic number to initialize commit hash computation
const COMMIT_HASH_MAGIC: [u8; 4] = [0x68, 0xA4, 0x19, 0x30];

// The length of an encoded CommitHash in base58.
// To calculate this compute ceil(32 * (log2(256) / log2(58)))
//
// Note: we require that CommitHash's always are their maximum length
// This is currently guaranteed for well-constructed CommitHash's due to the
// CommitHashPrefix occupying the most significant byte.
const ENCODED_LEN: usize = 44;

/// BLAKE3 hasher adapter for rs-merkle.
#[derive(Clone)]
pub struct Blake3Algorithm;

impl MerkleHasher for Blake3Algorithm {
    type Hash = [u8; 32];
    fn hash(data: &[u8]) -> [u8; 32] {
        blake3::hash(data).into()
    }
}

/// Errors that can occur when generating or deserializing a Merkle inclusion proof.
#[derive(Error, Debug)]
pub enum MerkleProofError {
    #[error("cannot generate proof for empty tree")]
    EmptyTree,
    #[error("page index {0} not found in tree")]
    PageNotFound(PageIdx),
    #[error("proof deserialization failed: {0}")]
    Deserialize(String),
}

#[derive(Debug, Error, PartialEq)]
pub enum CommitHashParseErr {
    #[error("invalid base58 encoding")]
    DecodeErr(#[from] bs58::decode::Error),

    #[error("invalid zerocopy encoding")]
    ZerocopyErr(#[from] zerocopy_ext::ZerocopyErr),

    #[error("invalid length")]
    InvalidLength,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    TryFromBytes,
    IntoBytes,
    Immutable,
    KnownLayout,
    Unaligned,
)]
#[repr(u8)]
pub enum CommitHashPrefix {
    #[default]
    Value = b'C',
}

#[derive(
    Clone, PartialEq, Eq, Default, TryFromBytes, IntoBytes, Immutable, KnownLayout, Unaligned,
)]
#[repr(C)]
pub struct CommitHash {
    prefix: CommitHashPrefix,
    hash: [u8; HASH_SIZE],
}

static_assertions::assert_eq_size!(CommitHash, [u8; COMMIT_HASH_SIZE]);

impl CommitHash {
    pub const ZERO: Self = Self {
        prefix: CommitHashPrefix::Value,
        hash: [0; HASH_SIZE],
    };

    #[cfg(any(test, feature = "testutil"))]
    pub fn testonly_random() -> Self {
        Self {
            prefix: CommitHashPrefix::Value,
            hash: rand::random(),
        }
    }

    /// Encodes the `CommitHash` to base58 and returns it as a string
    #[inline]
    pub fn pretty(&self) -> String {
        bs58::encode(self.as_bytes()).into_string()
    }
}

impl TryFrom<[u8; COMMIT_HASH_SIZE]> for CommitHash {
    type Error = CommitHashParseErr;

    #[inline]
    fn try_from(value: [u8; COMMIT_HASH_SIZE]) -> Result<Self, Self::Error> {
        Ok(zerocopy::try_transmute!(value).map_err(ZerocopyErr::from)?)
    }
}

impl From<CommitHash> for [u8; COMMIT_HASH_SIZE] {
    #[inline]
    fn from(value: CommitHash) -> Self {
        zerocopy::transmute!(value)
    }
}

impl FromStr for CommitHash {
    type Err = CommitHashParseErr;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // verify the length
        if value.len() != ENCODED_LEN {
            return Err(CommitHashParseErr::InvalidLength);
        }

        // parse from base58
        let bytes: [u8; COMMIT_HASH_SIZE] = bs58::decode(value.as_bytes()).into_array_const()?;
        bytes.try_into()
    }
}

impl Debug for CommitHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CommitHash({})", self.pretty())
    }
}

impl Display for CommitHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.pretty())
    }
}

derive_zerocopy_encoding!(
    encode type (CommitHash)
    with size (COMMIT_HASH_SIZE)
    with empty (CommitHash::ZERO)
);

/// Metadata needed to verify a Merkle inclusion proof against a commit hash.
///
/// Contains the same fields fed to `CommitHashBuilder::new()`:
/// magic + `LogId` + LSN + `vol_pages` + `commit_pages`.
#[derive(Clone, Debug)]
pub struct CommitMetadata {
    bytes: Vec<u8>,
}

impl CommitMetadata {
    /// Build metadata bytes from the same parameters as `CommitHashBuilder::new()`.
    pub fn new(log: LogId, lsn: LSN, vol_pages: PageCount, commit_pages: PageCount) -> Self {
        let mut bytes = Vec::with_capacity(4 + 16 + 8 + 4 + 4);
        bytes.extend_from_slice(&COMMIT_HASH_MAGIC);
        bytes.extend_from_slice(log.as_bytes());
        bytes.extend_from_slice(CBE64::from(lsn).as_bytes());
        bytes.extend_from_slice(&vol_pages.to_u32().to_be_bytes());
        bytes.extend_from_slice(&commit_pages.to_u32().to_be_bytes());
        Self { bytes }
    }

    /// Returns the raw metadata bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Builder for computing commit hashes using a Merkle tree of BLAKE3 leaf hashes.
///
/// The hash incorporates the Log ID, LSN, page count, and page data
/// to ensure uniqueness and integrity verification. Each page becomes
/// a leaf in the Merkle tree, enabling per-page inclusion proofs.
pub struct CommitHashBuilder {
    metadata_bytes: Vec<u8>,
    leaves: Vec<[u8; 32]>,
    leaf_page_indices: Vec<PageIdx>,
    last_pageidx: Option<PageIdx>,
}

impl CommitHashBuilder {
    /// Creates a new `CommitHashBuilder` initialized with the given metadata.
    pub fn new(log: LogId, lsn: LSN, vol_pages: PageCount, commit_pages: PageCount) -> Self {
        let metadata = CommitMetadata::new(log, lsn, vol_pages, commit_pages);
        Self {
            metadata_bytes: metadata.bytes,
            leaves: Vec::new(),
            leaf_page_indices: Vec::new(),
            last_pageidx: None,
        }
    }

    /// Writes a page to the hash computation.
    ///
    /// # Panics
    /// This method will panic if pages are written out of order by pageidx
    pub fn write_page(&mut self, pageidx: PageIdx, page: &Page) {
        // Ensure pages are written in order
        if let Some(last_pageidx) = self.last_pageidx.replace(pageidx) {
            assert!(
                pageidx > last_pageidx,
                "Pages must be written in order by pageidx. Last: {last_pageidx}, Current: {pageidx}"
            );
        }

        let mut leaf_hasher = blake3::Hasher::new();
        leaf_hasher.update(&pageidx.to_u32().to_be_bytes());
        leaf_hasher.update(page.as_ref());
        self.leaves.push(*leaf_hasher.finalize().as_bytes());
        self.leaf_page_indices.push(pageidx);
    }

    /// Internal method that computes the Merkle tree and commit hash.
    ///
    /// Constructs the tree once and reuses it, using the EMPTY_MERKLE sentinel
    /// as the root for empty trees.
    #[allow(clippy::type_complexity)]
    fn build_inner(
        self,
    ) -> (
        CommitHash,
        MerkleTree<Blake3Algorithm>,
        [u8; 32],
        Vec<[u8; 32]>,
        Vec<PageIdx>,
        Vec<u8>,
    ) {
        let tree = MerkleTree::<Blake3Algorithm>::from_leaves(&self.leaves);

        let merkle_root: [u8; 32] = if self.leaves.is_empty() {
            blake3::hash(b"EMPTY_MERKLE").into()
        } else {
            tree.root().expect("non-empty tree must have root")
        };

        let mut final_hasher = blake3::Hasher::new();
        final_hasher.update(&self.metadata_bytes);
        final_hasher.update(&merkle_root);
        let hash = final_hasher.finalize();
        let mut bytes = *hash.as_bytes();
        bytes[0] = CommitHashPrefix::Value as u8;
        let commit_hash: CommitHash =
            zerocopy::try_transmute!(bytes).expect("prefix byte manually set");

        (
            commit_hash,
            tree,
            merkle_root,
            self.leaves,
            self.leaf_page_indices,
            self.metadata_bytes,
        )
    }

    /// Finalizes the hash computation and returns the `CommitHash`.
    pub fn build(self) -> CommitHash {
        self.build_inner().0
    }

    /// Finalizes the hash computation and returns both the `CommitHash`
    /// and a `CommitMerkleTree` for generating inclusion proofs.
    pub fn build_with_tree(self) -> (CommitHash, CommitMerkleTree) {
        let (hash, tree, merkle_root, leaves, indices, metadata_bytes) = self.build_inner();
        (
            hash,
            CommitMerkleTree {
                tree,
                merkle_root,
                leaves,
                leaf_page_indices: indices,
                metadata_bytes,
            },
        )
    }
}

/// A Merkle tree built from a commit's pages, used to generate inclusion proofs.
pub struct CommitMerkleTree {
    tree: MerkleTree<Blake3Algorithm>,
    merkle_root: [u8; 32],
    leaves: Vec<[u8; 32]>,
    leaf_page_indices: Vec<PageIdx>,
    metadata_bytes: Vec<u8>,
}

impl CommitMerkleTree {
    /// Returns the Merkle root hash (including the EMPTY_MERKLE sentinel for empty trees).
    pub fn root(&self) -> [u8; 32] {
        self.merkle_root
    }

    /// Returns the total number of leaves in the tree.
    pub fn total_leaves(&self) -> usize {
        self.leaves.len()
    }

    /// Returns the metadata bytes used to bind the Merkle root to the commit hash.
    pub fn metadata_bytes(&self) -> &[u8] {
        &self.metadata_bytes
    }

    /// Generates a Merkle inclusion proof for the given page indices.
    ///
    /// Returns an error if any of the requested page indices are not in the tree.
    pub fn proof(
        &self,
        page_indices: &[PageIdx],
    ) -> Result<MerkleInclusionProof, MerkleProofError> {
        if self.leaves.is_empty() {
            return Err(MerkleProofError::EmptyTree);
        }

        // Map PageIdx -> leaf position using binary search (leaf_page_indices is sorted)
        let mut paired: Vec<(usize, PageIdx)> = Vec::with_capacity(page_indices.len());
        for pidx in page_indices {
            let pos = self
                .leaf_page_indices
                .binary_search(pidx)
                .map_err(|_| MerkleProofError::PageNotFound(*pidx))?;
            paired.push((pos, *pidx));
        }

        // Sort and deduplicate by position (rs-merkle requires sorted indices)
        paired.sort_unstable_by_key(|(pos, _)| *pos);
        paired.dedup_by_key(|(pos, _)| *pos);

        let (positions, leaf_page_indices): (Vec<usize>, Vec<PageIdx>) =
            paired.into_iter().unzip();

        let proof = self.tree.proof(&positions);
        let proof_bytes = proof.to_bytes();

        Ok(MerkleInclusionProof {
            proof_bytes,
            leaf_positions: positions,
            leaf_page_indices,
            total_leaves: self.leaves.len(),
        })
    }
}

/// A serializable Merkle inclusion proof for one or more pages in a commit.
#[derive(Clone, Debug)]
pub struct MerkleInclusionProof {
    proof_bytes: Vec<u8>,
    leaf_positions: Vec<usize>,
    leaf_page_indices: Vec<PageIdx>,
    total_leaves: usize,
}

impl MerkleInclusionProof {
    /// Verifies that the given pages are included in the commit identified by `commit_hash`.
    ///
    /// Reconstructs leaf hashes from the provided page entries, uses the Merkle proof
    /// to compute the root, then binds it to the metadata and checks against the commit hash.
    pub fn verify(
        &self,
        commit_hash: &CommitHash,
        metadata: &CommitMetadata,
        page_entries: &[(PageIdx, &Page)],
    ) -> bool {
        if page_entries.len() != self.leaf_page_indices.len() {
            return false;
        }

        // Reconstruct leaf hashes ordered by the proof's leaf_page_indices (which are
        // sorted by position). Match page entries by PageIdx for deterministic ordering.
        let mut leaf_hashes: Vec<[u8; 32]> = Vec::with_capacity(self.leaf_page_indices.len());
        for expected_pidx in &self.leaf_page_indices {
            let Some((_pidx, page)) = page_entries.iter().find(|(pidx, _)| pidx == expected_pidx)
            else {
                return false;
            };
            let mut leaf_hasher = blake3::Hasher::new();
            leaf_hasher.update(&expected_pidx.to_u32().to_be_bytes());
            leaf_hasher.update(page.as_ref());
            leaf_hashes.push(*leaf_hasher.finalize().as_bytes());
        }

        // Deserialize the proof
        let proof = match rs_merkle::MerkleProof::<Blake3Algorithm>::try_from(
            self.proof_bytes.as_slice(),
        ) {
            Ok(p) => p,
            Err(_) => return false,
        };

        // Compute the root from the proof
        let computed_root = match proof.root(&self.leaf_positions, &leaf_hashes, self.total_leaves)
        {
            Ok(root) => root,
            Err(_) => return false,
        };

        // Bind metadata to the computed root, same as build_inner
        let mut final_hasher = blake3::Hasher::new();
        final_hasher.update(metadata.as_bytes());
        final_hasher.update(&computed_root);
        let hash = final_hasher.finalize();
        let mut bytes = *hash.as_bytes();
        bytes[0] = CommitHashPrefix::Value as u8;

        let reconstructed: CommitHash = match zerocopy::try_transmute!(bytes) {
            Ok(h) => h,
            Err(_) => return false,
        };

        reconstructed == *commit_hash
    }

    /// Serializes the proof to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();

        // total_leaves as u32
        out.extend_from_slice(&(self.total_leaves as u32).to_be_bytes());

        // number of leaf positions as u32
        out.extend_from_slice(&(self.leaf_positions.len() as u32).to_be_bytes());

        // each (leaf position as u32, page index as u32) pair
        for (&pos, pidx) in self.leaf_positions.iter().zip(&self.leaf_page_indices) {
            out.extend_from_slice(&(pos as u32).to_be_bytes());
            out.extend_from_slice(&pidx.to_u32().to_be_bytes());
        }

        // proof bytes length as u32
        out.extend_from_slice(&(self.proof_bytes.len() as u32).to_be_bytes());

        // proof bytes
        out.extend_from_slice(&self.proof_bytes);

        out
    }

    /// Deserializes a proof from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, MerkleProofError> {
        if data.len() < 8 {
            return Err(MerkleProofError::Deserialize(
                "proof data too short".to_string(),
            ));
        }

        let mut offset = 0;

        let total_leaves =
            u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        let num_positions =
            u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        // Each entry is a (u32 position, u32 page_idx) pair = 8 bytes
        if data.len() < offset + num_positions * 8 + 4 {
            return Err(MerkleProofError::Deserialize(
                "proof data too short for positions".to_string(),
            ));
        }

        let mut leaf_positions = Vec::with_capacity(num_positions);
        let mut leaf_page_indices = Vec::with_capacity(num_positions);
        for _ in 0..num_positions {
            let pos = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            let pidx_raw = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let pidx = PageIdx::try_new(pidx_raw).ok_or_else(|| {
                MerkleProofError::Deserialize(format!("invalid page index: {pidx_raw}"))
            })?;
            leaf_positions.push(pos);
            leaf_page_indices.push(pidx);
        }

        let proof_len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        if data.len() < offset + proof_len {
            return Err(MerkleProofError::Deserialize(
                "proof data too short for proof bytes".to_string(),
            ));
        }

        let proof_bytes = data[offset..offset + proof_len].to_vec();

        Ok(Self {
            proof_bytes,
            leaf_positions,
            leaf_page_indices,
            total_leaves,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::panic;

    use super::*;
    use crate::{lsn, pageidx};
    use bilrost::{Message, OwnedMessage};
    use test_log::test;

    #[test]
    fn test_commit_hash_bilrost() {
        #[derive(Message, Debug, PartialEq, Eq)]
        struct TestMsg {
            hash: Option<CommitHash>,
        }

        let msg = TestMsg {
            hash: Some(CommitHash::testonly_random()),
        };
        let b = msg.encode_to_bytes();
        let decoded: TestMsg = TestMsg::decode(b).unwrap();
        assert_eq!(decoded, msg, "Decoded message does not match original");
    }

    #[test]
    fn test_commit_hash_builder_table() {
        let log: LogId = "74ggbzxuMf-2uAmM7FwXntwW".parse().unwrap();

        struct TestCase {
            name: &'static str,
            log: LogId,
            lsn: LSN,
            page_count: PageCount,
            pages: Vec<(PageIdx, Page)>,
            expected_hash: &'static str,
        }

        let test_cases = vec![
            TestCase {
                name: "empty_log",
                log: log.clone(),
                lsn: lsn!(1),
                page_count: PageCount::ZERO,
                pages: vec![],
                expected_hash: "5XLfiYoRNuPT236PErRR4MDrx3SRnoCjtzg2tDwAoBf8",
            },
            TestCase {
                name: "single_page",
                log: log.clone(),
                lsn: lsn!(42),
                page_count: PageCount::new(1),
                pages: vec![(pageidx!(1), Page::test_filled(0xAA))],
                expected_hash: "5XZNaGb3NvnQbzY4pnqKT64aJq9XiY3whDWGjCj3KLNt",
            },
            TestCase {
                name: "multiple_pages",
                log,
                lsn: lsn!(123),
                page_count: PageCount::new(2),
                pages: vec![
                    (pageidx!(1), Page::test_filled(0x11)),
                    (pageidx!(2), Page::test_filled(0x22)),
                ],
                expected_hash: "5ZGDFuqfYg5tEdN8giQVf8m9eppf8yPmAgwhhf9WdFxF",
            },
        ];

        for test_case in test_cases {
            let commit_pages = PageCount::new(test_case.pages.len() as u32);
            let mut builder = CommitHashBuilder::new(
                test_case.log,
                test_case.lsn,
                test_case.page_count,
                commit_pages,
            );

            for (pageidx, page) in test_case.pages {
                builder.write_page(pageidx, &page);
            }

            let hash = builder.build();
            println!("hash for case {}: {}", test_case.name, hash.pretty());
            let expected_hash: CommitHash = test_case.expected_hash.parse().unwrap();

            assert_eq!(
                hash,
                expected_hash,
                "Hash mismatch for test case: {}. Expected: {}, Got: {}",
                test_case.name,
                test_case.expected_hash,
                hash.pretty()
            );
            assert_eq!(
                &hash.pretty(),
                test_case.expected_hash,
                "Pretty format mismatch for test case: {}. Expected: {}, Got: {}",
                test_case.name,
                test_case.expected_hash,
                hash.pretty()
            );
        }
    }

    #[test]
    #[should_panic(expected = "Pages must be written in order by pageidx")]
    fn test_commit_hash_builder_page_order_panic() {
        let mut builder = CommitHashBuilder::new(
            LogId::random(),
            LSN::FIRST,
            PageCount::ZERO,
            PageCount::ZERO,
        );
        builder.write_page(pageidx!(2), &Page::test_filled(0x22));
        builder.write_page(pageidx!(1), &Page::test_filled(0x11)); // This should panic
    }

    #[test]
    fn test_commit_hash_from_str() {
        let hash: CommitHash = "5aNs8RN7tSRqfi66ubcPqSVqrWBGbaPU6C4mBVp6NYgo"
            .parse()
            .unwrap();
        let encoded = hash.pretty();
        let decoded: CommitHash = encoded.parse().unwrap();
        assert_eq!(hash, decoded);
    }

    #[test]
    fn test_commit_hash_from_str_invalid() {
        // Test various invalid inputs
        let invalid_cases = vec![
            "",      // empty string
            "short", // too short
            "verylongstringthatiswaytoologtobeahashverylongstringthatiswaytoologtobeahashverylongstringthatiswaytoologtobeahash", // too long
            "invalid!@#$%^&*()characters", // invalid characters
            "5aNs8RN7tSRqfi66ubcPqSVqrWBGbaPU6C4mBVp6NYg", // wrong length (43 chars)
            "5aNs8RN7tSRqfi66ubcPqSVqrWBGbaPU6C4mBVp6NYgoY", // wrong length (45 chars)
            "4aNs8RN7tSRqfi66ubcPqSVqrWBGbaPU6C4mBVp6NYgo", // wrong prefix
        ];

        for case in invalid_cases {
            if let Ok(hash) = case.parse::<CommitHash>() {
                panic!(
                    "Expected error for case: `{}`, but parsed successfully: {}",
                    case,
                    hash.pretty()
                )
            }
        }
    }

    #[test]
    fn test_build_matches_build_with_tree() {
        let log = LogId::random();
        let lsn = lsn!(10);
        let vol_pages = PageCount::new(3);
        let commit_pages = PageCount::new(3);

        let pages = vec![
            (pageidx!(1), Page::test_filled(0x01)),
            (pageidx!(2), Page::test_filled(0x02)),
            (pageidx!(3), Page::test_filled(0x03)),
        ];

        // Build with build()
        let mut builder1 = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        for (idx, page) in &pages {
            builder1.write_page(*idx, page);
        }
        let hash1 = builder1.build();

        // Build with build_with_tree()
        let mut builder2 = CommitHashBuilder::new(log, lsn, vol_pages, commit_pages);
        for (idx, page) in &pages {
            builder2.write_page(*idx, page);
        }
        let (hash2, _tree) = builder2.build_with_tree();

        assert_eq!(
            hash1, hash2,
            "build() and build_with_tree() must produce the same hash"
        );
    }

    #[test]
    fn test_merkle_proof_single_page() {
        let log = LogId::random();
        let lsn = lsn!(1);
        let vol_pages = PageCount::new(1);
        let commit_pages = PageCount::new(1);
        let page = Page::test_filled(0xAB);

        let mut builder = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        builder.write_page(pageidx!(1), &page);
        let (hash, tree) = builder.build_with_tree();

        let proof = tree
            .proof(&[pageidx!(1)])
            .expect("proof generation should succeed");

        let metadata = CommitMetadata::new(log, lsn, vol_pages, commit_pages);
        assert!(
            proof.verify(&hash, &metadata, &[(pageidx!(1), &page)]),
            "proof should verify for correct page"
        );
    }

    #[test]
    fn test_merkle_proof_multi_page() {
        let log = LogId::random();
        let lsn = lsn!(5);
        let vol_pages = PageCount::new(4);
        let commit_pages = PageCount::new(4);

        let pages = vec![
            (pageidx!(1), Page::test_filled(0x10)),
            (pageidx!(2), Page::test_filled(0x20)),
            (pageidx!(3), Page::test_filled(0x30)),
            (pageidx!(4), Page::test_filled(0x40)),
        ];

        let mut builder = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        for (idx, page) in &pages {
            builder.write_page(*idx, page);
        }
        let (hash, tree) = builder.build_with_tree();

        let metadata = CommitMetadata::new(log.clone(), lsn, vol_pages, commit_pages);

        // Prove a subset (pages 2 and 4)
        let proof = tree
            .proof(&[pageidx!(2), pageidx!(4)])
            .expect("proof generation should succeed");
        assert!(
            proof.verify(
                &hash,
                &metadata,
                &[(pageidx!(2), &pages[1].1), (pageidx!(4), &pages[3].1)]
            ),
            "proof should verify for subset of pages"
        );

        // Prove all pages
        let proof_all = tree
            .proof(&[pageidx!(1), pageidx!(2), pageidx!(3), pageidx!(4)])
            .expect("proof generation should succeed");
        let all_entries: Vec<(PageIdx, &Page)> = pages.iter().map(|(i, p)| (*i, p)).collect();
        assert!(
            proof_all.verify(&hash, &metadata, &all_entries),
            "proof should verify for all pages"
        );
    }

    #[test]
    fn test_merkle_proof_roundtrip() {
        let log = LogId::random();
        let lsn = lsn!(1);
        let vol_pages = PageCount::new(2);
        let commit_pages = PageCount::new(2);

        let pages = vec![
            (pageidx!(1), Page::test_filled(0xAA)),
            (pageidx!(2), Page::test_filled(0xBB)),
        ];

        let mut builder = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        for (idx, page) in &pages {
            builder.write_page(*idx, page);
        }
        let (hash, tree) = builder.build_with_tree();

        let proof = tree
            .proof(&[pageidx!(1)])
            .expect("proof generation should succeed");

        // Serialize and deserialize
        let serialized = proof.to_bytes();
        let deserialized =
            MerkleInclusionProof::from_bytes(&serialized).expect("deserialization should succeed");

        let metadata = CommitMetadata::new(log, lsn, vol_pages, commit_pages);
        assert!(
            deserialized.verify(&hash, &metadata, &[(pageidx!(1), &pages[0].1)]),
            "deserialized proof should still verify"
        );
    }

    #[test]
    fn test_merkle_proof_wrong_page_fails() {
        let log = LogId::random();
        let lsn = lsn!(1);
        let vol_pages = PageCount::new(2);
        let commit_pages = PageCount::new(2);

        let pages = vec![
            (pageidx!(1), Page::test_filled(0xAA)),
            (pageidx!(2), Page::test_filled(0xBB)),
        ];

        let mut builder = CommitHashBuilder::new(log.clone(), lsn, vol_pages, commit_pages);
        for (idx, page) in &pages {
            builder.write_page(*idx, page);
        }
        let (hash, tree) = builder.build_with_tree();

        // Generate proof for page 1
        let proof = tree
            .proof(&[pageidx!(1)])
            .expect("proof generation should succeed");

        let metadata = CommitMetadata::new(log, lsn, vol_pages, commit_pages);

        // Try to verify with wrong page data
        let wrong_page = Page::test_filled(0xCC);
        assert!(
            !proof.verify(&hash, &metadata, &[(pageidx!(1), &wrong_page)]),
            "proof should NOT verify with wrong page data"
        );
    }
}
