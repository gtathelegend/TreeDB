use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::engine::types::{Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{resolve_column_in_schema, BinaryOp, Expr};
use crate::query::operator::CanonicalValue;
use crate::query::optimizer::{recombine_conjunction, split_conjunction};
use crate::sql::ast::JoinType;

/// Default in-memory hash join build budget before spilling: 64 KB.
pub const DEFAULT_JOIN_MEMORY_BUDGET_BYTES: usize = 64 * 1024;

/// Default maximum number of temporary partition files open concurrently during external join.
pub const DEFAULT_MAX_TEMP_FILES: usize = 32;

/// Default maximum total disk space permitted for temporary join partitions: 1 GB.
pub const DEFAULT_MAX_TEMP_BYTES: usize = 1024 * 1024 * 1024;

/// Default maximum partition passes before aborting: 100 passes.
pub const DEFAULT_MAX_PARTITION_PASSES: usize = 100;

/// Magic bytes identifying a TreeDB temporary join partition file: `b"TJRN"`.
pub const TEMP_JOIN_MAGIC: &[u8; 4] = b"TJRN";

/// Version identifier for the temporary join partition format.
pub const TEMP_JOIN_VERSION: u16 = 1;

static JOIN_RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Runtime configuration parameters controlling external hash join and spill-to-disk execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinConfig {
    /// In-memory hash join build-side buffer budget in bytes before spilling to disk.
    pub memory_budget_bytes: usize,
    /// Maximum number of temporary partition files open simultaneously.
    pub max_temp_files: usize,
    /// Maximum disk space in bytes that can be consumed by temporary join partitions.
    pub max_temp_bytes: usize,
    /// Maximum number of partition/re-partitioning passes before returning an error.
    pub max_partition_passes: usize,
    /// Custom directory for temporary partition files. If `None`, `std::env::temp_dir()` is used.
    pub temp_dir: Option<PathBuf>,
    /// Whether to build and apply runtime join filters during join execution.
    pub enable_runtime_filter: bool,
    /// Strategy type for runtime join filters (ExactSet or Bloom).
    pub runtime_filter_type: crate::query::runtime_filter::RuntimeFilterType,
}

impl Default for JoinConfig {
    fn default() -> Self {
        Self {
            memory_budget_bytes: DEFAULT_JOIN_MEMORY_BUDGET_BYTES,
            max_temp_files: DEFAULT_MAX_TEMP_FILES,
            max_temp_bytes: DEFAULT_MAX_TEMP_BYTES,
            max_partition_passes: DEFAULT_MAX_PARTITION_PASSES,
            temp_dir: None,
            enable_runtime_filter: true,
            runtime_filter_type: crate::query::runtime_filter::RuntimeFilterType::ExactSet,
        }
    }
}

impl JoinConfig {
    /// Creates a new `JoinConfig` with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the in-memory hash join buffer budget in bytes.
    pub fn with_memory_budget_bytes(mut self, budget: usize) -> Self {
        self.memory_budget_bytes = budget.max(1);
        self
    }

    /// Sets the maximum number of temporary partition files open simultaneously.
    pub fn with_max_temp_files(mut self, max_files: usize) -> Self {
        self.max_temp_files = max_files.max(2);
        self
    }

    /// Sets the maximum allowable disk space in bytes for temporary join runs.
    pub fn with_max_temp_bytes(mut self, max_bytes: usize) -> Self {
        self.max_temp_bytes = max_bytes;
        self
    }

    /// Sets the maximum number of partition passes allowed.
    pub fn with_max_partition_passes(mut self, max_passes: usize) -> Self {
        self.max_partition_passes = max_passes.max(1);
        self
    }

    /// Sets the directory path for temporary join partition files.
    pub fn with_temp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(path.into());
        self
    }

    /// Enables or disables runtime join filtering.
    pub fn with_enable_runtime_filter(mut self, enabled: bool) -> Self {
        self.enable_runtime_filter = enabled;
        self
    }

    /// Sets the runtime filter representation type.
    pub fn with_runtime_filter_type(
        mut self,
        filter_type: crate::query::runtime_filter::RuntimeFilterType,
    ) -> Self {
        self.runtime_filter_type = filter_type;
        self
    }

    /// Returns the effective temporary directory path.
    pub fn effective_temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

/// Execution telemetry and statistics for a join operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoinExecutionMetrics {
    /// Total number of rows consumed from the left relation.
    pub rows_left: usize,
    /// Total number of rows consumed from the right relation.
    pub rows_right: usize,
    /// Total number of rows emitted by the join operator.
    pub rows_output: usize,
    /// Number of distinct spill partitions used during external hash join.
    pub spill_partitions: usize,
    /// Number of temporary partition run files created on disk.
    pub runs_created: usize,
    /// Total bytes written to disk across all temporary join runs.
    pub bytes_spilled: usize,
    /// Total partition passes executed (0 for in-memory, >= 1 for spilled join).
    pub partition_passes: usize,
    /// Total sort merge passes executed if external sorting was required.
    pub merge_passes: usize,
    /// Peak in-memory byte footprint estimated during join execution.
    pub peak_memory_bytes: usize,
    /// Candidate cross-product pairs examined during join evaluation.
    pub candidate_pairs: usize,
    /// Total rows that satisfied the join condition.
    pub matched_rows: usize,
    /// Outer rows that had at least one match (for SEMI/ANTI).
    pub matched_outer_rows: usize,
    /// Outer rows that had no match (for SEMI/ANTI).
    pub unmatched_outer_rows: usize,
    /// Number of rows built into the hash table.
    pub hash_build_rows: usize,
    /// Number of rows probed against the hash table.
    pub hash_probe_rows: usize,
    /// Whether a runtime join filter was actively constructed and evaluated.
    pub runtime_filter_enabled: bool,
    /// Semantic type of the active runtime filter (ExactSet or Bloom).
    pub runtime_filter_type: Option<String>,
    /// Number of build-side rows inserted into the runtime filter.
    pub runtime_filter_build_rows: usize,
    /// Number of distinct keys stored in the runtime filter.
    pub runtime_filter_distinct_keys: usize,
    /// In-memory byte footprint of the runtime filter.
    pub runtime_filter_memory_bytes: usize,
    /// Number of probe-side rows evaluated against the runtime filter.
    pub runtime_filter_probe_rows: usize,
    /// Number of probe-side rows rejected by the runtime filter before hash lookup.
    pub runtime_filter_rejected_rows: usize,
    /// Number of probe-side rows passing the runtime filter into hash lookup.
    pub runtime_filter_passed_rows: usize,
    /// Number of probe-side rows with NULL keys skipped before runtime filter evaluation.
    pub runtime_filter_null_probe_rows: usize,
    /// Number of false positives observed (passed filter but produced zero join hash matches).
    pub runtime_filter_false_positive_rows: usize,
    /// Merge join specific: Left rows consumed
    pub merge_join_left_rows: usize,
    /// Merge join specific: Right rows consumed
    pub merge_join_right_rows: usize,
    /// Merge join specific: Total key comparisons performed
    pub merge_join_comparisons: usize,
    /// Merge join specific: Total key matches
    pub merge_join_key_matches: usize,
    /// Merge join specific: Duplicate left rows encountered
    pub merge_join_duplicate_left_rows: usize,
    /// Merge join specific: Duplicate right rows encountered
    pub merge_join_duplicate_right_rows: usize,
    /// Merge join specific: Total rows emitted
    pub merge_join_output_rows: usize,
    /// Merge join specific: Unmatched left rows emitted
    pub merge_join_unmatched_left_rows: usize,
    /// Merge join specific: Unmatched right rows emitted
    pub merge_join_unmatched_right_rows: usize,
    /// Merge join specific: Left keys containing NULL
    pub merge_join_null_left_keys: usize,
    /// Merge join specific: Right keys containing NULL
    pub merge_join_null_right_keys: usize,
    /// Merge join specific: Residual predicate evaluations
    pub merge_join_residual_evaluations: usize,
    /// Merge join specific: Residual predicate rejections
    pub merge_join_residual_rejections: usize,
    /// Merge join specific: Whether left input required sort
    pub merge_join_sort_left: bool,
    /// Merge join specific: Whether right input required sort
    pub merge_join_sort_right: bool,
}

