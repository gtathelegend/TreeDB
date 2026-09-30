use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::error::{Error, Result};
use crate::index::RecordId;
use crate::wal::crc32;

/// Default in-memory candidate budget before spilling to disk: 64 KB.
pub const DEFAULT_MULTI_INDEX_MEMORY_BUDGET_BYTES: usize = 64 * 1024;

/// Default maximum number of temporary run files open simultaneously during multi-index merge: 32.
pub const DEFAULT_MAX_TEMP_FILES: usize = 32;

/// Default maximum total disk space permitted for temporary multi-index runs: 1 GB.
pub const DEFAULT_MAX_TEMP_BYTES: usize = 1024 * 1024 * 1024;

/// Default maximum merge passes before aborting: 100 passes.
pub const DEFAULT_MAX_MERGE_PASSES: usize = 100;

/// Magic bytes identifying a TreeDB temporary multi-index spill file: `b"TMIS"`.
pub const TEMP_MULTI_INDEX_MAGIC: &[u8; 4] = b"TMIS";

/// Version identifier for the temporary multi-index spill file format.
pub const TEMP_MULTI_INDEX_VERSION: u16 = 1;

/// Estimated in-memory byte footprint per candidate `RecordId` accounting for Vec/collection overhead.
pub const RECORD_ID_IN_MEMORY_BYTES: usize = 24;

/// Exact binary payload bytes per serialized `RecordId` in a spill file (4B page_id + 4B slot_id).
pub const RECORD_ID_PAYLOAD_BYTES: usize = 8;

/// Header size in bytes: 4B magic + 2B version + 1B op_type + 8B record_count + 4B crc32 = 19B.
pub const SPILL_HEADER_SIZE: usize = 19;

static MULTI_INDEX_RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Execution mode for multi-index candidate processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultiIndexExecutionMode {
    /// In-memory processing within configured memory budget.
    InMemory,
    /// Spilled external processing for large candidate sets.
    External,
}

impl fmt::Display for MultiIndexExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InMemory => write!(f, "InMemory"),
            Self::External => write!(f, "External"),
        }
    }
}

/// Runtime configuration parameters controlling external multi-index set operations and spilling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiIndexConfig {
    /// In-memory candidate buffer budget in bytes before spilling to disk.
    pub memory_budget_bytes: usize,
    /// Maximum number of temporary run files open simultaneously during merge.
    pub max_temp_files: usize,
    /// Maximum disk space in bytes that can be consumed by temporary spill files.
    pub max_temp_bytes: usize,
    /// Maximum number of merge passes before returning an error.
    pub max_merge_passes: usize,
    /// Custom directory for temporary run files. If `None`, `std::env::temp_dir()` is used.
    pub temp_dir: Option<PathBuf>,
}

impl Default for MultiIndexConfig {
    fn default() -> Self {
        Self {
            memory_budget_bytes: DEFAULT_MULTI_INDEX_MEMORY_BUDGET_BYTES,
            max_temp_files: DEFAULT_MAX_TEMP_FILES,
            max_temp_bytes: DEFAULT_MAX_TEMP_BYTES,
            max_merge_passes: DEFAULT_MAX_MERGE_PASSES,
            temp_dir: None,
        }
    }
}

impl MultiIndexConfig {
    /// Creates a new `MultiIndexConfig` with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the in-memory candidate buffer budget in bytes.
    pub fn with_memory_budget_bytes(mut self, budget: usize) -> Self {
        self.memory_budget_bytes = budget.max(1);
        self
    }

    /// Sets the maximum number of temporary run files open simultaneously during merge.
    pub fn with_max_temp_files(mut self, max_files: usize) -> Self {
        self.max_temp_files = max_files.max(2);
        self
    }

    /// Sets the maximum allowable disk space in bytes for temporary runs.
    pub fn with_max_temp_bytes(mut self, max_bytes: usize) -> Self {
        self.max_temp_bytes = max_bytes;
        self
    }

