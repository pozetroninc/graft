# Merkle Verification System

## Overview

Graft's Merkle verification system provides end-to-end data integrity for
pages stored on untrusted remotes. Every page pushed to a remote is bound
into a per-commit Merkle tree, producing a `CommitHash` that acts as a
tamper-evident seal over the commit's contents. On the read path, individual
pages are verified against pre-computed leaf hashes without needing to
reconstruct the entire tree.

The system answers a simple question: **"Is this page the same data that was
originally committed?"** It does so with three properties:

1. **Per-page verification** -- each page read is checked against its BLAKE3
   leaf hash, catching corruption or substitution at the individual page level.
2. **Commit binding** -- all leaf hashes roll up into a Merkle root that is
   bound to commit metadata (LogId, LSN, page counts), producing a single
   `CommitHash`. After hydration, the full `CommitHash` is recomputed and
   verified.
3. **Anti-stripping** -- once leaf hashes appear on a log, subsequent commits
   without them are rejected, preventing a compromised remote from silently
   downgrading integrity protection.

## Threat Model

### Defended Against

- **Storage corruption** -- bit rot, disk errors, or bugs in the storage
  backend are caught by leaf hash mismatches on read.
- **Remote tampering** -- a compromised or malicious remote cannot substitute,
  modify, or reorder pages without detection. The `CommitHash` binds page
  content to commit identity (LogId + LSN + page counts).
- **Integrity stripping** -- once a client has seen leaf hashes on a log, a
  remote cannot strip them from future commits. The trust-on-first-use (TOFU)
  boundary and the `leaf_hashes_required` in-band flag both prevent this.
- **Page reordering** -- each leaf hash includes the `PageIdx` in its
  preimage, so swapping two pages produces different hashes.

### Not Defended Against

- **First-fetch compromise (TOFU gap)** -- a fresh client that has never seen
  a log has no baseline. If the remote serves tampered data on the very first
  pull, the client will trust it. The `require_leaf_hashes` config flag
  mitigates this for new deployments.
- **Server-side access control** -- the system verifies integrity, not
  authorization. It does not prevent unauthorized reads or writes at the
  remote API level.
- **Denial of service** -- a remote can refuse to serve data or serve
  incomplete data. This causes fetch errors, not silent corruption.
- **Side channels** -- page sizes, access patterns, and timing are not
  protected.

## Architecture

### Leaf Hashes

Every page is hashed with BLAKE3 to produce a 32-byte leaf hash. The hash
input includes the page index to bind position:

```rust
pub fn compute_leaf_hash(pageidx: PageIdx, page: &Page) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&pageidx.to_u32().to_be_bytes());
    hasher.update(page.as_ref());
    *hasher.finalize().as_bytes()
}
```

Leaf hashes are stored in the `Commit` struct as a `LeafHashIndex` -- a flat,
sorted byte buffer where each entry is 36 bytes (4 bytes `PageIdx` + 32 bytes
hash). Binary search provides O(log n) lookup by page index.

### Merkle Tree

Leaf hashes are assembled into a Merkle tree using BLAKE3 as the hash
function (via `rs-merkle`). The tree structure enables two things:

1. Computing a single `CommitHash` that covers all pages.
2. Generating inclusion proofs for subsets of pages (used for future
   partial-verification use cases).

For empty commits (no pages), a sentinel root `BLAKE3("EMPTY_MERKLE")` is
used instead of an actual tree root.

### Two-Phase Hash Binding

The `CommitHash` is produced by hashing commit metadata together with the
Merkle root:

```
CommitHash = BLAKE3(metadata || merkle_root)

metadata = MAGIC || LogId || LSN || vol_pages || commit_pages
```

The first byte of the resulting hash is replaced with the `CommitHashPrefix`
(`'C'` = `0x43`) to make commit hashes visually distinguishable and
self-describing in their base58 encoding.

This two-phase structure (leaf hashes -> Merkle root -> `CommitHash`) means:

- **Leaf hashes** can verify individual pages on the read path without
  touching other pages or reconstructing the tree.
- **CommitHash** verifies the entire commit after hydration, when all pages
  are locally available.

The `CommitHashBuilder` enforces that pages are written in sorted order by
`PageIdx`, ensuring deterministic tree construction:

```rust
pub fn write_page(&mut self, pageidx: PageIdx, page: &Page) {
    if let Some(last_pageidx) = self.last_pageidx.replace(pageidx) {
        assert!(pageidx > last_pageidx,
            "Pages must be written in order by pageidx.");
    }
    self.leaves.push(compute_leaf_hash(pageidx, page));
    self.leaf_page_indices.push(pageidx);
}
```

### Merkle Inclusion Proofs