/// Composite join key constructed from evaluated equi-join expressions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JoinKey(pub Vec<CanonicalValue>);

impl JoinKey {
    /// Evaluates a sequence of key expressions on a row to construct a `JoinKey`.
    pub fn from_row(row: &Row, schema: &Schema, exprs: &[Expr]) -> Result<Self> {
        let mut canonical_keys = Vec::with_capacity(exprs.len());
        for expr in exprs {
            let val = expr.evaluate(row, schema)?;
            canonical_keys.push(CanonicalValue::from_value(&val));
        }
        Ok(JoinKey(canonical_keys))
    }

    /// Returns true if any component of the join key is `Value::Null`.
    /// Under SQL semantics, `NULL = NULL` is UNKNOWN and never matches.
    pub fn has_null(&self) -> bool {
        self.0.iter().any(|v| matches!(v, CanonicalValue::Null))
    }
}

/// Computes a deterministic salted hash for a join key given a partition pass index.
pub fn hash_join_key(key: &JoinKey, pass: usize) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let seed = 0x9e3779b97f4a7c15u64.wrapping_mul((pass as u64).wrapping_add(1));
    seed.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

/// Computes an accurate estimate of in-memory heap usage for a single row.
pub fn estimate_row_memory(row: &Row) -> usize {
    let mut size =
        std::mem::size_of::<Row>() + row.values.capacity() * std::mem::size_of::<Value>() + 16;
    for val in &row.values {
        match val {
            Value::String(s) => size += s.capacity(),
            Value::Bytes(b) => size += b.capacity(),
            _ => {}
        }
    }
    size
}

/// Computes an estimate of in-memory heap usage for an entry in the build hash table.
pub fn estimate_hash_entry_memory(key: &JoinKey, rows: &[Row]) -> usize {
    let mut size = std::mem::size_of::<JoinKey>()
        + key.0.capacity() * std::mem::size_of::<CanonicalValue>()
        + std::mem::size_of::<Vec<Row>>()
        + std::mem::size_of_val(rows)
        + 48; // HashMap bucket overhead

    for cv in &key.0 {
        match cv {
            CanonicalValue::String(s) => size += s.capacity(),
            CanonicalValue::Bytes(b) => size += b.capacity(),
            _ => {}
        }
    }
    for row in rows {
        size += estimate_row_memory(row);
    }
    size
}

