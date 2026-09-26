use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::engine::types::{Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::operator::{compare_rows_by_sort_keys, SortKey};

/// Default in-memory sort memory budget: 64 KB.
pub const DEFAULT_SORT_MEMORY_BUDGET_BYTES: usize = 64 * 1024;

/// Default maximum number of temporary run files open concurrently during merge passes.
pub const DEFAULT_MAX_TEMP_FILES: usize = 32;

/// Default maximum total disk space permitted for temporary sort runs: 1 GB.
pub const DEFAULT_MAX_TEMP_BYTES: usize = 1024 * 1024 * 1024;

/// Default maximum merge passes before aborting: 100 passes.
pub const DEFAULT_MAX_MERGE_PASSES: usize = 100;

/// Magic bytes identifying a TreeDB temporary sort run file: `b"TSRN"`.
pub const TEMP_RUN_MAGIC: &[u8; 4] = b"TSRN";

/// Version identifier for the temporary run file format.
pub const TEMP_RUN_VERSION: u16 = 1;

static RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Runtime configuration parameters controlling the external sorting and spill-to-disk engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortConfig {
    /// In-memory sort buffer budget in bytes before spilling to disk.
    pub memory_budget_bytes: usize,
    /// Maximum number of temporary run files open simultaneously during merge.
    /// If total runs exceed this threshold, multi-pass merging is executed.
    pub max_temp_files: usize,
    /// Maximum disk space in bytes that can be consumed by temporary runs.
    pub max_temp_bytes: usize,
    /// Maximum number of merge passes before returning an error.
    pub max_merge_passes: usize,
    /// Custom directory for temporary run files. If `None`, `std::env::temp_dir()` is used.
    pub temp_dir: Option<PathBuf>,
}

impl Default for SortConfig {
    fn default() -> Self {
        Self {
            memory_budget_bytes: DEFAULT_SORT_MEMORY_BUDGET_BYTES,
            max_temp_files: DEFAULT_MAX_TEMP_FILES,
            max_temp_bytes: DEFAULT_MAX_TEMP_BYTES,
            max_merge_passes: DEFAULT_MAX_MERGE_PASSES,
            temp_dir: None,
        }
    }
}

impl SortConfig {
    /// Creates a new `SortConfig` with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the in-memory sort buffer budget in bytes.
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

    /// Sets the directory path for temporary sort run files.
    pub fn with_temp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(path.into());
        self
    }

    /// Returns the effective temporary directory path.
    pub fn effective_temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

/// Execution telemetry and statistics for a sort operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SortExecutionMetrics {
    /// Total number of rows consumed by the sort operator.
    pub rows_input: usize,
    /// Total number of rows emitted by the sort operator.
    pub rows_output: usize,
    /// Number of temporary runs created on disk (0 indicates in-memory fast path).
    pub runs_created: usize,
    /// Total bytes written to disk across all temporary runs (including intermediate passes).
    pub bytes_spilled: usize,
    /// Total merge passes executed (0 for in-memory, >= 1 for external sort).
    pub merge_passes: usize,
    /// Peak in-memory byte footprint estimated during sorting.
    pub peak_memory_bytes: usize,
}

/// Computes an accurate estimate of in-memory heap usage for a single `Row`.
pub fn estimate_row_memory(row: &Row) -> usize {
    let mut size = std::mem::size_of::<Row>()
        + row.values.capacity() * std::mem::size_of::<Value>()
        + std::mem::size_of::<(u64, Row)>(); // include sequence wrapper overhead
    for val in &row.values {
        match val {
            Value::String(s) => size += s.capacity(),
            Value::Bytes(b) => size += b.capacity(),
            _ => {}
        }
    }
    size
}

/// An RAII-managed temporary file for external sort runs that guarantees deletion on `Drop`.
#[derive(Debug)]
pub struct TempRunFile {
    path: PathBuf,
}

