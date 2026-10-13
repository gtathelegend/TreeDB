//! In-memory runtime MVCC Visibility Map for safe index-only scans and fast-path visibility checks.
//!
//! # Architecture & All-Visible Invariant
//!
//! A record or page may bypass normal `RecordStore` version-header lookups only if it satisfies
//! the conservative all-visible invariant:
//! 1. All relevant versions are committed at or before commit sequence `H` (`created_at <= H`).
//! 2. No uncommitted modifications exist (`xmin` is committed or system-created).
//! 3. No unresolved or in-flight transaction can change visibility (`xmax.is_none()`).
//! 4. No deletions or updates have occurred (`deleted_at.is_none()`).
//! 5. The reader's snapshot horizon satisfies `snapshot.as_of_commit_seq >= H`.
//! 6. The reader transaction has not modified the page or table in its local uncommitted write set.
//!
//! If any of these conditions cannot be established with certainty, the fast path falls back
//! to ordinary `RecordStore` version-chain inspection.

use std::collections::{HashMap, HashSet};

use crate::engine::isolation::CommitSeq;
use crate::error::Result;
use crate::index::node::{RecordId, INVALID_PAGE_ID};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::page::{PageId, PageType, PAGE_PAYLOAD_SIZE};
use crate::storage::record_page::{OVERFLOW_FLAG, RECORD_PAGE_HEADER_SIZE, SLOT_SIZE};
use crate::storage::record_store::{MVCC_VERSION_FORMAT, VERSION_HEADER_SIZE};
use crate::wal::lsn::TxnId;

/// Upper bound on the number of all-visible pages tracked concurrently in memory.
pub const MAX_ALL_VISIBLE_PAGES: usize = 65_536;

/// Upper bound on the negative cache for non-all-visible pages.
pub const MAX_NON_ALL_VISIBLE_PAGES: usize = 16_384;

/// Upper bound on individual cached visible records.
pub const MAX_CACHED_RECORDS: usize = 65_536;

/// Performance and visibility check execution counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisibilityMapStats {
    /// Total index entries evaluated for visibility.
    pub candidate_entries_examined: usize,
    /// Total visibility checks evaluated by falling back to `RecordStore`.
    pub visibility_checks_evaluated: usize,
    /// Total visibility checks safely skipped via the visibility fast path.
    pub visibility_checks_skipped: usize,
    /// Number of hits on all-visible page metadata.
    pub page_map_hits: usize,
    /// Number of misses on all-visible page metadata.
    pub page_map_misses: usize,
    /// Number of hits on record-level all-visible cache.
    pub record_map_hits: usize,
    /// Number of misses on record-level all-visible cache.
    pub record_map_misses: usize,
    /// Number of pages verified and marked all-visible.
    pub pages_marked_all_visible: usize,
    /// Number of individual records cached as all-visible.
    pub records_marked_all_visible: usize,
    /// Total page invalidation events.
    pub pages_invalidated: usize,
    /// Total record invalidation events.
    pub records_invalidated: usize,
}

/// Runtime-only, conservative MVCC Visibility Map.
///
/// Holds in-memory verification metadata that is safe across snapshot readers.
/// All cached metadata is invalidated on mutations, rollbacks, and vacuum,
/// and starts completely fresh on database reopen and recovery.
#[derive(Debug, Clone)]
pub struct VisibilityMap {
    /// Pages where every active slot has been verified as:
    /// - Inline record (no overflow).
    /// - Committed with `created_at <= CommitSeq`.
    /// - `xmax == None` and `deleted_at == None`.
    all_visible_pages: HashMap<PageId, CommitSeq>,

    /// Negative cache for pages that contain tombstones, uncommitted versions,
    /// overflow records, or updates. Avoids repeated whole-page inspection.
    non_all_visible_pages: HashSet<PageId>,

    /// Individual records verified as committed and unmodified.
    all_visible_records: HashMap<RecordId, CommitSeq>,

    /// Operational metrics.
    pub stats: VisibilityMapStats,

    /// Whether the fast path is enabled (permits differential testing with fallback).
    pub enabled: bool,
}

impl Default for VisibilityMap {
    fn default() -> Self {
        Self::new()
    }
}

impl VisibilityMap {
    /// Creates a new, empty `VisibilityMap`.
    pub fn new() -> Self {
        Self {
            all_visible_pages: HashMap::new(),
            non_all_visible_pages: HashSet::new(),
            all_visible_records: HashMap::new(),
            stats: VisibilityMapStats::default(),
            enabled: true,
        }
    }