    /// Sets the maximum number of merge passes allowed.
    pub fn with_max_merge_passes(mut self, max_passes: usize) -> Self {
        self.max_merge_passes = max_passes.max(1);
        self
    }

    /// Sets the directory path for temporary multi-index run files.
    pub fn with_temp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(path.into());
        self
    }

    /// Returns the effective temporary directory path.
    pub fn effective_temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

/// Execution telemetry and statistics for an external multi-index operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MultiIndexSpillMetrics {
    /// Number of temporary spill runs created on disk (0 indicates in-memory execution).
    pub runs_created: usize,
    /// Total bytes written to disk across all temporary spill runs.
    pub bytes_written: usize,
    /// Total bytes read from disk during merge passes.
    pub bytes_read: usize,
    /// Total candidate RecordIds spilled to disk.
    pub rows_spilled: usize,
    /// Total candidate RecordIds merged across runs/streams.
    pub rows_merged: usize,
    /// Peak in-memory byte footprint estimated during processing.
    pub peak_memory_bytes: usize,
}

/// RAII wrapper around a temporary multi-index spill file that guarantees deletion on `Drop`.
#[derive(Debug)]
pub struct TempMultiIndexRunFile {
    path: PathBuf,
}

impl TempMultiIndexRunFile {
    /// Creates and opens a new unique temporary spill file in the specified directory.
    pub fn create_in(temp_dir: &Path) -> Result<(Self, File)> {
        if !temp_dir.exists() {
            std::fs::create_dir_all(temp_dir).map_err(|e| {
                Error::ExternalMultiIndexIo(format!(
                    "failed to create temporary multi-index directory {}: {e}",
                    temp_dir.display()
                ))
            })?;
        }

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = MULTI_INDEX_RUN_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let filename = format!("treedb_multi_idx_{pid}_{nanos}_{counter}.tmp");
        let path = temp_dir.join(filename);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                Error::ExternalMultiIndexIo(format!(
                    "failed to create temporary multi-index run file {}: {e}",
                    path.display()
                ))
            })?;

        Ok((Self { path }, file))
    }

    /// Returns the filesystem path of the temporary file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempMultiIndexRunFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Operation type tag recorded in the spill file header.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillOpType {
    /// Child scan candidate run.
    ChildRun = 0,
    /// Intersection intermediate run.
    Intersection = 1,
    /// Union intermediate run.
    Union = 2,
}

/// Serializes sorted `RecordId` candidates into an RAII-managed temporary file with CRC32 checksum.
#[derive(Debug)]
pub struct RecordIdRunWriter {
    temp_file: TempMultiIndexRunFile,
    writer: BufWriter<File>,
    record_count: u64,
    op_type: SpillOpType,
    payload_bytes: Vec<u8>,
}

impl RecordIdRunWriter {
    /// Creates a new run writer in the given temporary directory.
    pub fn create_in(temp_dir: &Path, op_type: SpillOpType) -> Result<Self> {
        let (temp_file, file) = TempMultiIndexRunFile::create_in(temp_dir)?;
        let mut writer = BufWriter::new(file);

        // Placeholder header of 19 bytes
        let placeholder = [0u8; SPILL_HEADER_SIZE];
        writer.write_all(&placeholder).map_err(|e| {
            Error::ExternalMultiIndexIo(format!("failed to write run header placeholder: {e}"))
        })?;

        Ok(Self {
            temp_file,
            writer,
            record_count: 0,
            op_type,
            payload_bytes: Vec::new(),
        })
    }

    /// Writes a single `RecordId` to the run file.
    pub fn write_record(&mut self, rid: RecordId) -> Result<()> {
        let bytes = rid.to_bytes();
        self.writer
            .write_all(&bytes)
            .map_err(|e| Error::ExternalMultiIndexIo(format!("failed to write record: {e}")))?;
        self.payload_bytes.extend_from_slice(&bytes);
        self.record_count += 1;
        Ok(())
    }

    /// Writes a slice of `RecordId`s to the run file.
    pub fn write_records(&mut self, rids: &[RecordId]) -> Result<()> {
        for &rid in rids {
            self.write_record(rid)?;
        }
        Ok(())
    }

