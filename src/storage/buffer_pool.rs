use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::storage::page::{Page, PageId, PageType, PAGE_SIZE};
use crate::storage::page_manager::PageManager;
use crate::wal::log_manager::LogManager;
use crate::wal::log_record::{LogRecord, LogRecordBody};
use crate::wal::lsn::{Lsn, TxnId, NULL_LSN};

/// Frame identifier within the buffer pool.
pub type FrameId = usize;

/// Internal representation of a single frame in the buffer pool.
#[derive(Debug)]
struct Frame {
    page_id: Option<PageId>,
    page: Option<Page>,
    before_image: Option<[u8; PAGE_SIZE]>,
    pin_count: u32,
    dirty: bool,
    last_accessed: u64,
}

impl Frame {
    fn empty() -> Self {
        Self {
            page_id: None,
            page: None,
            before_image: None,
            pin_count: 0,
            dirty: false,
            last_accessed: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.page_id.is_none()
    }

    fn is_pinned(&self) -> bool {
        self.pin_count > 0
    }
}

/// Runtime statistics and metrics for the Buffer Pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferPoolStats {
    /// Maximum number of page frames held in memory.
    pub capacity: usize,
    /// Total number of page fetch requests served from memory.
    pub hits: u64,
    /// Total number of page fetch requests that required reading from disk.
    pub misses: u64,
    /// Total number of page evictions performed.
    pub evictions: u64,
    /// Total number of dirty pages written to disk due to eviction or explicit flushes.
    pub dirty_flushes: u64,
    /// Number of resident pages currently marked as dirty.
    pub current_dirty_pages: usize,
}

impl BufferPoolStats {
    /// Calculates cache hit rate as a fraction between 0.0 and 1.0.
    /// Returns 0.0 if there have been zero total page accesses.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

/// Buffer Pool Manager caching disk pages in a bounded number of memory frames.
///
/// Operates between higher-level storage components (such as B+ Trees) and the
/// physical `PageManager`, handling page fetching, pinning, dirty tracking,
/// LRU-style eviction, and Write-Ahead Logging flush ordering.
#[derive(Debug)]
pub struct BufferPool {
    capacity: usize,
    frames: Vec<Frame>,
    page_table: HashMap<PageId, FrameId>,
    page_manager: PageManager,
    wal: Option<LogManager>,
    active_txn: Option<TxnId>,
    access_counter: u64,
    stats: BufferPoolStats,
    test_failure_point: Option<crate::engine::failure::StorageFailurePoint>,
}

impl BufferPool {
    /// Creates a new `BufferPool` with a fixed frame capacity over the given `PageManager`.
    ///
    /// # Errors
    /// Returns `Error::InvalidCapacity` if `capacity == 0`.
    pub fn new(capacity: usize, page_manager: PageManager) -> Result<Self> {
        if capacity == 0 {
            return Err(Error::InvalidCapacity(0));
        }

        let mut frames = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            frames.push(Frame::empty());
        }

        Ok(Self {
            capacity,
            frames,
            page_table: HashMap::with_capacity(capacity),
            page_manager,
            wal: None,
            active_txn: None,
            access_counter: 0,
            stats: BufferPoolStats {
                capacity,
                ..Default::default()
            },
            test_failure_point: None,
        })
    }

    /// Creates a new `BufferPool` attached with an active `LogManager` to enforce Write-Ahead ordering.
    pub fn with_wal(capacity: usize, page_manager: PageManager, wal: LogManager) -> Result<Self> {
        let mut pool = Self::new(capacity, page_manager)?;
        pool.wal = Some(wal);
        Ok(pool)
    }

    /// Attaches an active `LogManager` to this buffer pool.
    pub fn attach_wal(&mut self, wal: LogManager) {
        self.wal = Some(wal);
    }

    /// Detaches and returns the active `LogManager`, if attached.
    pub fn detach_wal(&mut self) -> Option<LogManager> {
        self.wal.take()
    }