    /// Returns a copy of the current visibility metrics.
    pub fn stats(&self) -> VisibilityMapStats {
        self.stats
    }

    /// Resets execution counters to zero.
    pub fn reset_stats(&mut self) {
        self.stats = VisibilityMapStats::default();
    }

    /// Enables or disables the visibility fast path.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns `true` if `page_id` is currently registered as all-visible.
    pub fn is_page_all_visible(&self, page_id: PageId, as_of: CommitSeq) -> bool {
        if !self.enabled {
            return false;
        }
        match self.all_visible_pages.get(&page_id) {
            Some(&max_seq) => as_of >= max_seq,
            None => false,
        }
    }

    /// Returns `true` if `rid` is individually cached as all-visible.
    pub fn is_record_all_visible(&self, rid: RecordId, as_of: CommitSeq) -> bool {
        if !self.enabled {
            return false;
        }
        match self.all_visible_records.get(&rid) {
            Some(&c_seq) => as_of >= c_seq,
            None => false,
        }
    }

    /// Evaluates the visibility fast path for a candidate `rid`.
    ///
    /// # Invariants Checked
    /// 1. `self.enabled` is `true`.
    /// 2. `reader_has_mutation` is `false` (reader txn has not modified the target page).
    /// 3. Either the page is verified all-visible with `max_created_seq <= as_of`, OR
    ///    the record is cached all-visible with `created_seq <= as_of`.
    ///
    /// If verified, returns `Ok(true)` and increments `visibility_checks_skipped`.
    /// Otherwise, returns `Ok(false)` and increments `visibility_checks_evaluated`.
    pub fn check_visibility_fast_path(
        &mut self,
        buffer_pool: &mut BufferPool,
        rid: RecordId,
        as_of: CommitSeq,
        reader_has_mutation: bool,
    ) -> Result<bool> {
        self.stats.candidate_entries_examined += 1;

        if !self.enabled || reader_has_mutation || rid.page_id == INVALID_PAGE_ID {
            self.stats.visibility_checks_evaluated += 1;
            return Ok(false);
        }

        // 1. Check all-visible page metadata
        if let Some(&max_seq) = self.all_visible_pages.get(&rid.page_id) {
            if as_of >= max_seq {
                self.stats.page_map_hits += 1;
                self.stats.visibility_checks_skipped += 1;
                return Ok(true);
            }
        } else if !self.non_all_visible_pages.contains(&rid.page_id) {
            // Page not yet evaluated: inspect page slots
            match Self::verify_page(buffer_pool, rid.page_id)? {
                Some(max_seq) => {
                    if self.all_visible_pages.len() >= MAX_ALL_VISIBLE_PAGES {
                        self.all_visible_pages.clear();
                    }
                    self.all_visible_pages.insert(rid.page_id, max_seq);
                    self.stats.pages_marked_all_visible += 1;
                    if as_of >= max_seq {
                        self.stats.page_map_hits += 1;
                        self.stats.visibility_checks_skipped += 1;
                        return Ok(true);
                    }
                }
                None => {
                    if self.non_all_visible_pages.len() >= MAX_NON_ALL_VISIBLE_PAGES {
                        self.non_all_visible_pages.clear();
                    }
                    self.non_all_visible_pages.insert(rid.page_id);
                    self.stats.page_map_misses += 1;
                }
            }
        } else {
            self.stats.page_map_misses += 1;
        }

        // 2. Check record-level all-visible cache
        if let Some(&c_seq) = self.all_visible_records.get(&rid) {
            if as_of >= c_seq {
                self.stats.record_map_hits += 1;
                self.stats.visibility_checks_skipped += 1;
                return Ok(true);
            }
        } else {
            self.stats.record_map_misses += 1;
        }

        self.stats.visibility_checks_evaluated += 1;
        Ok(false)
    }

    /// Records an individual verified record version if it is committed, live, and unedited.
    pub fn record_version_visibility(
        &mut self,
        rid: RecordId,
        created_seq: Option<CommitSeq>,
        xmax: Option<TxnId>,
        deleted_seq: Option<CommitSeq>,
    ) {
        if !self.enabled || rid.page_id == INVALID_PAGE_ID {
            return;
        }
        if xmax.is_none() && deleted_seq.is_none() {
            if let Some(c) = created_seq {
                if self.all_visible_records.len() >= MAX_CACHED_RECORDS {
                    self.all_visible_records.clear();
                }
                self.all_visible_records.insert(rid, c);
                self.stats.records_marked_all_visible += 1;
            }
        }
    }

