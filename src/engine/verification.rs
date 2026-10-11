use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::engine::catalog::Catalog;
use crate::engine::types::Row;
use crate::engine::TreeDb;
use crate::error::{Error, Result};
use crate::index::bplus_tree::BPlusTree;
use crate::index::node::RecordId;
use crate::index::secondary::{IndexKey, SecondaryIndex};
use crate::storage::freelist::FreePageLayout;
use crate::storage::metadata::{
    is_reserved_page, CURRENT_FORMAT_VERSION, MAGIC_BYTES, NO_FREE_PAGE,
};
use crate::storage::overflow_page::{OverflowPage, NO_OVERFLOW_PAGE};
use crate::storage::page::{Key, PageId, PageType, PAGE_SIZE};
use crate::storage::record_page::RecordPage;
use crate::wal::log_manager::{WAL_HEADER_SIZE, WAL_MAGIC_BYTES};
use crate::wal::log_record::{LogRecord, LogRecordBody};
use crate::wal::lsn::{Lsn, NULL_LSN};

/// Classification of consistency issue severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConsistencySeverity {
    /// Informational prerequisite or harmless deviation (e.g. unapplied WAL tail).
    Info = 0,
    /// Structural or logical anomaly that does not prevent immediate reads (e.g. orphan page).
    Warning = 1,
    /// Definite invariant violation (e.g. cycle in chain, live row in freelist, value mismatch).
    Error = 2,
    /// Fatal database corruption (e.g. corrupted file header, out-of-bounds pointer).
    Fatal = 3,
}

impl fmt::Display for ConsistencySeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => write!(f, "INFO"),
            Self::Warning => write!(f, "WARNING"),
            Self::Error => write!(f, "ERROR"),
            Self::Fatal => write!(f, "FATAL"),
        }
    }
}

/// Structured classification of specific consistency issues detected across TreeDB layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsistencyIssueKind {
    // Database Header & File Layout
    DatabaseFileTruncated {
        expected_min_bytes: u64,
        actual_bytes: u64,
    },
    InvalidHeaderMagic {
        found: [u8; 8],
    },
    UnsupportedFormatVersion {
        found: u32,
        supported: u32,
    },
    InvalidPageSize {
        found: u32,
        expected: u32,
    },
    PageCountMismatch {
        header_page_count: u32,
        file_page_count: u32,
    },
    PageOutOfBounds {
        page_id: PageId,
        max_page_id: PageId,
    },
    InvalidPageTypeTag {
        page_id: PageId,
        tag: u8,
    },

    // Freelist & Allocator Invariants
    FreelistCycle {
        page_id: PageId,
    },
    FreelistContainsReservedPage {
        page_id: PageId,
    },
    FreelistContainsLivePage {
        page_id: PageId,
        owner: String,
    },
    FreelistCountMismatch {
        header_count: u32,
        traversed_count: u32,
    },
    InvalidFreelistPageType {
        page_id: PageId,
        tag: u8,
    },

    // Cross-Layer Page Ownership
    DuplicatePageOwnership {
        page_id: PageId,
        owner1: String,
        owner2: String,
    },
    OrphanedPage {
        page_id: PageId,
    },

    // Primary B+ Tree
    BPlusTreeCorruption {
        page_id: Option<PageId>,
        reason: String,
    },
    DuplicateLogicalPrimaryKey {
        key: Key,
        head1: RecordId,
        head2: RecordId,
    },
    PrimaryHeadPointsToFreedPage {
        key: Key,
        rid: RecordId,
    },
    PrimaryHeadPointsToDeletedSlot {
        key: Key,
        rid: RecordId,
    },
    MissingPrimaryIndexEntry {
        key: Key,
        rid: RecordId,
    },

    // RecordStore, Slotted Pages & MVCC Chains
    InvalidSlotDirectory {
        page_id: PageId,
        reason: String,
    },
    MalformedVersionMetadata {
        rid: RecordId,
        reason: String,
    },
    VersionChainCycle {
        head_rid: RecordId,
        cycle_rid: RecordId,
    },
    VersionChainTooDeep {
        head_rid: RecordId,
        depth: usize,
        limit: usize,
    },
    VersionChainOrderViolation {
        rid: RecordId,
        reason: String,
    },
    VersionPointsToFreedPage {
        rid: RecordId,
        page_id: PageId,
    },

    // Overflow Pages
    OverflowCorruption {
        page_id: PageId,
        reason: String,
    },
    OverflowChainCycle {
        head_page_id: PageId,
        cycle_page_id: PageId,
    },
    OverflowBelongsToFreelist {
        page_id: PageId,
    },

    // Secondary, Unique, and Covering Indexes
    SecondaryIndexCorruption {
        index_name: String,
        reason: String,
    },
    SecondaryIndexEntryPointsToInvalidVersion {
        index_name: String,
        rid: RecordId,
    },
    SecondaryIndexValueMismatch {
        index_name: String,
        rid: RecordId,
        expected_key: String,
        found_key: String,
    },
    CoveringPayloadMismatch {
        index_name: String,
        rid: RecordId,
    },
    UniqueConstraintViolation {
        index_name: String,
        key: String,
        rid1: RecordId,
        rid2: RecordId,
    },
    MissingSecondaryIndexEntry {
        index_name: String,
        key: Key,
        rid: RecordId,
    },

    // WAL & Crash Recovery State
    WalHeaderCorruption {
        reason: String,
    },
    WalRecordCorruption {
        lsn: Lsn,
        reason: String,
    },
    WalMonotonicityViolation {
        prev_lsn: Lsn,
        curr_lsn: Lsn,
    },
    CommitSequenceRegression {
        max_durable_seq: u64,
        found_seq: u64,
    },
    UnappliedWalTail {
        unapplied_records: usize,
        max_lsn: Lsn,
    },

    // Verification Resource Limits & Administrative
    VerificationLimitReached {
        limit_name: String,
        limit_value: usize,
    },
    CheckSkipped {
        check_name: String,
        reason: String,
    },
    IoError {
        detail: String,
    },
}

/// An individual consistency issue discovered during database audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyIssue {
    /// Severity classification.
    pub severity: ConsistencySeverity,
    /// Typed classification of the issue.
    pub kind: ConsistencyIssueKind,
    /// Relevant table name, if associated with a table.
    pub table_name: Option<String>,
    /// Relevant index name, if associated with an index.
    pub index_name: Option<String>,
    /// Physical page identifier involved, if known.
    pub page_id: Option<PageId>,
    /// Physical record identifier involved, if known.
    pub record_id: Option<RecordId>,
    /// Human-readable diagnostic description.
    pub message: String,
}

impl ConsistencyIssue {
    /// Constructs a new `ConsistencyIssue`.
    pub fn new(
        severity: ConsistencySeverity,
        kind: ConsistencyIssueKind,
        table_name: Option<String>,
        index_name: Option<String>,
        page_id: Option<PageId>,
        record_id: Option<RecordId>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            kind,
            table_name,
            index_name,
            page_id,
            record_id,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConsistencyIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", self.severity)?;
        if let Some(ref t) = self.table_name {
            write!(f, " [table: {t}]")?;
        }
        if let Some(ref i) = self.index_name {
            write!(f, " [index: {i}]")?;
        }
        if let Some(p) = self.page_id {
            write!(f, " [page: {p}]")?;
        }
        if let Some(r) = self.record_id {
            write!(f, " [record: {:?}]", r)?;
        }
        write!(f, " {}", self.message)
    }
}

/// Cumulative statistics recorded during the consistency audit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsistencyStatistics {
    /// Total pages recorded in the database file.
    pub total_pages_in_file: u32,
    /// Physical pages fetched and examined.
    pub pages_examined: usize,
    /// Tables inspected.
    pub tables_examined: usize,
    /// Secondary/covering indexes inspected.
    pub indexes_examined: usize,
    /// Primary key entries traversed.
    pub primary_keys_examined: usize,
    /// Record versions read and verified.
    pub records_examined: usize,
    /// MVCC version chains traversed.
    pub version_chains_examined: usize,
    /// Overflow pages validated.
    pub overflow_pages_examined: usize,
    /// Freelist pages traversed.
    pub freelist_pages_traversed: usize,
    /// WAL log records read and verified.
    pub wal_records_examined: usize,
    /// Total issues found.
    pub issues_found: usize,
    /// Subsystem check passes completed successfully.
    pub checks_passed: usize,
    /// Subsystem checks skipped due to limits or prerequisites.
    pub checks_skipped: usize,
}