/// An RAII-managed temporary file for external join partition runs that guarantees deletion on `Drop`.
#[derive(Debug)]
pub struct TempJoinFile {
    path: PathBuf,
}

impl TempJoinFile {
    /// Creates and opens a new unique temporary file in the specified directory.
    pub fn create_in(temp_dir: &Path) -> Result<(Self, File)> {
        if !temp_dir.exists() {
            std::fs::create_dir_all(temp_dir).map_err(|e| {
                Error::ExternalJoinIo(format!(
                    "failed to create temporary join directory {}: {e}",
                    temp_dir.display()
                ))
            })?;
        }

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = JOIN_RUN_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let filename = format!("treedb_join_part_{pid}_{nanos}_{counter}.tmp");
        let path = temp_dir.join(filename);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                Error::ExternalJoinIo(format!(
                    "failed to create temporary join partition file {}: {e}",
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

impl Drop for TempJoinFile {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Serializes a single `Value` to a binary writer using little-endian encoding.
fn serialize_value<W: Write>(w: &mut W, val: &Value) -> Result<usize> {
    let mut written = 0;
    match val {
        Value::Null => {
            w.write_all(&[0u8])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 1;
        }
        Value::Boolean(b) => {
            w.write_all(&[if *b { 2u8 } else { 1u8 }])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 1;
        }
        Value::Int64(i) => {
            w.write_all(&[3u8])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            w.write_all(&i.to_le_bytes())
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 9;
        }
        Value::UInt64(u) => {
            w.write_all(&[4u8])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            w.write_all(&u.to_le_bytes())
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 9;
        }
        Value::Float64(f) => {
            w.write_all(&[5u8])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            w.write_all(&f.to_bits().to_le_bytes())
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 9;
        }
        Value::String(s) => {
            w.write_all(&[6u8])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            let len = s.len() as u32;
            w.write_all(&len.to_le_bytes())
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            w.write_all(s.as_bytes())
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 1 + 4 + s.len();
        }
        Value::Bytes(b) => {
            w.write_all(&[7u8])
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            let len = b.len() as u32;
            w.write_all(&len.to_le_bytes())
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            w.write_all(b)
                .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
            written += 1 + 4 + b.len();
        }
    }
    Ok(written)
}

/// Deserializes a single `Value` from a binary reader.
fn deserialize_value<R: Read>(r: &mut R) -> Result<Value> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag)
        .map_err(|e| Error::ExternalJoinIo(format!("failed to read value tag: {e}")))?;

    match tag[0] {
        0 => Ok(Value::Null),
        1 => Ok(Value::Boolean(false)),
        2 => Ok(Value::Boolean(true)),
        3 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read Int64 value: {e}")))?;
            Ok(Value::Int64(i64::from_le_bytes(buf)))
        }
        4 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read UInt64 value: {e}")))?;
            Ok(Value::UInt64(u64::from_le_bytes(buf)))
        }
        5 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read Float64 value: {e}")))?;
            Ok(Value::Float64(f64::from_bits(u64::from_le_bytes(buf))))
        }
        6 => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read String length: {e}")))?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut str_bytes = vec![0u8; len];
            r.read_exact(&mut str_bytes)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read String data: {e}")))?;
            let s = String::from_utf8(str_bytes).map_err(|e| {
                Error::ExternalJoinRunCorrupted(format!("invalid UTF-8 string: {e}"))
            })?;
            Ok(Value::String(s))
        }
        7 => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read Bytes length: {e}")))?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut byte_vec = vec![0u8; len];
            r.read_exact(&mut byte_vec)
                .map_err(|e| Error::ExternalJoinIo(format!("failed to read Bytes data: {e}")))?;
            Ok(Value::Bytes(byte_vec))
        }
        other => Err(Error::ExternalJoinRunCorrupted(format!(
            "unknown value type discriminant: {other}"
        ))),
    }
}

/// Serializes a `Row` to a partition run file.
fn write_row_record<W: Write>(w: &mut W, row: &Row) -> Result<usize> {
    let mut written = 0;
    let val_count = row.values.len() as u16;
    w.write_all(&val_count.to_le_bytes())
        .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
    written += 2;

    for val in &row.values {
        written += serialize_value(w, val)?;
    }
    Ok(written)
}

/// Deserializes a `Row` from a partition run file.
fn read_row_record<R: Read>(r: &mut R) -> Result<Option<Row>> {
    let mut count_buf = [0u8; 2];
    match r.read_exact(&mut count_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => {
            return Err(Error::ExternalJoinIo(format!(
                "failed to read row header: {e}"
            )))
        }
    }

    let val_count = u16::from_le_bytes(count_buf) as usize;
    let mut values = Vec::with_capacity(val_count);
    for _ in 0..val_count {
        values.push(deserialize_value(r)?);
    }

    Ok(Some(Row::new(values)))
}

/// A buffered streaming writer for an individual temporary join partition run.
pub struct PartitionWriter {
    temp_file: TempJoinFile,
    writer: BufWriter<File>,
    record_count: usize,
    bytes_written: usize,
}