    /// Finalizes the run file, writes the header and checksum, and returns the RAII file handle.
    pub fn finish(mut self) -> Result<(TempMultiIndexRunFile, usize, usize)> {
        self.writer.flush().map_err(|e| {
            Error::ExternalMultiIndexIo(format!("failed to flush run file payload: {e}"))
        })?;

        let checksum = crc32(&self.payload_bytes);
        let mut file = self.writer.into_inner().map_err(|e| {
            Error::ExternalMultiIndexIo(format!("failed to unwrap run file writer: {e}"))
        })?;

        file.seek(SeekFrom::Start(0)).map_err(|e| {
            Error::ExternalMultiIndexIo(format!("failed to seek to run file header: {e}"))
        })?;

        let mut header = [0u8; SPILL_HEADER_SIZE];
        header[0..4].copy_from_slice(TEMP_MULTI_INDEX_MAGIC);
        header[4..6].copy_from_slice(&TEMP_MULTI_INDEX_VERSION.to_le_bytes());
        header[6] = self.op_type as u8;
        header[7..15].copy_from_slice(&self.record_count.to_le_bytes());
        header[15..19].copy_from_slice(&checksum.to_le_bytes());

        file.write_all(&header).map_err(|e| {
            Error::ExternalMultiIndexIo(format!("failed to write finalized run header: {e}"))
        })?;
        file.flush().map_err(|e| {
            Error::ExternalMultiIndexIo(format!("failed to flush finalized run header: {e}"))
        })?;

        let total_bytes = SPILL_HEADER_SIZE + self.payload_bytes.len();
        Ok((self.temp_file, self.record_count as usize, total_bytes))
    }
}

/// Reads sorted `RecordId`s from a temporary spill file, validating header and CRC32 checksum.
#[derive(Debug)]
pub struct RecordIdRunReader {
    reader: BufReader<File>,
    records_remaining: usize,
    total_records: usize,
    next_item: Option<RecordId>,
    bytes_read: usize,
}