The system supports generating and verifying Merkle inclusion proofs for
arbitrary subsets of pages via `CommitMerkleTree::proof()` and
`MerkleInclusionProof::verify()`. Proofs are serializable to bytes for
transport. Verification reconstructs the Merkle root from the proof and the
provided page data, then checks that `BLAKE3(metadata || reconstructed_root)`
matches the expected `CommitHash`.

## Verification Points

Verification happens at three distinct points in the data lifecycle:

### 1. Fetch-Time Verification

When a segment frame is fetched from the remote (`FetchSegment` action),
each page is verified against its leaf hash before being written to local
storage:

```rust
// In FetchSegment::run()
for (pageidx, page) in pageidxs.zip(pages) {
    if !self.leaf_hashes.is_empty() {
        let expected = self.leaf_hashes.get(pageidx).ok_or_else(|| {
            LogicalErr::MissingLeafHash { sid, pageidx }
        })?;
        let actual = compute_leaf_hash(pageidx, &page);
        if actual != expected {
            return Err(LogicalErr::PageIntegrity {
                sid, pageidx, expected, actual,
            }.into());
        }
    }
    batch.write_page(sid, pageidx, page);
}
```

This is the primary defense against remote tampering. Pages that fail
verification are never written to local storage.

### 2. Cache-Hit Verification

When a page is read from local storage (cache hit in `Runtime::read_page`),
it is verified against its leaf hash:

```rust
// In Runtime::read_page()
if !commit.leaf_hashes.is_empty() {
    let expected = commit.leaf_hashes.get(pageidx).ok_or_else(|| {
        LogicalErr::MissingLeafHash { sid, pageidx }
    })?;
    let actual = compute_leaf_hash(pageidx, &page);
    if actual != expected {
        return Err(LogicalErr::PageIntegrity {
            sid, pageidx, expected, actual,
        }.into());
    }
}
```

This catches local storage corruption (bit rot, disk errors) and provides
defense-in-depth in case a page was somehow written to storage without
fetch-time verification.

If `require_leaf_hashes` is enabled and the commit has no leaf hashes, the
read is rejected with `LogicalErr::MissingLeafHashes`.

### 3. Post-Hydrate CommitHash Verification

After `snapshot_hydrate()` downloads all missing pages for a snapshot, the
runtime recomputes `CommitHash` for every commit by reading all pages back
from storage and feeding them through `CommitHashBuilder`:

```rust
// In Runtime::verify_snapshot_commit_hashes()
for commit in reader.commits(snapshot) {
    // ... rebuild CommitHashBuilder from stored pages ...
    let recomputed = builder.build();
    if &recomputed != commit_hash {
        return Err(LogicalErr::CommitHashMismatch {
            log, lsn, expected, actual,
        }.into());
    }
}
```

This is the strongest verification: it proves that the complete set of pages
for a commit matches the `CommitHash` that was recorded when the commit was
originally pushed. It catches any inconsistency that per-page leaf hash
checks might miss (e.g., a missing page, a page assigned to the wrong
commit, or metadata tampering).

This verification is only possible when all pages for a commit are locally
available, which is why it runs after hydration rather than on every read.

## Trust Boundary

### Trust-on-First-Use (TOFU)

The first time a client pulls a log, it has no prior knowledge of whether
that log should contain leaf hashes. The client records the first LSN where
it observes leaf hashes as the **TOFU boundary** (`leaf_hash_min_lsn` on the
`Volume`). From that point forward, any commit at or above that LSN without
leaf hashes is rejected.

The boundary is established in two places:

- **On pull**: after fetching the log, the runtime scans for the first commit
  with leaf hashes and persists the boundary via `set_leaf_hash_min_lsn`.
- **On push**: after a successful push (which always includes leaf hashes),
  the boundary is set to the pushed LSN.

### Anti-Stripping Defenses

Three mechanisms prevent a compromised remote from stripping leaf hashes:

1. **TOFU boundary** (`leaf_hash_min_lsn` on `Volume`) -- persisted locally
   per volume. Once set, commits at or above that LSN without leaf hashes
   cause `LogicalErr::MissingLeafHashes` during fetch.

2. **In-band flag** (`leaf_hashes_required` on `Commit`) -- set to `true` on
   every pushed commit. When a client sees this flag during fetch, it
   establishes the TOFU boundary if not already set. This protects new
   clients pulling an existing log: the flag is embedded in the commit stream
   itself, so a remote cannot strip it without altering the commit (which
   would break the `CommitHash`).

3. **Config flag** (`require_leaf_hashes`) -- when enabled, the client
   requires leaf hashes on all commits regardless of TOFU state. This
   eliminates the TOFU gap entirely but rejects legacy commits that predate
   the leaf hash feature.

### Enforcement in FetchLog

The `FetchLog` action enforces the trust boundary on every fetched commit:

