//! Subsystem for consistent offline backup, validation, and safe restore.
//!
//! Provides deterministic, atomic database imaging, backup validation (verifying
//! byte integrity, header compatibility, and non-destructive read-only consistency),
//! and safe staged restore with rollback protection and failure injection.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::engine::failure::StorageFailurePoint;
use crate::engine::TreeDb;
use crate::error::Error;
use crate::storage::metadata::{DbHeader, CURRENT_FORMAT_VERSION, MAGIC_BYTES};
use crate::storage::page::PAGE_SIZE;
use crate::wal::lsn::TxnId;

static BACKUP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Recognizable magic bytes for TreeDB backup manifests.
pub const BACKUP_MANIFEST_MAGIC: &[u8; 8] = b"TDBACKUP";

/// Current format version for backup manifests.
pub const CURRENT_BACKUP_VERSION: u32 = 1;

/// Standard file name for the backup manifest.
pub const MANIFEST_FILE_NAME: &str = "treedb.manifest";

/// Streaming IEEE 802.3 CRC32 calculation matching TreeDB's log record CRC.
#[derive(Debug, Clone, Copy)]
pub struct Crc32Hasher {
    state: u32,
}

impl Default for Crc32Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32Hasher {
    /// Creates a new CRC32 hasher initialized to all ones.
    pub const fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Updates the running CRC32 state with a slice of bytes.
    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.state ^= byte as u32;
            for _ in 0..8 {
                let mask = ((self.state & 1) != 0) as u32;
                self.state = (self.state >> 1) ^ (0xEDB8_8320 & (!mask).wrapping_add(1));
            }
        }
    }

    /// Finalizes and returns the inverted CRC32 digest.
    pub const fn finalize(self) -> u32 {
        !self.state
    }
}

/// Errors occurring during backup creation, image validation, or database restoration.
#[derive(Error, Debug)]
pub enum BackupError {
    /// Underlying I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Core TreeDB engine error occurred.
    #[error("Database error: {0}")]
    Engine(String),

    /// Simulated failure injected at a deterministic boundary.
    #[error("Injected fault at boundary: {0:?}")]
    InjectedFault(StorageFailurePoint),

    /// Source database file was not found.
    #[error("Source database not found: {}", .0.display())]
    SourceNotFound(PathBuf),

    /// Backup rejected because an active transaction is open.
    #[error("Active transaction {0:?} is running on the source database")]
    ActiveTransaction(TxnId),

    /// Source and destination paths collide or refer to the same location.
    #[error("Source and destination path collision: source {} and destination {}", .source_path.display(), .destination_path.display())]
    PathCollision {
        /// Source path.
        source_path: PathBuf,
        /// Destination path.
        destination_path: PathBuf,
    },

    /// Destination already exists and overwrite was not explicitly allowed.
    #[error("Destination already exists: {} (overwrite disallowed)", .0.display())]
    DestinationExists(PathBuf),

    /// Backup manifest is corrupt, missing, or malformed.
    #[error("Invalid backup manifest: {0}")]
    InvalidManifest(String),

    /// Backup format version is unsupported.
    #[error("Unsupported backup format version: found {found}, supported {supported}")]
    UnsupportedBackupVersion {
        /// Version found in manifest.
        found: u32,
        /// Version supported by this engine.
        supported: u32,
    },

    /// TreeDB database format version is unsupported.
    #[error("Unsupported database format version: found {found}, supported {supported}")]
    UnsupportedDatabaseVersion {
        /// Version found in image/manifest.
        found: u32,
        /// Version supported by this engine.
        supported: u32,
    },

    /// Checksum mismatch detected on an image file or manifest.
    #[error("Checksum mismatch for {file}: expected 0x{expected:08x}, computed 0x{computed:08x}")]
    ChecksumMismatch {
        /// File name or description.
        file: String,
        /// Expected checksum.
        expected: u32,
        /// Computed checksum.
        computed: u32,
    },

    /// File size mismatch between recorded metadata and physical file on disk.
    #[error("Size mismatch for {file}: expected {expected} bytes, found {found} bytes")]
    SizeMismatch {
        /// File name or description.
        file: String,
        /// Expected byte length.
        expected: u64,
        /// Actual byte length on disk.
        found: u64,
    },

    /// Required image file missing from backup directory.
    #[error("Missing required backup file: {0}")]
    MissingFile(String),

    /// Database header on Page 0 is corrupt or fails invariant validation.
    #[error("Database header corruption: {0}")]
    CorruptHeader(String),

    /// Database image length is not an exact multiple of the configured page size.
    #[error("Database image length {len} bytes is not a multiple of page size {page_size}")]
    ImageNotPageAligned {
        /// Physical file length.
        len: u64,
        /// Page size in bytes.
        page_size: u32,
    },

    /// Cross-layer consistency verification failed on the backup or restored database.
    #[error("Consistency verification failed: {0}")]
    ConsistencyFailure(String),

    /// Extraneous unrecognized artifact found in backup directory.
    #[error("Unexpected extra artifact in backup directory: {0}")]
    UnexpectedArtifact(String),
}

impl From<Error> for BackupError {
    fn from(err: Error) -> Self {
        match err {
            Error::InjectedFault(pt) => BackupError::InjectedFault(pt),
            Error::Io(io_err) => BackupError::Io(io_err),
            other => BackupError::Engine(other.to_string()),
        }
    }
}

/// Configuration options for creating and validating database backups.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    /// Optional failure injection point for deterministic boundary testing.
    pub fault_point: Option<StorageFailurePoint>,
    /// Whether to force a full checkpoint and WAL truncation prior to backup.
    pub checkpoint_before_backup: bool,
    /// Whether to perform deep non-destructive read-only consistency verification.
    pub verify_image: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            fault_point: None,
            checkpoint_before_backup: true,
            verify_image: true,
        }
    }
}

/// Configuration options for restoring database backups.
#[derive(Debug, Clone)]
pub struct RestoreConfig {
    /// Whether restoring can overwrite an existing target database and WAL file.
    pub allow_overwrite: bool,
    /// Optional failure injection point for deterministic boundary testing.
    pub fault_point: Option<StorageFailurePoint>,
    /// Whether to validate the backup manifest and image integrity prior to restoring.
    pub validate_before_restore: bool,
    /// Whether to run consistency verification on the restored database.
    pub verify_after_restore: bool,
    /// Buffer pool size to use for post-restore verification.
    pub buffer_pool_size: usize,
}

impl Default for RestoreConfig {
    fn default() -> Self {
        Self {
            allow_overwrite: false,
            fault_point: None,
            validate_before_restore: true,
            verify_after_restore: true,
            buffer_pool_size: 64,
        }
    }
}

/// Validates that a file name does not contain illegal characters, path traversal, or exceed length bounds.
pub fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && name != "."
        && name != ".."
}