impl RecordIdRunReader {
    /// Opens and validates a temporary multi-index spill file.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).map_err(|e| {
            Error::ExternalMultiIndexIo(format!(
                "failed to open spill run file {}: {e}",
                path.display()
            ))
        })?;

        let file_len = file
            .metadata()
            .map_err(|e| {
                Error::ExternalMultiIndexIo(format!(
                    "failed to inspect spill run file metadata {}: {e}",
                    path.display()
                ))
            })?
            .len() as usize;

        if file_len < SPILL_HEADER_SIZE {
            return Err(Error::ExternalMultiIndexRunCorrupted(format!(
                "spill file {} truncated: file size {} bytes is less than header size {}",
                path.display(),
                file_len,
                SPILL_HEADER_SIZE
            )));
        }

        let mut reader = BufReader::new(file);
        let mut header = [0u8; SPILL_HEADER_SIZE];
        reader.read_exact(&mut header).map_err(|e| {
            Error::ExternalMultiIndexRunCorrupted(format!(
                "failed to read spill run file header {}: {e}",
                path.display()
            ))
        })?;

        // 1. Validate magic
        if &header[0..4] != TEMP_MULTI_INDEX_MAGIC {
            return Err(Error::ExternalMultiIndexRunCorrupted(format!(
                "invalid magic bytes in spill run file {}: expected {:?}, found {:?}",
                path.display(),
                TEMP_MULTI_INDEX_MAGIC,
                &header[0..4]
            )));
        }

        // 2. Validate version
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        if version != TEMP_MULTI_INDEX_VERSION {
            return Err(Error::ExternalMultiIndexRunCorrupted(format!(
                "unsupported spill run file version {version} in {} (expected {TEMP_MULTI_INDEX_VERSION})",
                path.display()
            )));
        }

        // 3. Read record count and checksum
        let record_count = u64::from_le_bytes(header[7..15].try_into().unwrap()) as usize;
        let expected_checksum = u32::from_le_bytes(header[15..19].try_into().unwrap());

        let expected_payload_len = record_count * RECORD_ID_PAYLOAD_BYTES;
        if file_len < SPILL_HEADER_SIZE + expected_payload_len {
            return Err(Error::ExternalMultiIndexRunCorrupted(format!(
                "spill file {} truncated: expected at least {} bytes, found {}",
                path.display(),
                SPILL_HEADER_SIZE + expected_payload_len,
                file_len
            )));
        }

        // 4. Validate CRC32 over the payload by reading the entire payload
        let mut payload = vec![0u8; expected_payload_len];
        reader.read_exact(&mut payload).map_err(|e| {
            Error::ExternalMultiIndexRunCorrupted(format!(
                "failed to read payload for checksum verification in {}: {e}",
                path.display()
            ))
        })?;

        let actual_checksum = crc32(&payload);
        if actual_checksum != expected_checksum {
            return Err(Error::ExternalMultiIndexRunCorrupted(format!(
                "CRC32 mismatch in spill file {}: expected {expected_checksum:#010X}, found {actual_checksum:#010X}",
                path.display()
            )));
        }

        // Rewind reader back to start of payload
        let mut file = reader.into_inner();
        file.seek(SeekFrom::Start(SPILL_HEADER_SIZE as u64))
            .map_err(|e| {
                Error::ExternalMultiIndexIo(format!(
                    "failed to seek back to payload in {}: {e}",
                    path.display()
                ))
            })?;
        let reader = BufReader::new(file);

        let mut run_reader = Self {
            reader,
            records_remaining: record_count,
            total_records: record_count,
            next_item: None,
            bytes_read: SPILL_HEADER_SIZE,
        };

        // Preload the first item
        run_reader.advance()?;
        Ok(run_reader)
    }

    /// Advances to the next RecordId in the stream.
    fn advance(&mut self) -> Result<()> {
        if self.records_remaining == 0 {
            self.next_item = None;
            return Ok(());
        }

        let mut buf = [0u8; RECORD_ID_PAYLOAD_BYTES];
        self.reader.read_exact(&mut buf).map_err(|e| {
            Error::ExternalMultiIndexRunCorrupted(format!(
                "unexpected EOF reading RecordId from spill run: {e}"
            ))
        })?;

        self.next_item = Some(RecordId::from_bytes(buf));
        self.records_remaining -= 1;
        self.bytes_read += RECORD_ID_PAYLOAD_BYTES;
        Ok(())
    }

    /// Peeks at the next `RecordId` without advancing the reader.
    pub fn peek(&self) -> Option<RecordId> {
        self.next_item
    }

    /// Returns the next `RecordId` and advances the stream.
    pub fn next_record(&mut self) -> Result<Option<RecordId>> {
        let current = self.next_item;
        if current.is_some() {
            self.advance()?;
        }
        Ok(current)
    }

    /// Returns the total number of records originally in this run.
    pub fn total_records(&self) -> usize {
        self.total_records
    }

    /// Returns the total bytes read so far.
    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

/// An entry in a multi-way merge priority queue.
#[derive(Debug, Eq, PartialEq)]
struct MergeHeapEntry {
    rid: RecordId,
    run_index: usize,
}

impl Ord for MergeHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse for min-heap
        other
            .rid
            .cmp(&self.rid)
            .then_with(|| other.run_index.cmp(&self.run_index))
    }
}

impl PartialOrd for MergeHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Merges multiple sorted `RecordIdRunReader`s into a single sorted stream via min-heap.
pub struct KWayRecordIdMerger {
    readers: Vec<RecordIdRunReader>,
    heap: BinaryHeap<MergeHeapEntry>,
}

impl KWayRecordIdMerger {
    /// Creates a new k-way merger from an array of run readers.
    pub fn new(mut readers: Vec<RecordIdRunReader>) -> Result<Self> {
        let mut heap = BinaryHeap::with_capacity(readers.len());
        for (i, reader) in readers.iter_mut().enumerate() {
            if let Some(rid) = reader.peek() {
                heap.push(MergeHeapEntry { rid, run_index: i });
            }
        }
        Ok(Self { readers, heap })
    }

