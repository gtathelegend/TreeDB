# MVCC Storage: Persistent Record Versions & Version Chaining

## 1. Overview

TreeDB Phase 68 introduces persistent multi-version record storage (MVCC) without physical background vacuuming or concurrent thread locking. Every physical slotted record in the database is wrapped with a canonical **46-byte persistent version header** (`VersionMetadata`), enabling point-in-time snapshot isolation, non-blocking readers, and durable transaction rollback.

Primary index B+ Trees and secondary indexes point to physical `RecordId`s representing either the head (newest) version or specific indexed versions. Historical versions are preserved immutably in singly-linked backward chains (`prev_version`) within the record store.

---

## 2. On-Disk Version Header Layout

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

## 3. Storage Compatibility & Overflow Handling

### Inline Records
Records where `VERSION_HEADER_SIZE + payload.len() <= MAX_RECORD_SIZE` (4070 bytes) are stored contiguously in a single slot on a `PageType::Record` page.

### Overflow Records
Records where `VERSION_HEADER_SIZE + payload.len() > MAX_RECORD_SIZE` are stored with:
1. An 8-byte `OverflowDescriptor` installed in the slot on the `PageType::Record` page pointing to `head_page_id`.
2. The 46-byte `VersionMetadata` stored at the beginning of the `head_page_id` `PageType::Overflow` payload area.
3. The remaining user payload distributed across the singly-linked chain of `PageType::Overflow` pages.

---

## 4. In-Place Version Metadata Updates

When a transaction updates or deletes an existing record:
1. The existing record's `xmax` is stamped with the active `TxnId`.
2. When the transaction commits, `deleted_at` is stamped with the assigned `CommitSeq`.
3. To eliminate unnecessary copying and disk I/O, `RecordStore::update_version_metadata` overwrites only the 46 header bytes in place on the record page (for inline records) or the overflow head page (for overflow records).
4. The user payload is never rewritten or moved.

---

## 5. Version Chaining Mechanism

Version chains follow a **newest-to-oldest** backward chaining topology:
- When a row is inserted:
  - `new_meta.prev_version = None`.
  - The B+ Tree points directly to `new_rid`.
- When a row is updated:
  - The active version `old_rid` is marked with `xmax = active_txn`.
  - A new version `new_rid` is inserted with `prev_version = Some(old_rid)`.
  - The B+ Tree is updated to point to `new_rid`.
  - The chain forms: `new_rid -> old_rid -> oldest_rid -> None`.
- Readers traverse the chain via `prev_version` until finding the version visible to their snapshot horizon.

### Guardrails
- **Cycle Detection**: Traversal tracks visited `RecordId`s in a `HashSet`. If a cycle is detected, `Error::VersionChainCycle` is returned.
- **Depth Limit**: Traversal enforces `MAX_VERSION_CHAIN_DEPTH = 1000`. If a version chain exceeds 1000 versions, `Error::VersionChainTooDeep` is returned to prevent pathological stack usage.