impl PartitionWriter {
    /// Creates and initializes a new partition writer with the standard 24-byte header.
    pub fn create(temp_dir: &Path) -> Result<Self> {
        let (temp_file, file) = TempJoinFile::create_in(temp_dir)?;
        let mut writer = BufWriter::with_capacity(64 * 1024, file);

        // Header: Magic (4B), Version (2B), Reserved (2B), Record Count (8B) = 16 bytes
        writer
            .write_all(TEMP_JOIN_MAGIC)
            .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
        writer
            .write_all(&TEMP_JOIN_VERSION.to_le_bytes())
            .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
        writer
            .write_all(&[0u8; 2])
            .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;
        writer
            .write_all(&0u64.to_le_bytes())
            .map_err(|e| Error::ExternalJoinIo(e.to_string()))?;

        Ok(Self {
            temp_file,
            writer,
            record_count: 0,
            bytes_written: 16,
        })
    }

    /// Appends a row record to this partition run file.
    pub fn write_row(&mut self, row: &Row) -> Result<usize> {
        let written = write_row_record(&mut self.writer, row)?;
        self.record_count += 1;
        self.bytes_written += written;
        Ok(written)
    }

    /// Flushes the writer, updates the total record count in the header, and returns the finished partition file.
    pub fn finish(mut self) -> Result<(TempJoinFile, usize, usize)> {
        self.writer
            .flush()
            .map_err(|e| Error::ExternalJoinIo(format!("failed to flush partition writer: {e}")))?;

        let mut file = self.writer.into_inner().map_err(|e| {
            Error::ExternalJoinIo(format!("failed to finalize partition writer: {e}"))
        })?;

        file.seek(SeekFrom::Start(8)).map_err(|e| {
            Error::ExternalJoinIo(format!("failed to seek partition record count: {e}"))
        })?;
        file.write_all(&(self.record_count as u64).to_le_bytes())
            .map_err(|e| {
                Error::ExternalJoinIo(format!("failed to write partition record count: {e}"))
            })?;
        file.flush()
            .map_err(|e| Error::ExternalJoinIo(format!("failed to sync partition file: {e}")))?;

        Ok((self.temp_file, self.record_count, self.bytes_written))
    }
}

/// Reads all rows from a temporary join partition file, validating header and record count.
pub fn read_partition_rows(part_file: &TempJoinFile) -> Result<Vec<Row>> {
    let file = File::open(part_file.path()).map_err(|e| {
        Error::ExternalJoinIo(format!(
            "failed to open join partition file {}: {e}",
            part_file.path().display()
        ))
    })?;

    let mut reader = BufReader::with_capacity(64 * 1024, file);

    // Read and validate 16-byte header
    let mut magic_buf = [0u8; 4];
    reader
        .read_exact(&mut magic_buf)
        .map_err(|e| Error::ExternalJoinRunCorrupted(format!("missing join header magic: {e}")))?;
    if &magic_buf != TEMP_JOIN_MAGIC {
        return Err(Error::ExternalJoinRunCorrupted(format!(
            "invalid join partition magic bytes: expected {:?}, got {:?}",
            TEMP_JOIN_MAGIC, magic_buf
        )));
    }

    let mut version_buf = [0u8; 2];
    reader.read_exact(&mut version_buf).map_err(|e| {
        Error::ExternalJoinRunCorrupted(format!("missing join header version: {e}"))
    })?;
    let version = u16::from_le_bytes(version_buf);
    if version != TEMP_JOIN_VERSION {
        return Err(Error::ExternalJoinRunCorrupted(format!(
            "unsupported join partition version: expected {}, got {}",
            TEMP_JOIN_VERSION, version
        )));
    }

    let mut reserved = [0u8; 2];
    reader
        .read_exact(&mut reserved)
        .map_err(|e| Error::ExternalJoinRunCorrupted(format!("missing header padding: {e}")))?;

    let mut count_buf = [0u8; 8];
    reader.read_exact(&mut count_buf).map_err(|e| {
        Error::ExternalJoinRunCorrupted(format!("missing header record count: {e}"))
    })?;
    let expected_records = u64::from_le_bytes(count_buf) as usize;

    let mut rows = Vec::with_capacity(expected_records);
    let mut read_count = 0;

    while let Some(row) = read_row_record(&mut reader)? {
        rows.push(row);
        read_count += 1;
    }

    if read_count != expected_records {
        return Err(Error::ExternalJoinRunCorrupted(format!(
            "join partition record count mismatch: expected {expected_records}, got {read_count}"
        )));
    }

    Ok(rows)
}

/// Checks if an expression can be evaluated purely against the given `Schema`.
pub fn expr_can_evaluate_on(expr: &Expr, schema: &Schema) -> bool {
    match expr {
        Expr::Literal(_) | Expr::Parameter { .. } => true,
        Expr::Column(name) => resolve_column_in_schema(schema, name).is_some(),
        Expr::OuterColumn { .. } => false,
        Expr::Binary { left, right, .. } => {
            expr_can_evaluate_on(left, schema) && expr_can_evaluate_on(right, schema)
        }
        Expr::Unary { expr, .. } => expr_can_evaluate_on(expr, schema),
        Expr::IsNull(expr) | Expr::IsNotNull(expr) => expr_can_evaluate_on(expr, schema),
        Expr::InList { expr, list, .. } => {
            expr_can_evaluate_on(expr, schema)
                && list.iter().all(|e| expr_can_evaluate_on(e, schema))
        }
        Expr::Subquery(_) => false,
    }
}