    /// Fetches the next smallest `RecordId` from the merger.
    pub fn next_record(&mut self) -> Result<Option<RecordId>> {
        let entry = match self.heap.pop() {
            Some(e) => e,
            None => return Ok(None),
        };

        // Advance the corresponding reader
        let _ = self.readers[entry.run_index].next_record()?;
        if let Some(next_rid) = self.readers[entry.run_index].peek() {
            self.heap.push(MergeHeapEntry {
                rid: next_rid,
                run_index: entry.run_index,
            });
        }

        Ok(Some(entry.rid))
    }

    /// Total bytes read across all active run readers.
    pub fn total_bytes_read(&self) -> usize {
        self.readers.iter().map(|r| r.bytes_read()).sum()
    }
}

/// Abstract streaming source of sorted `RecordId`s.
pub enum RecordIdStream {
    /// In-memory sorted vector iterator.
    InMemory {
        records: Vec<RecordId>,
        cursor: usize,
    },
    /// Single run reader.
    SpillReader(RecordIdRunReader),
    /// Multi-run k-way merger.
    KWayMerger(KWayRecordIdMerger),
}

impl RecordIdStream {
    /// Creates an in-memory stream from a slice of `RecordId`s (sorting and deduplicating within stream if needed).
    pub fn from_vec(mut rids: Vec<RecordId>) -> Self {
        rids.sort_unstable();
        Self::InMemory {
            records: rids,
            cursor: 0,
        }
    }

    /// Peeks at the next `RecordId` in the stream without advancing.
    pub fn peek(&self) -> Option<RecordId> {
        match self {
            Self::InMemory { records, cursor } => records.get(*cursor).copied(),
            Self::SpillReader(reader) => reader.peek(),
            Self::KWayMerger(merger) => merger.heap.peek().map(|e| e.rid),
        }
    }

    /// Advances the stream and returns the next `RecordId`.
    pub fn next_record(&mut self) -> Result<Option<RecordId>> {
        match self {
            Self::InMemory { records, cursor } => {
                if *cursor < records.len() {
                    let item = records[*cursor];
                    *cursor += 1;
                    Ok(Some(item))
                } else {
                    Ok(None)
                }
            }
            Self::SpillReader(reader) => reader.next_record(),
            Self::KWayMerger(merger) => merger.next_record(),
        }
    }

    /// Returns the bytes read if backed by spill files.
    pub fn bytes_read(&self) -> usize {
        match self {
            Self::InMemory { .. } => 0,
            Self::SpillReader(reader) => reader.bytes_read(),
            Self::KWayMerger(merger) => merger.total_bytes_read(),
        }
    }
}

/// Executes sort-merge intersection across multiple child candidate RecordId streams.
/// Streams can be in-memory or spilled to disk.
pub fn execute_multi_index_intersection_streams(
    mut streams: Vec<RecordIdStream>,
) -> Result<(Vec<RecordId>, usize)> {
    if streams.is_empty() {
        return Ok((Vec::new(), 0));
    }
    if streams.len() == 1 {
        let mut out = Vec::new();
        let mut last = None;
        while let Some(rid) = streams[0].next_record()? {
            if Some(rid) != last {
                out.push(rid);
                last = Some(rid);
            }
        }
        return Ok((out, 0));
    }

    let num_streams = streams.len();
    let mut current_heads: Vec<Option<RecordId>> = Vec::with_capacity(num_streams);
    for stream in &mut streams {
        current_heads.push(stream.next_record()?);
    }

    let mut result = Vec::new();
    let mut duplicates_removed = 0;
    let mut last_emitted = None;

    loop {
        // Check if any stream is exhausted
        if current_heads.iter().any(|h| h.is_none()) {
            break;
        }

        // Find maximum RecordId among current heads
        let mut max_rid = current_heads[0].unwrap();
        let mut all_equal = true;

        for &head in &current_heads[1..] {
            let rid = head.unwrap();
            if rid > max_rid {
                max_rid = rid;
                all_equal = false;
            } else if rid < max_rid {
                all_equal = false;
            }
        }

        if all_equal {
            if Some(max_rid) != last_emitted {
                result.push(max_rid);
                last_emitted = Some(max_rid);
            } else {
                duplicates_removed += 1;
            }

            // Advance all streams
            for i in 0..num_streams {
                // Advance past all instances of max_rid to skip intra-stream duplicates
                while current_heads[i] == Some(max_rid) {
                    current_heads[i] = streams[i].next_record()?;
                }
            }
        } else {
            // Advance all streams whose current head is strictly less than max_rid
            for i in 0..num_streams {
                if let Some(rid) = current_heads[i] {
                    if rid < max_rid {
                        current_heads[i] = streams[i].next_record()?;
                    }
                }
            }
        }
    }

    Ok((result, duplicates_removed))
}

