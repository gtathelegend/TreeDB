# TreeDB Buffer Pool Manager

The Buffer Pool Manager is responsible for caching disk pages in memory, serving page read/write requests, and managing page eviction using a deterministic Least Recently Used (LRU) policy.

---

## 1. Responsibilities & Architecture

```text
               Higher Storage Layers (e.g. B+ Tree)
                               │
                               ▼
                       ┌──────────────┐
                       │  BufferPool  │
                       └──────┬───────┘
                              │
          ┌───────────────────┼───────────────────┐
          ▼                   ▼                   ▼
    ┌───────────┐       ┌───────────┐       ┌───────────┐
    │  Frame 0  │       │  Frame 1  │       │  Frame 2  │
    │  Page 42  │       │  Page 10  │       │  (Empty)  │
    └─────┬─────┘       └─────┬─────┘       └───────────┘
          │                   │
          └───────────────────┼───────────────────┘
                              ▼
                         PageManager
                              ▼
                        Database File
```

* **Cache Resident Pages:** Maintains a fixed, pre-allocated array of `Frame` objects.
* **Avoid Redundant Disk I/O:** Serves repeated fetches for cached pages directly from memory (cache hit).
* **Guaranteed Durability on Eviction:** Ensures dirty pages are flushed to disk through `PageManager` before frame reuse.
* **Strict Pinning Protection:** Protects actively referenced pages from being evicted.

---

## 2. Core Concepts

### `PageId` vs `FrameId`
* **`PageId` (`u32`):** Identifies the persistent page slot on physical disk.
* **`FrameId` (`usize`):** Identifies the memory slot index within `BufferPool::frames`.
* **Page Table (`HashMap<PageId, FrameId>`):** Maintains the current mapping between disk page IDs and their resident frame locations.

### Frame State
Each frame tracks:
* `page_id: Option<PageId>` — Active resident page ID, or `None` if empty.
* `page: Option<Page>` — The 4096-byte page data.
* `pin_count: u32` — Number of concurrent active references.
* `dirty: bool` — Whether page content was modified in memory since load.
* `last_accessed: u64` — Monotonic logical clock counter used for LRU ordering.

---

## 3. Pinning Lifecycle

```text
fetch_page(P)       ──> pin_count = 1
fetch_page(P)       ──> pin_count = 2 (Hit)
unpin_page(P, false)──> pin_count = 1
unpin_page(P, false)──> pin_count = 0 (Eligible for eviction)
unpin_page(P, false)──> Error::PinCountUnderflow (Underflow prevented)
```

### Invariants:
1. **Unpinned Eviction Only:** A frame is only eligible for replacement when `pin_count == 0`.
2. **No Pin Underflow:** Calling `unpin_page` on an unpinned page immediately yields `Error::PinCountUnderflow`.
3. **Sticky Dirty State:** Calling `unpin_page(page_id, false)` never clears an already-dirty page. Only `flush_page` or `flush_all` clears `dirty`.

---

## 4. Eviction & LRU Policy

When fetching an uncached page and all frames are occupied:
1. Scan all frames with `pin_count == 0`.
2. Select the candidate with the smallest `last_accessed` timestamp (Least Recently Used).
3. If all frames in the pool have `pin_count > 0`, return `Error::NoEvictablePage`.
4. If the selected victim frame is dirty (`dirty == true`):
   * Write the victim page to disk via `PageManager::write_page`.
   * Increment `stats.dirty_flushes`.
   * Clear `dirty = false`.
5. Remove `old_page_id` from the `page_table`.
6. Read the newly requested page from disk into the vacated frame.
7. Set `pin_count = 1`, record new `last_accessed` timestamp, and insert new mapping into `page_table`.

---

## 5. Buffer Pool API

```rust
impl BufferPool {
    pub fn new(capacity: usize, page_manager: PageManager) -> Result<Self>;
    pub fn fetch_page(&mut self, page_id: PageId) -> Result<&Page>;
    pub fn fetch_page_mut(&mut self, page_id: PageId) -> Result<&mut Page>;
    pub fn unpin_page(&mut self, page_id: PageId, dirty: bool) -> Result<()>;
    pub fn get_page(&self, page_id: PageId) -> Result<&Page>;
    pub fn get_page_mut(&mut self, page_id: PageId) -> Result<&mut Page>;
    pub fn flush_page(&mut self, page_id: PageId) -> Result<()>;
    pub fn flush_all(&mut self) -> Result<()>;
    pub fn sync(&mut self) -> Result<()>;
    pub fn allocate_page(&mut self, page_type: PageType) -> Result<PageId>;
    pub fn stats(&self) -> BufferPoolStats;
}
```

---

## 6. Statistics & Metrics

`BufferPool::stats()` provides real-time performance insights:
* `capacity`: Total available frame slots.
* `hits`: Number of requests served from memory.
* `misses`: Number of requests requiring disk I/O.
* `evictions`: Number of pages displaced from memory.
* `dirty_flushes`: Number of dirty pages written to disk.
* `current_dirty_pages`: Number of resident pages currently dirty.
* `hit_rate()`: Derived metric: `hits / (hits + misses)` (returns `0.0` if no accesses).