/// Analyzed equi-join specification deconstructed from a JOIN ON condition.
#[derive(Debug, Clone, PartialEq)]
pub struct EquiJoinPlan {
    /// Left-hand side equi-join key expressions.
    pub left_keys: Vec<Expr>,
    /// Right-hand side equi-join key expressions.
    pub right_keys: Vec<Expr>,
    /// Residual filter predicate to evaluate on candidate joined rows.
    pub residual_predicate: Option<Expr>,
}

/// Analyzes a JOIN `on` predicate to determine whether it contains eligible equi-join conjuncts.
///
/// Returns `Some(EquiJoinPlan)` if at least one equality predicate can be extracted,
/// or `None` if the join must fall back to nested-loop evaluation.
pub fn analyze_equi_join(
    on: &Expr,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Option<EquiJoinPlan> {
    let conjuncts = split_conjunction(on);
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    let mut residual_conjuncts = Vec::new();

    for conj in conjuncts {
        if let Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
        } = &conj
        {
            if expr_can_evaluate_on(left, left_schema) && expr_can_evaluate_on(right, right_schema)
            {
                left_keys.push((**left).clone());
                right_keys.push((**right).clone());
                continue;
            } else if expr_can_evaluate_on(left, right_schema)
                && expr_can_evaluate_on(right, left_schema)
            {
                left_keys.push((**right).clone());
                right_keys.push((**left).clone());
                continue;
            }
        }
        residual_conjuncts.push(conj);
    }

    if left_keys.is_empty() {
        return None;
    }

    let residual_predicate = recombine_conjunction(residual_conjuncts);
    Some(EquiJoinPlan {
        left_keys,
        right_keys,
        residual_predicate,
    })
}

/// Constructs the output `Schema` for a relational join between `left_schema` and `right_schema`.
pub fn create_joined_schema(
    left_schema: &Schema,
    right_schema: &Schema,
    join_type: JoinType,
) -> Schema {
    if join_type == JoinType::Semi || join_type == JoinType::Anti {
        return left_schema.clone();
    }
    let mut combined_columns =
        Vec::with_capacity(left_schema.columns.len() + right_schema.columns.len());
    for col in &left_schema.columns {
        let mut col_copy = col.clone();
        if join_type == JoinType::Right || join_type == JoinType::Full {
            col_copy.nullable = true;
        }
        combined_columns.push(col_copy);
    }
    for col in &right_schema.columns {
        let mut col_copy = col.clone();
        if join_type == JoinType::Left || join_type == JoinType::Full {
            col_copy.nullable = true;
        }
        combined_columns.push(col_copy);
    }
    Schema::new_projected(combined_columns)
}

/// External hash joiner coordinating in-memory and spill-to-disk execution of equi-joins.
pub struct ExternalHashJoiner<'a> {
    left_schema: &'a Schema,
    right_schema: &'a Schema,
    join_type: JoinType,
    left_keys: Vec<Expr>,
    right_keys: Vec<Expr>,
    residual_predicate: Option<Expr>,
    config: JoinConfig,
}

impl<'a> ExternalHashJoiner<'a> {
    /// Creates a new `ExternalHashJoiner`.
    pub fn new(
        left_schema: &'a Schema,
        right_schema: &'a Schema,
        join_type: JoinType,
        equi_plan: EquiJoinPlan,
        config: JoinConfig,
    ) -> Self {
        Self {
            left_schema,
            right_schema,
            join_type,
            left_keys: equi_plan.left_keys,
            right_keys: equi_plan.right_keys,
            residual_predicate: equi_plan.residual_predicate,
            config,
        }
    }

    /// Executes the join between `left_rows` and `right_rows`.
    pub fn execute(
        &self,
        left_rows: Vec<Row>,
        right_rows: Vec<Row>,
    ) -> Result<(Schema, Vec<Row>, JoinExecutionMetrics)> {
        let joined_schema =
            create_joined_schema(self.left_schema, self.right_schema, self.join_type);

        let mut metrics = JoinExecutionMetrics {
            rows_left: left_rows.len(),
            rows_right: right_rows.len(),
            ..Default::default()
        };

        if left_rows.is_empty() {
            if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
                let left_nulls = vec![Value::Null; self.left_schema.columns.len()];
                let mut out_rows = Vec::with_capacity(right_rows.len());
                for r_row in right_rows {
                    let mut vals = Vec::with_capacity(left_nulls.len() + r_row.values.len());
                    vals.extend_from_slice(&left_nulls);
                    vals.extend_from_slice(&r_row.values);
                    out_rows.push(Row::new(vals));
                }
                metrics.rows_output = out_rows.len();
                return Ok((joined_schema, out_rows, metrics));
            } else {
                return Ok((joined_schema, Vec::new(), metrics));
            }
        }