/// Self-describing, CRC32-protected metadata manifest for a database backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupManifest {
    /// Version of the backup manifest format.
    pub backup_version: u32,
    /// Database file format version recorded in Page 0 header.
    pub db_format_version: u32,
    /// Database page size in bytes (typically 4096).
    pub page_size: u32,
    /// Total page count recorded in the Page 0 header.
    pub page_count: u32,
    /// File name of the primary database file within the backup directory.
    pub db_file_name: String,
    /// Exact byte size of the primary database file.
    pub db_file_size: u64,
    /// IEEE 802.3 CRC32 checksum of the primary database file bytes.
    pub db_file_checksum: u32,
    /// Whether an associated WAL file was included in this backup unit.
    pub has_wal: bool,
    /// File name of the companion WAL file, if present.
    pub wal_file_name: Option<String>,
    /// Exact byte size of the companion WAL file, if present.
    pub wal_file_size: u64,
    /// IEEE 802.3 CRC32 checksum of the companion WAL file bytes, if present.
    pub wal_file_checksum: u32,
    /// Durable maximum commit sequence number recorded at backup time.
    pub max_commit_seq: u64,
    /// Last checkpoint LSN recorded in the database header.
    pub last_checkpoint_lsn: u64,
    /// Timestamp (UTC epoch seconds) when the backup was created.
    pub created_at_utc_epoch_secs: u64,
}

impl BackupManifest {
    /// Serializes the manifest into a binary byte representation protected by a trailing CRC32.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(BACKUP_MANIFEST_MAGIC);
        buf.extend_from_slice(&self.backup_version.to_le_bytes());
        buf.extend_from_slice(&self.db_format_version.to_le_bytes());
        buf.extend_from_slice(&self.page_size.to_le_bytes());
        buf.extend_from_slice(&self.page_count.to_le_bytes());
        buf.extend_from_slice(&self.db_file_size.to_le_bytes());
        buf.extend_from_slice(&self.db_file_checksum.to_le_bytes());
        buf.extend_from_slice(&self.max_commit_seq.to_le_bytes());
        buf.extend_from_slice(&self.last_checkpoint_lsn.to_le_bytes());
        buf.extend_from_slice(&self.created_at_utc_epoch_secs.to_le_bytes());

        let db_name_bytes = self.db_file_name.as_bytes();
        buf.extend_from_slice(&(db_name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(db_name_bytes);

        if self.has_wal {
            buf.push(1);
            let wal_name = self.wal_file_name.as_deref().unwrap_or("");
            let wal_name_bytes = wal_name.as_bytes();
            buf.extend_from_slice(&(wal_name_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(wal_name_bytes);
            buf.extend_from_slice(&self.wal_file_size.to_le_bytes());
            buf.extend_from_slice(&self.wal_file_checksum.to_le_bytes());
        } else {
            buf.push(0);
        }

        let crc = crate::wal::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Deserializes and validates a binary backup manifest buffer.
    pub fn deserialize(bytes: &[u8]) -> std::result::Result<Self, BackupError> {
        if bytes.len() > 65536 {
            return Err(BackupError::InvalidManifest(
                "manifest buffer exceeds 64KB limit".into(),
            ));
        }

        if bytes.len() < 8 + 4 + 4 + 4 + 4 + 8 + 4 + 8 + 8 + 8 + 2 + 1 + 4 {
            return Err(BackupError::InvalidManifest(
                "manifest buffer is too short".into(),
            ));
        }

        let payload_len = bytes.len() - 4;
        let expected_crc = u32::from_le_bytes(bytes[payload_len..].try_into().unwrap());
        let computed_crc = crate::wal::crc32(&bytes[..payload_len]);
        if computed_crc != expected_crc {
            return Err(BackupError::ChecksumMismatch {
                file: MANIFEST_FILE_NAME.into(),
                expected: expected_crc,
                computed: computed_crc,
            });
        }

        let mut cursor = 0;
        if &bytes[cursor..cursor + 8] != BACKUP_MANIFEST_MAGIC {
            return Err(BackupError::InvalidManifest(
                "invalid manifest magic identifier".into(),
            ));
        }
        cursor += 8;

        let backup_version = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
        if backup_version != CURRENT_BACKUP_VERSION {
            return Err(BackupError::UnsupportedBackupVersion {
                found: backup_version,
                supported: CURRENT_BACKUP_VERSION,
            });
        }

        let db_format_version = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
        if db_format_version != CURRENT_FORMAT_VERSION {
            return Err(BackupError::UnsupportedDatabaseVersion {
                found: db_format_version,
                supported: CURRENT_FORMAT_VERSION,
            });
        }

        let page_size = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;

        let page_count = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;

        let db_file_size = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;

        let db_file_checksum = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;

        let max_commit_seq = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;

        let last_checkpoint_lsn = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;

        let created_at_utc_epoch_secs =
            u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;

        let db_name_len =
            u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().unwrap()) as usize;
        cursor += 2;
        if cursor + db_name_len > payload_len {
            return Err(BackupError::InvalidManifest(
                "db_file_name length out of bounds".into(),
            ));
        }
        let db_file_name = std::str::from_utf8(&bytes[cursor..cursor + db_name_len])
            .map_err(|e| {
                BackupError::InvalidManifest(format!("invalid UTF-8 in db_file_name: {e}"))
            })?
            .to_string();
        if !is_safe_file_name(&db_file_name) {
            return Err(BackupError::InvalidManifest(
                "db_file_name contains invalid or unsafe path characters".into(),
            ));
        }
        cursor += db_name_len;

        if cursor >= payload_len {
            return Err(BackupError::InvalidManifest("truncated WAL flag".into()));
        }
        let has_wal_byte = bytes[cursor];
        cursor += 1;
        let has_wal = has_wal_byte != 0;

        let (wal_file_name, wal_file_size, wal_file_checksum) = if has_wal {
            if cursor + 2 > payload_len {
                return Err(BackupError::InvalidManifest(
                    "truncated wal_file_name length".into(),
                ));
            }
            let wal_name_len =
                u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().unwrap()) as usize;
            cursor += 2;
            if cursor + wal_name_len + 8 + 4 > payload_len {
                return Err(BackupError::InvalidManifest(
                    "truncated WAL metadata fields".into(),
                ));
            }
            let wal_name = std::str::from_utf8(&bytes[cursor..cursor + wal_name_len])
                .map_err(|e| {
                    BackupError::InvalidManifest(format!("invalid UTF-8 in wal_file_name: {e}"))
                })?
                .to_string();
            if !is_safe_file_name(&wal_name) {
                return Err(BackupError::InvalidManifest(
                    "wal_file_name contains invalid or unsafe path characters".into(),
                ));
            }
            cursor += wal_name_len;

            let wal_size = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
            cursor += 8;

            let wal_crc = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());

            (Some(wal_name), wal_size, wal_crc)
        } else {
            (None, 0, 0)
        };

        Ok(Self {
            backup_version,
            db_format_version,
            page_size,
            page_count,
            db_file_name,
            db_file_size,
            db_file_checksum,
            has_wal,
            wal_file_name,
            wal_file_size,
            wal_file_checksum,
            max_commit_seq,
            last_checkpoint_lsn,
            created_at_utc_epoch_secs,
        })
    }
}

/// Overall audit and health classification for a validated backup image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupAuditStatus {
    /// Database image and companion WAL are fully verified and logically clean.
    FullyHealthy,
    /// Backup is valid and recoverable, but contains unapplied WAL records requiring crash recovery upon reopen.
    RecoverablePendingRecovery,
    /// Backup exhibits structural, header, checksum, or integrity inconsistencies.
    StructurallyInconsistent,
    /// Audit is incomplete or inconclusive because required checks were skipped or bounded.
    Inconclusive,
}

