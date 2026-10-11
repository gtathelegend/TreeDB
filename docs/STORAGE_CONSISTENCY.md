# Storage Consistency Verification & Cross-Layer Invariant Enforcement

TreeDB — Persistent Storage Engine

## 1. Overview and Design Objectives

Phase 73 introduces a systematic, multi-layered consistency verification capability for TreeDB that checks physical, structural, and logical invariants across all engine subsystems:

1. **Database Header and File Layout**: Verifies format versions, magic identifiers, page size, page counts, and physical boundaries.
2. **Freelist and Allocation Structures**: Traverses free page chains, detects cycles, ensures absence of reserved pages, and validates page counts against header metadata.
3. **Cross-Layer Page Ownership**: Constructs a comprehensive ownership graph mapping pages across catalog, primary B+ trees, secondary/covering indexes, slotted record pages, overflow chains, and the freelist to detect duplicate ownership, unreferenced orphans, and freed page collisions.
4. **Primary B+ Tree and Slotted Records**: Validates B+ tree structural integrity, key ordering, leaf sibling pointers, slot directory boundaries, and primary-key-to-record-locator validity.
5. **MVCC Version Chains and Overflows**: Inspects version headers, validates `xmin`/`xmax`/`created_at`/`deleted_at` relationships, checks commit sequence monotonicity, guards against version chain cycles/depth limits, and audits multi-page overflow chains.
6. **Secondary, Composite, Unique, and Covering Indexes**: Performs two-way consistency audits ensuring index entries match underlying record attributes, covered payloads match row values, unique constraints are not violated across live rows, and live committed rows are not missing from secondary indexes.
7. **Write-Ahead Log (WAL) and Recovery State**: Reads WAL records sequentially, verifies magic bytes, format versions, payload CRC32 checksums, LSN monotonicity, and flags unapplied WAL tails as prerequisites without mutating state.

**Core Invariant**: This is a read-only audit capability. Verification never repairs, mutates, reclaims, or modifies database files, persistent headers, indexes, or log streams.

---

## 2. Verification API

TreeDB provides consistency verification both on active `TreeDb` instances and directly on database files:

### Public Engine Methods

```rust
impl TreeDb {
    /// Executes a comprehensive, read-only cross-layer consistency verification
    /// using custom resource limits and subsystem selectors.
    pub fn check_consistency(&mut self, config: &ConsistencyCheckConfig) -> Result<ConsistencyReport>;

    /// Executes a default comprehensive consistency verification.
    pub fn verify_consistency(&mut self) -> Result<ConsistencyReport>;

    /// Audits a database directly from its file path, handling corrupt headers
    /// and truncated files that would prevent normal `TreeDb::open`.
    pub fn check_file(path: impl AsRef<Path>, config: &ConsistencyCheckConfig) -> Result<ConsistencyReport>;

    /// Executes a default consistency audit directly on a database file by path.
    pub fn verify_file(path: impl AsRef<Path>) -> Result<ConsistencyReport>;
}
```

### Core Configuration & Report Types

* `ConsistencyCheckConfig`:
  - `max_pages: Option<usize>`: Page verification budget.
  - `max_records: Option<usize>`: Record verification budget.
  - `max_index_entries: Option<usize>`: Secondary index entries per index budget.
  - `max_version_chain_depth: usize` (default 64): Maximum permitted version chain depth.
  - `max_overflow_chain_depth: usize` (default 1024): Maximum permitted overflow chain depth.
  - `max_issues: usize` (default 100): Maximum reported issues before truncation.
  - `check_freelist: bool`, `check_bplus_trees: bool`, `check_record_store: bool`, `check_secondary_indexes: bool`, `check_page_ownership: bool`, `check_wal: bool`.

* `ConsistencySeverity`:
  - `Fatal`: Catastrophic physical or layout corruption (e.g. truncated file, invalid header magic, out-of-bounds pointer, page shared by live tree and freelist).
  - `Error`: Subsystem invariant violation (e.g. version chain cycle, secondary index value mismatch, missing index entry, unique constraint violation, CRC32 checksum failure).
  - `Warning`: Structural irregularity or recoverable anomaly (e.g. orphaned unreferenced page, version chain approaching depth limit).
  - `Info`: Informational condition (e.g. unapplied WAL tail requiring recovery, audit truncated due to configured resource limit).