/// Executes sort-merge union with streaming deduplication across multiple child candidate RecordId streams.
pub fn execute_multi_index_union_streams(
    streams: Vec<RecordIdStream>,
) -> Result<(Vec<RecordId>, usize)> {
    if streams.is_empty() {
        return Ok((Vec::new(), 0));
    }

    // Wrap all streams in a min-heap merger
    struct StreamHeapEntry {
        rid: RecordId,
        stream_index: usize,
    }

    impl Ord for StreamHeapEntry {
        fn cmp(&self, other: &Self) -> Ordering {
            other
                .rid
                .cmp(&self.rid)
                .then_with(|| other.stream_index.cmp(&self.stream_index))
        }
    }

    impl PartialOrd for StreamHeapEntry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    impl PartialEq for StreamHeapEntry {
        fn eq(&self, other: &Self) -> bool {
            self.rid == other.rid && self.stream_index == other.stream_index
        }
    }

    impl Eq for StreamHeapEntry {}

    let mut streams = streams;
    let mut heap = BinaryHeap::with_capacity(streams.len());

    for (i, stream) in streams.iter_mut().enumerate() {
        if let Some(rid) = stream.next_record()? {
            heap.push(StreamHeapEntry {
                rid,
                stream_index: i,
            });
        }
    }

    let mut result = Vec::new();
    let mut duplicates_removed = 0;
    let mut last_emitted: Option<RecordId> = None;

    while let Some(entry) = heap.pop() {
        if Some(entry.rid) != last_emitted {
            result.push(entry.rid);
            last_emitted = Some(entry.rid);
        } else {
            duplicates_removed += 1;
        }

        if let Some(next_rid) = streams[entry.stream_index].next_record()? {
            heap.push(StreamHeapEntry {
                rid: next_rid,
                stream_index: entry.stream_index,
            });
        }
    }

    Ok((result, duplicates_removed))
}

/// Spills a list of RecordIds into one or more sorted temporary runs if its size exceeds the memory budget.
/// Returns either the in-memory sorted vector or a list of RAII temporary run files.
pub fn prepare_child_candidate_stream(
    mut rids: Vec<RecordId>,
    budget_bytes: usize,
    temp_dir: &Path,
    metrics: &mut MultiIndexSpillMetrics,
) -> Result<(Option<Vec<RecordId>>, Vec<TempMultiIndexRunFile>)> {
    let candidate_bytes = rids.len() * RECORD_ID_IN_MEMORY_BYTES;
    metrics.peak_memory_bytes = metrics.peak_memory_bytes.max(candidate_bytes);

    if candidate_bytes <= budget_bytes {
        // Fits in memory
        rids.sort_unstable();
        return Ok((Some(rids), Vec::new()));
    }

    // Must spill to sorted run(s)
    rids.sort_unstable();
    let mut writer = RecordIdRunWriter::create_in(temp_dir, SpillOpType::ChildRun)?;
    writer.write_records(&rids)?;
    let (run_file, count, written_bytes) = writer.finish()?;

    metrics.runs_created += 1;
    metrics.bytes_written += written_bytes;
    metrics.rows_spilled += count;

    Ok((None, vec![run_file]))
}