/// Structured report returned upon comprehensive validation of a backup image.
#[derive(Debug, Clone)]
pub struct BackupValidationReport {
    /// Overall audit and health classification for the backup image.
    pub status: BackupAuditStatus,
    /// Whether the backup passed all integrity, format, and consistency checks.
    pub is_valid: bool,
    /// Backup format version.
    pub backup_version: u32,
    /// Database format version.
    pub db_format_version: u32,
    /// Configured page size.
    pub page_size: u32,
    /// Total database pages.
    pub page_count: u32,
    /// Database image length in bytes.
    pub db_file_size: u64,
    /// Computed database image CRC32 checksum.
    pub db_file_checksum: u32,
    /// Whether a companion WAL is present.
    pub has_wal: bool,
    /// Companion WAL length in bytes.
    pub wal_file_size: u64,
    /// Companion WAL CRC32 checksum.
    pub wal_file_checksum: u32,
    /// Durable maximum commit sequence.
    pub max_commit_seq: u64,
    /// Last checkpoint LSN recorded.
    pub last_checkpoint_lsn: u64,
    /// Individual passed validation passes.
    pub checks_passed: Vec<String>,
    /// Any non-fatal issues or warnings detected.
    pub issues: Vec<String>,
}

/// Report returned upon successful creation of a database backup.
#[derive(Debug, Clone)]
pub struct BackupReport {
    /// Final directory path containing published backup artifacts.
    pub backup_dir: PathBuf,
    /// The published backup manifest.
    pub manifest: BackupManifest,
    /// Total bytes written for the database image.
    pub db_bytes_written: u64,
    /// Total bytes written for the WAL companion image.
    pub wal_bytes_written: u64,
    /// Validation report generated prior to publication.
    pub validation: Option<BackupValidationReport>,
}

/// Report returned upon successful restoration of a database backup.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    /// Published destination path of the restored database file.
    pub destination_db_path: PathBuf,
    /// Published destination path of the companion WAL file, if restored.
    pub destination_wal_path: Option<PathBuf>,
    /// Total bytes restored for the database file.
    pub db_bytes_restored: u64,
    /// Total bytes restored for the companion WAL file.
    pub wal_bytes_restored: u64,
    /// Restored maximum commit sequence number.
    pub restored_max_commit_seq: u64,
    /// Restored total page count.
    pub restored_page_count: u32,
    /// Whether post-restore consistency verification succeeded.
    pub consistency_checked: bool,
}

/// Persistent state machine states for the two-phase restore publication protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RestoreJournalState {
    /// Staging files copied and validated. Existing destination untouched.
    Staged = 0,
    /// Existing destination files backed up to temporary .bak_* files.
    ExistingBackedUp = 1,
    /// Primary database file published into destination path. WAL publication pending.
    DbPublished = 2,
    /// Both database and companion WAL files published into destination paths.
    AllPublished = 3,
}

impl TryFrom<u8> for RestoreJournalState {
    type Error = BackupError;

    fn try_from(val: u8) -> std::result::Result<Self, Self::Error> {
        match val {
            0 => Ok(Self::Staged),
            1 => Ok(Self::ExistingBackedUp),
            2 => Ok(Self::DbPublished),
            3 => Ok(Self::AllPublished),
            other => Err(BackupError::InvalidManifest(format!(
                "invalid restore journal state tag: {other}"
            ))),
        }
    }
}

/// Magic identifier bytes for restore journals.
pub const RESTORE_JOURNAL_MAGIC: &[u8; 8] = b"TDRESJRN";

/// Format version for restore journals.
pub const CURRENT_RESTORE_JOURNAL_VERSION: u32 = 1;

/// Durable journal tracking multi-file publication progress across crash boundaries.
#[derive(Debug, Clone)]
pub struct RestoreJournal {
    /// Physical path to the journal file on disk.
    pub journal_path: PathBuf,
    /// Current publication state machine state.
    pub state: RestoreJournalState,
    /// Destination primary database path.
    pub dest_db_path: PathBuf,
    /// Destination companion WAL path, if present.
    pub dest_wal_path: Option<PathBuf>,
    /// Staged primary database path.
    pub staged_db_path: PathBuf,
    /// Staged companion WAL path, if present.
    pub staged_wal_path: Option<PathBuf>,
    /// Backup file path for pre-existing database.
    pub bak_db_path: Option<PathBuf>,
    /// Backup file path for pre-existing companion WAL.
    pub bak_wal_path: Option<PathBuf>,
    /// Whether destination had an existing database file before restore.
    pub dest_had_db: bool,
    /// Whether destination had an existing WAL file before restore.
    pub dest_had_wal: bool,
}

impl RestoreJournal {
    /// Serializes the journal into a CRC32-protected binary payload.
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(RESTORE_JOURNAL_MAGIC);
        buf.extend_from_slice(&CURRENT_RESTORE_JOURNAL_VERSION.to_le_bytes());
        buf.push(self.state as u8);

        let mut flags = 0u8;
        if self.dest_had_db {
            flags |= 1 << 0;
        }
        if self.dest_had_wal {
            flags |= 1 << 1;
        }
        if self.dest_wal_path.is_some() {
            flags |= 1 << 2;
        }
        if self.staged_wal_path.is_some() {
            flags |= 1 << 3;
        }
        if self.bak_db_path.is_some() {
            flags |= 1 << 4;
        }
        if self.bak_wal_path.is_some() {
            flags |= 1 << 5;
        }
        buf.push(flags);

        fn write_string(buf: &mut Vec<u8>, s: &str) {
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(bytes);
        }

        write_string(&mut buf, &self.dest_db_path.to_string_lossy());
        if let Some(ref p) = self.dest_wal_path {
            write_string(&mut buf, &p.to_string_lossy());
        }
        write_string(&mut buf, &self.staged_db_path.to_string_lossy());
        if let Some(ref p) = self.staged_wal_path {
            write_string(&mut buf, &p.to_string_lossy());
        }
        if let Some(ref p) = self.bak_db_path {
            write_string(&mut buf, &p.to_string_lossy());
        }
        if let Some(ref p) = self.bak_wal_path {
            write_string(&mut buf, &p.to_string_lossy());
        }