impl TempRunFile {
    /// Creates and opens a new unique temporary file in the specified directory.
    pub fn create_in(temp_dir: &Path) -> Result<(Self, File)> {
        if !temp_dir.exists() {
            std::fs::create_dir_all(temp_dir).map_err(|e| {
                Error::ExternalSortIo(format!(
                    "failed to create temporary sort directory {}: {e}",
                    temp_dir.display()
                ))
            })?;
        }

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = RUN_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let filename = format!("treedb_sort_run_{pid}_{nanos}_{counter}.tmp");
        let path = temp_dir.join(filename);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                Error::ExternalSortIo(format!(
                    "failed to create temporary sort run file {}: {e}",
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

impl Drop for TempRunFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serializes a single `Value` to a binary writer using little-endian encoding.
fn serialize_value<W: Write>(w: &mut W, val: &Value) -> Result<usize> {
    let mut written = 0;
    match val {
        Value::Null => {
            w.write_all(&[0u8])?;
            written += 1;
        }
        Value::Boolean(b) => {
            w.write_all(&[if *b { 2u8 } else { 1u8 }])?;
            written += 1;
        }
        Value::Int64(i) => {
            w.write_all(&[3u8])?;
            w.write_all(&i.to_le_bytes())?;
            written += 1 + 8;
        }
        Value::UInt64(u) => {
            w.write_all(&[4u8])?;
            w.write_all(&u.to_le_bytes())?;
            written += 1 + 8;
        }
        Value::Float64(f) => {
            w.write_all(&[5u8])?;
            w.write_all(&f.to_bits().to_le_bytes())?;
            written += 1 + 8;
        }
        Value::String(s) => {
            w.write_all(&[6u8])?;
            let bytes = s.as_bytes();
            let len = bytes.len() as u32;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(bytes)?;
            written += 1 + 4 + bytes.len();
        }
        Value::Bytes(b) => {
            w.write_all(&[7u8])?;
            let len = b.len() as u32;
            w.write_all(&len.to_le_bytes())?;
            w.write_all(b)?;
            written += 1 + 4 + b.len();
        }
    }
    Ok(written)
}

/// Deserializes a single `Value` from a binary reader.
fn deserialize_value<R: Read>(r: &mut R) -> Result<Value> {
    let mut tag_buf = [0u8; 1];
    r.read_exact(&mut tag_buf).map_err(|e| {
        Error::ExternalSortRunCorrupted(format!(
            "unexpected EOF or read error reading value tag: {e}"
        ))
    })?;

    match tag_buf[0] {
        0 => Ok(Value::Null),
        1 => Ok(Value::Boolean(false)),
        2 => Ok(Value::Boolean(true)),
        3 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)?;
            Ok(Value::Int64(i64::from_le_bytes(buf)))
        }
        4 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)?;
            Ok(Value::UInt64(u64::from_le_bytes(buf)))
        }
        5 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)?;
            Ok(Value::Float64(f64::from_bits(u64::from_le_bytes(buf))))
        }
        6 => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut bytes = vec![0u8; len];
            r.read_exact(&mut bytes)?;
            let s = String::from_utf8(bytes).map_err(|e| {
                Error::ExternalSortRunCorrupted(format!("invalid UTF-8 string in sort run: {e}"))
            })?;
            Ok(Value::String(s))
        }
        7 => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut bytes = vec![0u8; len];
            r.read_exact(&mut bytes)?;
            Ok(Value::Bytes(bytes))
        }
        other => Err(Error::ExternalSortRunCorrupted(format!(
            "invalid value type discriminant tag {other} in sort run"
        ))),
    }
}

/// Serializes a tagged row `(sequence, Row)` to a binary writer.
fn serialize_tagged_row<W: Write>(w: &mut W, seq: u64, row: &Row) -> Result<usize> {
    let mut written = 0;
    w.write_all(&seq.to_le_bytes())?;
    written += 8;

    let col_count = row.values.len() as u32;
    w.write_all(&col_count.to_le_bytes())?;
    written += 4;

    for val in &row.values {
        written += serialize_value(w, val)?;
    }
    Ok(written)
}

/// Deserializes a tagged row `(sequence, Row)` from a binary reader.
fn deserialize_tagged_row<R: Read>(r: &mut R) -> Result<(u64, Row)> {
    let mut seq_buf = [0u8; 8];
    r.read_exact(&mut seq_buf)?;
    let seq = u64::from_le_bytes(seq_buf);

    let mut col_count_buf = [0u8; 4];
    r.read_exact(&mut col_count_buf)?;
    let col_count = u32::from_le_bytes(col_count_buf) as usize;

    let mut values = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        values.push(deserialize_value(r)?);
    }

    Ok((seq, Row::new(values)))
}