    /// Returns a reference to the active `LogManager`, if attached.
    pub fn wal(&self) -> Option<&LogManager> {
        self.wal.as_ref()
    }

    /// Returns a mutable reference to the active `LogManager`, if attached.
    pub fn wal_mut(&mut self) -> Option<&mut LogManager> {
        self.wal.as_mut()
    }

    /// Sets the active transaction ID for automated page delta logging on dirty frames.
    pub fn set_active_txn(&mut self, txn_id: TxnId) {
        self.active_txn = Some(txn_id);
        for frame in &mut self.frames {
            if let Some(ref page) = frame.page {
                frame.before_image = Some(*page.as_bytes());
            }
        }
    }

    /// Clears the active transaction ID.
    pub fn clear_active_txn(&mut self) {
        self.active_txn = None;
    }

    /// Sets or clears a deterministic test failure injection point.
    pub fn set_test_failure_point(
        &mut self,
        point: Option<crate::engine::failure::StorageFailurePoint>,
    ) {
        self.test_failure_point = point;
        self.page_manager.set_test_failure_point(point);
    }

    /// Returns the currently active test failure injection point, if any.
    pub fn test_failure_point(&self) -> Option<crate::engine::failure::StorageFailurePoint> {
        self.test_failure_point
    }

    /// Evaluates if the current failure injection point matches `target` and triggers `Error::InjectedFault`.
    pub fn check_fault(
        &mut self,
        target: crate::engine::failure::StorageFailurePoint,
    ) -> Result<()> {
        if self.test_failure_point == Some(target) {
            self.test_failure_point = None;
            self.page_manager.set_test_failure_point(None);
            return Err(Error::InjectedFault(target));
        }
        Ok(())
    }

    /// Returns the currently active transaction ID, if set.
    pub fn active_txn(&self) -> Option<TxnId> {
        self.active_txn
    }

    /// Fetches a page by ID into the buffer pool and pins it for immutable reading.
    ///
    /// Increments the page's pin count and updates its LRU access timestamp.
    /// The caller must eventually call `unpin_page(page_id, false)` when done.
    pub fn fetch_page(&mut self, page_id: PageId) -> Result<&Page> {
        let frame_id = self.fetch_frame_internal(page_id, false)?;
        Ok(self.frames[frame_id].page.as_ref().unwrap())
    }

    /// Fetches a page by ID into the buffer pool, marks it dirty, and pins it for mutable writing.
    ///
    /// Increments the page's pin count, marks the frame dirty, and updates its LRU timestamp.
    /// The caller must eventually call `unpin_page(page_id, true)` when done.
    pub fn fetch_page_mut(&mut self, page_id: PageId) -> Result<&mut Page> {
        let frame_id = self.fetch_frame_internal(page_id, true)?;
        Ok(self.frames[frame_id].page.as_mut().unwrap())
    }

    /// Unpins a resident page, decrementing its pin count.
    ///
    /// If `dirty` is true, the frame is marked as dirty. If `dirty` is false,
    /// an already-dirty page remains dirty.
    ///
    /// # Errors
    /// * `Error::PageNotResident` if the page is not in the buffer pool.
    /// * `Error::PinCountUnderflow` if the page was already at pin count 0.
    pub fn unpin_page(&mut self, page_id: PageId, dirty: bool) -> Result<()> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;

        let frame = &mut self.frames[frame_id];
        if frame.pin_count == 0 {
            return Err(Error::PinCountUnderflow(page_id));
        }

        if dirty {
            frame.dirty = true;
        }

        frame.pin_count -= 1;