        let crc = crate::wal::crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Deserializes and validates a binary restore journal buffer.
    pub fn deserialize(
        bytes: &[u8],
        journal_path: PathBuf,
    ) -> std::result::Result<Self, BackupError> {
        if bytes.len() < 8 + 4 + 1 + 1 + 2 + 2 + 4 {
            return Err(BackupError::InvalidManifest(
                "restore journal buffer too short".into(),
            ));
        }
        let payload_len = bytes.len() - 4;
        let expected_crc = u32::from_le_bytes(bytes[payload_len..].try_into().unwrap());
        let computed_crc = crate::wal::crc32(&bytes[..payload_len]);
        if computed_crc != expected_crc {
            return Err(BackupError::ChecksumMismatch {
                file: "restore_journal".into(),
                expected: expected_crc,
                computed: computed_crc,
            });
        }
        if &bytes[0..8] != RESTORE_JOURNAL_MAGIC {
            return Err(BackupError::InvalidManifest(
                "invalid restore journal magic identifier".into(),
            ));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != CURRENT_RESTORE_JOURNAL_VERSION {
            return Err(BackupError::InvalidManifest(format!(
                "unsupported restore journal version: {version}"
            )));
        }
        let state = RestoreJournalState::try_from(bytes[12])?;
        let flags = bytes[13];
        let dest_had_db = (flags & (1 << 0)) != 0;
        let dest_had_wal = (flags & (1 << 1)) != 0;
        let has_dest_wal = (flags & (1 << 2)) != 0;
        let has_staged_wal = (flags & (1 << 3)) != 0;
        let has_bak_db = (flags & (1 << 4)) != 0;
        let has_bak_wal = (flags & (1 << 5)) != 0;

        let mut cursor = 14;
        fn read_string(
            bytes: &[u8],
            cursor: &mut usize,
            max: usize,
        ) -> std::result::Result<String, BackupError> {
            if *cursor + 2 > max {
                return Err(BackupError::InvalidManifest(
                    "truncated string length in restore journal".into(),
                ));
            }
            let len = u16::from_le_bytes(bytes[*cursor..*cursor + 2].try_into().unwrap()) as usize;
            *cursor += 2;
            if *cursor + len > max {
                return Err(BackupError::InvalidManifest(
                    "truncated string payload in restore journal".into(),
                ));
            }
            let s = std::str::from_utf8(&bytes[*cursor..*cursor + len])
                .map_err(|e| {
                    BackupError::InvalidManifest(format!("invalid UTF-8 in restore journal: {e}"))
                })?
                .to_string();
            *cursor += len;
            Ok(s)
        }

        let dest_db_path = PathBuf::from(read_string(bytes, &mut cursor, payload_len)?);
        let dest_wal_path = if has_dest_wal {
            Some(PathBuf::from(read_string(bytes, &mut cursor, payload_len)?))
        } else {
            None
        };
        let staged_db_path = PathBuf::from(read_string(bytes, &mut cursor, payload_len)?);
        let staged_wal_path = if has_staged_wal {
            Some(PathBuf::from(read_string(bytes, &mut cursor, payload_len)?))
        } else {
            None
        };
        let bak_db_path = if has_bak_db {
            Some(PathBuf::from(read_string(bytes, &mut cursor, payload_len)?))
        } else {
            None
        };
        let bak_wal_path = if has_bak_wal {
            Some(PathBuf::from(read_string(bytes, &mut cursor, payload_len)?))
        } else {
            None
        };

        Ok(Self {
            journal_path,
            state,
            dest_db_path,
            dest_wal_path,
            staged_db_path,
            staged_wal_path,
            bak_db_path,
            bak_wal_path,
            dest_had_db,
            dest_had_wal,
        })
    }

    /// Atomically writes and flushes the journal state to disk.
    pub fn write_to_disk(&self) -> std::result::Result<(), BackupError> {
        let bytes = self.serialize();
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.journal_path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }

    /// Deletes the journal file from disk upon successful publication completion.
    pub fn delete_from_disk(&self) {
        let _ = fs::remove_file(&self.journal_path);
    }

    /// Executes deterministic crash recovery of an interrupted restore publication.
    pub fn execute_recovery(&self) -> std::result::Result<(), BackupError> {
        match self.state {
            RestoreJournalState::Staged => {
                let _ = fs::remove_file(&self.staged_db_path);
                if let Some(ref p) = self.staged_wal_path {
                    let _ = fs::remove_file(p);
                }
                self.delete_from_disk();
            }
            RestoreJournalState::ExistingBackedUp => {
                if self.dest_had_db {
                    if let Some(ref bak) = self.bak_db_path {
                        if bak.exists() {
                            fs::rename(bak, &self.dest_db_path)?;
                        }
                    }
                }
                if self.dest_had_wal {
                    if let (Some(ref bak), Some(ref dest_wal)) =
                        (&self.bak_wal_path, &self.dest_wal_path)
                    {
                        if bak.exists() {
                            fs::rename(bak, dest_wal)?;
                        }
                    }
                }
                let _ = fs::remove_file(&self.staged_db_path);
                if let Some(ref p) = self.staged_wal_path {
                    let _ = fs::remove_file(p);
                }
                self.delete_from_disk();
            }
            RestoreJournalState::DbPublished => {
                if let (Some(ref staged_wal), Some(ref dest_wal)) =
                    (&self.staged_wal_path, &self.dest_wal_path)
                {
                    if staged_wal.exists() {
                        fs::rename(staged_wal, dest_wal)?;
                    }
                }
                if let Some(ref bak) = self.bak_db_path {
                    let _ = fs::remove_file(bak);
                }
                if let Some(ref bak) = self.bak_wal_path {
                    let _ = fs::remove_file(bak);
                }
                self.delete_from_disk();
            }
            RestoreJournalState::AllPublished => {
                if let Some(ref bak) = self.bak_db_path {
                    let _ = fs::remove_file(bak);
                }
                if let Some(ref bak) = self.bak_wal_path {
                    let _ = fs::remove_file(bak);
                }
                let _ = fs::remove_file(&self.staged_db_path);
                if let Some(ref p) = self.staged_wal_path {
                    let _ = fs::remove_file(p);
                }
                self.delete_from_disk();
            }
        }
        Ok(())
    }
}

/// Resolves the standard restore journal path for a target destination database.
pub fn restore_journal_path(dest_db: &Path) -> PathBuf {
    PathBuf::from(format!("{}.restore_journal", dest_db.display()))
}

/// Recovers any pending restore journal at `journal_path`. Returns true if a journal was recovered.
pub fn recover_journal(journal_path: &Path) -> std::result::Result<bool, BackupError> {
    if !journal_path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(journal_path)?;
    let journal = RestoreJournal::deserialize(&bytes, journal_path.to_path_buf())?;
    journal.execute_recovery()?;
    Ok(true)
}

// =============================================================================
// Helper Utilities
// =============================================================================

fn check_fault(
    configured: Option<StorageFailurePoint>,
    target: StorageFailurePoint,
) -> std::result::Result<(), BackupError> {
    if configured == Some(target) {
        Err(BackupError::InjectedFault(target))
    } else {
        Ok(())
    }
}

/// Streams a file from `src` to `dst`, computing its CRC32 and flushing it to disk.
fn copy_and_hash_file(
    src: &Path,
    dst: &Path,
    fault: Option<StorageFailurePoint>,
    copy_fault: Option<StorageFailurePoint>,
) -> std::result::Result<(u64, u32), BackupError> {
    let mut reader = File::open(src)?;
    let mut writer = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;

    let mut hasher = Crc32Hasher::new();
    let mut buffer = [0u8; 65536];
    let mut total_bytes = 0u64;

    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
        total_bytes += n as u64;

        if let Some(cf) = copy_fault {
            check_fault(fault, cf)?;
        }
    }