/// Writes a sorted batch of tagged rows to a temporary run file, returning the `TempRunFile` and byte count.
pub fn write_sorted_run(temp_dir: &Path, rows: &[(u64, Row)]) -> Result<(TempRunFile, usize)> {
    let (run_file, file) = TempRunFile::create_in(temp_dir)?;
    let mut writer = BufWriter::new(file);

    let mut total_bytes = 0;
    writer.write_all(TEMP_RUN_MAGIC)?;
    total_bytes += 4;

    writer.write_all(&TEMP_RUN_VERSION.to_le_bytes())?;
    total_bytes += 2;

    writer.write_all(&[0u8; 2])?; // reserved
    total_bytes += 2;

    let row_count = rows.len() as u64;
    writer.write_all(&row_count.to_le_bytes())?;
    total_bytes += 8;

    for (seq, row) in rows {
        total_bytes += serialize_tagged_row(&mut writer, *seq, row)?;
    }

    writer.flush()?;
    Ok((run_file, total_bytes))
}

/// Streaming reader for an on-disk temporary sort run.
pub struct RunReader {
    _file_handle: TempRunFile,
    reader: BufReader<File>,
    total_rows: u64,
    rows_read: u64,
}

impl RunReader {
    /// Opens and validates a temporary run file for sequential reading.
    pub fn open(file: TempRunFile) -> Result<Self> {
        let mut std_file = File::open(file.path()).map_err(|e| {
            Error::ExternalSortIo(format!(
                "failed to open temporary sort run file {}: {e}",
                file.path().display()
            ))
        })?;

        std_file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(std_file);

        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic).map_err(|e| {
            Error::ExternalSortRunCorrupted(format!(
                "failed to read temporary run magic header: {e}"
            ))
        })?;

        if &magic != TEMP_RUN_MAGIC {
            return Err(Error::ExternalSortRunCorrupted(format!(
                "invalid run magic header: expected {TEMP_RUN_MAGIC:?}, found {magic:?}"
            )));
        }

        let mut version_buf = [0u8; 2];
        reader.read_exact(&mut version_buf)?;
        let version = u16::from_le_bytes(version_buf);
        if version != TEMP_RUN_VERSION {
            return Err(Error::ExternalSortRunCorrupted(format!(
                "unsupported run format version: found {version}, expected {TEMP_RUN_VERSION}"
            )));
        }

        let mut reserved = [0u8; 2];
        reader.read_exact(&mut reserved)?;

        let mut row_count_buf = [0u8; 8];
        reader.read_exact(&mut row_count_buf)?;
        let total_rows = u64::from_le_bytes(row_count_buf);

        Ok(Self {
            _file_handle: file,
            reader,
            total_rows,
            rows_read: 0,
        })
    }

    /// Reads the next tagged row from the run file. Returns `None` when all rows have been read.
    pub fn next_row(&mut self) -> Result<Option<(u64, Row)>> {
        if self.rows_read >= self.total_rows {
            return Ok(None);
        }

        let tagged = deserialize_tagged_row(&mut self.reader)?;
        self.rows_read += 1;
        Ok(Some(tagged))
    }

    /// Returns the number of unread rows remaining in this run.
    pub fn remaining(&self) -> u64 {
        self.total_rows.saturating_sub(self.rows_read)
    }
}

/// Helper struct representing an active item in the k-way merge priority queue.
struct MergeItem<'a> {
    seq: u64,
    row: Row,
    reader_idx: usize,
    schema: &'a Schema,
    sort_keys: &'a [SortKey],
}

impl<'a> PartialEq for MergeItem<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}

impl<'a> Eq for MergeItem<'a> {}

impl<'a> Ord for MergeItem<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; we invert comparisons to yield a min-heap.
        match compare_rows_by_sort_keys(&self.row, &other.row, self.schema, self.sort_keys) {
            Ok(Ordering::Equal) => other.seq.cmp(&self.seq),
            Ok(ord) => ord.reverse(),
            Err(_) => other.seq.cmp(&self.seq),
        }
    }
}

impl<'a> PartialOrd for MergeItem<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Core external sorting and spill-to-disk engine.
pub struct ExternalSorter<'a> {
    schema: &'a Schema,
    sort_keys: &'a [SortKey],
    config: SortConfig,
}

impl<'a> ExternalSorter<'a> {
    /// Creates a new `ExternalSorter` with the provided schema, sort specification, and configuration.
    pub fn new(schema: &'a Schema, sort_keys: &'a [SortKey], config: SortConfig) -> Self {
        Self {
            schema,
            sort_keys,
            config,
        }
    }

