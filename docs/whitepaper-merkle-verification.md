# Verifiable Page Integrity for Lazy-Replicated SQLite Databases

**Neil Davenport**

*Orbitinghail*

---

## Abstract

Lazy partial replication enables edge applications to synchronize only the
database pages they access, dramatically reducing bandwidth and startup latency.
However, this replication model introduces an integrity gap: when individual
pages are fetched on demand from untrusted remote storage, clients cannot verify
that the data they receive is authentic without downloading the entire database.
We present a three-layer Merkle verification system for Graft, an open-source
transactional storage engine for SQLite databases. Our construction provides
per-page integrity verification on the read path using BLAKE3 leaf hashes,
full-commit binding via a Merkle tree whose root is incorporated into the
CommitHash, and a trust-on-first-use boundary with anti-stripping defenses that
prevent downgrade attacks. The system adds 36 bytes of storage overhead per page
in the commit metadata, introduces no additional network round-trips, and
imposes sub-microsecond verification latency per page read on modern hardware.
We analyze the security properties of each layer and demonstrate that the
construction detects storage-level corruption, transport-level tampering, and
application-level forgery while maintaining backward compatibility with
pre-existing unverified commits.

---

## 1. Introduction

### 1.1 Motivation

Edge and mobile applications increasingly require local database replicas to
provide offline-first experiences and low-latency reads. Traditional replication
strategies---full replication, log shipping, or statement-based
replication---require clients to receive and process the entire dataset or a
complete change stream before the replica is usable. This is impractical for
resource-constrained environments where bandwidth is expensive and storage is
limited.

Graft [1] addresses this problem through *lazy partial replication*: a
client-side storage engine for SQLite that replicates only the database pages
actually accessed by the application. Metadata (commit records, page indices,
segment maps) is replicated eagerly, but page data is fetched on demand from
remote object storage. This design enables instant read replicas---a client can
begin querying a multi-gigabyte database after downloading only a few kilobytes
of metadata, with individual 4 KB pages fetched as SQL queries touch them.

### 1.2 The Integrity Gap

Lazy partial replication introduces a fundamental integrity challenge that does
not arise in full-replication systems. In a traditional replicated database,
the client receives a complete, self-consistent snapshot and can verify its
integrity using a single checksum over the entire dataset. In a lazy system,
the client may hold only a sparse subset of pages at any given time, and each
page may have been fetched independently over a different network path or at a
different point in time.

This creates multiple attack surfaces:

1. **Storage-level corruption.** Silent bit-rot in object storage or on local
   disk can corrupt individual pages without any higher-level detection.

2. **Transport-level tampering.** A compromised CDN, proxy, or gateway can
   substitute page data in transit.

3. **Application-level forgery.** A compromised remote storage service can
   serve internally consistent but forged data---pages that are individually
   well-formed but do not correspond to any legitimate committed state.

4. **Integrity stripping.** An attacker who controls the commit metadata
   channel can strip verification data from commits, forcing clients to accept
   unverified pages.

Traditional approaches to database integrity---full-file checksums, write-ahead
log checksums, or page-level CRCs embedded in the storage format---are
insufficient. Full-file checksums require all data to be present. WAL checksums
verify ordering but not content authenticity. Page-level CRCs detect accidental
corruption but not intentional tampering, as an attacker who can modify page
data can also recompute the CRC.

### 1.3 Contributions

We present a Merkle-based verification system that addresses all four attack
surfaces while preserving the performance characteristics that make lazy
replication practical. Our contributions are:

- A **per-page leaf hash scheme** using BLAKE3 [2] with domain-separated
  inputs, enabling constant-time verification of individual pages on the read
  path without requiring any other pages to be present.

- A **Merkle tree construction** that binds all page hashes for a commit into
  a single CommitHash, enabling full-commit verification after hydration and
  supporting Merkle inclusion proofs for arbitrary page subsets.

- A **trust-on-first-use (TOFU) boundary** with three independent
  anti-stripping mechanisms that prevent downgrade attacks against the
  verification system itself.

- An analysis of the security properties and performance characteristics of
  the construction in the context of a production storage engine.

### 1.4 Organization

Section 2 formalizes the problem. Section 3 surveys related work. Section 4
presents the construction in detail. Section 5 analyzes the security
properties. Section 6 evaluates performance. Section 7 discusses limitations
and future work. Section 8 concludes.

---

## 2. Problem Statement

### 2.1 System Model

We consider a storage system with the following components:

- **Volume.** A logical database identified by a `VolumeId`. Each volume is
  backed by a *local log* and a *remote log*.

- **Log.** An append-only sequence of commits, identified by a `LogId`. Each
  commit is assigned a monotonically increasing *Log Sequence Number* (LSN).

- **Commit.** A record at a specific LSN containing:
  - The `LogId` and `LSN` identifying this commit.
  - A `PageCount` representing the total number of pages in the volume at this
    point.
  - An optional `CommitHash` binding the commit to its page data.
  - A `SegmentIdx` mapping `PageIdx` values to their locations in remote
    object storage.
  - A `LeafHashIndex` containing per-page integrity hashes.
  - A `leaf_hashes_required` flag for in-band anti-stripping.

- **Segment.** A blob in object storage containing one or more compressed page
  frames. Segments are immutable once written.

- **Page.** A fixed-size (4096-byte) unit of data, the fundamental unit of
  both storage and replication.

### 2.2 Threat Model

We consider an adversary `A` who may control one or more of the following:

- **T1 (Storage corruption).** `A` can modify individual bytes in stored
  segments or local page cache, modeling disk faults, firmware bugs, or
  bit-rot.

