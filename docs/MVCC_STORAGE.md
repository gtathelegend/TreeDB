# MVCC Storage: Persistent Record Versions & Version Chaining

## 1. Overview

TreeDB Phases 68 and 69 implement persistent multi-version record storage (MVCC) without background vacuuming or concurrent thread locking. Every physical slotted record in the database is wrapped with a canonical **46-byte persistent version header** (`VersionMetadata`), enabling point-in-time snapshot isolation, non-blocking readers, and durable transaction rollback.

Primary index B+ Trees point to the newest physical `RecordId` representing the logical record head. Secondary indexes point to specific physical versions (`VersionId`), retaining entries for historical versions to satisfy point-in-time snapshots. Historical versions are preserved immutably in singly-linked backward chains (`prev_version`) within the record store.

---

## 2. Logical Record vs. Physical Version Identity

Phase 69 establishes a clean architectural separation between logical records and physical versions:
- **Logical Record Identity**: Identified by table primary key (or row identity). In the primary B+ Tree, the primary key maps to the current head version `RecordId`.
- **Physical Version Identity (`VersionId`)**: Strongly-typed newtype `VersionId(pub RecordId)` representing a specific physical snapshot version on a specific page and slot.
- **Secondary Index References**: Secondary index entries store the physical `VersionId` (`RecordId`) of the version that created the indexed value. During query execution, secondary indexes filter candidates using `is_version_visible` or fetch exact payloads using `read_exact_version_if_visible`, ensuring older snapshots read the exact historical version without traversing `prev_version` links.

---

## 3. On-Disk Version Header Layout

Every record written to a `PageType::Record` page contains the 46-byte version header prepended to the user payload bytes:

```
+-------------------------------------------------------------------------------------------------------+
| Offset | Field           | Type    | Size | Description                                               |
+--------+-----------------+---------+------+-----------------------------------------------------------+
| 0..2   | format_version  | u16     | 2 B  | MVCC version format identifier (currently 1).             |
| 2..10  | xmin            | u64     | 8 B  | Transaction ID (TxnId) that created this version.         |
| 10     | has_xmax        | u8      | 1 B  | 1 if xmax is present, 0 if None.                          |
| 11..19 | xmax            | u64     | 8 B  | Transaction ID (TxnId) that deleted/superseded this row.  |
| 19     | has_created_at  | u8      | 1 B  | 1 if created_at CommitSeq is present, 0 if None.          |
| 20..28 | created_at      | u64     | 8 B  | Monotonic CommitSeq at which this version committed.      |
| 28     | has_deleted_at  | u8      | 1 B  | 1 if deleted_at CommitSeq is present, 0 if None.          |
| 29..37 | deleted_at      | u64     | 8 B  | Monotonic CommitSeq at which this version was deleted.    |
| 37     | has_prev        | u8      | 1 B  | 1 if prev_version RecordId is present, 0 if None.         |
| 38..42 | prev_page_id    | u32     | 4 B  | PageId of previous version (0 if None).                   |
| 42..46 | prev_slot_id    | u32     | 4 B  | SlotId of previous version (0 if None).                   |
+-------------------------------------------------------------------------------------------------------+
| 46..N  | payload         | bytes   | N-46 | Encoded tuple data (inline or overflow head).             |
+-------------------------------------------------------------------------------------------------------+
```

Total fixed header overhead: **46 bytes** (`VERSION_HEADER_SIZE`).

---

## 4. Storage Compatibility & Overflow Handling

### Inline Records
Records where `VERSION_HEADER_SIZE + payload.len() <= MAX_RECORD_SIZE` (4070 bytes) are stored contiguously in a single slot on a `PageType::Record` page.

### Overflow Records
Records where `VERSION_HEADER_SIZE + payload.len() > MAX_RECORD_SIZE` are stored with:
1. An 8-byte `OverflowDescriptor` installed in the slot on the `PageType::Record` page pointing to `head_page_id`.
2. The 46-byte `VersionMetadata` stored at the beginning of the `head_page_id` `PageType::Overflow` payload area.
3. The remaining user payload distributed across the singly-linked chain of `PageType::Overflow` pages.

### Overflow Preservation
Because MVCC updates and deletes only update `xmax` and `deleted_at` metadata rather than invoking `RecordStore::delete()`, historical overflow pages and multi-page chains are never reclaimed or returned to the freelist prematurely.

---

## 5. In-Place Version Metadata Updates