    writer.sync_all()?;
    Ok((total_bytes, hasher.finalize()))
}

/// Computes the length and CRC32 checksum of a file on disk without copying.
fn hash_file(path: &Path) -> std::result::Result<(u64, u32), BackupError> {
    let mut reader = File::open(path)?;
    let mut hasher = Crc32Hasher::new();
    let mut buffer = [0u8; 65536];
    let mut total_bytes = 0u64;

    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total_bytes += n as u64;
    }

    Ok((total_bytes, hasher.finalize()))
}

/// Resolves candidate WAL file paths associated with a primary database file.
pub fn resolve_source_wal_path(db_path: &Path) -> Option<PathBuf> {
    let candidate1 = PathBuf::from(format!("{}.wal", db_path.display()));
    let candidate2 = db_path.with_extension("wal");
    if candidate1.exists() {
        Some(candidate1)
    } else if candidate2.exists() {
        Some(candidate2)
    } else {
        None
    }
}

/// Checks if two paths refer to the same location or overlap unsafely.
fn check_path_collision(src: &Path, dst: &Path) -> std::result::Result<(), BackupError> {
    let norm_src = if src.exists() {
        src.canonicalize().unwrap_or_else(|_| src.to_path_buf())
    } else {
        src.to_path_buf()
    };

    let norm_dst = if dst.exists() {
        dst.canonicalize().unwrap_or_else(|_| dst.to_path_buf())
    } else {
        dst.to_path_buf()
    };

    if norm_src == norm_dst {
        return Err(BackupError::PathCollision {
            source_path: src.to_path_buf(),
            destination_path: dst.to_path_buf(),
        });
    }

    // Check if dst is directly inside src (if src is a dir)
    if norm_dst.starts_with(&norm_src) && norm_src.is_dir() {
        return Err(BackupError::PathCollision {
            source_path: src.to_path_buf(),
            destination_path: dst.to_path_buf(),
        });
    }

    // Check if src is directly inside dst (if dst is a dir)
    if norm_src.starts_with(&norm_dst) && norm_dst.is_dir() {
        return Err(BackupError::PathCollision {
            source_path: src.to_path_buf(),
            destination_path: dst.to_path_buf(),
        });
    }

    Ok(())
}

// =============================================================================
// Implementation on TreeDb
// =============================================================================

impl TreeDb {
    /// Creates a complete offline backup of an active database instance.
    ///
    /// # Lifecycle Contract
    /// 1. Verifies that no transaction is actively uncommitted.
    /// 2. If configured, runs a synchronous checkpoint and truncates obsolete WAL records.
    /// 3. Performs atomic backup imaging into `backup_dir` via staging.
    pub fn create_backup(
        &mut self,
        backup_dir: impl AsRef<Path>,
        config: &BackupConfig,
    ) -> std::result::Result<BackupReport, BackupError> {
        if let Some(active) = self.active_txn {
            return Err(BackupError::ActiveTransaction(active));
        }

        if config.checkpoint_before_backup {
            let _ = self.checkpoint_and_truncate_wal()?;
        }

        let db_path = self.buffer_pool.page_manager().path().to_path_buf();
        Self::backup_database(&db_path, backup_dir.as_ref(), config)
    }

    /// Creates an offline backup directly from a database file path.
    ///
    /// # Operational Preconditions
    /// * The source database file must not be concurrently modified by another process.
    /// * Generates an independent, self-contained backup directory containing the database
    ///   image, companion WAL (if present), and `treedb.manifest`.
    pub fn backup_database(
        source_db_path: impl AsRef<Path>,
        backup_dir: impl AsRef<Path>,
        config: &BackupConfig,
    ) -> std::result::Result<BackupReport, BackupError> {
        let src_path = source_db_path.as_ref();
        let bkp_dir = backup_dir.as_ref();

        if !src_path.exists() {
            return Err(BackupError::SourceNotFound(src_path.to_path_buf()));
        }

        check_path_collision(src_path, bkp_dir)?;

        check_fault(
            config.fault_point,
            StorageFailurePoint::BeforeBackupCreation,
        )?;

        let bkp_parent = bkp_dir.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(bkp_parent)?;

        let staging_name = format!(
            ".tmp_backup_{}_{}",
            std::process::id(),
            BACKUP_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let staging_dir = bkp_parent.join(staging_name);
        if staging_dir.exists() {
            let _ = fs::remove_dir_all(&staging_dir);
        }
        fs::create_dir_all(&staging_dir)?;

        // Execute staging with guaranteed cleanup on failure
        let res = Self::stage_backup_internal(src_path, &staging_dir, bkp_dir, config);

        match res {
            Ok(report) => Ok(report),
            Err(err) => {
                let _ = fs::remove_dir_all(&staging_dir);
                Err(err)
            }
        }
    }

    fn stage_backup_internal(
        src_path: &Path,
        staging_dir: &Path,
        bkp_dir: &Path,
        config: &BackupConfig,
    ) -> std::result::Result<BackupReport, BackupError> {
        let db_file_name = src_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("database.treedb")
            .to_string();

        let staged_db_path = staging_dir.join(&db_file_name);

        // 1. Copy and hash primary database file
        let (db_bytes_written, db_checksum) = copy_and_hash_file(
            src_path,
            &staged_db_path,
            config.fault_point,
            Some(StorageFailurePoint::DuringBackupDbCopy),
        )?;

        // Validate image size page-alignment
        if db_bytes_written % (PAGE_SIZE as u64) != 0 {
            return Err(BackupError::ImageNotPageAligned {
                len: db_bytes_written,
                page_size: PAGE_SIZE as u32,
            });
        }

        // 2. Read database header from Page 0 of the copied image
        let mut header_bytes = [0u8; PAGE_SIZE];
        {
            let mut f = File::open(&staged_db_path)?;
            f.read_exact(&mut header_bytes)?;
        }
        let header = DbHeader::deserialize(&header_bytes).map_err(|e| {
            BackupError::CorruptHeader(format!("failed to parse Page 0 header: {e}"))
        })?;

        // 3. Copy companion WAL if present
        let maybe_src_wal = resolve_source_wal_path(src_path);
        let (has_wal, wal_file_name, wal_bytes_written, wal_checksum) =
            if let Some(ref wal_path) = maybe_src_wal {
                if wal_path.exists() && wal_path.metadata()?.len() > 0 {
                    let wal_name = wal_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("database.treedb.wal")
                        .to_string();
                    let staged_wal_path = staging_dir.join(&wal_name);
                    let (bytes, crc) = copy_and_hash_file(
                        wal_path,
                        &staged_wal_path,
                        config.fault_point,
                        Some(StorageFailurePoint::DuringBackupWalCopy),
                    )?;
                    (true, Some(wal_name), bytes, crc)
                } else {
                    (false, None, 0, 0)
                }
            } else {
                (false, None, 0, 0)
            };

        // 4. Construct and write manifest
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let manifest = BackupManifest {
            backup_version: CURRENT_BACKUP_VERSION,
            db_format_version: header.format_version,
            page_size: header.page_size,
            page_count: header.page_count,
            db_file_name: db_file_name.clone(),
            db_file_size: db_bytes_written,
            db_file_checksum: db_checksum,
            has_wal,
            wal_file_name,
            wal_file_size: wal_bytes_written,
            wal_file_checksum: wal_checksum,
            max_commit_seq: header.max_commit_seq,
            last_checkpoint_lsn: header.last_checkpoint_lsn.0,
            created_at_utc_epoch_secs: now_secs,
        };

        let manifest_bytes = manifest.serialize();
        let manifest_path = staging_dir.join(MANIFEST_FILE_NAME);
        {
            let mut mf = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&manifest_path)?;
            mf.write_all(&manifest_bytes)?;
            mf.sync_all()?;
        }

        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringBackupManifestWrite,
        )?;