- **T2 (Transport tampering).** `A` can intercept and modify page data in
  transit between object storage and the client, modeling a compromised CDN,
  proxy, or man-in-the-middle.

- **T3 (Remote compromise).** `A` controls the remote storage service and can
  serve arbitrary commit metadata and page data, subject to consistency
  constraints imposed by any external anchors (e.g., a previously observed
  CommitHash).

- **T4 (Integrity stripping).** `A` can modify commit metadata to remove
  verification data (leaf hashes, CommitHash), attempting to force the client
  into an unverified mode.

We assume the client runtime is trusted and that BLAKE3 provides collision
resistance, second-preimage resistance, and pseudorandomness as specified [2].

### 2.3 Security Goals

Given the threat model above, we require the following properties:

**Property 1 (Page integrity).** For any page `p` at index `i` in a commit
with leaf hashes, the client detects with overwhelming probability if the page
data has been modified after the commit was created.

Formally: given a commit `C` with `LeafHashIndex` `L`, for any page index `i`
such that `L[i]` exists, and any page `p' != p` where `p` is the authentic
page, `Pr[H(i || p') = L[i]] <= 2^{-256}` where `H` is BLAKE3.

**Property 2 (Commit binding).** The `CommitHash` uniquely identifies the
complete set of pages in a commit. Any modification to any page, the page
count, the log identity, or the LSN produces a different `CommitHash` with
overwhelming probability.

Formally: for any two distinct inputs `(LogId, LSN, PageCount, Pages)` and
`(LogId', LSN', PageCount', Pages')`, the probability that the resulting
`CommitHash` values collide is at most `2^{-248}` (accounting for the
fixed prefix byte).

**Property 3 (Anti-stripping).** Once a client has observed leaf hashes on a
log, subsequent commits on that log without leaf hashes are rejected. This
property is enforced through multiple independent mechanisms to resist
degradation.

**Property 4 (Domain separation).** Leaf hashes for different page indices
are computed over distinct domains, preventing an attacker from substituting
one page's data at a different index without detection.

**Property 5 (Backward compatibility).** The system gracefully handles
commits created before the verification system was deployed, verifying what
is available without rejecting legacy data.

---

## 3. Related Work

### 3.1 Merkle Trees

Merkle trees [3] are the foundational authenticated data structure. A binary
tree of hash values allows `O(log n)` verification that a leaf is a member of
a committed set, given the root hash. Our construction uses Merkle trees in the
standard binary form, with BLAKE3 as the internal hash function.

### 3.2 Authenticated Data Structures

Authenticated data structures (ADS) generalize Merkle trees to support
efficient verification of queries over structured data [4]. Examples include
authenticated skip lists [5], authenticated red-black trees, and various
B-tree-based constructions. Our system is simpler than a general ADS because
the page set within a single commit is static (commits are immutable once
created), eliminating the need for authenticated update operations.

### 3.3 Certificate Transparency

Certificate Transparency (CT) [6] uses Merkle trees to provide a publicly
auditable log of TLS certificates. CT's append-only log structure and
consistency proofs between log states are conceptually similar to our
commit log model. However, CT operates at the level of entire certificates
and focuses on public verifiability, whereas our system operates at the
page level and focuses on client-side verification against a known root.

### 3.4 SUNDR and Fork Consistency

SUNDR [7] addresses the problem of detecting server misbehavior in a
network file system. SUNDR achieves fork consistency: if a malicious server
presents inconsistent views to different clients, those clients' views are
permanently forked and will eventually be detected. While Graft's remote log
model faces similar challenges, our current construction focuses on
single-client verification against known commit hashes rather than
multi-client consistency detection.

### 3.5 CONIKS and Key Transparency

CONIKS [8] and the broader key transparency effort [9] use Merkle prefix trees
to provide verifiable key directories. These systems share our goal of enabling
clients to verify that a server is not presenting forged data. However, key
transparency systems must handle a dynamic, multi-writer setting with privacy
constraints, whereas our system operates on a simpler model of immutable
commits from a single writer per log.

### 3.6 BLAKE3

BLAKE3 [2] is a cryptographic hash function based on the Bao tree hashing
mode, offering parallelizable computation and a security level of 256 bits
against collision, preimage, and length-extension attacks. We selected BLAKE3
for its combination of cryptographic strength and exceptional throughput
(exceeding 1 GB/s on a single core for large inputs via SIMD), which is
critical for minimizing the overhead of per-page verification on the hot
read path.

### 3.7 SQLite Integrity Mechanisms

SQLite itself provides several integrity mechanisms: page-level checksums in
WAL mode (via `PRAGMA wal_autocheckpoint`), the `PRAGMA integrity_check`
command for full-database verification, and write-ahead log checksums for
crash recovery. These mechanisms are designed for single-node crash consistency
and do not address the distributed trust model inherent in lazy replication.
Graft's verification system operates at the replication layer, complementing
rather than replacing SQLite's internal integrity checks.

---

## 4. Construction

Our verification system consists of three layers, each addressing a subset of
the threat model. The layers are designed to compose: Layer 1 provides
immediate per-page verification on every read, Layer 2 provides full-commit
verification after all pages are available, and Layer 3 prevents an attacker
from disabling Layers 1 and 2.

### 4.1 Layer 1: Per-Page Leaf Hashes

#### 4.1.1 Leaf Hash Computation

For each page in a commit, we compute a leaf hash that binds the page content
to its index within the volume:

```
Algorithm 1: ComputeLeafHash
Input:  pageidx (PageIdx), page (4096 bytes)
Output: leaf_hash (32 bytes)

1. Let hasher <- BLAKE3.new()
2. hasher.update(pageidx.to_u32().to_be_bytes())    // 4 bytes, big-endian
3. hasher.update(page)                               // 4096 bytes
4. Return hasher.finalize()                          // 32 bytes
```