/// Configurable resource limits and subsystem selectors for consistency verification.
#[derive(Debug, Clone)]
pub struct ConsistencyCheckConfig {
    /// Maximum number of physical pages to examine before truncating audit.
    pub max_pages: Option<usize>,
    /// Maximum number of logical records to examine before truncating audit.
    pub max_records: Option<usize>,
    /// Maximum number of secondary index entries to examine per index.
    pub max_index_entries: Option<usize>,
    /// Maximum permitted version chain traversal depth before flagging error.
    pub max_version_chain_depth: usize,
    /// Maximum permitted overflow chain traversal depth before flagging error.
    pub max_overflow_chain_depth: usize,
    /// Maximum number of issues to record before truncating audit.
    pub max_issues: usize,
    /// Whether to audit freelist structure and counts.
    pub check_freelist: bool,
    /// Whether to audit B+ Tree structural invariants and leaf links.
    pub check_bplus_trees: bool,
    /// Whether to audit slotted pages, record headers, and MVCC version chains.
    pub check_record_store: bool,
    /// Whether to audit secondary, composite, unique, and covering indexes.
    pub check_secondary_indexes: bool,
    /// Whether to build and check cross-layer page ownership.
    pub check_page_ownership: bool,
    /// Whether to audit WAL file headers, records, CRC32, and LSN ordering.
    pub check_wal: bool,
}

impl Default for ConsistencyCheckConfig {
    fn default() -> Self {
        Self {
            max_pages: None,
            max_records: None,
            max_index_entries: None,
            max_version_chain_depth: 64,
            max_overflow_chain_depth: 1024,
            max_issues: 100,
            check_freelist: true,
            check_bplus_trees: true,
            check_record_store: true,
            check_secondary_indexes: true,
            check_page_ownership: true,
            check_wal: true,
        }
    }
}

/// Comprehensive report produced by the read-only consistency verification engine.
#[derive(Debug, Clone)]
pub struct ConsistencyReport {
    /// List of issues identified during verification, ordered deterministically.
    pub issues: Vec<ConsistencyIssue>,
    /// Cumulative verification metrics.
    pub statistics: ConsistencyStatistics,
    /// List of named check suites that passed completely without issues.
    pub passed_checks: Vec<String>,
    /// List of named check suites that were skipped or truncated.
    pub skipped_checks: Vec<String>,
    /// Whether verification stopped early due to reaching a configured resource limit.
    pub truncated: bool,
}

impl ConsistencyReport {
    /// Returns `true` if no `Fatal` or `Error` issues were detected and verification was not truncated.
    pub fn is_healthy(&self) -> bool {
        !self.truncated
            && !self.issues.iter().any(|i| {
                matches!(
                    i.severity,
                    ConsistencySeverity::Fatal | ConsistencySeverity::Error
                )
            })
    }

    /// Returns `true` if any `Fatal` issue was detected.
    pub fn has_fatal(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity == ConsistencySeverity::Fatal)
    }

    /// Returns `true` if any `Error` issue was detected.
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity == ConsistencySeverity::Error)
    }

    /// Returns `true` if any `Warning` issue was detected.
    pub fn has_warnings(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity == ConsistencySeverity::Warning)
    }

    /// Returns count of fatal issues.
    pub fn fatal_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == ConsistencySeverity::Fatal)
            .count()
    }

    /// Returns count of error issues.
    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == ConsistencySeverity::Error)
            .count()
    }

    /// Returns count of warning issues.
    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == ConsistencySeverity::Warning)
            .count()
    }

    /// Generates a human-readable summary of the verification outcome.
    pub fn summary(&self) -> String {
        let status = if self.is_healthy() {
            "HEALTHY"
        } else if self.has_fatal() {
            "FATAL_CORRUPTION"
        } else if self.has_errors() {
            "INCONSISTENT"
        } else if self.has_warnings() {
            "DEGRADED"
        } else {
            "TRUNCATED"
        };

        format!(
            "TreeDB Consistency Report: status={}, pages_examined={}, records_examined={}, \
             tables={}, indexes={}, freelist_pages={}, issues_found={} (fatal={}, error={}, warn={}), truncated={}",
            status,
            self.statistics.pages_examined,
            self.statistics.records_examined,
            self.statistics.tables_examined,
            self.statistics.indexes_examined,
            self.statistics.freelist_pages_traversed,
            self.issues.len(),
            self.fatal_count(),
            self.error_count(),
            self.warning_count(),
            self.truncated,
        )
    }
}

/// Orchestrator for read-only cross-layer consistency verification.
pub struct ConsistencyVerifier<'a> {
    db: &'a mut TreeDb,
    config: ConsistencyCheckConfig,
    issues: Vec<ConsistencyIssue>,
    statistics: ConsistencyStatistics,
    passed_checks: Vec<String>,
    skipped_checks: Vec<String>,
    truncated: bool,
    freelist_pages: HashSet<PageId>,
    page_ownership: HashMap<PageId, String>,
}