* `ConsistencyReport`:
  - `issues: Vec<ConsistencyIssue>`: Deterministically sorted issues (ordered by severity descending, table, index, page, record, message).
  - `statistics: ConsistencyStatistics`: Detailed counts of pages, tables, indexes, records, chains, and freelist entries inspected.
  - `passed_checks: Vec<String>`: Names of verified subsystem passes.
  - `skipped_checks: Vec<String>`: Checks intentionally skipped or bounded.
  - `truncated: bool`: True if verification stopped early due to resource bounds.
  - `is_healthy(&self) -> bool`: Returns `true` only if `!truncated` and no `Fatal` or `Error` issues exist.

---

## 3. Structural vs. Logical Consistency

TreeDB explicitly distinguishes structural consistency from logical consistency:

| Dimension | Scope | Verification Strategy |
| :--- | :--- | :--- |
| **Physical / Layout** | File sizes, page boundaries, header tags, page format versions. | Checked against physical file size, `DbHeader`, and `PageType` discriminants. |
| **Structural** | B+ Tree node balance, child pointers, leaf sibling chains, freelist pointers, overflow chains. | Recursive tree traversal (`validate_tree`), cycle detection sets, depth bounds. |
| **Logical / Cross-Layer** | Concordance between primary key and slotted record, MVCC visibility, index values vs. row values, covering payload equality. | Two-way cross-layer verification: index entries probed against decoded row columns; live rows probed for presence in secondary indexes. |

A B+ Tree may be structurally flawless while containing an index entry whose `RecordId` points to a deleted slot or whose search key disagrees with the underlying tuple. TreeDB's cross-layer verifier identifies both classes of defects.

---

## 4. Page Ownership Model & Anomaly Detection

TreeDB constructs a complete page-ownership mapping across the entire page identifier domain `0..header.page_count`:

1. **Page 0**: Reserved for Database Header (`DbHeader`).
2. **Page 1**: Reserved for initial Catalog Metadata (`CatalogPage`).
3. **Catalog Chain**: All chained metadata pages reachable via `next_catalog_page_id`.
4. **Freelist**: All pages traversed along `freelist_head` via `FreePageLayout::read_next_free`.
5. **Primary Indexes**: All internal and leaf node pages collected via `tree.collect_all_pages()`.
6. **Secondary Indexes**: All internal and leaf node pages collected via `sec_tree.collect_all_pages()`.
7. **Record Pages**: Initial table record page (`record_page_id`) and all slotted pages containing version heads and historical chain members.
8. **Overflow Pages**: All singly-linked overflow pages backing large records.

### Detected Anomalies

* **`DuplicatePageOwnership`** (Fatal): Multiple distinct structures claim ownership of the same page (e.g. Table A and Table B share a record page).
* **`FreelistContainsLivePage`** (Fatal): A page is present on the freelist while simultaneously linked by an active index, catalog, or record structure.
* **`FreelistContainsReservedPage`** (Fatal): Page 0 or Page 1 linked into the free-page list.
* **`OrphanedPage`** (Warning): An allocated page within `page_count` that is neither on the freelist nor reachable by any table, index, or catalog structure.
* **`PageOutOfBounds`** (Fatal): Pointer to `page_id >= page_count`.

---

## 5. MVCC & Historical-Version Handling

MVCC architectures retain multiple physical versions of a logical row. The verifier respects historical version preservation:

1. **Visibility Semantics**: Historical versions are not expected to match current live primary index heads; they represent valid historical state.
2. **Commit Sequence Monotonicity**: Across any version chain originating at `head_rid`, older versions must have `created_at <= newer version created_at`.
3. **Creation vs. Deletion Order**: For any individual version where both timestamps exist, `created_at <= deleted_at`.
4. **Cycle & Depth Prevention**: Chains are guarded by a visited set (`VersionChainCycle`) and bounded by `config.max_version_chain_depth` (`VersionChainTooDeep`).
5. **Freelist Collisions**: If any version in a chain resides on a page currently listed on the freelist, `VersionPointsToFreedPage` is reported.
6. **Slot State**: Live primary index heads must point to active (non-deleted) slots (`PrimaryHeadPointsToDeletedSlot`).

---