The `PageIdx` is encoded as a 4-byte big-endian unsigned integer and prepended
to the page data before hashing. This provides domain separation (Section 5.4):
the same page content at two different indices produces different leaf hashes,
preventing index-swapping attacks.

The choice of big-endian encoding for the `PageIdx` prefix is deliberate.
Big-endian encoding produces a fixed-width 4-byte representation regardless
of the index value, eliminating any ambiguity in the hash input boundary
between the index and the page data. Since `PageIdx` is always exactly 4 bytes
and page data is always exactly 4096 bytes, the total input to the hash
function is always exactly 4100 bytes, providing unambiguous parsing of the
hash preimage.

#### 4.1.2 LeafHashIndex

Leaf hashes are stored in a compact sorted index structure within the `Commit`
record:

```
Algorithm 2: LeafHashIndex Structure
Format: Flat byte buffer, each entry is 36 bytes:
        [4 bytes: PageIdx (big-endian)] [32 bytes: BLAKE3 leaf hash]
Invariant: Entries are strictly sorted by PageIdx.
Lookup: O(log n) via binary search.
```

The `LeafHashIndex` is serialized as a contiguous byte buffer where each entry
is exactly 36 bytes (4 bytes for the `PageIdx` in big-endian format, followed
by 32 bytes for the BLAKE3 leaf hash). Entries are maintained in strictly
ascending order by `PageIdx`, enabling efficient `O(log n)` lookup via binary
search.

The sorted invariant is validated at deserialization time: when a
`LeafHashIndex` is decoded from the wire format, the deserializer verifies that
all entries are strictly ordered. An unsorted index is rejected as invalid,
because binary search over an unsorted array could silently fail to find an
entry that is present, effectively bypassing per-page integrity checks for
those pages.

#### 4.1.3 Read-Path Verification

On every page read, the runtime verifies the page against its leaf hash:

```
Algorithm 3: ReadPageVerified
Input:  snapshot, pageidx
Output: page or error

1.  commit <- SearchPage(snapshot, pageidx)
2.  If commit is None: return Page.EMPTY
3.  page <- ReadPageFromStorage(commit.segment_id, pageidx)
4.  If page is None:
5.      FetchSegmentFromRemote(commit, pageidx)  // includes verification
6.      page <- ReadPageFromStorage(commit.segment_id, pageidx)
7.  If require_leaf_hashes AND commit.leaf_hashes.is_empty():
8.      return Error(MissingLeafHashes)
9.  If NOT commit.leaf_hashes.is_empty():
10.     expected <- commit.leaf_hashes.get(pageidx)
11.     If expected is None: return Error(MissingLeafHash)
12.     actual <- ComputeLeafHash(pageidx, page)
13.     If actual != expected: return Error(PageIntegrity)
14. Return page
```

Verification is performed both when pages are read from local storage
(Algorithm 3, lines 9--13) and when pages are fetched from the remote
(during `FetchSegmentFromRemote` at line 5). This dual verification ensures
that corruption is detected regardless of whether it occurs in transit from
the remote, at rest in local storage, or during any intermediate processing.

When a page is fetched from remote storage, the `FetchSegment` action
decompresses the segment frame and verifies each page against the leaf hashes
*before* writing the page to local storage:

```
Algorithm 4: FetchSegmentVerified
Input:  segment_range, leaf_hashes
Output: success or error

1. bytes <- Remote.GetSegmentRange(segment_range)
2. For each (pageidx, page) in decompress(bytes):
3.     If NOT leaf_hashes.is_empty():
4.         expected <- leaf_hashes.get(pageidx)
5.         If expected is None: return Error(MissingLeafHash)
6.         actual <- ComputeLeafHash(pageidx, page)
7.         If actual != expected: return Error(PageIntegrity)
8.     WritePageToLocalStorage(segment_id, pageidx, page)
```

This verify-before-store pattern ensures that corrupted or tampered pages
never enter the local storage layer. A page that fails verification at fetch
time is rejected immediately, and the fetch operation fails with a
`PageIntegrity` error.

### 4.2 Layer 2: Merkle Tree and CommitHash

#### 4.2.1 Merkle Tree Construction

The `CommitHash` is computed by building a Merkle tree over the leaf hashes of
all pages in a commit, then binding the Merkle root to commit metadata:

```
Algorithm 5: BuildCommitHash
Input:  log (LogId), lsn (LSN), vol_pages (PageCount),
        commit_pages (PageCount), pages [(PageIdx, Page)]
Output: commit_hash (CommitHash)

1.  metadata <- COMMIT_HASH_MAGIC || log || CBE64(lsn)
                || vol_pages.to_be_bytes() || commit_pages.to_be_bytes()
2.  leaves <- []
3.  For each (pageidx, page) in pages (sorted by pageidx):
4.      leaves.append(ComputeLeafHash(pageidx, page))
5.  If leaves is empty:
6.      merkle_root <- BLAKE3("EMPTY_MERKLE")
7.  Else:
8.      tree <- MerkleTree.from_leaves(leaves)
9.      merkle_root <- tree.root()
10. final_hasher <- BLAKE3.new()
11. final_hasher.update(metadata)
12. final_hasher.update(merkle_root)
13. hash <- final_hasher.finalize()
14. hash[0] <- 'C'  // CommitHashPrefix
15. Return CommitHash(hash)
```

Several design choices merit discussion:

