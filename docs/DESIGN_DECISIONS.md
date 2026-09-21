# TreeDB Design Decisions

This document records key architectural and design choices, their rationale, and alternatives considered.

---

## Decision 1: Fixed 4096-Byte (4 KiB) Page Size

* **Decision:** Enforce a constant `PAGE_SIZE = 4096` across the entire storage layer.
* **Rationale:** 
  * 4 KiB aligns with standard OS memory pages and modern SSD block/cluster sizes.
  * Simplifies pointer arithmetic and frame allocation in the Buffer Pool.
  * Matches traditional database storage defaults (SQLite default, etc.).
* **Alternatives Considered:** Dynamic page size configurable per database instance. Rejected for Phase 1 to minimize complexity while establishing storage invariants.

---

## Decision 2: Little-Endian Integer Serialization

* **Decision:** Multi-byte integer fields (such as `PageId`, `Key`, and header metadata) are serialized using explicit little-endian byte ordering (`to_le_bytes()` and `from_le_bytes()`).
* **Rationale:** 
  * Portability across different host architectures (x86_64, ARM64, etc.).
  * Avoids undefined behavior or compiler layout discrepancies from raw struct memory dumps (`std::mem::transmute`).
* **Alternatives Considered:** Big-endian (network byte order). Rejected because modern servers and desktop hardware are almost exclusively little-endian.

---

## Decision 3: Append-Only Page Allocation for Phase 1

* **Decision:** The initial allocator assigns the next sequential `PageId` (`header.page_count`), increments the count, and writes the updated header.
* **Rationale:** 
  * Provides a deterministic and verifiable baseline for disk I/O and persistence without early freelist complexity.
  * Isolates Page Manager responsibilities cleanly before introducing free-page recycling.
* **Alternatives Considered:** Free-space bitmap or linked freelist on disk. Deferred to subsequent phases once page deletion and node merging are implemented.

---

## Decision 4: Dedicated Database Header on Page 0

* **Decision:** Page 0 is reserved strictly for database identification, version verification, and global catalog pointers (`magic`, `format_version`, `page_size`, `page_count`, `root_page_id`, `metadata_page_id`).
* **Rationale:**
  * Fast validation of file integrity upon open before attempting any higher-level operations.
  * Allows backward compatibility detection and future header extension in the reserved 4068 bytes.
* **Alternatives Considered:** Embedding header in the first few bytes of Page 1. Rejected to maintain strict 4 KiB block boundaries for all pages.

---

## Decision 5: Explicit Error Types via `thiserror`

* **Decision:** Use structured enum variants in `treedb::Error` covering all storage conditions (I/O, corruption, page out-of-bounds, version mismatch).
* **Rationale:**
  * Enables callers and higher-level layers to programmatically inspect and handle specific database faults.
  * Avoids loss of context or brittle string matching.