    /// Sorts the input rows, using an in-memory fast path if the memory budget is respected,
    /// or spilling to temporary runs and performing a k-way merge if the budget is exceeded.
    pub fn sort(&self, rows: Vec<Row>) -> Result<(Vec<Row>, SortExecutionMetrics)> {
        if self.sort_keys.is_empty() || rows.is_empty() {
            let len = rows.len();
            return Ok((
                rows,
                SortExecutionMetrics {
                    rows_input: len,
                    rows_output: len,
                    runs_created: 0,
                    bytes_spilled: 0,
                    merge_passes: 0,
                    peak_memory_bytes: 0,
                },
            ));
        }

        // Validate sort expressions against schema upfront
        for key in self.sort_keys {
            key.expression.infer_type(self.schema)?;
        }

        let total_input_rows = rows.len();
        let temp_dir = self.config.effective_temp_dir();

        let mut current_batch: Vec<(u64, Row)> = Vec::new();
        let mut current_batch_bytes = 0usize;
        let mut peak_memory_bytes = 0usize;
        let mut runs: Vec<TempRunFile> = Vec::new();
        let mut bytes_spilled = 0usize;

        for (seq_idx, row) in rows.into_iter().enumerate() {
            let row_mem = estimate_row_memory(&row);
            if current_batch_bytes + row_mem > self.config.memory_budget_bytes
                && !current_batch.is_empty()
            {
                // Sort current in-memory batch
                self.sort_tagged_batch(&mut current_batch)?;

                // Spill to disk
                let (run_file, written_bytes) = write_sorted_run(&temp_dir, &current_batch)?;
                bytes_spilled += written_bytes;
                if bytes_spilled > self.config.max_temp_bytes {
                    return Err(Error::ExternalSortQuotaExceeded {
                        bytes_spilled,
                        limit: self.config.max_temp_bytes,
                    });
                }
                runs.push(run_file);

                current_batch.clear();
                current_batch_bytes = 0;
            }

            let seq = seq_idx as u64;
            current_batch.push((seq, row));
            current_batch_bytes += row_mem;
            peak_memory_bytes = peak_memory_bytes.max(current_batch_bytes);
        }

        // Case 1: In-Memory Fast Path (all rows fit within memory budget)
        if runs.is_empty() {
            self.sort_tagged_batch(&mut current_batch)?;
            let out_rows = current_batch.into_iter().map(|(_, r)| r).collect();
            return Ok((
                out_rows,
                SortExecutionMetrics {
                    rows_input: total_input_rows,
                    rows_output: total_input_rows,
                    runs_created: 0,
                    bytes_spilled: 0,
                    merge_passes: 0,
                    peak_memory_bytes,
                },
            ));
        }

        // Case 2: Spill Path (write final batch if non-empty)
        if !current_batch.is_empty() {
            self.sort_tagged_batch(&mut current_batch)?;
            let (run_file, written_bytes) = write_sorted_run(&temp_dir, &current_batch)?;
            bytes_spilled += written_bytes;
            if bytes_spilled > self.config.max_temp_bytes {
                return Err(Error::ExternalSortQuotaExceeded {
                    bytes_spilled,
                    limit: self.config.max_temp_bytes,
                });
            }
            runs.push(run_file);
            current_batch.clear();
        }

        let runs_created = runs.len();
        let mut merge_passes = 0usize;

        // Perform multi-pass merge if runs exceed max_temp_files
        while runs.len() > self.config.max_temp_files {
            merge_passes += 1;
            if merge_passes > self.config.max_merge_passes {
                return Err(Error::ExternalSortMaxPassesExceeded {
                    passes: merge_passes,
                    limit: self.config.max_merge_passes,
                });
            }

            let mut next_level_runs = Vec::new();
            for chunk in runs.chunks(self.config.max_temp_files) {
                let mut chunk_readers = Vec::with_capacity(chunk.len());
                for run in chunk {
                    let reader = RunReader::open(TempRunFile {
                        path: run.path().to_path_buf(),
                    })?;
                    chunk_readers.push(reader);
                }

                let (merged_file, written_bytes) =
                    self.merge_readers_to_run(&temp_dir, chunk_readers)?;
                bytes_spilled += written_bytes;
                if bytes_spilled > self.config.max_temp_bytes {
                    return Err(Error::ExternalSortQuotaExceeded {
                        bytes_spilled,
                        limit: self.config.max_temp_bytes,
                    });
                }
                next_level_runs.push(merged_file);
            }
            runs = next_level_runs;
        }

        // Final merge pass directly to output rows
        merge_passes += 1;
        let mut final_readers = Vec::with_capacity(runs.len());
        for run in runs {
            final_readers.push(RunReader::open(run)?);
        }

        let out_rows = self.merge_readers_to_output(final_readers, total_input_rows)?;

        Ok((
            out_rows,
            SortExecutionMetrics {
                rows_input: total_input_rows,
                rows_output: total_input_rows,
                runs_created,
                bytes_spilled,
                merge_passes,
                peak_memory_bytes,
            },
        ))
    }