When a transaction updates or deletes an existing record:
1. The existing record's `xmax` is stamped with the active `TxnId`.
2. When the transaction commits, `deleted_at` is stamped with the assigned `CommitSeq`.
3. To eliminate unnecessary copying and disk I/O, `RecordStore::update_version_metadata` overwrites only the 46 header bytes in place on the record page (for inline records) or the overflow head page (for overflow records).
4. The user payload is never rewritten or moved.

---

## 6. Version Chaining & Traversal Semantics

Version chains follow a **newest-to-oldest** backward chaining topology:
- When a row is inserted:
  - `new_meta.prev_version = None`.
  - The B+ Tree points directly to `new_rid`.
- When a row is updated:
  - The active version `old_rid` is marked with `xmax = active_txn`.
  - A new version `new_rid` is inserted with `prev_version = Some(old_rid)`.
  - The B+ Tree is updated to point to `new_rid`.
  - The chain forms: `new_rid -> old_rid -> oldest_rid -> None`.
- Primary key lookups and sequential table scans traverse the chain via `prev_version` until finding the version visible to the snapshot horizon.
- Secondary index lookups point directly to physical `VersionId` entries and evaluate `read_exact_version_if_visible`, bypassing chain traversal.

### Guardrails
- **Cycle Detection**: Traversal tracks visited `RecordId`s in a `HashSet` and checks for immediate self-referential links (`prev == curr_rid`). If a cycle is detected, structured `Error::VersionChainCycle` is returned.
- **Depth Limit**: Traversal enforces `MAX_VERSION_CHAIN_DEPTH = 1000`. If a version chain exceeds 1000 versions, `Error::VersionChainTooDeep` is returned.

---

## 7. Page Compaction and Reuse Safety

Slotted-page compaction in TreeDB (`RecordPage::compact`) packs active record payload bytes to reclaim fragmentation within a page without reallocating slot IDs or altering existing slot offsets. Since historical versions remain active slots (neither tombstoned nor marked deleted in the slotted-page header), compaction preserves `RecordId(page_id, slot_id)` stability. Free pages are only reclaimed when an entire table is dropped, preventing dangling version references.

---

## 8. Phase 70: MVCC Vacuum, Version Garbage Collection & Storage Reclamation

Phase 70 introduces explicit, snapshot-safe physical reclamation of historical versions:
- **Reclamation Horizon**: Derived from the active snapshot registry ($\min_{s \in \text{active}} s.\text{as\_of}$ or $\text{current\_commit\_seq}$ when no snapshots are active).
- **Version-Chain Pruning**: Intermediate and tail versions superseded before the reclamation horizon are safely cut from `prev_version` chains (`cut_meta.prev_version = None`) and their slots deleted.
- **Overflow Reclamation**: Overflow chains belonging to reclaimed versions are returned to the PageManager freelist via `RecordStore::delete_and_count`.
- **Empty Page Recycling**: Completely empty slotted record pages are recycled back to the database freelist via `RecordStore::try_free_empty_record_page`.
- **Index Cleanup**: Obsolete secondary and covering index entries referencing reclaimed `RecordId`s are removed atomically with the physical version.
- Detailed architecture is documented in [MVCC_VACUUM.md](file:///d:/Projects/TreeDB/docs/MVCC_VACUUM.md).

---

## 9. Phase 71: Transaction Conflict Detection & Commit Validation

Phase 71 introduces an explicit write-intent tracking and commit-validation layer:
- **Write-Set Tracking**: Transactions record `RowWriteIntent` records mapping logical rows `(table, key)` to operation kinds, targeted version IDs, and new version IDs.
- **Commit Validation**: Before writing a `Commit` record to the WAL, write intents are validated against current committed head versions to detect write-write conflicts, stale version modifications, and unique index collisions.
- **Atomic Abort on Conflict**: Conflicting transactions are automatically aborted without leaving uncommitted physical versions or corrupting version chain links.
- Detailed architecture is documented in [MVCC_CONFLICTS.md](file:///d:/Projects/TreeDB/docs/MVCC_CONFLICTS.md).

---

## 10. Phase 72: Storage Invariants Under Crash Recovery

Phase 72 verifies the physical storage and allocator invariants across crash boundaries:
- **Slotted Version Chains**: Uncommitted version insertions are undone by ARIES recovery; version chain links (`prev_version`) are restored to valid prior versions.
- **Overflow Chain Consistency**: Interrupted multi-page overflow allocations or deallocations leave no dangling chains, unreferenced overflow pages, or freelist leaks.
- **Page Allocator Integrity**: Recovery reconciles `PageManager` freelist headers with physical page types on disk; no page is simultaneously allocated and free.
- Detailed architecture is documented in [TRANSACTION_ATOMICITY.md](file:///d:/Projects/TreeDB/docs/TRANSACTION_ATOMICITY.md).