## 6. Secondary, Composite, Unique & Covering Index Rules

All registered secondary indexes are audited in both directions:

### Forward Check (Index -> Row)
For every entry `(idx_key, payload, rid)` in a secondary B+ Tree:
* `rid` must reference an existing, decodable version.
* Extracting the key from the decoded row must match `idx_key` (`SecondaryIndexValueMismatch`).
* For covering indexes, binary payload must match `idx_meta.extract_covered_payload(&row)` (`CoveringPayloadMismatch`).
* For unique indexes, no two live visible versions (`deleted_at.is_none()`) may share the same index key (`UniqueConstraintViolation`).

### Reverse Check (Row -> Index)
For every live committed row in the base table:
* The index key extracted from the row must exist in the secondary index and point to `rid` (`MissingSecondaryIndexEntry`).

---

## 7. WAL and Recovery Prerequisites

TreeDB checks the Write-Ahead Log without mutating persistent state:

1. **Header Validation**: Checks magic bytes (`TREEWAL1`), version, and minimal length.
2. **Record Decoding & CRC32**: Verifies 4-byte length prefix, body deserialization, and CRC32 checksums (`WalRecordCorruption`).
3. **LSN Monotonicity**: Verifies strictly increasing LSN sequence (`WalMonotonicityViolation`).
4. **Unapplied WAL Tail**: If records exist with `lsn > header.last_checkpoint_lsn`, the verifier reports `UnappliedWalTail` as an `Info` item indicating that crash recovery is a prerequisite to bring disk state up to date. **Verification never silently triggers crash recovery.**

---

## 8. Read-Only Verification Guarantee

TreeDB guarantees that consistency verification is completely read-only:

* All buffer pool page fetches during verification are explicitly unpinned with `dirty = false`.
* No pages are written to disk.
* Database header fields (`page_count`, `freelist_head`, `free_page_count`, `last_checkpoint_lsn`, `max_commit_seq`) are never rewritten.
* No WAL records are logged.
* No automatic repairs or page reclaims are performed.

This invariant is verified in `tests/storage_consistency.rs` by asserting 100% byte-for-byte equality of the database file before and after running verification.

---

## 9. Resource Limits & Bounded Verification

To ensure predictability under corrupt or adversarial database files:

* Traversal depths for version chains and overflow chains are bounded.
* Configurable limits (`max_pages`, `max_records`, `max_index_entries`, `max_issues`) allow bounded audits on large or resource-constrained deployments.
* When any budget is exhausted, `report.truncated` is set to `true`, `report.is_healthy()` returns `false`, and `VerificationLimitReached` is recorded. TreeDB never disguises an incomplete audit as a healthy result.

---

## 10. Known Verification Blind Spots

1. **Non-Durable In-Memory Uncommitted State**: Transactions active only in memory and uncommitted to WAL/disk are not tracked by offline or file-based audits.
2. **Semantic Interpretation of Raw Blobs**: Columns typed as arbitrary bytes (`DataType::Bytes`) cannot be checked for user-level application semantics beyond schema tuple layout.
3. **Unreferenced Reclaimed Slots**: Fragmented space in slotted record pages is inspected for offset validity, but semantic historical contents of reclaimed holes are inaccessible by design.

---

## 11. Phase 74: Recovery Certification & Pre-Recovery Inspection Integration

Phase 74 enhances offline file verification (`TreeDb::check_file`, `TreeDb::verify_file`):
* **True Read-Only Pre-Recovery Inspection**: `verify_file` opens files via `TreeDb::open_read_only_unrecovered`, avoiding silent ARIES recovery side effects when inspecting raw pre-recovery disk images.
* **Unapplied WAL Tail Detection**: When a database has suffered an ungraceful crash, `verify_file` detects unapplied log records beyond `last_checkpoint_lsn` and flags `ConsistencyIssueKind::UnappliedWalTail` with `ConsistencySeverity::Info`, explicitly documenting crash recovery as a prerequisite for offline consistency.
* **Dual WAL Path Resolution**: Supports both `format!("{}.wal", db_path.display())` and `db_path.with_extension("wal")` conventions.
* Detailed recovery certification architecture is documented in [RECOVERY_CERTIFICATION.md](file:///d:/Projects/TreeDB/docs/RECOVERY_CERTIFICATION.md).