    /// Sorts a vector of tagged rows `(sequence, row)` in-memory using the configured sort keys.
    fn sort_tagged_batch(&self, batch: &mut [(u64, Row)]) -> Result<()> {
        let mut sort_err = Ok(());
        batch.sort_by(|(seq_a, row_a), (seq_b, row_b)| {
            if sort_err.is_err() {
                return Ordering::Equal;
            }
            match compare_rows_by_sort_keys(row_a, row_b, self.schema, self.sort_keys) {
                Ok(Ordering::Equal) => seq_a.cmp(seq_b),
                Ok(ord) => ord,
                Err(e) => {
                    sort_err = Err(e);
                    Ordering::Equal
                }
            }
        });
        sort_err
    }

    /// Merges multiple run readers into a new temporary run file.
    fn merge_readers_to_run(
        &self,
        temp_dir: &Path,
        mut readers: Vec<RunReader>,
    ) -> Result<(TempRunFile, usize)> {
        let (run_file, file) = TempRunFile::create_in(temp_dir)?;
        let mut writer = BufWriter::new(file);

        let mut total_bytes = 0;
        writer.write_all(TEMP_RUN_MAGIC)?;
        total_bytes += 4;

        writer.write_all(&TEMP_RUN_VERSION.to_le_bytes())?;
        total_bytes += 2;

        writer.write_all(&[0u8; 2])?;
        total_bytes += 2;

        let total_run_rows: u64 = readers.iter().map(|r| r.remaining()).sum();
        writer.write_all(&total_run_rows.to_le_bytes())?;
        total_bytes += 8;

        let mut heap: BinaryHeap<MergeItem<'a>> = BinaryHeap::with_capacity(readers.len());

        for (reader_idx, reader) in readers.iter_mut().enumerate() {
            if let Some((seq, row)) = reader.next_row()? {
                heap.push(MergeItem {
                    seq,
                    row,
                    reader_idx,
                    schema: self.schema,
                    sort_keys: self.sort_keys,
                });
            }
        }

        while let Some(item) = heap.pop() {
            total_bytes += serialize_tagged_row(&mut writer, item.seq, &item.row)?;
            let reader_idx = item.reader_idx;
            if let Some((seq, row)) = readers[reader_idx].next_row()? {
                heap.push(MergeItem {
                    seq,
                    row,
                    reader_idx,
                    schema: self.schema,
                    sort_keys: self.sort_keys,
                });
            }
        }

        writer.flush()?;
        Ok((run_file, total_bytes))
    }

    /// Merges multiple run readers directly into a vector of output `Row`s.
    fn merge_readers_to_output(
        &self,
        mut readers: Vec<RunReader>,
        capacity: usize,
    ) -> Result<Vec<Row>> {
        let mut out_rows = Vec::with_capacity(capacity);
        let mut heap: BinaryHeap<MergeItem<'a>> = BinaryHeap::with_capacity(readers.len());

        for (reader_idx, reader) in readers.iter_mut().enumerate() {
            if let Some((seq, row)) = reader.next_row()? {
                heap.push(MergeItem {
                    seq,
                    row,
                    reader_idx,
                    schema: self.schema,
                    sort_keys: self.sort_keys,
                });
            }
        }

        while let Some(item) = heap.pop() {
            let reader_idx = item.reader_idx;
            out_rows.push(item.row);
            if let Some((seq, row)) = readers[reader_idx].next_row()? {
                heap.push(MergeItem {
                    seq,
                    row,
                    reader_idx,
                    schema: self.schema,
                    sort_keys: self.sort_keys,
                });
            }
        }

        Ok(out_rows)
    }
}