        if right_rows.is_empty() {
            if self.join_type == JoinType::Anti {
                metrics.rows_output = left_rows.len();
                metrics.unmatched_outer_rows = left_rows.len();
                return Ok((joined_schema, left_rows, metrics));
            } else if self.join_type == JoinType::Semi {
                metrics.unmatched_outer_rows = left_rows.len();
                return Ok((joined_schema, Vec::new(), metrics));
            } else if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                let right_nulls = vec![Value::Null; self.right_schema.columns.len()];
                let mut out_rows = Vec::with_capacity(left_rows.len());
                for l_row in left_rows {
                    let mut vals = Vec::with_capacity(l_row.values.len() + right_nulls.len());
                    vals.extend_from_slice(&l_row.values);
                    vals.extend_from_slice(&right_nulls);
                    out_rows.push(Row::new(vals));
                }
                metrics.rows_output = out_rows.len();
                return Ok((joined_schema, out_rows, metrics));
            } else {
                return Ok((joined_schema, Vec::new(), metrics));
            }
        }

        // Estimate memory required to build the right-side hash table
        let mut estimated_build_mem = 0;
        for row in &right_rows {
            estimated_build_mem += estimate_row_memory(row) + 64;
        }

        // In-memory fast path: Build side fits in memory
        if estimated_build_mem < self.config.memory_budget_bytes {
            let (out_rows, peak_mem) = self.join_in_memory(
                &left_rows,
                &right_rows,
                &joined_schema,
                crate::query::runtime_filter::RuntimeFilterScope::Global,
                &mut metrics,
            )?;
            metrics.rows_output = out_rows.len();
            metrics.peak_memory_bytes = peak_mem;
            return Ok((joined_schema, out_rows, metrics));
        }

        // External partition and probe path
        metrics.partition_passes = 1;
        let (out_rows, peak_mem) =
            self.partition_and_join(left_rows, right_rows, &joined_schema, 0, &mut metrics)?;

        metrics.rows_output = out_rows.len();
        metrics.peak_memory_bytes = peak_mem;

        Ok((joined_schema, out_rows, metrics))
    }

    /// Executes an in-memory hash join between `left_rows` and `right_rows` (Right as build, Left as probe).
    fn join_in_memory(
        &self,
        left_rows: &[Row],
        right_rows: &[Row],
        joined_schema: &Schema,
        scope: crate::query::runtime_filter::RuntimeFilterScope,
        metrics: &mut JoinExecutionMetrics,
    ) -> Result<(Vec<Row>, usize)> {
        let mut build_map: HashMap<JoinKey, Vec<(usize, Row)>> = HashMap::new();
        let mut build_mem = 0;
        let mut peak_mem = 0;
        let mut right_matched = vec![false; right_rows.len()];

        let mut runtime_filter = if self.config.enable_runtime_filter
            && (self.join_type == JoinType::Inner || self.join_type == JoinType::Semi)
        {
            metrics.runtime_filter_enabled = true;
            metrics.runtime_filter_type = Some(self.config.runtime_filter_type.to_string());
            match self.config.runtime_filter_type {
                crate::query::runtime_filter::RuntimeFilterType::ExactSet => {
                    Some(crate::query::runtime_filter::RuntimeFilter::exact(scope))
                }
                crate::query::runtime_filter::RuntimeFilterType::Bloom => Some(
                    crate::query::runtime_filter::RuntimeFilter::bloom(right_rows.len(), scope),
                ),
            }
        } else {
            None
        };

        for (r_idx, r_row) in right_rows.iter().enumerate() {
            let key = JoinKey::from_row(r_row, self.right_schema, &self.right_keys)?;
            // SQL NULL semantics: NULL keys cannot match in equi-join
            if key.has_null() {
                continue;
            }

            metrics.hash_build_rows += 1;
            if let Some(rf) = &mut runtime_filter {
                metrics.runtime_filter_build_rows += 1;
                if rf.insert(key.clone()) {
                    metrics.runtime_filter_distinct_keys += 1;
                }
            }

            let row_mem = estimate_row_memory(r_row);
            build_mem += row_mem;
            if build_mem > peak_mem {
                peak_mem = build_mem;
            }

            build_map
                .entry(key)
                .or_default()
                .push((r_idx, r_row.clone()));
        }

        if let Some(rf) = &runtime_filter {
            metrics.runtime_filter_memory_bytes += rf.memory_bytes();
        }

        let left_nulls = vec![Value::Null; self.left_schema.columns.len()];
        let right_nulls = vec![Value::Null; self.right_schema.columns.len()];
        let mut out_rows = Vec::new();

        for l_row in left_rows {
            let key = JoinKey::from_row(l_row, self.left_schema, &self.left_keys)?;
            if key.has_null() {
                if runtime_filter.is_some() {
                    metrics.runtime_filter_null_probe_rows += 1;
                }
                if self.join_type == JoinType::Anti {
                    metrics.unmatched_outer_rows += 1;
                    out_rows.push(l_row.clone());
                } else if self.join_type == JoinType::Semi {
                    metrics.unmatched_outer_rows += 1;
                } else if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                    let mut combined = Vec::with_capacity(l_row.values.len() + right_nulls.len());
                    combined.extend_from_slice(&l_row.values);
                    combined.extend_from_slice(&right_nulls);
                    out_rows.push(Row::new(combined));
                }
                continue;
            }

            if let Some(rf) = &runtime_filter {
                metrics.runtime_filter_probe_rows += 1;
                if !rf.contains(&key) {
                    metrics.runtime_filter_rejected_rows += 1;
                    metrics.unmatched_outer_rows += 1;
                    continue;
                }
                metrics.runtime_filter_passed_rows += 1;
            }

            metrics.hash_probe_rows += 1;
            let mut left_matched = false;
            if let Some(r_matches) = build_map.get(&key) {
                metrics.candidate_pairs += r_matches.len();
                for (r_idx, r_row) in r_matches {
                    let mut combined = Vec::with_capacity(l_row.values.len() + r_row.values.len());
                    combined.extend_from_slice(&l_row.values);
                    combined.extend_from_slice(&r_row.values);
                    let candidate = Row::new(combined);

                    let passed = if let Some(res_expr) = &self.residual_predicate {
                        let eval_schema = if self.join_type == JoinType::Semi
                            || self.join_type == JoinType::Anti
                        {
                            let mut combined_columns = Vec::with_capacity(
                                self.left_schema.columns.len() + self.right_schema.columns.len(),
                            );
                            combined_columns.extend_from_slice(&self.left_schema.columns);
                            combined_columns.extend_from_slice(&self.right_schema.columns);
                            Schema::new_projected(combined_columns)
                        } else {
                            joined_schema.clone()
                        };
                        let res_val = res_expr.evaluate(&candidate, &eval_schema)?;
                        res_val == Value::Boolean(true)
                    } else {
                        true
                    };

                    if passed {
                        left_matched = true;
                        right_matched[*r_idx] = true;
                        metrics.matched_rows += 1;
                        if self.join_type != JoinType::Semi && self.join_type != JoinType::Anti {
                            out_rows.push(candidate);
                        } else {
                            break;
                        }
                    }
                }
            } else if let Some(rf) = &runtime_filter {
                if rf.filter_type() == crate::query::runtime_filter::RuntimeFilterType::Bloom {
                    metrics.runtime_filter_false_positive_rows += 1;
                }
            }

            if left_matched {
                metrics.matched_outer_rows += 1;
            } else {
                metrics.unmatched_outer_rows += 1;
            }

            if self.join_type == JoinType::Semi {
                if left_matched {
                    out_rows.push(l_row.clone());
                }
            } else if self.join_type == JoinType::Anti {
                if !left_matched {
                    out_rows.push(l_row.clone());
                }
            } else if !left_matched
                && (self.join_type == JoinType::Left || self.join_type == JoinType::Full)
            {
                let mut combined = Vec::with_capacity(l_row.values.len() + right_nulls.len());
                combined.extend_from_slice(&l_row.values);
                combined.extend_from_slice(&right_nulls);
                out_rows.push(Row::new(combined));
            }
        }

        if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
            for (r_idx, r_row) in right_rows.iter().enumerate() {
                if !right_matched[r_idx] {
                    let mut combined = Vec::with_capacity(left_nulls.len() + r_row.values.len());
                    combined.extend_from_slice(&left_nulls);
                    combined.extend_from_slice(&r_row.values);
                    out_rows.push(Row::new(combined));
                }
            }
        }

        Ok((out_rows, peak_mem))
    }

    /// Partitions left and right relations into temporary files and joins matching partition pairs.
    fn partition_and_join(
        &self,
        left_rows: Vec<Row>,
        right_rows: Vec<Row>,
        joined_schema: &Schema,
        pass: usize,
        metrics: &mut JoinExecutionMetrics,
    ) -> Result<(Vec<Row>, usize)> {
        if pass >= self.config.max_partition_passes {
            return Err(Error::ExternalJoinMaxPassesExceeded {
                passes: pass,
                limit: self.config.max_partition_passes,
            });
        }

        let num_partitions = self.config.max_temp_files.clamp(2, 32);
        let temp_dir = self.config.effective_temp_dir();

        let mut left_writers: Vec<Option<PartitionWriter>> =
            (0..num_partitions).map(|_| None).collect();
        let mut right_writers: Vec<Option<PartitionWriter>> =
            (0..num_partitions).map(|_| None).collect();

        // 1. Partition Left rows
        for l_row in left_rows {
            let key = JoinKey::from_row(&l_row, self.left_schema, &self.left_keys)?;
            let part_idx = if key.has_null() {
                // If key has NULL, place in partition 0 for LEFT/FULL JOIN preservation;
                // for INNER/RIGHT JOIN it won't match any right partition row.
                0
            } else {
                (hash_join_key(&key, pass) as usize) % num_partitions
            };

            if left_writers[part_idx].is_none() {
                let pw = PartitionWriter::create(&temp_dir)?;
                metrics.runs_created += 1;
                left_writers[part_idx] = Some(pw);
            }
            let pw = left_writers[part_idx].as_mut().unwrap();
            let written = pw.write_row(&l_row)?;
            metrics.bytes_spilled += written;

            if metrics.bytes_spilled > self.config.max_temp_bytes {
                return Err(Error::ExternalJoinQuotaExceeded {
                    bytes_spilled: metrics.bytes_spilled,
                    limit: self.config.max_temp_bytes,
                });
            }
        }

        // 2. Partition Right rows
        for r_row in right_rows {
            let key = JoinKey::from_row(&r_row, self.right_schema, &self.right_keys)?;
            let part_idx = if key.has_null() {
                // For RIGHT/FULL JOIN, place in partition 0 for unmatched right preservation;
                // for INNER/LEFT JOIN it won't match any left row.
                0
            } else {
                (hash_join_key(&key, pass) as usize) % num_partitions
            };

            if right_writers[part_idx].is_none() {
                let pw = PartitionWriter::create(&temp_dir)?;
                metrics.runs_created += 1;
                right_writers[part_idx] = Some(pw);
            }
            let pw = right_writers[part_idx].as_mut().unwrap();
            let written = pw.write_row(&r_row)?;
            metrics.bytes_spilled += written;

            if metrics.bytes_spilled > self.config.max_temp_bytes {
                return Err(Error::ExternalJoinQuotaExceeded {
                    bytes_spilled: metrics.bytes_spilled,
                    limit: self.config.max_temp_bytes,
                });
            }
        }

        // 3. Finalize partition files
        let mut left_files: Vec<Option<TempJoinFile>> = Vec::with_capacity(num_partitions);
        for lw_opt in left_writers {
            if let Some(pw) = lw_opt {
                let (file, count, _) = pw.finish()?;
                if count > 0 {
                    left_files.push(Some(file));
                } else {
                    left_files.push(None);
                }
            } else {
                left_files.push(None);
            }
        }

        let mut right_files: Vec<Option<TempJoinFile>> = Vec::with_capacity(num_partitions);
        for rw_opt in right_writers {
            if let Some(pw) = rw_opt {
                let (file, count, _) = pw.finish()?;
                if count > 0 {
                    right_files.push(Some(file));
                } else {
                    right_files.push(None);
                }
            } else {
                right_files.push(None);
            }
        }

        let mut active_partitions = 0;
        for i in 0..num_partitions {
            if left_files[i].is_some() || right_files[i].is_some() {
                active_partitions += 1;
            }
        }
        if active_partitions > metrics.spill_partitions {
            metrics.spill_partitions = active_partitions;
        }

        let mut out_rows = Vec::new();
        let mut peak_mem = 0;

        // 4. Process matching partition pairs
        for i in 0..num_partitions {
            let left_part = match &left_files[i] {
                Some(f) => read_partition_rows(f)?,
                None => Vec::new(),
            };

            let right_part = match &right_files[i] {
                Some(f) => read_partition_rows(f)?,
                None => Vec::new(),
            };

            if left_part.is_empty() && right_part.is_empty() {
                continue;
            }

            if left_part.is_empty() {
                if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
                    let left_nulls = vec![Value::Null; self.left_schema.columns.len()];
                    for r_row in right_part {
                        let mut combined =
                            Vec::with_capacity(left_nulls.len() + r_row.values.len());
                        combined.extend_from_slice(&left_nulls);
                        combined.extend_from_slice(&r_row.values);
                        out_rows.push(Row::new(combined));
                    }
                }
                continue;
            }

            if right_part.is_empty() {
                if self.join_type == JoinType::Anti {
                    out_rows.extend(left_part);
                } else if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                    let right_nulls = vec![Value::Null; self.right_schema.columns.len()];
                    for l_row in left_part {
                        let mut combined =
                            Vec::with_capacity(l_row.values.len() + right_nulls.len());
                        combined.extend_from_slice(&l_row.values);
                        combined.extend_from_slice(&right_nulls);
                        out_rows.push(Row::new(combined));
                    }
                }
                continue;
            }

            // Estimate memory of this partition's right-side hash table
            let mut part_build_mem = 0;
            for r_row in &right_part {
                part_build_mem += estimate_row_memory(r_row) + 64;
            }

            if part_build_mem < self.config.memory_budget_bytes {
                let (part_out, p_mem) = self.join_in_memory(
                    &left_part,
                    &right_part,
                    joined_schema,
                    crate::query::runtime_filter::RuntimeFilterScope::Partition(i as u32),
                    metrics,
                )?;
                if p_mem > peak_mem {
                    peak_mem = p_mem;
                }
                out_rows.extend(part_out);
            } else {
                // Check if all keys in this right partition are identical (skew/duplicate keys)
                let first_key =
                    JoinKey::from_row(&right_part[0], self.right_schema, &self.right_keys)?;
                let all_same = right_part.iter().all(|r| {
                    JoinKey::from_row(r, self.right_schema, &self.right_keys)
                        .map(|k| k == first_key)
                        .unwrap_or(false)
                });

                if all_same {
                    // All keys are identical; further partitioning won't split them.
                    // Join in-memory with atomic memory accommodation for this single skewed group.
                    let (part_out, p_mem) = self.join_in_memory(
                        &left_part,
                        &right_part,
                        joined_schema,
                        crate::query::runtime_filter::RuntimeFilterScope::Partition(i as u32),
                        metrics,
                    )?;
                    if p_mem > peak_mem {
                        peak_mem = p_mem;
                    }
                    out_rows.extend(part_out);
                } else {
                    // Recursively re-partition oversized partition pair
                    if pass + 1 >= metrics.partition_passes {
                        metrics.partition_passes = pass + 2;
                    }
                    let (sub_out, sub_peak) = self.partition_and_join(
                        left_part,
                        right_part,
                        joined_schema,
                        pass + 1,
                        metrics,
                    )?;
                    if sub_peak > peak_mem {
                        peak_mem = sub_peak;
                    }
                    out_rows.extend(sub_out);
                }
            }
        }

        Ok((out_rows, peak_mem))
    }
}