        // If marked dirty and an active transaction is present, log delta to WAL
        if frame.dirty {
            if let Some(txn_id) = self.active_txn {
                if let (Some(ref mut page), Some(ref before)) =
                    (&mut frame.page, &frame.before_image)
                {
                    let cur_bytes = *page.as_bytes();
                    if &cur_bytes != before {
                        let mut start = 0;
                        while start < PAGE_SIZE && cur_bytes[start] == before[start] {
                            start += 1;
                        }
                        let mut end = PAGE_SIZE;
                        while end > start && cur_bytes[end - 1] == before[end - 1] {
                            end -= 1;
                        }
                        if start < end {
                            let before_slice = before[start..end].to_vec();
                            let after_slice = cur_bytes[start..end].to_vec();
                            if let Some(ref mut wal) = self.wal {
                                let lsn = wal.append_page_delta(
                                    txn_id,
                                    page_id,
                                    start as u16,
                                    before_slice,
                                    after_slice,
                                )?;
                                page.set_lsn(lsn);
                                if self.test_failure_point == Some(crate::engine::failure::StorageFailurePoint::AfterRecordPayloadLogged) {
                                    self.test_failure_point = None;
                                    self.page_manager.set_test_failure_point(None);
                                    return Err(Error::InjectedFault(crate::engine::failure::StorageFailurePoint::AfterRecordPayloadLogged));
                                }
                            }
                            frame.before_image = Some(cur_bytes);
                        }
                    }
                }
            } else if let Some(ref page) = frame.page {
                frame.before_image = Some(*page.as_bytes());
            }
        }