impl<'a> ConsistencyVerifier<'a> {
    /// Executes a comprehensive consistency audit on `db` using `config`.
    pub fn verify(db: &mut TreeDb, config: &ConsistencyCheckConfig) -> Result<ConsistencyReport> {
        let mut verifier = ConsistencyVerifier {
            db,
            config: config.clone(),
            issues: Vec::new(),
            statistics: ConsistencyStatistics::default(),
            passed_checks: Vec::new(),
            skipped_checks: Vec::new(),
            truncated: false,
            freelist_pages: HashSet::new(),
            page_ownership: HashMap::new(),
        };

        verifier.run_verification()?;

        // Sort issues deterministically: severity descending, then by table, index, page, record, message
        verifier.issues.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| a.table_name.cmp(&b.table_name))
                .then_with(|| a.index_name.cmp(&b.index_name))
                .then_with(|| a.page_id.cmp(&b.page_id))
                .then_with(|| a.record_id.cmp(&b.record_id))
                .then_with(|| a.message.cmp(&b.message))
        });

        verifier.statistics.issues_found = verifier.issues.len();
        verifier.statistics.checks_passed = verifier.passed_checks.len();
        verifier.statistics.checks_skipped = verifier.skipped_checks.len();

        Ok(ConsistencyReport {
            issues: verifier.issues,
            statistics: verifier.statistics,
            passed_checks: verifier.passed_checks,
            skipped_checks: verifier.skipped_checks,
            truncated: verifier.truncated,
        })
    }

    /// Audits a database directly from its file path, handling corrupt headers and truncated files
    /// that would otherwise cause `TreeDb::open` to fail.
    pub fn verify_file(
        path: impl AsRef<Path>,
        config: &ConsistencyCheckConfig,
    ) -> Result<ConsistencyReport> {
        let p = path.as_ref();
        let file_len = match std::fs::metadata(p) {
            Ok(m) => m.len(),
            Err(e) => {
                return Ok(ConsistencyReport {
                    issues: vec![ConsistencyIssue::new(
                        ConsistencySeverity::Fatal,
                        ConsistencyIssueKind::IoError {
                            detail: e.to_string(),
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("failed to open database file: {e}"),
                    )],
                    statistics: ConsistencyStatistics::default(),
                    passed_checks: Vec::new(),
                    skipped_checks: Vec::new(),
                    truncated: false,
                });
            }
        };

        let min_bytes = (2 * PAGE_SIZE) as u64;
        if file_len < min_bytes {
            return Ok(ConsistencyReport {
                issues: vec![ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::DatabaseFileTruncated {
                        expected_min_bytes: min_bytes,
                        actual_bytes: file_len,
                    },
                    None, None, Some(0), None,
                    format!("database file size ({file_len} bytes) < minimum 2 pages ({min_bytes} bytes)"),
                )],
                statistics: ConsistencyStatistics::default(),
                passed_checks: Vec::new(),
                skipped_checks: Vec::new(),
                truncated: false,
            });
        }

        // Read Page 0 header
        let mut file = match File::open(p) {
            Ok(f) => f,
            Err(e) => {
                return Ok(ConsistencyReport {
                    issues: vec![ConsistencyIssue::new(
                        ConsistencySeverity::Fatal,
                        ConsistencyIssueKind::IoError {
                            detail: e.to_string(),
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("failed to open database file: {e}"),
                    )],
                    statistics: ConsistencyStatistics::default(),
                    passed_checks: Vec::new(),
                    skipped_checks: Vec::new(),
                    truncated: false,
                });
            }
        };

        let mut header_bytes = [0u8; PAGE_SIZE];
        if let Err(e) = file.read_exact(&mut header_bytes) {
            return Ok(ConsistencyReport {
                issues: vec![ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::IoError {
                        detail: e.to_string(),
                    },
                    None,
                    None,
                    Some(0),
                    None,
                    format!("failed to read database header page: {e}"),
                )],
                statistics: ConsistencyStatistics::default(),
                passed_checks: Vec::new(),
                skipped_checks: Vec::new(),
                truncated: false,
            });
        }

        // Check magic
        if &header_bytes[0..8] != MAGIC_BYTES {
            let mut magic = [0u8; 8];
            magic.copy_from_slice(&header_bytes[0..8]);
            return Ok(ConsistencyReport {
                issues: vec![ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::InvalidHeaderMagic { found: magic },
                    None,
                    None,
                    Some(0),
                    None,
                    format!("invalid database magic bytes: {magic:?}"),
                )],
                statistics: ConsistencyStatistics::default(),
                passed_checks: Vec::new(),
                skipped_checks: Vec::new(),
                truncated: false,
            });
        }

        // Check format version
        let format_version = u32::from_le_bytes([
            header_bytes[8],
            header_bytes[9],
            header_bytes[10],
            header_bytes[11],
        ]);
        if format_version != CURRENT_FORMAT_VERSION {
            return Ok(ConsistencyReport {
                issues: vec![ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::UnsupportedFormatVersion {
                        found: format_version,
                        supported: CURRENT_FORMAT_VERSION,
                    },
                    None, None, Some(0), None,
                    format!("unsupported format version {format_version} (expected {CURRENT_FORMAT_VERSION})"),
                )],
                statistics: ConsistencyStatistics::default(),
                passed_checks: Vec::new(),
                skipped_checks: Vec::new(),
                truncated: false,
            });
        }

        // Now open with TreeDb::open and run verify
        let mut db = match TreeDb::open(p, 64) {
            Ok(db) => db,
            Err(Error::WalCorruption(msg)) => {
                return Ok(ConsistencyReport {
                    issues: vec![ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::WalRecordCorruption {
                            lsn: NULL_LSN,
                            reason: msg.clone(),
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("corrupted WAL record detected during recovery check: {msg}"),
                    )],
                    statistics: ConsistencyStatistics::default(),
                    passed_checks: Vec::new(),
                    skipped_checks: Vec::new(),
                    truncated: false,
                });
            }
            Err(e) => {
                return Ok(ConsistencyReport {
                    issues: vec![ConsistencyIssue::new(
                        ConsistencySeverity::Fatal,
                        ConsistencyIssueKind::IoError {
                            detail: e.to_string(),
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("failed to open database: {e}"),
                    )],
                    statistics: ConsistencyStatistics::default(),
                    passed_checks: Vec::new(),
                    skipped_checks: Vec::new(),
                    truncated: false,
                });
            }
        };

        Self::verify(&mut db, config)
    }

    fn record_issue(&mut self, issue: ConsistencyIssue) {
        if self.issues.len() < self.config.max_issues {
            self.issues.push(issue);
        } else if !self.truncated {
            self.truncated = true;
            self.issues.push(ConsistencyIssue::new(
                ConsistencySeverity::Warning,
                ConsistencyIssueKind::VerificationLimitReached {
                    limit_name: "max_issues".to_string(),
                    limit_value: self.config.max_issues,
                },
                None,
                None,
                None,
                None,
                format!("reached maximum issue limit of {}", self.config.max_issues),
            ));
        }
    }

    fn should_stop(&mut self) -> bool {
        if self.issues.len() >= self.config.max_issues {
            self.truncated = true;
            return true;
        }
        if self.check_page_limit() {
            return true;
        }
        if let Some(max_r) = self.config.max_records {
            if self.statistics.records_examined >= max_r {
                if !self.truncated {
                    self.truncated = true;
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Info,
                        ConsistencyIssueKind::VerificationLimitReached {
                            limit_name: "max_records".to_string(),
                            limit_value: max_r,
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("reached maximum record verification limit of {max_r}"),
                    ));
                }
                return true;
            }
        }
        false
    }

    fn check_page_limit(&mut self) -> bool {
        if let Some(max_p) = self.config.max_pages {
            if self.statistics.pages_examined >= max_p {
                if !self.truncated {
                    self.truncated = true;
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Info,
                        ConsistencyIssueKind::VerificationLimitReached {
                            limit_name: "max_pages".to_string(),
                            limit_value: max_p,
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("reached maximum page verification limit of {max_p}"),
                    ));
                }
                return true;
            }
        }
        false
    }

    fn run_verification(&mut self) -> Result<()> {
        // Pass 1: Database File and Header Invariants
        self.verify_file_and_header()?;
        if self.truncated || self.has_fatal_header_issue() {
            return Ok(());
        }

        // Pass 2: Freelist Structure and Integrity
        if self.config.check_freelist {
            self.verify_freelist()?;
        } else {
            self.skipped_checks.push("freelist".to_string());
        }
        if self.should_stop() {
            return Ok(());
        }

        // Pass 3: Catalog & Table Metadata
        let tables = self.verify_catalog_metadata()?;
        if self.should_stop() {
            return Ok(());
        }

        // Pass 4: Page Ownership Mapping
        if self.config.check_page_ownership {
            self.verify_page_ownership(&tables)?;
        } else {
            self.skipped_checks.push("page_ownership".to_string());
        }
        if self.should_stop() {
            return Ok(());
        }

        // Pass 5: Primary B+ Trees, Slotted Pages, and MVCC Chains
        self.verify_tables_and_records(&tables)?;
        if self.should_stop() {
            return Ok(());
        }

        // Pass 6: Secondary, Unique, and Covering Indexes
        if self.config.check_secondary_indexes {
            self.verify_secondary_indexes(&tables)?;
        } else {
            self.skipped_checks.push("secondary_indexes".to_string());
        }
        if self.should_stop() {
            return Ok(());
        }

        // Pass 7: WAL and Recovery State
        if self.config.check_wal {
            self.verify_wal_state()?;
        } else {
            self.skipped_checks.push("wal".to_string());
        }

        Ok(())
    }

    fn has_fatal_header_issue(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity == ConsistencySeverity::Fatal)
    }

    // -------------------------------------------------------------------------
    // Pass 1: Database File & Header Invariants
    // -------------------------------------------------------------------------
    fn verify_file_and_header(&mut self) -> Result<()> {
        let db_path = self.db.buffer_pool.page_manager().path().to_path_buf();
        let file_len = match std::fs::metadata(&db_path) {
            Ok(m) => m.len(),
            Err(e) => {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::IoError {
                        detail: e.to_string(),
                    },
                    None,
                    None,
                    None,
                    None,
                    format!("failed to read database file metadata: {e}"),
                ));
                return Ok(());
            }
        };

        let min_expected = (2 * PAGE_SIZE) as u64;
        if file_len < min_expected {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::DatabaseFileTruncated {
                    expected_min_bytes: min_expected,
                    actual_bytes: file_len,
                },
                None,
                None,
                Some(0),
                None,
                format!("database file size ({file_len} bytes) is less than minimum 2 pages ({min_expected} bytes)"),
            ));
            return Ok(());
        }

        let header = self.db.buffer_pool.page_manager().header().clone();
        self.statistics.total_pages_in_file = header.page_count;

        // Check magic bytes
        if header.magic != *MAGIC_BYTES {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::InvalidHeaderMagic {
                    found: header.magic,
                },
                None,
                None,
                Some(0),
                None,
                format!("invalid database magic bytes: {:?}", header.magic),
            ));
        }

        // Check format version
        if header.format_version != CURRENT_FORMAT_VERSION {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::UnsupportedFormatVersion {
                    found: header.format_version,
                    supported: CURRENT_FORMAT_VERSION,
                },
                None,
                None,
                Some(0),
                None,
                format!(
                    "unsupported format version {} (expected {CURRENT_FORMAT_VERSION})",
                    header.format_version
                ),
            ));
        }

        // Check page size
        if header.page_size != PAGE_SIZE as u32 {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::InvalidPageSize {
                    found: header.page_size,
                    expected: PAGE_SIZE as u32,
                },
                None,
                None,
                Some(0),
                None,
                format!(
                    "configured page size {} != system PAGE_SIZE {PAGE_SIZE}",
                    header.page_size
                ),
            ));
        }

        // Check file size matches header.page_count * PAGE_SIZE
        let expected_file_bytes = (header.page_count as u64) * (PAGE_SIZE as u64);
        if file_len < expected_file_bytes {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::PageCountMismatch {
                    header_page_count: header.page_count,
                    file_page_count: (file_len / PAGE_SIZE as u64) as u32,
                },
                None,
                None,
                Some(0),
                None,
                format!(
                    "file length ({file_len} bytes) is shorter than recorded page_count {} ({expected_file_bytes} bytes)",
                    header.page_count
                ),
            ));
        }

        // Check metadata page reference
        if header.metadata_page_id >= header.page_count {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::PageOutOfBounds {
                    page_id: header.metadata_page_id,
                    max_page_id: header.page_count,
                },
                None,
                None,
                Some(header.metadata_page_id),
                None,
                format!(
                    "metadata_page_id {} >= total page count {}",
                    header.metadata_page_id, header.page_count
                ),
            ));
        }

        // Check root page reference if set
        if header.root_page_id != 0 && header.root_page_id >= header.page_count {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::PageOutOfBounds {
                    page_id: header.root_page_id,
                    max_page_id: header.page_count,
                },
                None,
                None,
                Some(header.root_page_id),
                None,
                format!(
                    "engine root_page_id {} >= total page count {}",
                    header.root_page_id, header.page_count
                ),
            ));
        }

        // Mark Page 0 as Header owned
        self.page_ownership
            .insert(0, "Database Header (Page 0)".to_string());
        self.statistics.pages_examined += 1;
        self.passed_checks.push("database_header".to_string());
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Pass 2: Freelist Structure & Integrity
    // -------------------------------------------------------------------------
    fn verify_freelist(&mut self) -> Result<()> {
        let header = self.db.buffer_pool.page_manager().header().clone();
        let total_pages = header.page_count;
        let mut curr = header.freelist_head;
        let expected_count = header.free_page_count as usize;
        let mut traversed = HashSet::new();

        while curr != NO_FREE_PAGE {
            if self.check_page_limit() {
                break;
            }

            if is_reserved_page(curr) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::FreelistContainsReservedPage { page_id: curr },
                    None,
                    None,
                    Some(curr),
                    None,
                    format!("freelist links reserved page {curr}"),
                ));
                break;
            }

            if curr >= total_pages {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::PageOutOfBounds {
                        page_id: curr,
                        max_page_id: total_pages,
                    },
                    None,
                    None,
                    Some(curr),
                    None,
                    format!("freelist page {curr} >= total pages {total_pages}"),
                ));
                break;
            }

            if !traversed.insert(curr) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::FreelistCycle { page_id: curr },
                    None,
                    None,
                    Some(curr),
                    None,
                    format!("cycle detected in freelist at page {curr}"),
                ));
                break;
            }

            self.statistics.pages_examined += 1;
            let page = match self.db.buffer_pool.fetch_page(curr) {
                Ok(p) => p,
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::IoError {
                            detail: e.to_string(),
                        },
                        None,
                        None,
                        Some(curr),
                        None,
                        format!("failed to fetch freelist page {curr}: {e}"),
                    ));
                    break;
                }
            };

            let pt = page.page_type();
            let next_free = if pt != PageType::Free {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::InvalidFreelistPageType {
                        page_id: curr,
                        tag: pt.to_u8(),
                    },
                    None,
                    None,
                    Some(curr),
                    None,
                    format!(
                        "freelist page {curr} has page_type {:?} instead of Free",
                        pt
                    ),
                ));
                NO_FREE_PAGE
            } else {
                FreePageLayout::read_next_free(page)
            };

            self.db.buffer_pool.unpin_page(curr, false)?;
            curr = next_free;
        }

        self.statistics.freelist_pages_traversed = traversed.len();

        if !self.truncated && traversed.len() != expected_count {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Error,
                ConsistencyIssueKind::FreelistCountMismatch {
                    header_count: expected_count as u32,
                    traversed_count: traversed.len() as u32,
                },
                None,
                None,
                None,
                None,
                format!(
                    "freelist count mismatch: header recorded {expected_count}, but traversed {}",
                    traversed.len()
                ),
            ));
        }

        for &pid in &traversed {
            self.page_ownership.insert(pid, "Freelist".to_string());
        }
        self.freelist_pages = traversed;
        self.passed_checks.push("freelist_invariants".to_string());
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Pass 3: Catalog & Table Metadata Invariants
    // -------------------------------------------------------------------------
    fn verify_catalog_metadata(&mut self) -> Result<Vec<crate::engine::catalog::TableMetadata>> {
        let header = self.db.buffer_pool.page_manager().header().clone();
        let mut curr_cat_pid = header.metadata_page_id;
        let mut cat_pages = HashSet::new();

        while curr_cat_pid != crate::engine::catalog::NO_NEXT_CATALOG_PAGE {
            if curr_cat_pid >= header.page_count {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::PageOutOfBounds {
                        page_id: curr_cat_pid,
                        max_page_id: header.page_count,
                    },
                    None,
                    None,
                    Some(curr_cat_pid),
                    None,
                    format!(
                        "catalog page {curr_cat_pid} >= total pages {}",
                        header.page_count
                    ),
                ));
                break;
            }

            if !cat_pages.insert(curr_cat_pid) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::DuplicatePageOwnership {
                        page_id: curr_cat_pid,
                        owner1: "Catalog".to_string(),
                        owner2: "Catalog Cycle".to_string(),
                    },
                    None,
                    None,
                    Some(curr_cat_pid),
                    None,
                    format!("cycle detected in catalog page chain at {curr_cat_pid}"),
                ));
                break;
            }

            let page = match self.db.buffer_pool.fetch_page(curr_cat_pid) {
                Ok(p) => p,
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::IoError {
                            detail: e.to_string(),
                        },
                        None,
                        None,
                        Some(curr_cat_pid),
                        None,
                        format!("failed to fetch catalog page {curr_cat_pid}: {e}"),
                    ));
                    break;
                }
            };

            let pt = page.page_type();
            let next_pid = if pt != PageType::Metadata && pt != PageType::Catalog {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::InvalidPageTypeTag {
                        page_id: curr_cat_pid,
                        tag: pt.to_u8(),
                    },
                    None,
                    None,
                    Some(curr_cat_pid),
                    None,
                    format!(
                        "expected Metadata/Catalog page type at {curr_cat_pid}, found {:?}",
                        pt
                    ),
                ));
                crate::engine::catalog::NO_NEXT_CATALOG_PAGE
            } else {
                match crate::engine::catalog::CatalogPage::deserialize(page) {
                    Ok(cp) => cp.next_catalog_page_id,
                    Err(e) => {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Error,
                            ConsistencyIssueKind::InvalidSlotDirectory {
                                page_id: curr_cat_pid,
                                reason: e.to_string(),
                            },
                            None,
                            None,
                            Some(curr_cat_pid),
                            None,
                            format!("failed to deserialize catalog page {curr_cat_pid}: {e}"),
                        ));
                        crate::engine::catalog::NO_NEXT_CATALOG_PAGE
                    }
                }
            };

            self.db.buffer_pool.unpin_page(curr_cat_pid, false)?;
            self.statistics.pages_examined += 1;
            self.page_ownership
                .insert(curr_cat_pid, format!("Catalog (Page {curr_cat_pid})"));
            curr_cat_pid = next_pid;
        }

        let tables = match Catalog::list_tables(&mut self.db.buffer_pool) {
            Ok(t) => t,
            Err(e) => {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::IoError {
                        detail: e.to_string(),
                    },
                    None,
                    None,
                    None,
                    None,
                    format!("failed to list catalog tables: {e}"),
                ));
                Vec::new()
            }
        };

        self.statistics.tables_examined = tables.len();
        self.passed_checks.push("catalog_metadata".to_string());
        Ok(tables)
    }

    // -------------------------------------------------------------------------
    // Pass 4: Cross-Layer Page Ownership & Orphan Detection
    // -------------------------------------------------------------------------
    fn verify_page_ownership(
        &mut self,
        tables: &[crate::engine::catalog::TableMetadata],
    ) -> Result<()> {
        let total_pages = self.db.buffer_pool.page_manager().header().page_count;

        // Collect engine primary tree pages if present
        if self.db.tree.root_page_id() != 0 {
            match self.db.tree.collect_all_pages(&mut self.db.buffer_pool) {
                Ok(pages) => {
                    for pid in pages {
                        self.register_page_owner(pid, "Engine Primary Tree".to_string());
                    }
                }
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::BPlusTreeCorruption {
                            page_id: Some(self.db.tree.root_page_id()),
                            reason: e.to_string(),
                        },
                        None,
                        None,
                        Some(self.db.tree.root_page_id()),
                        None,
                        format!("failed to collect engine tree pages: {e}"),
                    ));
                }
            }
        }

        // For each table, register primary tree pages, secondary index pages, and record pages
        for table_meta in tables {
            let tname = &table_meta.name;

            // 1. Table Primary B+ Tree
            let primary_tree = BPlusTree::from_root(table_meta.root_page_id);
            if primary_tree.root_page_id() != 0 {
                match primary_tree.collect_all_pages(&mut self.db.buffer_pool) {
                    Ok(pages) => {
                        for pid in pages {
                            self.register_page_owner(pid, format!("Table '{tname}' Primary Tree"));
                        }
                    }
                    Err(e) => {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Error,
                            ConsistencyIssueKind::BPlusTreeCorruption {
                                page_id: Some(primary_tree.root_page_id()),
                                reason: e.to_string(),
                            },
                            Some(tname.clone()),
                            None,
                            Some(primary_tree.root_page_id()),
                            None,
                            format!("failed to collect primary tree pages for '{tname}': {e}"),
                        ));
                    }
                }
            }

            // 2. Table Initial Record Page
            if table_meta.record_page_id != 0 && table_meta.record_page_id < total_pages {
                self.register_page_owner(
                    table_meta.record_page_id,
                    format!("Table '{tname}' Record Page"),
                );
            }

            // 3. Secondary Indexes
            let idx_metas =
                Catalog::list_indexes_for_table(&mut self.db.buffer_pool, table_meta.table_id)
                    .unwrap_or_default();

            for idx_meta in idx_metas {
                let sec_tree = SecondaryIndex::from_root(idx_meta.root_page_id);
                if sec_tree.root_page_id() != 0 {
                    match sec_tree.collect_all_pages(&mut self.db.buffer_pool) {
                        Ok(pages) => {
                            for pid in pages {
                                self.register_page_owner(
                                    pid,
                                    format!("Table '{tname}' Index '{}'", idx_meta.name),
                                );
                            }
                        }
                        Err(e) => {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::SecondaryIndexCorruption {
                                    index_name: idx_meta.name.clone(),
                                    reason: e.to_string(),
                                },
                                Some(tname.clone()),
                                Some(idx_meta.name.clone()),
                                Some(sec_tree.root_page_id()),
                                None,
                                format!("failed to collect secondary index pages: {e}"),
                            ));
                        }
                    }
                }
            }
        }

        // Detect Orphaned Pages:
        // Any page in 2..total_pages not registered in page_ownership and not in freelist
        for pid in 2..total_pages {
            if !self.page_ownership.contains_key(&pid) && !self.freelist_pages.contains(&pid) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Warning,
                    ConsistencyIssueKind::OrphanedPage { page_id: pid },
                    None,
                    None,
                    Some(pid),
                    None,
                    format!("page {pid} is unreferenced by any table, index, or freelist"),
                ));
            }
        }

        self.passed_checks.push("page_ownership".to_string());
        Ok(())
    }

    fn register_page_owner(&mut self, page_id: PageId, owner: String) {
        let total_pages = self.db.buffer_pool.page_manager().header().page_count;
        if page_id >= total_pages {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::PageOutOfBounds {
                    page_id,
                    max_page_id: total_pages,
                },
                None,
                None,
                Some(page_id),
                None,
                format!("page {page_id} referenced by '{owner}' exceeds total pages {total_pages}"),
            ));
            return;
        }

        // Check if page is also in freelist
        if self.freelist_pages.contains(&page_id) {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Fatal,
                ConsistencyIssueKind::FreelistContainsLivePage {
                    page_id,
                    owner: owner.clone(),
                },
                None,
                None,
                Some(page_id),
                None,
                format!("page {page_id} belongs to '{owner}' but is also present in freelist"),
            ));
        }

        if let Some(existing_owner) = self.page_ownership.insert(page_id, owner.clone()) {
            if existing_owner != owner {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::DuplicatePageOwnership {
                        page_id,
                        owner1: existing_owner,
                        owner2: owner,
                    },
                    None,
                    None,
                    Some(page_id),
                    None,
                    format!("page {page_id} is claimed by multiple distinct entities"),
                ));
            }
        }
    }

    // -------------------------------------------------------------------------
    // Pass 5: Tables, Slotted Pages & MVCC Chains
    // -------------------------------------------------------------------------
    fn verify_tables_and_records(
        &mut self,
        tables: &[crate::engine::catalog::TableMetadata],
    ) -> Result<()> {
        let total_pages = self.db.buffer_pool.page_manager().header().page_count;

        for table_meta in tables {
            if self.should_stop() {
                break;
            }

            let tname = &table_meta.name;
            let primary_tree = BPlusTree::from_root(table_meta.root_page_id);

            // 1. Structural B+ Tree Validation
            if self.config.check_bplus_trees {
                if let Err(e) = primary_tree.validate_tree(&mut self.db.buffer_pool) {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::BPlusTreeCorruption {
                            page_id: Some(primary_tree.root_page_id()),
                            reason: e.to_string(),
                        },
                        Some(tname.clone()),
                        None,
                        Some(primary_tree.root_page_id()),
                        None,
                        format!("primary tree structural corruption in '{tname}': {e}"),
                    ));
                }
            }

            // 2. Primary Index Entry Traversal
            let entries = match primary_tree.scan(&mut self.db.buffer_pool, None, None) {
                Ok(entries) => entries,
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::BPlusTreeCorruption {
                            page_id: Some(primary_tree.root_page_id()),
                            reason: e.to_string(),
                        },
                        Some(tname.clone()),
                        None,
                        Some(primary_tree.root_page_id()),
                        None,
                        format!("failed to scan primary tree for '{tname}': {e}"),
                    ));
                    continue;
                }
            };

            let mut seen_keys = HashSet::new();

            for (key, head_rid) in entries {
                if self.should_stop() {
                    break;
                }

                self.statistics.primary_keys_examined += 1;

                if !seen_keys.insert(key) {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::DuplicateLogicalPrimaryKey {
                            key,
                            head1: head_rid,
                            head2: head_rid,
                        },
                        Some(tname.clone()),
                        None,
                        Some(head_rid.page_id),
                        Some(head_rid),
                        format!("duplicate logical primary key {key} in '{tname}'"),
                    ));
                }

                // Check bounds of head_rid
                if head_rid.page_id >= total_pages {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Fatal,
                        ConsistencyIssueKind::PageOutOfBounds {
                            page_id: head_rid.page_id,
                            max_page_id: total_pages,
                        },
                        Some(tname.clone()),
                        None,
                        Some(head_rid.page_id),
                        Some(head_rid),
                        format!(
                            "primary key {key} points to out-of-bounds page {}",
                            head_rid.page_id
                        ),
                    ));
                    continue;
                }

                if self.freelist_pages.contains(&head_rid.page_id) {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Fatal,
                        ConsistencyIssueKind::PrimaryHeadPointsToFreedPage { key, rid: head_rid },
                        Some(tname.clone()),
                        None,
                        Some(head_rid.page_id),
                        Some(head_rid),
                        format!(
                            "primary key {key} points to freed page {}",
                            head_rid.page_id
                        ),
                    ));
                    continue;
                }

                // Check slot directory on head record page
                let (is_active_slot, is_overflow, head_opid, total_len) = {
                    let page = match self.db.buffer_pool.fetch_page(head_rid.page_id) {
                        Ok(p) => p,
                        Err(e) => {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::IoError {
                                    detail: e.to_string(),
                                },
                                Some(tname.clone()),
                                None,
                                Some(head_rid.page_id),
                                Some(head_rid),
                                format!("failed to fetch head record page: {e}"),
                            ));
                            continue;
                        }
                    };

                    let res = match RecordPage::deserialize(page) {
                        Ok(rp) => {
                            if (head_rid.slot_id as usize) < rp.slots.len() {
                                let slot = &rp.slots[head_rid.slot_id as usize];
                                let active = slot.is_active();
                                let ovf = match rp.get_overflow_descriptor(head_rid.slot_id) {
                                    Some(d) => (true, d.head_page_id, d.total_length),
                                    None => (false, 0, 0),
                                };
                                (active, ovf.0, ovf.1, ovf.2)
                            } else {
                                (false, false, 0, 0)
                            }
                        }
                        Err(e) => {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::InvalidSlotDirectory {
                                    page_id: head_rid.page_id,
                                    reason: e.to_string(),
                                },
                                Some(tname.clone()),
                                None,
                                Some(head_rid.page_id),
                                Some(head_rid),
                                format!("corrupted record page {}: {e}", head_rid.page_id),
                            ));
                            (false, false, 0, 0)
                        }
                    };

                    self.db.buffer_pool.unpin_page(head_rid.page_id, false)?;
                    self.statistics.pages_examined += 1;
                    if self.check_page_limit() {
                        break;
                    }
                    res
                };

                if !is_active_slot {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::PrimaryHeadPointsToDeletedSlot { key, rid: head_rid },
                        Some(tname.clone()),
                        None,
                        Some(head_rid.page_id),
                        Some(head_rid),
                        format!("primary key {key} points to deleted or invalid slot {head_rid:?}"),
                    ));
                    continue;
                }

                // Register record page ownership
                self.register_page_owner(head_rid.page_id, format!("Table '{tname}' Record Page"));

                // 3. Version Chain Inspection
                if self.config.check_record_store {
                    self.verify_version_chain(tname, key, head_rid)?;
                }

                // 4. Overflow Chain Validation
                if is_overflow {
                    self.verify_overflow_chain(tname, head_rid, head_opid, total_len)?;
                }
            }
        }

        self.passed_checks
            .push("primary_trees_and_record_stores".to_string());
        Ok(())
    }

    fn verify_version_chain(
        &mut self,
        table_name: &str,
        primary_key: Key,
        head_rid: RecordId,
    ) -> Result<()> {
        let mut curr_rid = head_rid;
        let mut visited = HashSet::new();
        let mut depth = 0;
        let mut next_created_at = None;
        let total_pages = self.db.buffer_pool.page_manager().header().page_count;

        self.statistics.version_chains_examined += 1;

        while curr_rid.page_id != 0 {
            if !visited.insert(curr_rid) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::VersionChainCycle {
                        head_rid,
                        cycle_rid: curr_rid,
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr_rid.page_id),
                    Some(curr_rid),
                    format!("cycle in version chain originating at primary key {primary_key} ({head_rid:?})"),
                ));
                break;
            }

            depth += 1;
            if depth > self.config.max_version_chain_depth {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::VersionChainTooDeep {
                        head_rid,
                        depth,
                        limit: self.config.max_version_chain_depth,
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr_rid.page_id),
                    Some(curr_rid),
                    format!(
                        "version chain depth {depth} exceeds limit {}",
                        self.config.max_version_chain_depth
                    ),
                ));
                break;
            }

            if curr_rid.page_id >= total_pages {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::PageOutOfBounds {
                        page_id: curr_rid.page_id,
                        max_page_id: total_pages,
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr_rid.page_id),
                    Some(curr_rid),
                    format!(
                        "version pointer points to out-of-bounds page {}",
                        curr_rid.page_id
                    ),
                ));
                break;
            }

            if self.freelist_pages.contains(&curr_rid.page_id) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::VersionPointsToFreedPage {
                        rid: curr_rid,
                        page_id: curr_rid.page_id,
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr_rid.page_id),
                    Some(curr_rid),
                    format!(
                        "version {curr_rid:?} resides on freed page {}",
                        curr_rid.page_id
                    ),
                ));
                break;
            }

            self.statistics.records_examined += 1;

            // Fetch and inspect the version
            let version_read = self
                .db
                .record_store
                .read_version(&mut self.db.buffer_pool, curr_rid);
            let (metadata, _) = match version_read {
                Ok(Some(v)) => v,
                Ok(None) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::MalformedVersionMetadata {
                            rid: curr_rid,
                            reason: "missing record payload in slot".to_string(),
                        },
                        Some(table_name.to_string()),
                        None,
                        Some(curr_rid.page_id),
                        Some(curr_rid),
                        format!("version {curr_rid:?} could not be read from slot"),
                    ));
                    break;
                }
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::MalformedVersionMetadata {
                            rid: curr_rid,
                            reason: e.to_string(),
                        },
                        Some(table_name.to_string()),
                        None,
                        Some(curr_rid.page_id),
                        Some(curr_rid),
                        format!("failed to deserialize version {curr_rid:?}: {e}"),
                    ));
                    break;
                }
            };

            // Invariant: created_at <= deleted_at
            if let (Some(c), Some(d)) = (metadata.created_at, metadata.deleted_at) {
                if c > d {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::VersionChainOrderViolation {
                            rid: curr_rid,
                            reason: format!("created_at {c} > deleted_at {d}"),
                        },
                        Some(table_name.to_string()),
                        None,
                        Some(curr_rid.page_id),
                        Some(curr_rid),
                        format!("version {curr_rid:?} has created_at {c} > deleted_at {d}"),
                    ));
                }
            }

            // Invariant: older version created_at <= newer version created_at
            if let (Some(newer_created), Some(older_created)) =
                (next_created_at, metadata.created_at)
            {
                if older_created > newer_created {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::VersionChainOrderViolation {
                            rid: curr_rid,
                            reason: format!("older version seq {older_created} > newer seq {newer_created}"),
                        },
                        Some(table_name.to_string()),
                        None,
                        Some(curr_rid.page_id),
                        Some(curr_rid),
                        format!("older version commit seq {older_created} > newer version commit seq {newer_created}"),
                    ));
                }
            }

            next_created_at = metadata.created_at;
            curr_rid = metadata.prev_version.unwrap_or(RecordId {
                page_id: 0,
                slot_id: 0,
            });
        }

        Ok(())
    }

    fn verify_overflow_chain(
        &mut self,
        table_name: &str,
        head_rid: RecordId,
        head_page_id: PageId,
        expected_len: u32,
    ) -> Result<()> {
        let mut curr = head_page_id;
        let mut visited = HashSet::new();
        let total_pages = self.db.buffer_pool.page_manager().header().page_count;
        let mut total_bytes = 0usize;

        while curr != NO_OVERFLOW_PAGE {
            if is_reserved_page(curr) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::OverflowCorruption {
                        page_id: curr,
                        reason: format!("overflow chain contains reserved page {curr}"),
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr),
                    Some(head_rid),
                    format!("overflow chain contains reserved page {curr}"),
                ));
                break;
            }

            if curr >= total_pages {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::PageOutOfBounds {
                        page_id: curr,
                        max_page_id: total_pages,
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr),
                    Some(head_rid),
                    format!("overflow page {curr} >= total pages {total_pages}"),
                ));
                break;
            }

            if self.freelist_pages.contains(&curr) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Fatal,
                    ConsistencyIssueKind::OverflowBelongsToFreelist { page_id: curr },
                    Some(table_name.to_string()),
                    None,
                    Some(curr),
                    Some(head_rid),
                    format!(
                        "overflow page {curr} belongs to version {head_rid:?} but is in freelist"
                    ),
                ));
                break;
            }

            if !visited.insert(curr) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::OverflowChainCycle {
                        head_page_id,
                        cycle_page_id: curr,
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr),
                    Some(head_rid),
                    format!("cycle detected in overflow chain starting at page {head_page_id}"),
                ));
                break;
            }

            if visited.len() > self.config.max_overflow_chain_depth {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::OverflowCorruption {
                        page_id: curr,
                        reason: "exceeded max overflow chain depth".to_string(),
                    },
                    Some(table_name.to_string()),
                    None,
                    Some(curr),
                    Some(head_rid),
                    format!(
                        "overflow chain exceeded maximum depth {}",
                        self.config.max_overflow_chain_depth
                    ),
                ));
                break;
            }

            self.statistics.overflow_pages_examined += 1;
            self.register_page_owner(curr, format!("Table '{table_name}' Overflow Page"));

            let page_fetch_res = self.db.buffer_pool.fetch_page(curr);
            let (is_overflow, bytes_used, next_page_id, err_opt) = match page_fetch_res {
                Ok(page) => {
                    let pt = page.page_type();
                    if pt != PageType::Overflow {
                        let _ = self.db.buffer_pool.unpin_page(curr, false);
                        (
                            false,
                            0,
                            NO_OVERFLOW_PAGE,
                            Some(format!(
                                "expected Overflow page type at {curr}, found {pt:?}"
                            )),
                        )
                    } else {
                        match OverflowPage::deserialize(page) {
                            Ok(op) => {
                                let bu = op.bytes_used();
                                let np = op.next_page_id();
                                let _ = self.db.buffer_pool.unpin_page(curr, false);
                                (true, bu, np, None)
                            }
                            Err(e) => {
                                let _ = self.db.buffer_pool.unpin_page(curr, false);
                                (
                                    true,
                                    0,
                                    NO_OVERFLOW_PAGE,
                                    Some(format!(
                                        "failed to deserialize overflow page {curr}: {e}"
                                    )),
                                )
                            }
                        }
                    }
                }
                Err(e) => (
                    false,
                    0,
                    NO_OVERFLOW_PAGE,
                    Some(format!("failed to fetch overflow page {curr}: {e}")),
                ),
            };

            if let Some(err_msg) = err_opt {
                if !is_overflow {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::InvalidPageTypeTag {
                            page_id: curr,
                            tag: 0,
                        },
                        Some(table_name.to_string()),
                        None,
                        Some(curr),
                        Some(head_rid),
                        err_msg,
                    ));
                } else {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::OverflowCorruption {
                            page_id: curr,
                            reason: err_msg.clone(),
                        },
                        Some(table_name.to_string()),
                        None,
                        Some(curr),
                        Some(head_rid),
                        err_msg,
                    ));
                }
                break;
            }

            total_bytes += bytes_used;
            curr = next_page_id;
        }

        if !self.truncated && total_bytes != expected_len as usize && !visited.is_empty() {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Error,
                ConsistencyIssueKind::OverflowCorruption {
                    page_id: head_page_id,
                    reason: format!("byte sum {total_bytes} != expected length {expected_len}"),
                },
                Some(table_name.to_string()),
                None,
                Some(head_page_id),
                Some(head_rid),
                format!("overflow total payload length mismatch: expected {expected_len}, found {total_bytes}"),
            ));
        }

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Pass 6: Secondary, Unique, and Covering Index Two-Way Invariants
    // -------------------------------------------------------------------------
    fn verify_secondary_indexes(
        &mut self,
        tables: &[crate::engine::catalog::TableMetadata],
    ) -> Result<()> {
        let total_pages = self.db.buffer_pool.page_manager().header().page_count;

        for table_meta in tables {
            if self.should_stop() {
                break;
            }

            let tname = &table_meta.name;
            let index_metas =
                Catalog::list_indexes_for_table(&mut self.db.buffer_pool, table_meta.table_id)
                    .unwrap_or_default();

            self.statistics.indexes_examined += index_metas.len();

            // Load all current live rows for reverse check
            let primary_tree = BPlusTree::from_root(table_meta.root_page_id);
            let live_rows: Vec<(Key, RecordId, Row)> = {
                let mut rows = Vec::new();
                if let Ok(entries) = primary_tree.scan(&mut self.db.buffer_pool, None, None) {
                    for (k, head_rid) in entries {
                        if head_rid.page_id < total_pages
                            && !self.freelist_pages.contains(&head_rid.page_id)
                        {
                            if let Ok(Some((vmeta, raw))) = self
                                .db
                                .record_store
                                .read_version(&mut self.db.buffer_pool, head_rid)
                            {
                                if vmeta.deleted_at.is_none() {
                                    if let Ok(row) = table_meta.schema.decode_tuple(&raw) {
                                        rows.push((k, head_rid, row));
                                    }
                                }
                            }
                        }
                    }
                }
                rows
            };

            for idx_meta in index_metas {
                if self.should_stop() {
                    break;
                }

                let iname = &idx_meta.name;
                let sec_tree = SecondaryIndex::from_root(idx_meta.root_page_id);

                // 1. Structural Tree Validation
                if let Err(e) = sec_tree.validate_tree(&mut self.db.buffer_pool, idx_meta.unique) {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::SecondaryIndexCorruption {
                            index_name: iname.clone(),
                            reason: e.to_string(),
                        },
                        Some(tname.clone()),
                        Some(iname.clone()),
                        Some(sec_tree.root_page_id()),
                        None,
                        format!("secondary index structural corruption in '{iname}': {e}"),
                    ));
                }

                // 2. Scan Index Entries
                let entries = match sec_tree.scan_entries(&mut self.db.buffer_pool, None, None) {
                    Ok(entries) => entries,
                    Err(e) => {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Error,
                            ConsistencyIssueKind::SecondaryIndexCorruption {
                                index_name: iname.clone(),
                                reason: e.to_string(),
                            },
                            Some(tname.clone()),
                            Some(iname.clone()),
                            Some(sec_tree.root_page_id()),
                            None,
                            format!("failed to scan secondary index entries for '{iname}': {e}"),
                        ));
                        continue;
                    }
                };

                let mut live_unique_keys: HashMap<IndexKey, RecordId> = HashMap::new();
                let mut index_entry_rids: HashSet<RecordId> = HashSet::new();

                for (idx_count, entry) in entries.into_iter().enumerate() {
                    if let Some(max_entries) = self.config.max_index_entries {
                        if idx_count >= max_entries {
                            break;
                        }
                    }

                    let rid = entry.rid;
                    index_entry_rids.insert(rid);

                    // Check bounds & freelist
                    if rid.page_id >= total_pages || self.freelist_pages.contains(&rid.page_id) {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Error,
                            ConsistencyIssueKind::SecondaryIndexEntryPointsToInvalidVersion {
                                index_name: iname.clone(),
                                rid,
                            },
                            Some(tname.clone()),
                            Some(iname.clone()),
                            Some(rid.page_id),
                            Some(rid),
                            format!(
                                "index '{iname}' entry points to freed or out-of-bounds page {}",
                                rid.page_id
                            ),
                        ));
                        continue;
                    }

                    // Read referenced version
                    let (vmeta, raw_bytes) = match self
                        .db
                        .record_store
                        .read_version(&mut self.db.buffer_pool, rid)
                    {
                        Ok(Some(v)) => v,
                        Ok(None) => {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::SecondaryIndexEntryPointsToInvalidVersion {
                                    index_name: iname.clone(),
                                    rid,
                                },
                                Some(tname.clone()),
                                Some(iname.clone()),
                                Some(rid.page_id),
                                Some(rid),
                                format!("index '{iname}' entry references missing record {rid:?}"),
                            ));
                            continue;
                        }
                        Err(e) => {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::MalformedVersionMetadata {
                                    rid,
                                    reason: e.to_string(),
                                },
                                Some(tname.clone()),
                                Some(iname.clone()),
                                Some(rid.page_id),
                                Some(rid),
                                format!("failed to read referenced version {rid:?}: {e}"),
                            ));
                            continue;
                        }
                    };

                    // Decode row
                    let row = match table_meta.schema.decode_tuple(&raw_bytes) {
                        Ok(r) => r,
                        Err(e) => {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::MalformedVersionMetadata {
                                    rid,
                                    reason: e.to_string(),
                                },
                                Some(tname.clone()),
                                Some(iname.clone()),
                                Some(rid.page_id),
                                Some(rid),
                                format!("failed to deserialize row payload at {rid:?}: {e}"),
                            ));
                            continue;
                        }
                    };

                    // Verify indexed value
                    let expected_key = idx_meta.extract_index_key(&row);
                    if expected_key != entry.key {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Error,
                            ConsistencyIssueKind::SecondaryIndexValueMismatch {
                                index_name: iname.clone(),
                                rid,
                                expected_key: format!("{expected_key:?}"),
                                found_key: format!("{:?}", entry.key),
                            },
                            Some(tname.clone()),
                            Some(iname.clone()),
                            Some(rid.page_id),
                            Some(rid),
                            format!(
                                "index value mismatch in '{iname}': expected {:?}, found {:?}",
                                expected_key, entry.key
                            ),
                        ));
                    }

                    // Verify covering index payload
                    if idx_meta.is_covering() {
                        let expected_payload = match idx_meta.extract_covered_payload(&row) {
                            Ok(p) => p,
                            Err(e) => {
                                self.record_issue(ConsistencyIssue::new(
                                    ConsistencySeverity::Error,
                                    ConsistencyIssueKind::CoveringPayloadMismatch {
                                        index_name: iname.clone(),
                                        rid,
                                    },
                                    Some(tname.clone()),
                                    Some(iname.clone()),
                                    Some(rid.page_id),
                                    Some(rid),
                                    format!("failed to extract covered payload: {e}"),
                                ));
                                Vec::new()
                            }
                        };

                        if expected_payload != entry.payload {
                            self.record_issue(ConsistencyIssue::new(
                                ConsistencySeverity::Error,
                                ConsistencyIssueKind::CoveringPayloadMismatch {
                                    index_name: iname.clone(),
                                    rid,
                                },
                                Some(tname.clone()),
                                Some(iname.clone()),
                                Some(rid.page_id),
                                Some(rid),
                                format!(
                                    "covering payload mismatch in index '{iname}' at record {rid:?}"
                                ),
                            ));
                        }
                    }

                    // Unique constraint check across live visible versions
                    if idx_meta.unique && vmeta.deleted_at.is_none() {
                        if let Some(other_rid) = live_unique_keys.insert(entry.key.clone(), rid) {
                            if other_rid != rid {
                                self.record_issue(ConsistencyIssue::new(
                                    ConsistencySeverity::Error,
                                    ConsistencyIssueKind::UniqueConstraintViolation {
                                        index_name: iname.clone(),
                                        key: format!("{:?}", entry.key),
                                        rid1: other_rid,
                                        rid2: rid,
                                    },
                                    Some(tname.clone()),
                                    Some(iname.clone()),
                                    Some(rid.page_id),
                                    Some(rid),
                                    format!(
                                        "unique index '{iname}' violation for key {:?}: shared by {other_rid:?} and {rid:?}",
                                        entry.key
                                    ),
                                ));
                            }
                        }
                    }
                }

                // 3. Reverse Check: ensure every live committed row has an index entry
                for (pk, live_rid, live_row) in &live_rows {
                    let expected_key = idx_meta.extract_index_key(live_row);
                    let found = match sec_tree.lookup(&mut self.db.buffer_pool, &expected_key) {
                        Ok(rids) => rids.contains(live_rid) || index_entry_rids.contains(live_rid),
                        Err(_) => false,
                    };

                    if !found {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Error,
                            ConsistencyIssueKind::MissingSecondaryIndexEntry {
                                index_name: iname.clone(),
                                key: *pk,
                                rid: *live_rid,
                            },
                            Some(tname.clone()),
                            Some(iname.clone()),
                            Some(live_rid.page_id),
                            Some(*live_rid),
                            format!(
                                "live row with primary key {pk} ({live_rid:?}) is missing from index '{iname}'"
                            ),
                        ));
                    }
                }
            }
        }

        self.passed_checks.push("secondary_indexes".to_string());
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Pass 7: WAL Header, Record CRC32 & Recovery State
    // -------------------------------------------------------------------------
    fn verify_wal_state(&mut self) -> Result<()> {
        let db_path = self.db.buffer_pool.page_manager().path();
        let wal_path = db_path.with_extension("wal");

        if !wal_path.exists() {
            self.passed_checks.push("wal_state".to_string());
            return Ok(());
        }

        let mut file = match File::open(&wal_path) {
            Ok(f) => f,
            Err(e) => {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Warning,
                    ConsistencyIssueKind::IoError {
                        detail: e.to_string(),
                    },
                    None,
                    None,
                    None,
                    None,
                    format!("failed to open WAL file for verification: {e}"),
                ));
                return Ok(());
            }
        };

        let file_len = match file.metadata() {
            Ok(m) => m.len(),
            Err(e) => {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Warning,
                    ConsistencyIssueKind::IoError {
                        detail: e.to_string(),
                    },
                    None,
                    None,
                    None,
                    None,
                    format!("failed to read WAL metadata: {e}"),
                ));
                return Ok(());
            }
        };

        if file_len < WAL_HEADER_SIZE as u64 {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Error,
                ConsistencyIssueKind::WalHeaderCorruption {
                    reason: format!(
                        "WAL file size ({file_len} bytes) < header size ({WAL_HEADER_SIZE} bytes)"
                    ),
                },
                None,
                None,
                None,
                None,
                "WAL file truncated before header completed",
            ));
            return Ok(());
        }

        let mut header_buf = [0u8; WAL_HEADER_SIZE];
        if let Err(e) = file.read_exact(&mut header_buf) {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Error,
                ConsistencyIssueKind::WalHeaderCorruption {
                    reason: e.to_string(),
                },
                None,
                None,
                None,
                None,
                format!("failed to read WAL header: {e}"),
            ));
            return Ok(());
        }

        if &header_buf[0..8] != WAL_MAGIC_BYTES {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Error,
                ConsistencyIssueKind::WalHeaderCorruption {
                    reason: "invalid WAL magic bytes".to_string(),
                },
                None,
                None,
                None,
                None,
                format!("invalid WAL magic bytes: {:?}", &header_buf[0..8]),
            ));
            return Ok(());
        }

        // Read records sequentially without modifying anything
        let mut prev_lsn = NULL_LSN;
        let mut unapplied_records = 0usize;
        let mut max_lsn = NULL_LSN;
        let header = self.db.buffer_pool.page_manager().header().clone();
        let checkpoint_lsn = header.last_checkpoint_lsn;

        loop {
            // Read 4-byte payload length prefix
            let mut len_buf = [0u8; 4];
            match file.read_exact(&mut len_buf) {
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::WalRecordCorruption {
                            lsn: prev_lsn,
                            reason: e.to_string(),
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("failed reading WAL record length: {e}"),
                    ));
                    break;
                }
            }

            let payload_len = u32::from_le_bytes(len_buf) as usize;
            if payload_len < 4 + 8 + 8 + 8 + 1 {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::WalRecordCorruption {
                        lsn: prev_lsn,
                        reason: format!("invalid WAL record payload length {payload_len}"),
                    },
                    None,
                    None,
                    None,
                    None,
                    format!("invalid WAL record payload length {payload_len}"),
                ));
                break;
            }

            let mut record_buf = vec![0u8; payload_len];
            if let Err(e) = file.read_exact(&mut record_buf) {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::WalRecordCorruption {
                        lsn: prev_lsn,
                        reason: format!("truncated WAL record payload: {e}"),
                    },
                    None,
                    None,
                    None,
                    None,
                    format!("truncated WAL record payload: {e}"),
                ));
                break;
            }

            let mut full_bytes = Vec::with_capacity(4 + payload_len);
            full_bytes.extend_from_slice(&len_buf);
            full_bytes.extend_from_slice(&record_buf);

            let (record, _) = match LogRecord::deserialize(&full_bytes) {
                Ok(r) => r,
                Err(e) => {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::WalRecordCorruption {
                            lsn: prev_lsn,
                            reason: e.to_string(),
                        },
                        None,
                        None,
                        None,
                        None,
                        format!("corrupted WAL record CRC or payload after LSN {prev_lsn}: {e}"),
                    ));
                    break;
                }
            };

            self.statistics.wal_records_examined += 1;

            // Invariant: LSN monotonicity
            if prev_lsn != NULL_LSN && record.lsn <= prev_lsn {
                self.record_issue(ConsistencyIssue::new(
                    ConsistencySeverity::Error,
                    ConsistencyIssueKind::WalMonotonicityViolation {
                        prev_lsn,
                        curr_lsn: record.lsn,
                    },
                    None,
                    None,
                    None,
                    None,
                    format!(
                        "LSN monotonicity violation: record LSN {} <= previous LSN {prev_lsn}",
                        record.lsn
                    ),
                ));
            }

            // Invariant: PageDelta/CLR target page bounds
            match &record.body {
                LogRecordBody::PageDelta { page_id, .. } | LogRecordBody::Clr { page_id, .. } => {
                    if *page_id >= header.page_count {
                        self.record_issue(ConsistencyIssue::new(
                            ConsistencySeverity::Warning,
                            ConsistencyIssueKind::PageOutOfBounds {
                                page_id: *page_id,
                                max_page_id: header.page_count,
                            },
                            None,
                            None,
                            Some(*page_id),
                            None,
                            format!(
                                "WAL record LSN {} references page {} >= page_count {}",
                                record.lsn, page_id, header.page_count
                            ),
                        ));
                    }
                }
                LogRecordBody::Commit { commit_seq }
                    if *commit_seq > 0
                        && *commit_seq > header.max_commit_seq
                        && record.lsn <= checkpoint_lsn =>
                {
                    self.record_issue(ConsistencyIssue::new(
                        ConsistencySeverity::Error,
                        ConsistencyIssueKind::CommitSequenceRegression {
                            max_durable_seq: header.max_commit_seq,
                            found_seq: *commit_seq,
                        },
                        None,
                        None,
                        None,
                        None,
                        format!(
                            "checkpointed commit sequence {} > header max_commit_seq {}",
                            commit_seq, header.max_commit_seq
                        ),
                    ));
                }
                _ => {}
            }

            if record.lsn > checkpoint_lsn {
                unapplied_records += 1;
            }

            prev_lsn = record.lsn;
            if record.lsn > max_lsn {
                max_lsn = record.lsn;
            }
        }

        if unapplied_records > 0 {
            self.record_issue(ConsistencyIssue::new(
                ConsistencySeverity::Info,
                ConsistencyIssueKind::UnappliedWalTail {
                    unapplied_records,
                    max_lsn,
                },
                None,
                None,
                None,
                None,
                format!(
                    "WAL contains {unapplied_records} unapplied records beyond checkpoint LSN {checkpoint_lsn} (max LSN {max_lsn}); crash recovery is a prerequisite to apply them",
                ),
            ));
        }

        self.passed_checks.push("wal_state".to_string());
        Ok(())
    }
}