    /// Inspects all physical slots on a Record page to verify the all-visible invariant.
    ///
    /// # Criteria for All-Visible Page
    /// 1. Page type is `PageType::Record`.
    /// 2. Page has at least one active slot (`slot_count > 0`).
    /// 3. Zero deleted slots (`slot_offset != 0`).
    /// 4. Zero overflow records (`(slot_length & OVERFLOW_FLAG) == 0`).
    /// 5. Every version header has `xmax == None` and `deleted_at == None`.
    /// 6. Every version header has committed `created_at == Some(seq)`.
    ///
    /// Returns `Some(max_created_seq)` if all conditions hold; otherwise returns `None`.
    pub fn verify_page(buffer_pool: &mut BufferPool, page_id: PageId) -> Result<Option<CommitSeq>> {
        if page_id == INVALID_PAGE_ID {
            return Ok(None);
        }

        let page = buffer_pool.fetch_page(page_id)?;
        if page.page_type() != PageType::Record {
            buffer_pool.unpin_page(page_id, false)?;
            return Ok(None);
        }

        let payload = page.payload();
        let slot_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
        let free_space_start = u16::from_le_bytes([payload[2], payload[3]]);

        if slot_count == 0 {
            buffer_pool.unpin_page(page_id, false)?;
            return Ok(None);
        }

        let max_slots = (PAGE_PAYLOAD_SIZE - RECORD_PAGE_HEADER_SIZE) / SLOT_SIZE;
        if slot_count > max_slots {
            buffer_pool.unpin_page(page_id, false)?;
            return Ok(None);
        }

        let mut max_commit_seq = CommitSeq(0);
        let mut active_slots = 0;

        for slot_id in 0..slot_count {
            let slot_offset_pos = RECORD_PAGE_HEADER_SIZE + slot_id * SLOT_SIZE;
            let slot_offset =
                u16::from_le_bytes([payload[slot_offset_pos], payload[slot_offset_pos + 1]]);
            let slot_length =
                u16::from_le_bytes([payload[slot_offset_pos + 2], payload[slot_offset_pos + 3]]);

            // Tombstone / deleted slot present: page cannot be safely treated as all-visible
            if slot_offset == 0 {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            // Overflow record requires multi-page validation
            if (slot_length & OVERFLOW_FLAG) != 0 {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            let raw_len = (slot_length & !OVERFLOW_FLAG) as usize;
            if raw_len < VERSION_HEADER_SIZE {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            let start = slot_offset as usize;
            let end = start + raw_len;
            if end > PAGE_PAYLOAD_SIZE || start < free_space_start as usize {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            let header = &payload[start..start + VERSION_HEADER_SIZE];
            let format_ver = u16::from_le_bytes([header[0], header[1]]);
            if format_ver != MVCC_VERSION_FORMAT {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            // xmax must be None (header[10] == 0)
            if header[10] != 0 {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            // deleted_at must be None (header[28] == 0)
            if header[28] != 0 {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            // created_at must be Some (header[19] == 1)
            if header[19] == 0 {
                buffer_pool.unpin_page(page_id, false)?;
                return Ok(None);
            }

            let created_seq = u64::from_le_bytes(header[20..28].try_into().unwrap());
            if created_seq > max_commit_seq.0 {
                max_commit_seq = CommitSeq(created_seq);
            }

            active_slots += 1;
        }

        buffer_pool.unpin_page(page_id, false)?;

        if active_slots == 0 {
            return Ok(None);
        }

        Ok(Some(max_commit_seq))
    }

    /// Invalidates visibility metadata for an entire physical page.
    pub fn invalidate_page(&mut self, page_id: PageId) {
        if self.all_visible_pages.remove(&page_id).is_some() {
            self.stats.pages_invalidated += 1;
        }
        self.non_all_visible_pages.remove(&page_id);
    }

    /// Invalidates visibility metadata for an individual record and its page.
    pub fn invalidate_record(&mut self, rid: RecordId) {
        if self.all_visible_records.remove(&rid).is_some() {
            self.stats.records_invalidated += 1;
        }
        self.invalidate_page(rid.page_id);
    }

    /// Clears all visibility metadata.
    pub fn clear(&mut self) {
        self.all_visible_pages.clear();
        self.non_all_visible_pages.clear();
        self.all_visible_records.clear();
    }
}