```rust
// In FetchLog::run()
if let Some(min_lsn) = leaf_hash_min {
    if commit.lsn >= min_lsn && commit.leaf_hashes.is_empty() {
        return Err(LogicalErr::MissingLeafHashes {
            log, lsn: commit.lsn, min_lsn,
        }.into());
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
```

## Backward Compatibility

The leaf hash system was introduced after the initial release. Legacy commits
(created before the feature existed) have an empty `LeafHashIndex` and no
`leaf_hashes_required` flag. The system handles these gracefully:

- **Empty leaf hashes are allowed below the TOFU boundary.** If no boundary
  has been established, legacy commits pass through without verification.
- **The TOFU boundary only moves forward.** Once established, it never
  regresses, so legacy commits at lower LSNs remain valid.
- **`require_leaf_hashes` breaks backward compatibility intentionally.** When
  this config flag is set, all commits must have leaf hashes. This is
  appropriate for new deployments but will reject legacy commits.
- **New pushes always include leaf hashes.** The `RemoteCommit` action
  unconditionally computes leaf hashes and sets `leaf_hashes_required = true`
  on every pushed commit, ensuring that the log transitions to full coverage
  over time.

## Performance

### Compute Overhead

- **BLAKE3 hashing** -- each page (4 KiB by default) is hashed once during
  push and once on each read. BLAKE3 is designed for speed: on modern x86
  hardware it processes data at memory bandwidth (~5-7 GB/s single-threaded).
  The per-page overhead is on the order of microseconds.
- **Merkle tree construction** -- built once per commit during push. Cost is
  O(n) in the number of pages, dominated by leaf hashing.
- **Post-hydrate verification** -- reads all pages for each commit in the
  snapshot. This is a one-time cost after hydration, not on the hot path.

### Storage Overhead

- **`LeafHashIndex`** -- 36 bytes per page (4 bytes `PageIdx` + 32 bytes
  hash), stored in the `Commit` protobuf message. For a 1 GiB volume with
  4 KiB pages (262,144 pages), this adds ~9 MiB to the commit metadata.
- **`CommitHash`** -- 32 bytes per commit, negligible.
- **Merkle tree** -- not persisted. Rebuilt on demand from leaf hashes when
  needed for proof generation.

### Read-Path Cost

On the read hot path (`read_page`), verification requires:

1. One binary search in the `LeafHashIndex` (O(log n) comparisons on 36-byte
   entries).
2. One BLAKE3 hash of `4 + PAGE_SIZE` bytes.
3. One 32-byte comparison.

This is lightweight relative to the cost of a page cache miss (which
involves a remote fetch, decompression, and I/O).

## Configuration

### `require_leaf_hashes`

A boolean flag set in `GraftConfig`:

```rust
pub struct GraftConfig {
    // ...
    /// if true, all commits must include leaf hashes --
    /// the TOFU grace period is eliminated
    #[serde(default)]
    pub require_leaf_hashes: bool,
}
```

When `true`:

- The `leaf_hash_min_lsn` passed to `FetchLog` is forced to `LSN::FIRST`,
  meaning every commit on every log must have leaf hashes.
- `read_page` rejects any page read from a commit without leaf hashes.

When `false` (default):

- The TOFU mechanism is used. Legacy commits without leaf hashes are accepted
  until the boundary is established.

This flag is appropriate for:

- **New deployments** where no legacy commits exist.
- **High-security environments** where the TOFU gap is unacceptable.

## Limitations

### Lazy Loading and CommitHash Verification

Graft supports lazy page loading: pages are fetched on demand rather than
all at once. The `CommitHash` covers all pages in a commit, so it can only
be verified when every page is locally available. The per-page leaf hash
check provides integrity on the lazy path, but the full `CommitHash`
verification only runs after explicit hydration (`snapshot_hydrate`).

An application that reads only a subset of pages will get per-page integrity
from leaf hashes but will not verify the `CommitHash` for that commit.

### TOFU Gap for Fresh Clients

A client connecting to a log for the first time has no prior state. If the
remote is compromised at that exact moment, it could serve commits without
leaf hashes (and without the `leaf_hashes_required` flag), and the client
would accept them. Subsequent pushes from the client will establish the
boundary, protecting future fetches, but the initial pull is vulnerable.

Mitigation: enable `require_leaf_hashes` in the client config for
environments where this gap is unacceptable.

### LeafHashIndex Size

The `LeafHashIndex` scales linearly with the number of pages in a commit.
For very large commits (hundreds of thousands of pages), the index adds
meaningful overhead to commit metadata size. This is a deliberate trade-off:
the per-page granularity enables verification on lazy reads without
requiring the full Merkle tree.

### No Cross-Commit Verification

Each `CommitHash` covers a single commit's pages. There is no chained hash
linking commits together (e.g., each commit hashing the previous commit's
hash). Commit ordering and log integrity are handled by the remote's LSN
sequencing, not by the Merkle system.