        Ok(())
    }

    /// Gets an immutable reference to a currently resident page without changing pin count.
    pub fn get_page(&self, page_id: PageId) -> Result<&Page> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;

        Ok(self.frames[frame_id].page.as_ref().unwrap())
    }

    /// Gets a mutable reference to a currently resident page without changing pin count,
    /// marking the frame as dirty.
    pub fn get_page_mut(&mut self, page_id: PageId) -> Result<&mut Page> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;

        let frame = &mut self.frames[frame_id];
        frame.dirty = true;
        Ok(frame.page.as_mut().unwrap())
    }

    /// Internal helper that logs pending page deltas and flushes a dirty frame to disk.
    fn flush_frame_internal(&mut self, frame_id: usize) -> Result<()> {
        let frame = &mut self.frames[frame_id];
        if !frame.dirty {
            return Ok(());
        }

        let page_id = match frame.page_id {
            Some(pid) => pid,
            None => return Ok(()),
        };

        // 1. If active transaction exists, ensure pending page delta is logged to WAL
        if let Some(txn_id) = self.active_txn {
            if let (Some(ref mut page), Some(ref before)) = (&mut frame.page, &frame.before_image) {
                let cur_bytes = *page.as_bytes();
                if &cur_bytes != before {
                    let mut start = 0;
                    while start < PAGE_SIZE && cur_bytes[start] == before[start] {
                        start += 1;
                    }
                    let mut end = PAGE_SIZE;
                    while end > start && cur_bytes[end - 1] == before[end - 1] {
                        end -= 1;
                    }
                    if start < end {
                        let before_slice = before[start..end].to_vec();
                        let after_slice = cur_bytes[start..end].to_vec();
                        if let Some(ref mut wal) = self.wal {
                            let lsn = wal.append_page_delta(
                                txn_id,
                                page_id,
                                start as u16,
                                before_slice,
                                after_slice,
                            )?;
                            page.set_lsn(lsn);
                        }
                        frame.before_image = Some(cur_bytes);
                    }
                }
            }
        }

        // 2. Write-Ahead rule: ensure WAL is flushed up to page.lsn() before writing page to disk
        if let Some(ref page) = frame.page {
            if let Some(ref mut wal) = self.wal {
                let lsn = page.lsn();
                if lsn.is_valid() {
                    wal.flush(lsn)?;
                }
            }
            self.page_manager.write_page(page)?;
            self.stats.dirty_flushes += 1;
            frame.dirty = false;
        }

        Ok(())
    }

    /// Flushes an individual dirty page to disk if resident, enforcing Write-Ahead order.
    ///
    /// If the page is clean, this is a no-op and returns `Ok(())`.
    ///
    /// # Errors
    /// Returns `Error::PageNotResident` if the page is not in the buffer pool.
    pub fn flush_page(&mut self, page_id: PageId) -> Result<()> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;

        self.flush_frame_internal(frame_id)
    }

    /// Flushes all resident dirty pages to disk through the `PageManager`, enforcing Write-Ahead order.
    pub fn flush_all(&mut self) -> Result<()> {
        for frame_id in 0..self.frames.len() {
            self.flush_frame_internal(frame_id)?;
        }
        Ok(())
    }

    /// Flushes all dirty pages and synchronizes the underlying database and WAL files.
    pub fn sync(&mut self) -> Result<()> {
        self.flush_all()?;
        if let Some(ref mut wal) = self.wal {
            let flushed_lsn = wal.flushed_lsn();
            if flushed_lsn.is_valid() {
                wal.flush(flushed_lsn)?;
            }
        }
        self.page_manager.sync()?;
        Ok(())
    }

    /// Executes a synchronous checkpoint of the database.
    ///
    /// 1. Flushes all resident dirty pages to disk, enforcing WAL write-ahead ordering for each page.
    /// 2. Queries active transactions from the WAL log manager.
    /// 3. Appends a `Checkpoint` log record to the WAL.
    /// 4. Flushes the WAL through the checkpoint record's LSN.
    /// 5. Writes the checkpoint LSN to the database header on Page 0 and syncs the database file.
    ///
    /// Returns the durable checkpoint LSN.
    pub fn checkpoint(&mut self) -> Result<Lsn> {
        self.check_fault(crate::engine::failure::StorageFailurePoint::BeforeCheckpointInitiation)?;

        // 1. Flush all dirty pages to PageManager, enforcing PageLSN <= durable WAL LSN
        self.check_fault(
            crate::engine::failure::StorageFailurePoint::DuringCheckpointDirtyPageFlush,
        )?;
        self.flush_all()?;

        let checkpoint_lsn = if let Some(ref mut wal) = self.wal {
            // 2. Query currently active uncommitted transactions
            let active_txns = wal.active_txns();

            // 3. Append Checkpoint log record with optional WAL flush failure injection
            let fault = self.test_failure_point;
            let lsn = wal.append_checkpoint_with_fault(active_txns, fault)?;
            if self.test_failure_point
                == Some(crate::engine::failure::StorageFailurePoint::DuringCheckpointWalFlush)
            {
                self.test_failure_point = None;
            }

            // 4. Flush WAL through checkpoint LSN
            wal.flush(lsn)?;
            lsn
        } else {
            NULL_LSN
        };

        // 5. Update Database Header on Page 0 with last_checkpoint_lsn and sync
        self.check_fault(
            crate::engine::failure::StorageFailurePoint::DuringCheckpointHeaderPersistence,
        )?;
        self.page_manager.set_last_checkpoint_lsn(checkpoint_lsn)?;
        self.page_manager.sync()?;

        self.check_fault(crate::engine::failure::StorageFailurePoint::AfterCheckpointCompleted)?;

        Ok(checkpoint_lsn)
    }

    /// Updates the durable max commit sequence in the page manager header and persists it.
    pub fn set_max_commit_seq(&mut self, seq: u64) -> Result<()> {
        self.page_manager.set_max_commit_seq(seq)
    }

    /// Returns the current durable max commit sequence from the page manager header.
    pub fn durable_max_commit_seq(&self) -> u64 {
        self.page_manager.header().max_commit_seq
    }

    /// Executes a synchronous checkpoint and safely truncates obsolete WAL records.
    ///
    /// Calculates the safe retention boundary:
    /// min(last_checkpoint_lsn, min(first_lsn_of_active_txns))
    /// and truncates WAL prefix prior to this boundary.
    pub fn checkpoint_and_truncate_wal(&mut self) -> Result<Lsn> {
        let checkpoint_lsn = self.checkpoint()?;
        self.check_fault(
            crate::engine::failure::StorageFailurePoint::BeforeCheckpointWalTruncation,
        )?;
        if self.wal.is_some() {
            self.check_fault(
                crate::engine::failure::StorageFailurePoint::DuringCheckpointWalTruncation,
            )?;
            if let Some(ref mut wal) = self.wal {
                let retention_lsn = wal.calculate_retention_lsn(checkpoint_lsn);
                wal.truncate_prefix(retention_lsn)?;
            }
        }
        Ok(checkpoint_lsn)
    }

    /// Allocates a page (reusing from freelist if available, or appending to file) and installs it in the buffer pool with pin_count = 1.
    pub fn allocate_page(&mut self, page_type: PageType) -> Result<PageId> {
        // Ensure a frame is available or can be evicted before allocating on disk.
        let frame_id = self.find_or_evict_frame()?;

        let new_page_id = self.page_manager.allocate_page()?;
        let mut new_page = Page::new(new_page_id, page_type);
        new_page.set_page_type(page_type);

        self.access_counter += 1;
        let frame = &mut self.frames[frame_id];
        frame.page_id = Some(new_page_id);
        frame.before_image = Some([0u8; PAGE_SIZE]);
        frame.page = Some(new_page);
        frame.pin_count = 1;
        frame.dirty = true;
        frame.last_accessed = self.access_counter;

        self.page_table.insert(new_page_id, frame_id);

        Ok(new_page_id)
    }

    /// Reclaims an unlinked page into the persistent freelist, logging deltas if a transaction is active.
    ///
    /// # Errors
    /// * `Error::CannotFreeReservedPage` if attempting to free Page 0 or Page 1.
    /// * `Error::InvalidPage` if `page_id >= page_count`.
    /// * `Error::PageCorruption` if the page is resident and actively pinned.
    pub fn free_page(&mut self, page_id: PageId) -> Result<()> {
        if crate::storage::metadata::is_reserved_page(page_id) {
            return Err(Error::CannotFreeReservedPage(page_id));
        }
        if page_id >= self.page_manager.page_count() {
            return Err(Error::InvalidPage(page_id));
        }

        // 1. If page is resident in buffer pool, ensure it is unpinned and extract its before-image
        let before_bytes = if let Some(&frame_id) = self.page_table.get(&page_id) {
            let frame = &mut self.frames[frame_id];
            if frame.pin_count > 0 {
                return Err(Error::PageCorruption {
                    page_id,
                    reason: "cannot free currently pinned page",
                });
            }
            let bytes = frame.page.as_ref().map(|p| *p.as_bytes());
            self.page_table.remove(&page_id);
            frame.page_id = None;
            frame.page = None;
            frame.before_image = None;
            frame.pin_count = 0;
            frame.dirty = false;
            bytes
        } else {
            None
        };

        // 2. Check if already in freelist to reject double-free
        if crate::storage::freelist::FreePageManager::is_free(&mut self.page_manager, page_id)? {
            return Err(Error::PageAlreadyFree(page_id));
        }

        // 3. Read current page before-bytes from disk if not resident
        let before = match before_bytes {
            Some(b) => b,
            None => *self.page_manager.read_page(page_id)?.as_bytes(),
        };

        // 4. Construct Free page
        let curr_head = self.page_manager.header().freelist_head;
        let mut free_page = Page::new(page_id, PageType::Free);
        crate::storage::freelist::FreePageLayout::write_next_free(&mut free_page, curr_head);

        // 5. If active transaction and WAL exist, append PageDelta and stamp LSN
        if let Some(txn_id) = self.active_txn {
            if let Some(ref mut wal) = self.wal {
                let lsn = wal.append_page_delta(
                    txn_id,
                    page_id,
                    0,
                    before.to_vec(),
                    free_page.as_bytes().to_vec(),
                )?;
                free_page.set_lsn(lsn);
            }
        } else if let Some(ref wal) = self.wal {
            let next_lsn = wal.next_lsn();
            if next_lsn.is_valid() {
                free_page.set_lsn(next_lsn);
            }
        }

        // 6. Write free page to disk
        self.page_manager.write_page(&free_page)?;

        // 7. Update DbHeader
        self.check_fault(crate::engine::failure::StorageFailurePoint::DuringFreelistUpdate)?;
        self.page_manager.header_mut().freelist_head = page_id;
        self.page_manager.header_mut().free_page_count += 1;
        self.page_manager.write_header_to_disk()?;

        Ok(())
    }

    /// Validates the structural integrity of the persistent freelist after flushing dirty pages.
    pub fn validate_freelist(&mut self) -> Result<()> {
        self.flush_all()?;
        self.page_manager.validate_freelist()
    }

    /// Aborts the specified transaction, rolling back physical page modifications
    /// using Compensation Log Records (CLRs) and flushing WAL through the abort LSN.
    pub fn abort_txn(&mut self, txn_id: TxnId) -> Result<Lsn> {
        let wal = match self.wal.as_mut() {
            Some(w) => w,
            None => return Ok(NULL_LSN),
        };

        let records = wal.read_all_records()?;
        let mut log_map: HashMap<Lsn, LogRecord> = HashMap::with_capacity(records.len());
        for rec in &records {
            log_map.insert(rec.lsn, rec.clone());
        }

        let mut curr_lsn = wal.txn_prev_lsn().get(&txn_id).copied().unwrap_or(NULL_LSN);

        while curr_lsn.is_valid() {
            if let Some(rec) = log_map.get(&curr_lsn) {
                if rec.txn_id == txn_id {
                    if let LogRecordBody::PageDelta {
                        page_id,
                        offset,
                        before_image,
                        ..
                    } = &rec.body
                    {
                        let pid = *page_id;
                        let off = *offset as usize;
                        let len = before_image.len();

                        // 1. Apply before_image to page in buffer pool or on disk
                        if let Some(&frame_id) = self.page_table.get(&pid) {
                            let frame = &mut self.frames[frame_id];
                            if let Some(ref mut page) = frame.page {
                                if off + len <= page.as_bytes().len() {
                                    page.as_bytes_mut()[off..off + len]
                                        .copy_from_slice(before_image);
                                }
                            }
                            frame.dirty = true;
                            if let Some(ref mut before) = frame.before_image {
                                if off + len <= before.len() {
                                    before[off..off + len].copy_from_slice(before_image);
                                }
                            }
                        } else if pid < self.page_manager.page_count() {
                            let mut page = self.page_manager.read_page(pid)?;
                            if off + len <= page.as_bytes().len() {
                                page.as_bytes_mut()[off..off + len].copy_from_slice(before_image);
                                self.page_manager.write_page(&page)?;
                            }
                        }

                        // 2. Append CLR and flush WAL for write-ahead durability
                        let wal = self.wal.as_mut().unwrap();
                        let clr_lsn = wal.append_clr(
                            txn_id,
                            pid,
                            *offset,
                            before_image.clone(),
                            rec.prev_lsn,
                        )?;
                        wal.flush(clr_lsn)?;

                        // 3. Update page LSN
                        if let Some(&frame_id) = self.page_table.get(&pid) {
                            if let Some(ref mut page) = self.frames[frame_id].page {
                                page.set_lsn(clr_lsn);
                            }
                        } else if pid < self.page_manager.page_count() {
                            if let Ok(mut page) = self.page_manager.read_page(pid) {
                                page.set_lsn(clr_lsn);
                                let _ = self.page_manager.write_page(&page);
                            }
                        }
                    }
                }
                curr_lsn = rec.prev_lsn;
            } else {
                break;
            }
        }

        let wal = self.wal.as_mut().unwrap();
        let lsn = wal.abort_txn(txn_id)?;
        if self.active_txn == Some(txn_id) {
            self.active_txn = None;
        }
        Ok(lsn)
    }

    /// Returns whether a page is currently resident in memory.
    pub fn is_resident(&self, page_id: PageId) -> bool {
        self.page_table.contains_key(&page_id)
    }

    /// Returns whether a resident page is currently pinned.
    pub fn is_pinned(&self, page_id: PageId) -> Result<bool> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;
        Ok(self.frames[frame_id].is_pinned())
    }

    /// Returns the active pin count of a resident page.
    pub fn pin_count(&self, page_id: PageId) -> Result<u32> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;
        Ok(self.frames[frame_id].pin_count)
    }

    /// Returns whether a resident page is marked dirty.
    pub fn is_dirty(&self, page_id: PageId) -> Result<bool> {
        let frame_id = self
            .page_table
            .get(&page_id)
            .copied()
            .ok_or(Error::PageNotResident(page_id))?;
        Ok(self.frames[frame_id].dirty)
    }

    /// Returns current Buffer Pool statistics.
    pub fn stats(&self) -> BufferPoolStats {
        let current_dirty_pages = self.frames.iter().filter(|f| f.dirty).count();
        BufferPoolStats {
            current_dirty_pages,
            ..self.stats
        }
    }

    /// Returns the capacity of the buffer pool.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns total number of database pages on disk.
    pub fn page_count(&self) -> u32 {
        self.page_manager.page_count()
    }

    /// Returns a reference to the underlying `PageManager`.
    pub fn page_manager(&self) -> &PageManager {
        &self.page_manager
    }

    /// Returns a mutable reference to the underlying `PageManager`.
    pub fn page_manager_mut(&mut self) -> &mut PageManager {
        &mut self.page_manager
    }

    // --- Internal Helpers ---

    /// Internal routine to fetch/load a frame and update access metadata.
    fn fetch_frame_internal(&mut self, page_id: PageId, mark_dirty: bool) -> Result<FrameId> {
        if page_id >= self.page_manager.page_count() {
            return Err(Error::InvalidPage(page_id));
        }

        self.access_counter += 1;

        // Case 1: Cache Hit
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            self.stats.hits += 1;
            let frame = &mut self.frames[frame_id];
            frame.pin_count += 1;
            frame.last_accessed = self.access_counter;
            if mark_dirty {
                frame.dirty = true;
            }
            return Ok(frame_id);
        }

        // Case 2: Cache Miss
        self.stats.misses += 1;
        let frame_id = self.find_or_evict_frame()?;

        // Read page from disk
        let page = self.page_manager.read_page(page_id)?;

        let frame = &mut self.frames[frame_id];
        frame.page_id = Some(page_id);
        frame.before_image = Some(*page.as_bytes());
        frame.page = Some(page);
        frame.pin_count = 1;
        frame.dirty = mark_dirty;
        frame.last_accessed = self.access_counter;

        self.page_table.insert(page_id, frame_id);

        Ok(frame_id)
    }

    /// Finds an empty frame or selects and evicts an unpinned victim frame using LRU.
    fn find_or_evict_frame(&mut self) -> Result<FrameId> {
        // 1. Check for an empty frame slot
        for (idx, frame) in self.frames.iter().enumerate() {
            if frame.is_empty() {
                return Ok(idx);
            }
        }

        // 2. Find unpinned frame with smallest last_accessed timestamp (LRU)
        let mut victim_frame_id: Option<FrameId> = None;
        let mut oldest_access = u64::MAX;

        for (idx, frame) in self.frames.iter().enumerate() {
            if !frame.is_pinned() && frame.last_accessed < oldest_access {
                oldest_access = frame.last_accessed;
                victim_frame_id = Some(idx);
            }
        }

        let victim_id = victim_frame_id.ok_or(Error::NoEvictablePage)?;

        // 3. If victim is dirty, flush it to disk first (enforcing Write-Ahead rule)
        self.flush_frame_internal(victim_id)?;

        // 4. Remove victim page from page table
        let victim_frame = &mut self.frames[victim_id];
        if let Some(old_page_id) = victim_frame.page_id.take() {
            self.page_table.remove(&old_page_id);
            self.stats.evictions += 1;
        }
        victim_frame.page = None;
        victim_frame.before_image = None;
        victim_frame.pin_count = 0;
        victim_frame.dirty = false;

        Ok(victim_id)
    }
}