        // 5. Non-destructive validation of the staged backup
        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringBackupValidation,
        )?;

        let validation_report = Self::validate_backup(staging_dir, config)?;
        if !validation_report.is_valid {
            return Err(BackupError::ConsistencyFailure(format!(
                "staged backup validation failed: {:?}",
                validation_report.issues
            )));
        }

        check_fault(config.fault_point, StorageFailurePoint::DuringBackupSync)?;

        check_fault(
            config.fault_point,
            StorageFailurePoint::BeforeBackupPublication,
        )?;

        // 6. Atomic publication to final backup_dir
        if bkp_dir.exists() {
            let backup_parent = bkp_dir.parent().unwrap_or_else(|| Path::new("."));
            let old_backup_staging = backup_parent.join(format!(
                ".tmp_old_backup_{}_{}",
                std::process::id(),
                BACKUP_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            fs::rename(bkp_dir, &old_backup_staging)?;
            if let Err(e) = fs::rename(staging_dir, bkp_dir) {
                let _ = fs::rename(&old_backup_staging, bkp_dir);
                return Err(BackupError::Io(e));
            }
            let _ = fs::remove_dir_all(&old_backup_staging);
        } else {
            fs::rename(staging_dir, bkp_dir)?;
        }

        Ok(BackupReport {
            backup_dir: bkp_dir.to_path_buf(),
            manifest,
            db_bytes_written,
            wal_bytes_written,
            validation: Some(validation_report),
        })
    }

    /// Inspects and parses the metadata manifest of a backup without running full validation.
    pub fn inspect_backup_manifest(
        backup_dir: impl AsRef<Path>,
    ) -> std::result::Result<BackupManifest, BackupError> {
        let manifest_path = backup_dir.as_ref().join(MANIFEST_FILE_NAME);
        if !manifest_path.exists() {
            return Err(BackupError::MissingFile(MANIFEST_FILE_NAME.into()));
        }
        let meta = fs::metadata(&manifest_path)?;
        if meta.len() > 65536 {
            return Err(BackupError::InvalidManifest(
                "manifest file exceeds 64KB limit".into(),
            ));
        }
        let bytes = fs::read(&manifest_path)?;
        BackupManifest::deserialize(&bytes)
    }

    /// Performs bounded, non-destructive validation of a backup image.
    ///
    /// Validates:
    /// 1. Manifest presence, deserialization, and CRC32 digest.
    /// 2. Supported backup and database format versions.
    /// 3. Primary database image existence, exact size, and CRC32 checksum.
    /// 4. Page 0 database header invariants (magic, versions, page size, page counts).
    /// 5. Page alignment of image file length.
    /// 6. Companion WAL presence, size, and CRC32 checksum if declared in manifest.
    /// 7. Absence of unexpected extraneous files.
    /// 8. Non-destructive read-only consistency verification via Phase 73/74 engine.
    pub fn validate_backup(
        backup_dir: impl AsRef<Path>,
        config: &BackupConfig,
    ) -> std::result::Result<BackupValidationReport, BackupError> {
        let dir = backup_dir.as_ref();
        if !dir.exists() || !dir.is_dir() {
            return Err(BackupError::SourceNotFound(dir.to_path_buf()));
        }

        let mut checks_passed = Vec::new();

        // 1. Manifest verification
        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        if !manifest_path.exists() {
            return Err(BackupError::MissingFile(MANIFEST_FILE_NAME.into()));
        }
        let meta = fs::metadata(&manifest_path)?;
        if meta.len() > 65536 {
            return Err(BackupError::InvalidManifest(
                "manifest file exceeds 64KB limit".into(),
            ));
        }
        let manifest_bytes = fs::read(&manifest_path)?;
        let manifest = BackupManifest::deserialize(&manifest_bytes)?;
        checks_passed.push("manifest_crc_and_format".into());

        // 2. Extraneous artifact detection
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let is_known = file_name == MANIFEST_FILE_NAME
                || file_name == manifest.db_file_name
                || manifest.wal_file_name.as_deref() == Some(&file_name);
            if !is_known {
                return Err(BackupError::UnexpectedArtifact(file_name));
            }
        }
        checks_passed.push("directory_artifact_set".into());

        // 3. Database file presence, length, and checksum
        let db_path = dir.join(&manifest.db_file_name);
        if !db_path.exists() {
            return Err(BackupError::MissingFile(manifest.db_file_name.clone()));
        }

        let (db_len, db_crc) = hash_file(&db_path)?;
        if db_len != manifest.db_file_size {
            return Err(BackupError::SizeMismatch {
                file: manifest.db_file_name.clone(),
                expected: manifest.db_file_size,
                found: db_len,
            });
        }
        if db_crc != manifest.db_file_checksum {
            return Err(BackupError::ChecksumMismatch {
                file: manifest.db_file_name.clone(),
                expected: manifest.db_file_checksum,
                computed: db_crc,
            });
        }
        checks_passed.push("database_file_crc_and_size".into());

        // 4. Page alignment check
        if db_len % (manifest.page_size as u64) != 0 {
            return Err(BackupError::ImageNotPageAligned {
                len: db_len,
                page_size: manifest.page_size,
            });
        }
        checks_passed.push("database_page_alignment".into());

        // 5. Database header inspection
        let mut header_bytes = [0u8; PAGE_SIZE];
        {
            let mut f = File::open(&db_path)?;
            f.read_exact(&mut header_bytes)?;
        }
        let header = DbHeader::deserialize(&header_bytes)
            .map_err(|e| BackupError::CorruptHeader(format!("invalid Page 0 header: {e}")))?;

        if &header.magic != MAGIC_BYTES {
            return Err(BackupError::CorruptHeader(
                "magic mismatch in header".into(),
            ));
        }
        if header.format_version != manifest.db_format_version {
            return Err(BackupError::CorruptHeader(
                "format version mismatch between manifest and header".into(),
            ));
        }
        if header.page_size != manifest.page_size {
            return Err(BackupError::CorruptHeader(
                "page size mismatch between manifest and header".into(),
            ));
        }
        if header.page_count != manifest.page_count {
            return Err(BackupError::CorruptHeader(
                "page count mismatch between manifest and header".into(),
            ));
        }
        if header.max_commit_seq != manifest.max_commit_seq {
            return Err(BackupError::CorruptHeader(
                "max_commit_seq mismatch between manifest and header".into(),
            ));
        }
        if header.last_checkpoint_lsn.0 != manifest.last_checkpoint_lsn {
            return Err(BackupError::CorruptHeader(
                "last_checkpoint_lsn mismatch between manifest and header".into(),
            ));
        }
        checks_passed.push("database_header_invariants".into());

        // 6. WAL file verification if declared
        if manifest.has_wal {
            let wal_name = manifest
                .wal_file_name
                .as_ref()
                .ok_or_else(|| BackupError::InvalidManifest("missing wal_file_name".into()))?;
            let wal_path = dir.join(wal_name);
            if !wal_path.exists() {
                return Err(BackupError::MissingFile(wal_name.clone()));
            }

            let (wal_len, wal_crc) = hash_file(&wal_path)?;
            if wal_len != manifest.wal_file_size {
                return Err(BackupError::SizeMismatch {
                    file: wal_name.clone(),
                    expected: manifest.wal_file_size,
                    found: wal_len,
                });
            }
            if wal_crc != manifest.wal_file_checksum {
                return Err(BackupError::ChecksumMismatch {
                    file: wal_name.clone(),
                    expected: manifest.wal_file_checksum,
                    computed: wal_crc,
                });
            }
            checks_passed.push("wal_file_crc_and_size".into());
        }

        // 7. Non-destructive read-only consistency verification
        let (status, issues) = if !config.verify_image {
            (BackupAuditStatus::Inconclusive, Vec::new())
        } else {
            let verify_report = Self::verify_file(&db_path)
                .map_err(|e| BackupError::ConsistencyFailure(format!("verify_file failed: {e}")))?;

            if verify_report.has_fatal() {
                return Err(BackupError::ConsistencyFailure(format!(
                    "database image contains fatal consistency issues: {:?}",
                    verify_report.issues
                )));
            }

            let audit_status = if verify_report
                .issues
                .iter()
                .any(|i| i.severity >= crate::engine::verification::ConsistencySeverity::Error)
            {
                BackupAuditStatus::StructurallyInconsistent
            } else if verify_report.issues.iter().any(|i| {
                matches!(
                    i.kind,
                    crate::engine::verification::ConsistencyIssueKind::UnappliedWalTail { .. }
                )
            }) {
                BackupAuditStatus::RecoverablePendingRecovery
            } else if !verify_report.skipped_checks.is_empty() || verify_report.truncated {
                BackupAuditStatus::Inconclusive
            } else {
                BackupAuditStatus::FullyHealthy
            };

            let mut issue_strs = Vec::new();
            for issue in &verify_report.issues {
                issue_strs.push(issue.to_string());
            }
            checks_passed.push("storage_consistency_audit".into());
            (audit_status, issue_strs)
        };

        let is_valid = status != BackupAuditStatus::StructurallyInconsistent;

        Ok(BackupValidationReport {
            status,
            is_valid,
            backup_version: manifest.backup_version,
            db_format_version: manifest.db_format_version,
            page_size: manifest.page_size,
            page_count: manifest.page_count,
            db_file_size: manifest.db_file_size,
            db_file_checksum: manifest.db_file_checksum,
            has_wal: manifest.has_wal,
            wal_file_size: manifest.wal_file_size,
            wal_file_checksum: manifest.wal_file_checksum,
            max_commit_seq: manifest.max_commit_seq,
            last_checkpoint_lsn: manifest.last_checkpoint_lsn,
            checks_passed,
            issues,
        })
    }

    /// Restores a database backup into a target destination path.
    ///
    /// # Safety and Atomic Staging Protocol
    /// 1. Validates the source backup image completely prior to touching the destination.
    /// 2. Verifies destination existence against `config.allow_overwrite`.
    /// 3. Performs staging into temporary files in the destination filesystem.
    /// 4. Synchronizes and validates staged database and WAL files.
    /// 5. Backs up existing destination files before atomic replacement.
    /// 6. In the event of replacement failure, rolls back the original files.
    /// 7. Cleans up temporary staged and backup files.
    /// 8. Runs optional post-restore consistency verification.
    pub fn restore_backup(
        backup_dir: impl AsRef<Path>,
        destination_db_path: impl AsRef<Path>,
        config: &RestoreConfig,
    ) -> std::result::Result<RestoreReport, BackupError> {
        let bkp_dir = backup_dir.as_ref();
        let dest_db = destination_db_path.as_ref();

        check_path_collision(bkp_dir, dest_db)?;

        // 1. Pre-restore validation
        check_fault(
            config.fault_point,
            StorageFailurePoint::BeforeRestoreValidation,
        )?;

        let backup_config = BackupConfig {
            fault_point: None,
            checkpoint_before_backup: false,
            verify_image: config.validate_before_restore,
        };

        let val_report = if config.validate_before_restore {
            Some(Self::validate_backup(bkp_dir, &backup_config)?)
        } else {
            None
        };

        let manifest = Self::inspect_backup_manifest(bkp_dir)?;

        // 2. Destination check
        let dest_wal = PathBuf::from(format!("{}.wal", dest_db.display()));
        let dest_exists = dest_db.exists() || dest_wal.exists();

        if dest_exists && !config.allow_overwrite {
            return Err(BackupError::DestinationExists(dest_db.to_path_buf()));
        }

        let dest_parent = dest_db.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dest_parent)?;

        // 3. Staging restore files
        let restore_seq = BACKUP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let staged_db = dest_parent.join(format!(".tmp_restore_db_{}_{}", pid, restore_seq));
        let staged_wal = PathBuf::from(format!("{}.wal", staged_db.display()));

        let res = Self::stage_restore_internal(
            bkp_dir,
            &manifest,
            &staged_db,
            &staged_wal,
            dest_db,
            &dest_wal,
            dest_exists,
            config,
            val_report,
        );

        // Clean up staging files only if no active restore journal was left behind
        let journal_path = restore_journal_path(dest_db);
        if !journal_path.exists() {
            let _ = fs::remove_file(&staged_db);
            let _ = fs::remove_file(&staged_wal);
        }

        res
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_restore_internal(
        bkp_dir: &Path,
        manifest: &BackupManifest,
        staged_db: &Path,
        staged_wal: &Path,
        dest_db: &Path,
        dest_wal: &Path,
        dest_exists: bool,
        config: &RestoreConfig,
        _val_report: Option<BackupValidationReport>,
    ) -> std::result::Result<RestoreReport, BackupError> {
        let journal_path = restore_journal_path(dest_db);
        if journal_path.exists() {
            let _ = recover_journal(&journal_path)?;
        }

        let src_db_path = bkp_dir.join(&manifest.db_file_name);

        // Copy DB image to staged location
        let (db_restored_bytes, db_restored_crc) = copy_and_hash_file(
            &src_db_path,
            staged_db,
            config.fault_point,
            Some(StorageFailurePoint::DuringRestoreDbCopy),
        )?;

        if db_restored_bytes != manifest.db_file_size {
            return Err(BackupError::SizeMismatch {
                file: manifest.db_file_name.clone(),
                expected: manifest.db_file_size,
                found: db_restored_bytes,
            });
        }
        if db_restored_crc != manifest.db_file_checksum {
            return Err(BackupError::ChecksumMismatch {
                file: manifest.db_file_name.clone(),
                expected: manifest.db_file_checksum,
                computed: db_restored_crc,
            });
        }

        // Copy companion WAL if present in backup
        let (wal_restored_bytes, wal_dest_path) = if manifest.has_wal {
            let wal_name = manifest.wal_file_name.as_ref().unwrap();
            let src_wal_path = bkp_dir.join(wal_name);
            let (wal_bytes, wal_crc) = copy_and_hash_file(
                &src_wal_path,
                staged_wal,
                config.fault_point,
                Some(StorageFailurePoint::DuringRestoreWalCopy),
            )?;

            if wal_bytes != manifest.wal_file_size {
                return Err(BackupError::SizeMismatch {
                    file: wal_name.clone(),
                    expected: manifest.wal_file_size,
                    found: wal_bytes,
                });
            }
            if wal_crc != manifest.wal_file_checksum {
                return Err(BackupError::ChecksumMismatch {
                    file: wal_name.clone(),
                    expected: manifest.wal_file_checksum,
                    computed: wal_crc,
                });
            }
            (wal_bytes, Some(dest_wal.to_path_buf()))
        } else {
            (0, None)
        };

        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringRestoreValidation,
        )?;

        // Validate staged image
        let verify_res = Self::verify_file(staged_db).map_err(|e| {
            BackupError::ConsistencyFailure(format!("staged restore verify_file failed: {e}"))
        })?;
        if verify_res.has_fatal() {
            return Err(BackupError::ConsistencyFailure(format!(
                "staged restored image failed consistency check: {:?}",
                verify_res.issues
            )));
        }

        // Safe two-phase publication with durable restore journal
        let dest_parent = dest_db.parent().unwrap_or_else(|| Path::new("."));
        let pid = std::process::id();
        let seq = BACKUP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let bak_db = dest_parent.join(format!(".bak_restore_db_{}_{}", pid, seq));
        let bak_wal = dest_parent.join(format!(".bak_restore_wal_{}_{}", pid, seq));

        let mut journal = RestoreJournal {
            journal_path: journal_path.clone(),
            state: RestoreJournalState::Staged,
            dest_db_path: dest_db.to_path_buf(),
            dest_wal_path: if manifest.has_wal {
                Some(dest_wal.to_path_buf())
            } else {
                None
            },
            staged_db_path: staged_db.to_path_buf(),
            staged_wal_path: if manifest.has_wal {
                Some(staged_wal.to_path_buf())
            } else {
                None
            },
            bak_db_path: if dest_db.exists() {
                Some(bak_db.clone())
            } else {
                None
            },
            bak_wal_path: if dest_wal.exists() {
                Some(bak_wal.clone())
            } else {
                None
            },
            dest_had_db: dest_db.exists(),
            dest_had_wal: dest_wal.exists(),
        };

        journal.write_to_disk()?;
        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringRestoreJournalSync,
        )?;

        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringRestoreDestinationBackup,
        )?;

        if dest_exists {
            if dest_db.exists() {
                fs::rename(dest_db, &bak_db)?;
            }
            if dest_wal.exists() {
                fs::rename(dest_wal, &bak_wal)?;
            }
        }

        journal.state = RestoreJournalState::ExistingBackedUp;
        journal.write_to_disk()?;

        check_fault(
            config.fault_point,
            StorageFailurePoint::BeforeRestorePublication,
        )?;
        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringRestoreDbPublish,
        )?;

        fs::rename(staged_db, dest_db)?;
        journal.state = RestoreJournalState::DbPublished;
        journal.write_to_disk()?;

        if manifest.has_wal {
            check_fault(
                config.fault_point,
                StorageFailurePoint::DuringRestoreWalPublish,
            )?;
            fs::rename(staged_wal, dest_wal)?;
        }

        journal.state = RestoreJournalState::AllPublished;
        journal.write_to_disk()?;

        check_fault(
            config.fault_point,
            StorageFailurePoint::DuringRestoreCleanup,
        )?;

        if bak_db.exists() {
            let _ = fs::remove_file(&bak_db);
        }
        if bak_wal.exists() {
            let _ = fs::remove_file(&bak_wal);
        }
        journal.delete_from_disk();

        // Optional post-restore verification with full recovery replay
        if config.verify_after_restore {
            let mut db = Self::open(dest_db, config.buffer_pool_size).map_err(|e| {
                BackupError::Engine(format!("failed to reopen restored database: {e}"))
            })?;
            let report = db.verify_consistency().map_err(|e| {
                BackupError::ConsistencyFailure(format!("verify_consistency failed: {e}"))
            })?;
            if !report.is_healthy() || report.has_fatal() {
                return Err(BackupError::ConsistencyFailure(format!(
                    "restored database failed post-recovery consistency audit: {:?}",
                    report.issues
                )));
            }
        }

        Ok(RestoreReport {
            destination_db_path: dest_db.to_path_buf(),
            destination_wal_path: wal_dest_path,
            db_bytes_restored: db_restored_bytes,
            wal_bytes_restored: wal_restored_bytes,
            restored_max_commit_seq: manifest.max_commit_seq,
            restored_page_count: manifest.page_count,
            consistency_checked: config.verify_after_restore,
        })
    }
}