**Metadata binding (line 1).** The metadata bytes include a 4-byte magic
number (`0x68A41930`), the 16-byte `LogId`, an 8-byte canonical big-endian
encoding of the `LSN`, a 4-byte volume page count, and a 4-byte commit page
count. This binding ensures that the same set of pages committed to different
logs, at different LSNs, or with different page counts produces a different
`CommitHash`. The magic number provides domain separation from other uses of
BLAKE3 within the system.

**Empty tree sentinel (lines 5--6).** Commits that contain no pages (e.g.,
commits that only update the volume's page count) use the hash of the string
`"EMPTY_MERKLE"` as the Merkle root. This avoids special-casing empty trees
in the final hash computation and ensures that empty commits still have
well-defined, deterministic `CommitHash` values.

**Prefix byte (line 14).** The first byte of the `CommitHash` is overwritten
with the ASCII byte `'C'` (0x43). This serves as a type tag in the Graft
identifier system, enabling `CommitHash` values to be distinguished from other
identifier types (e.g., `LogId`, `VolumeId`, `SegmentId`) when encoded in
base58. The prefix consumes 8 bits of the hash output, reducing the effective
collision resistance from `2^{-256}` to `2^{-248}`, which remains
overwhelmingly sufficient for all practical purposes.

**Page ordering invariant (line 3).** Pages must be written to the
`CommitHashBuilder` in strictly ascending order by `PageIdx`. This is enforced
at runtime via a panic on out-of-order insertions. The ordering invariant
ensures that the Merkle tree construction is deterministic: given the same set
of pages, the tree is always built in the same order, producing the same root.

#### 4.2.2 CommitHash Encoding

The `CommitHash` is a 32-byte value that is externally represented in base58
encoding, producing a 44-character string. The fixed output length is
guaranteed by the `CommitHashPrefix` byte occupying the most significant
position: since the prefix `'C'` (0x43) is nonzero, the base58 encoding
always uses the full 44 characters.

Example `CommitHash` values from the test suite:

- Empty commit: `5XLfiYoRNuPT236PErRR4MDrx3SRnoCjtzg2tDwAoBf8`
- Single page: `5XZNaGb3NvnQbzY4pnqKT64aJq9XiY3whDWGjCj3KLNt`
- Two pages: `5ZGDFuqfYg5tEdN8giQVf8m9eppf8yPmAgwhhf9WdFxF`

#### 4.2.3 Merkle Inclusion Proofs

The construction supports generating and verifying Merkle inclusion proofs for
arbitrary subsets of pages within a commit. This enables a verifier who holds
the `CommitHash` to confirm that specific pages belong to the commit without
possessing all pages.

```
Algorithm 6: GenerateInclusionProof
Input:  tree (CommitMerkleTree), page_indices [PageIdx]
Output: proof (MerkleInclusionProof)

1. For each pidx in page_indices:
2.     pos <- BinarySearch(tree.leaf_page_indices, pidx)
3.     If pos is None: return Error(PageNotFound)
4. Sort and deduplicate positions
5. proof_bytes <- tree.internal_tree.proof(positions).to_bytes()
6. Return MerkleInclusionProof {
       proof_bytes, leaf_positions, leaf_page_indices, total_leaves
   }
```

```
Algorithm 7: VerifyInclusionProof
Input:  proof, commit_hash, metadata, page_entries [(PageIdx, Page)]
Output: boolean

1.  If |page_entries| != |proof.leaf_page_indices|: return false
2.  For each expected_pidx in proof.leaf_page_indices:
3.      Find (pidx, page) in page_entries where pidx == expected_pidx
4.      If not found: return false
5.      leaf_hashes.append(ComputeLeafHash(expected_pidx, page))
6.  merkle_proof <- Deserialize(proof.proof_bytes)
7.  computed_root <- merkle_proof.root(proof.leaf_positions,
                                       leaf_hashes, proof.total_leaves)
8.  final_hasher <- BLAKE3.new()
9.  final_hasher.update(metadata)
10. final_hasher.update(computed_root)
11. hash <- final_hasher.finalize()
12. hash[0] <- 'C'
13. Return CommitHash(hash) == commit_hash
```

The inclusion proof verification (Algorithm 7) reconstructs the exact
computation performed during `CommitHash` construction (Algorithm 5), but
uses the Merkle proof to derive the root from a subset of leaves rather than
computing it from all leaves. If the reconstructed `CommitHash` matches the
known value, the verifier has cryptographic assurance that the provided pages
are members of the committed set.

#### 4.2.4 Proof Serialization

Merkle inclusion proofs are serialized to a compact binary format:

```
Proof Wire Format:
  [4 bytes] total_leaves (u32, big-endian)
  [4 bytes] num_positions (u32, big-endian)
  For each position:
    [4 bytes] leaf_position (u32, big-endian)
    [4 bytes] page_index (u32, big-endian)
  [4 bytes] proof_bytes_length (u32, big-endian)
  [N bytes] proof_bytes (Merkle proof hashes)
```

The serialization includes both the leaf positions (indices into the Merkle
tree's leaf array) and the corresponding `PageIdx` values, enabling the
verifier to reconstruct leaf hashes from page data without knowing the tree's
internal leaf ordering.

#### 4.2.5 Full-Commit Verification After Hydration

When a client hydrates a snapshot (downloads all missing pages), the runtime
performs full-commit verification by recomputing the `CommitHash` from the
stored page data:

```
Algorithm 8: VerifySnapshotCommitHashes
Input:  snapshot
Output: success or error

1. For each commit in snapshot.commits:
2.     If commit.commit_hash is None: continue
3.     If commit.segment_idx is None: continue
4.     builder <- CommitHashBuilder.new(commit.log, commit.lsn,
                                        commit.page_count,
                                        commit.segment_idx.page_count)
5.     For each pageidx in commit.segment_idx.pageset (sorted):
6.         page <- ReadPage(commit.segment_id, pageidx)
7.         If page is None: return Error(PageNotFound)
8.         builder.write_page(pageidx, page)
9.     recomputed <- builder.build()
10.    If recomputed != commit.commit_hash:
11.        return Error(CommitHashMismatch)
```

This full verification closes the gap inherent in lazy loading: while
individual page reads verify against leaf hashes, only full-commit
verification confirms that the Merkle tree structure itself is authentic. A
sophisticated attacker who controls the remote could potentially serve a
consistent set of fake leaf hashes and matching pages, but only if they can
also forge the `CommitHash` (see Section 5.2).

### 4.3 Layer 3: Trust-on-First-Use Boundary

#### 4.3.1 The Stripping Problem

Layers 1 and 2 provide strong integrity guarantees, but only when verification
data is present. An attacker who controls the commit metadata channel could
strip the `LeafHashIndex` and `CommitHash` from commits, causing the client
to silently fall back to unverified mode. This is analogous to the SSL
stripping attack in web security: the protection mechanism is defeated not by
breaking the cryptography but by preventing it from being applied.

#### 4.3.2 Trust Boundary Establishment

We address the stripping problem through a trust-on-first-use (TOFU) boundary
with three independent enforcement mechanisms:

**Mechanism 1: Per-volume persisted boundary.** Each `Volume` record includes
a `leaf_hash_min_lsn` field. When a client first observes a commit with leaf
hashes on a volume's remote log, it records that commit's LSN as the boundary.
The boundary is persisted to local storage and is never lowered:

```
Algorithm 9: EstablishTrustBoundary
Input:  volume, commit

1. If volume.leaf_hash_min_lsn is None
   AND NOT commit.leaf_hashes.is_empty():
2.     volume.leaf_hash_min_lsn <- commit.lsn
3.     PersistVolume(volume)
```

Once established, any commit at or above the boundary LSN that lacks leaf
hashes is rejected:

```
Algorithm 10: EnforceTrustBoundary
Input:  leaf_hash_min_lsn, commit
Output: success or error

1. If leaf_hash_min_lsn is Some(min_lsn):
2.     If commit.lsn >= min_lsn AND commit.leaf_hashes.is_empty():
3.         return Error(MissingLeafHashes)
```

The boundary is persisted on the client side, so an attacker who only
controls the remote cannot lower or reset it. A client that has ever seen
leaf hashes on a log will always require them going forward.

**Mechanism 2: Client-side configuration flag.** The Graft runtime accepts a
`require_leaf_hashes` configuration parameter. When enabled, the runtime
requires leaf hashes on *all* commits regardless of the TOFU boundary,
effectively setting the boundary to `LSN::FIRST`:

```
Algorithm 11: ConfigurationEnforcement
Input:  require_leaf_hashes flag, leaf_hash_min_lsn

1. If require_leaf_hashes:
2.     effective_min <- LSN::FIRST
3. Else:
4.     effective_min <- leaf_hash_min_lsn
```

This mechanism provides an escape hatch for deployments that can guarantee all
commits will have leaf hashes (e.g., newly provisioned systems with no legacy
data). It also serves as a defense-in-depth measure: even if an attacker
could somehow reset the persisted boundary, the configuration flag provides
an independent enforcement point.

**Mechanism 3: In-band `leaf_hashes_required` flag.** Each `Commit` record
includes a boolean `leaf_hashes_required` field. When set to `true` on any
commit in a log, it establishes the trust boundary for that log from that
commit's LSN onward. This flag is propagated through the log: once set, it
cannot be unset.

```
Algorithm 12: InBandEnforcement
Input:  commit, leaf_hash_min (mutable)

1. If commit.leaf_hashes_required AND leaf_hash_min is None:
2.     leaf_hash_min <- Some(commit.lsn)
```

The in-band flag provides a mechanism for the data *producer* to signal that
integrity data should be present, complementing the consumer-side TOFU
boundary. An attacker who strips leaf hashes from a commit must also strip the
`leaf_hashes_required` flag from all preceding commits to avoid triggering
the boundary, significantly raising the bar for a successful stripping attack.

#### 4.3.3 Push Boundary

When a client pushes a local commit to the remote, it always includes leaf
hashes and records the pushed LSN as the trust boundary. This ensures that any
client that has ever *written* to a log establishes a trust boundary, even
before observing any remote commits:

```
Algorithm 13: PushBoundary
Input:  volume, pushed_lsn

1. SetLeafHashMinLSN(volume.vid, pushed_lsn)
```

Since every push includes leaf hashes, the pushed LSN is a valid trust
boundary. Subsequent fetches from the remote will enforce this boundary,
ensuring that an attacker cannot strip leaf hashes from commits that were
pushed with them.

---

## 5. Security Analysis

### 5.1 Single-Page Corruption Detection

**Theorem 1.** *Under the collision resistance of BLAKE3, an adversary cannot
substitute a page `p'` for the authentic page `p` at index `i` without
detection, provided the commit contains a `LeafHashIndex` entry for index `i`.*

*Proof.* The leaf hash for page `p` at index `i` is computed as
`H(i || p)` where `H` is BLAKE3 and `||` denotes concatenation. The
`LeafHashIndex` stores this hash as `L[i] = H(i || p)`.

On read, the runtime recomputes `H(i || p')` and compares it to `L[i]`.
For the substitution to go undetected, we require `H(i || p') = H(i || p)`
with `p' != p`. Since `i` is fixed and `p' != p`, the inputs `i || p'` and
`i || p` are distinct. Finding such a collision requires breaking the
collision resistance of BLAKE3, which provides `2^{128}` bits of security
against birthday attacks on 256-bit output (and `2^{256}` against targeted
second-preimage attacks).

The verification is performed at two points: when pages are fetched from the
remote (Algorithm 4, before storage) and when pages are read from local
storage (Algorithm 3, before returning to the caller). This ensures detection
regardless of whether corruption occurs in transit or at rest. `\square`

### 5.2 Full-Commit Verification

**Theorem 2.** *If a client holds the authentic `CommitHash` for a commit, and
has all pages for that commit, then full-commit verification (Algorithm 8)
detects any modification to any page, the page ordering, the page count, the
log identity, or the LSN.*

*Proof.* The `CommitHash` is computed as:

```
CommitHash = H(metadata || MerkleRoot(H(i_1 || p_1), ..., H(i_n || p_n)))
```

where `metadata` encodes the `LogId`, `LSN`, volume `PageCount`, and commit
`PageCount`.

We consider each type of modification:

**(a) Page modification.** If page `p_j` is replaced with `p'_j != p_j`,
the leaf hash `H(i_j || p'_j) != H(i_j || p_j)` with overwhelming
probability (collision resistance). This changes the Merkle root, which
changes the `CommitHash`.

**(b) Page reordering.** The `CommitHashBuilder` enforces strictly ascending
`PageIdx` order. Attempting to reorder pages would change the leaf sequence
in the Merkle tree, changing the root. Even if two pages have identical
content, their leaf hashes differ due to the `PageIdx` prefix (domain
separation).

**(c) Page count modification.** The volume `PageCount` and commit `PageCount`
are encoded in the metadata, which is an input to the final hash. Changing
either value changes the metadata, which changes the `CommitHash`.

**(d) Log identity or LSN modification.** Similarly, the `LogId` and `LSN`
are encoded in the metadata. Changing either changes the `CommitHash`.

In each case, producing the original `CommitHash` from modified inputs
requires a second-preimage attack on BLAKE3, which succeeds with probability
at most `2^{-248}` (accounting for the fixed prefix byte). `\square`

**Corollary 1.** *Full-commit verification after hydration detects any forgery
by a compromised remote that serves a consistent but inauthentic set of pages,
provided the client has previously obtained the authentic `CommitHash` through
a trusted channel.*

The trust in the `CommitHash` may be established through the commit log
(where the `CommitHash` was observed in a previously verified commit) or
through an out-of-band mechanism. The TOFU boundary (Section 4.3) ensures
that once a `CommitHash` has been observed, future commits cannot downgrade
to an unverified mode.

### 5.3 Anti-Stripping Defense

**Theorem 3.** *An attacker who can modify commit metadata but cannot modify
the client's persistent state or configuration can strip leaf hashes from at
most the first sequence of commits observed by a fresh client.*

*Proof.* We analyze each anti-stripping mechanism:

**(a) TOFU boundary.** Once the client observes any commit with leaf hashes,
it records `leaf_hash_min_lsn` in persistent local storage. Subsequent commits
at or above this LSN without leaf hashes are rejected (Algorithm 10). The
attacker cannot modify local storage (by assumption), so the boundary cannot
be lowered.

**(b) Configuration flag.** If `require_leaf_hashes` is set, the effective
boundary is `LSN::FIRST`, requiring leaf hashes on all commits. This is a
client-side configuration that the attacker cannot modify.

**(c) In-band flag.** If any commit in the log has `leaf_hashes_required =
true`, the client establishes the trust boundary at that commit's LSN. To
strip leaf hashes without triggering this mechanism, the attacker must also
strip the `leaf_hashes_required` flag from all earlier commits.

The three mechanisms are independent: defeating any one of them does not
affect the others. An attacker must simultaneously defeat all three to
successfully strip leaf hashes from a log:

1. Prevent the client from ever observing a commit with leaf hashes (to
   avoid establishing the TOFU boundary).
2. Ensure the client is not configured with `require_leaf_hashes`.
3. Strip the `leaf_hashes_required` flag from all commits.

Condition (2) is outside the attacker's control. Condition (3) requires
modifying commit metadata, but if the attacker is selectively stripping
fields, they must do so consistently across all commits, which becomes
increasingly difficult as the log grows. Condition (1) means the attack is
only viable for fresh clients that have never observed the log. `\square`

### 5.4 Domain Separation

**Theorem 4.** *An attacker cannot substitute the page at index `i` with the
page from index `j != i` without detection, even if the pages have different
contents.*

*Proof.* The leaf hash at index `i` is `H(i || p_i)` and the leaf hash at
index `j` is `H(j || p_j)`. To perform an index-swap attack, the attacker
would need the page `p_j` to satisfy `H(i || p_j) = H(i || p_i)`, which
is a second-preimage attack, or `H(i || p_j) = H(j || p_j)`, which requires
finding a value that hashes identically under two different prefixes. Both
require breaking the preimage resistance of BLAKE3.

The domain separation prefix is a fixed-width 4-byte big-endian encoding of
the `PageIdx`. The fixed width is critical: if the prefix were variable-width,
an attacker might find inputs where the boundary between prefix and page data
is ambiguous. With a fixed 4-byte prefix and a fixed 4096-byte page, the
total hash input is always exactly 4100 bytes, and the prefix-data boundary
is unambiguous. `\square`

### 5.5 Trust Boundary Monotonicity

**Property.** *The `leaf_hash_min_lsn` for a volume is monotonically
non-decreasing: once set, it is never lowered, and it can only be advanced
(never retracted) by subsequent operations.*

The implementation enforces this property through the
`set_leaf_hash_min_lsn` method, which only sets the boundary if it is
currently `None`:

```rust
pub fn set_leaf_hash_min_lsn(&self, vid: &VolumeId, min_lsn: LSN) {
    let mut volume = self.read.volume(vid)?;
    if volume.leaf_hash_min_lsn.is_none() {
        volume.leaf_hash_min_lsn = Some(min_lsn);
        self.ks().volumes.insert(vid.clone(), volume)?;
    }
}
```

This write-once semantics ensures that an attacker who gains transient control
of the client (e.g., through a compromised library version) cannot lower the
trust boundary to re-enable stripping. The boundary is a monotonic commitment:
once the client has decided to require leaf hashes from a certain LSN, that
decision is permanent.

---

## 6. Performance Analysis

### 6.1 Hash Computation Overhead

BLAKE3 is designed for high throughput, leveraging SIMD instructions and a
tree-structured internal hash mode. On modern x86-64 hardware with AVX-512
support, BLAKE3 achieves throughput exceeding 1 GB/s on a single core for
large inputs [2].

For Graft's use case, the relevant benchmark is hashing a 4100-byte input
(4-byte `PageIdx` prefix + 4096-byte page):

| Operation | Input Size | Expected Latency |
|-----------|-----------|-------------------|
| Leaf hash computation | 4100 bytes | ~1 us |
| Merkle root (100 pages) | 100 x 32 bytes | ~3 us |
| Merkle root (10,000 pages) | 10,000 x 32 bytes | ~50 us |

The per-page leaf hash computation of approximately 1 us is the dominant
verification cost on the read path, as it is performed on every page read.
This is negligible compared to typical SSD read latencies (50--100 us) or
network fetch latencies (1--100 ms).

### 6.2 Storage Overhead

The `LeafHashIndex` adds 36 bytes per page to each `Commit` record:

| Component | Size | Notes |
|-----------|------|-------|
| PageIdx (big-endian) | 4 bytes | Per entry |
| BLAKE3 leaf hash | 32 bytes | Per entry |
| **Total per page** | **36 bytes** | |

For a commit touching `n` pages, the `LeafHashIndex` adds `36n` bytes to the
commit metadata. In context:

| Scenario | Pages | LeafHashIndex Size | Page Data Size | Overhead |
|----------|-------|--------------------|----------------|----------|
| Small commit | 10 | 360 bytes | 40 KB | 0.88% |
| Medium commit | 1,000 | 35.2 KB | 3.9 MB | 0.88% |
| Large commit | 100,000 | 3.4 MB | 390 MB | 0.88% |

The overhead is a constant 0.88% of the page data size, independent of the
number of pages. This is because each 4096-byte page incurs exactly 36 bytes
of verification metadata, yielding a ratio of `36 / 4096 = 0.879%`.

The `CommitHash` itself is 32 bytes per commit (not per page), which is
negligible.

### 6.3 Read-Path Overhead

Each page read incurs the following verification overhead:

1. **Binary search** in the `LeafHashIndex`: `O(log n)` comparisons over
   4-byte keys, where `n` is the number of pages in the commit. For a commit
   with 10,000 pages, this is approximately 14 comparisons over a contiguous
   byte buffer, which is highly cache-friendly.

2. **One BLAKE3 hash** computation over 4100 bytes: approximately 1 us.

3. **One 32-byte comparison**: negligible.

The total read-path overhead is dominated by the BLAKE3 computation and is
approximately 1 us per page read. This is well within the noise floor of
typical database operations, where page reads involve at minimum a B-tree
traversal and often a disk or network I/O operation.

### 6.4 Network Overhead

A critical design property is that per-page verification requires **no
additional network round-trips**. The `LeafHashIndex` is embedded in the
`Commit` record, which is fetched as part of the metadata synchronization
that occurs before any page reads. When a page is subsequently fetched on
demand, its leaf hash is already available locally in the commit metadata.

This is in contrast to designs that store verification data separately from
commit metadata, which would require an additional fetch for the verification
data before any page can be verified.

### 6.5 Merkle Proof Size

For Merkle inclusion proofs, the proof size is `O(log n)` hashes, where `n`
is the total number of leaves (pages) in the commit:

| Total Pages | Proof Hashes (1 page) | Proof Size |
|-------------|----------------------|------------|
| 10 | 4 | ~140 bytes |
| 1,000 | 10 | ~340 bytes |
| 100,000 | 17 | ~564 bytes |
| 1,000,000 | 20 | ~660 bytes |

Each proof hash is 32 bytes, plus a fixed overhead of 12 bytes for the header
(total_leaves, num_positions) and 8 bytes per proven page (position +
page_index). The proof size grows logarithmically with the total number of
pages, making it practical even for very large commits.

---

## 7. Limitations and Future Work

### 7.1 Lazy Loading Verification Gap

The most significant limitation of the current construction is the gap
between per-page verification (Layer 1) and full-commit verification
(Layer 2). During lazy operation, a client may hold only a subset of pages
and can verify each page individually against its leaf hash. However, the
client cannot verify the `CommitHash`---and therefore cannot confirm that the
leaf hashes themselves are authentic---until all pages have been fetched and
the full Merkle tree can be reconstructed.

This means that during lazy operation, the client trusts the `LeafHashIndex`
as received from the remote. A compromised remote that can forge both the
`LeafHashIndex` and the corresponding pages can serve convincing but
inauthentic data until full hydration occurs and the `CommitHash` mismatch is
detected.

The Merkle inclusion proof mechanism (Section 4.2.3) provides a partial
mitigation: a client that has obtained the `CommitHash` through a trusted
channel can verify individual pages against the `CommitHash` using
inclusion proofs, without possessing all pages. However, the current
implementation does not automatically generate and verify inclusion proofs
during lazy page fetches. Integrating inclusion proof verification into the
lazy fetch path is a direction for future work.

### 7.2 TOFU Gap for Fresh Clients

The TOFU boundary provides strong protection for clients that have previously
interacted with a log, but it offers no protection for a fresh client
encountering a log for the first time. If a client's first interaction with a
log is through a fully compromised remote, the attacker can serve commits
without leaf hashes, and the client will accept them because no trust boundary
has been established.

The `require_leaf_hashes` configuration flag mitigates this for deployments
where all legitimate commits are guaranteed to have leaf hashes. For
mixed-deployment scenarios where some legacy commits lack leaf hashes, the
TOFU gap remains.

Future work could explore integration with an external transparency
log or gossip protocol that would allow fresh clients to verify that they
are seeing the same log state as other clients, similar to Certificate
Transparency's approach.

### 7.3 Fixed Page Size

The current construction assumes a fixed 4096-byte page size, which is
standard for SQLite and most modern file systems. If Graft were extended
to support variable page sizes, the domain separation scheme would need
to be updated to include the page size in the hash input, preventing an
attacker from exploiting ambiguity at the `PageIdx`--page-data boundary.

### 7.4 Commit Chain Verification

The current construction verifies individual commits independently. It does
not chain commit hashes (e.g., including the previous commit's hash in the
next commit's metadata), so there is no mechanism to detect commit deletion
or reordering within the log. The log's monotonic LSN ordering provides some
protection, but a hash-chained commit structure would provide stronger
guarantees. This is a natural extension of the current architecture.

### 7.5 Multi-Party Verification

The current system is designed for single-client verification: the client
verifies data it receives from the remote. In a multi-client setting, there
is no mechanism for clients to verify that they are seeing the same log state.
A malicious remote could present different views to different clients (an
equivocation attack). Extending the system with gossiped signed tree heads,
similar to Certificate Transparency, would address this gap.

---

## 8. Conclusion

We have presented a three-layer Merkle verification system for Graft that
provides per-page integrity verification for lazy-replicated SQLite databases.
The construction addresses the fundamental challenge of verifying individual
pages fetched from untrusted remote storage when the client does not possess
the complete dataset.

Layer 1 (per-page leaf hashes) provides immediate, constant-time verification
on every page read, detecting both storage-level corruption and transport-level
tampering with approximately 1 us of overhead per page. Layer 2 (Merkle tree
binding) connects individual page hashes to a single `CommitHash`, enabling
full-commit verification after hydration and supporting efficient inclusion
proofs. Layer 3 (trust-on-first-use boundary with anti-stripping defenses)
prevents an attacker from disabling the verification system by stripping
integrity metadata from commits.

The system adds 36 bytes of storage overhead per page (0.88% of page data),
requires no additional network round-trips, and is fully backward-compatible
with pre-existing unverified commits. The construction has been implemented
and deployed in the Graft storage engine, providing integrity guarantees for
lazy-replicated SQLite databases at the edge.

---

## References

[1] Graft: Transactional storage engine for efficient data synchronization
at the edge. https://github.com/orbitinghail/graft

[2] J. O'Connor, J.-P. Aumasson, S. Neves, and Z. Wilcox-O'Hearn.
"BLAKE3: One function, fast everywhere." 2020.
https://github.com/BLAKE3-team/BLAKE3-specs/blob/master/blake3.pdf

[3] R. C. Merkle. "A Certified Digital Signature." In *Advances in
Cryptology --- CRYPTO '89*, Lecture Notes in Computer Science, vol. 435,
pp. 218--238. Springer, 1989.

[4] R. Tamassia. "Authenticated Data Structures." In *Algorithms --- ESA
2003*, Lecture Notes in Computer Science, vol. 2832, pp. 2--5. Springer, 2003.

[5] C. Papamanthou, R. Tamassia, and N. Triandopoulos. "Authenticated Hash
Tables." In *Proceedings of the 15th ACM Conference on Computer and
Communications Security (CCS '08)*, pp. 437--448. ACM, 2008.

[6] B. Laurie, A. Langley, and E. Kasper. "Certificate Transparency."
RFC 6962, Internet Engineering Task Force, June 2013.

[7] J. Li, M. Krohn, D. Mazieres, and D. Shasha. "Secure Untrusted Data
Repository (SUNDR)." In *Proceedings of the 6th Symposium on Operating
Systems Design and Implementation (OSDI '04)*, pp. 121--136. USENIX, 2004.

[8] M. S. Melara, A. Blankstein, J. Bonneau, E. W. Felten, and M. J.
Freedman. "CONIKS: Bringing Key Transparency to End Users." In *Proceedings
of the 24th USENIX Security Symposium*, pp. 383--398. USENIX, 2015.

[9] Google. "Key Transparency." https://github.com/google/keytransparency

[10] rs-merkle: A Rust library for computing Merkle trees and proofs.
https://github.com/antouhou/rs-merkle

---

*Appendix A: Notation Summary*

| Symbol | Description |
|--------|-------------|
| `H(x)` | BLAKE3 hash of input `x` |
| `\|\|` | Byte concatenation |
| `PageIdx` | 32-bit unsigned page index |
| `Page` | 4096-byte page data |
| `LSN` | Log Sequence Number (monotonically increasing) |
| `LogId` | 128-bit log identifier |
| `VolumeId` | 128-bit volume identifier |
| `CommitHash` | 32-byte commit identifier (1-byte prefix + 31-byte hash) |
| `LeafHashIndex` | Sorted array of `(PageIdx, H(PageIdx \|\| Page))` entries |
| `n` | Number of pages in a commit |
| `2^{k}` | Two raised to the power `k` |
