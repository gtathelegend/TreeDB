use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::engine::types::{Column, Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, Expr};
use crate::query::operator::{AggregateState, CanonicalValue, GroupKey};

/// Default in-memory aggregation budget before spilling: 64 KB.
pub const DEFAULT_AGGREGATE_MEMORY_BUDGET_BYTES: usize = 64 * 1024;

/// Default maximum number of temporary partition files open concurrently during aggregation.
pub const DEFAULT_MAX_TEMP_FILES: usize = 32;

/// Default maximum total disk space permitted for temporary aggregation runs: 1 GB.
pub const DEFAULT_MAX_TEMP_BYTES: usize = 1024 * 1024 * 1024;

/// Default maximum merge/partition passes before aborting: 100 passes.
pub const DEFAULT_MAX_MERGE_PASSES: usize = 100;

/// Magic bytes identifying a TreeDB temporary aggregation run file: `b"TAGR"`.
pub const TEMP_AGG_MAGIC: &[u8; 4] = b"TAGR";

/// Version identifier for the temporary aggregation run format.
pub const TEMP_AGG_VERSION: u16 = 1;

static AGG_RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Runtime configuration parameters controlling external hash aggregation and spill-to-disk execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateConfig {
    /// In-memory aggregation buffer budget in bytes before spilling to disk.
    pub memory_budget_bytes: usize,
    /// Maximum number of temporary partition files open simultaneously.
    pub max_temp_files: usize,
    /// Maximum disk space in bytes that can be consumed by temporary runs.
    pub max_temp_bytes: usize,
    /// Maximum number of merge/re-partitioning passes before returning an error.
    pub max_merge_passes: usize,
    /// Custom directory for temporary run files. If `None`, `std::env::temp_dir()` is used.
    pub temp_dir: Option<PathBuf>,
}

impl Default for AggregateConfig {
    fn default() -> Self {
        Self {
            memory_budget_bytes: DEFAULT_AGGREGATE_MEMORY_BUDGET_BYTES,
            max_temp_files: DEFAULT_MAX_TEMP_FILES,
            max_temp_bytes: DEFAULT_MAX_TEMP_BYTES,
            max_merge_passes: DEFAULT_MAX_MERGE_PASSES,
            temp_dir: None,
        }
    }
}

impl AggregateConfig {
    /// Creates a new `AggregateConfig` with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the in-memory aggregation buffer budget in bytes.
    pub fn with_memory_budget_bytes(mut self, budget: usize) -> Self {
        self.memory_budget_bytes = budget.max(1);
        self
    }

    /// Sets the maximum number of temporary partition files open simultaneously.
    pub fn with_max_temp_files(mut self, max_files: usize) -> Self {
        self.max_temp_files = max_files.max(2);
        self
    }

    /// Sets the maximum allowable disk space in bytes for temporary aggregation runs.
    pub fn with_max_temp_bytes(mut self, max_bytes: usize) -> Self {
        self.max_temp_bytes = max_bytes;
        self
    }

    /// Sets the maximum number of merge/partition passes allowed.
    pub fn with_max_merge_passes(mut self, max_passes: usize) -> Self {
        self.max_merge_passes = max_passes.max(1);
        self
    }

    /// Sets the directory path for temporary aggregation run files.
    pub fn with_temp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(path.into());
        self
    }

    /// Returns the effective temporary directory path.
    pub fn effective_temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

/// Execution telemetry and statistics for an aggregation operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AggregateExecutionMetrics {
    /// Total number of rows consumed by the aggregation operator.
    pub rows_input: usize,
    /// Total number of rows emitted by the aggregation operator.
    pub rows_output: usize,
    /// Total distinct group keys created.
    pub groups_created: usize,
    /// Number of distinct spill partitions used during external aggregation.
    pub spill_partitions: usize,
    /// Number of temporary partition run files created on disk.
    pub runs_created: usize,
    /// Total bytes written to disk across all temporary aggregation runs.
    pub bytes_spilled: usize,
    /// Total merge/partition passes executed (0 for in-memory, >= 1 for spilled aggregation).
    pub merge_passes: usize,
    /// Peak in-memory byte footprint estimated during aggregation.
    pub peak_memory_bytes: usize,
}

/// Computes an accurate estimate of in-memory heap usage for an aggregation group entry.
pub fn estimate_group_entry_memory(
    key: &GroupKey,
    group_values: &[Value],
    states: &[AggregateState],
) -> usize {
    let mut size = std::mem::size_of::<GroupKey>()
        + key.0.capacity() * std::mem::size_of::<CanonicalValue>()
        + std::mem::size_of::<(Vec<Value>, Vec<AggregateState>)>()
        + std::mem::size_of_val(group_values)
        + std::mem::size_of_val(states)
        + 48; // HashMap node/bucket overhead

    for cv in &key.0 {
        match cv {
            CanonicalValue::String(s) => size += s.capacity(),
            CanonicalValue::Bytes(b) => size += b.capacity(),
            _ => {}
        }
    }
    for val in group_values {
        match val {
            Value::String(s) => size += s.capacity(),
            Value::Bytes(b) => size += b.capacity(),
            _ => {}
        }
    }
    for st in states {
        match st {
            AggregateState::Min {
                value: Some(Value::String(s)),
                ..
            }
            | AggregateState::Max {
                value: Some(Value::String(s)),
                ..
            } => size += s.capacity(),
            AggregateState::Min {
                value: Some(Value::Bytes(b)),
                ..
            }
            | AggregateState::Max {
                value: Some(Value::Bytes(b)),
                ..
            } => size += b.capacity(),
            _ => {}
        }
    }
    size
}

/// An RAII-managed temporary file for external aggregation runs that guarantees deletion on `Drop`.
#[derive(Debug)]
pub struct TempAggregateFile {
    path: PathBuf,
}

impl TempAggregateFile {
    /// Creates and opens a new unique temporary file in the specified directory.
    pub fn create_in(temp_dir: &Path) -> Result<(Self, File)> {
        if !temp_dir.exists() {
            std::fs::create_dir_all(temp_dir).map_err(|e| {
                Error::ExternalAggregateIo(format!(
                    "failed to create temporary aggregation directory {}: {e}",
                    temp_dir.display()
                ))
            })?;
        }

        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = AGG_RUN_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let filename = format!("treedb_agg_run_{pid}_{nanos}_{counter}.tmp");
        let path = temp_dir.join(filename);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                Error::ExternalAggregateIo(format!(
                    "failed to create temporary aggregation run file {}: {e}",
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

impl Drop for TempAggregateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Hashes a `GroupKey` deterministically with a given pass seed.
pub fn hash_group_key(key: &GroupKey, seed: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
}

/// Serializes a single `Value` to a binary writer using little-endian encoding.
fn serialize_value<W: Write>(w: &mut W, val: &Value) -> Result<usize> {
    let mut written = 0;
    match val {
        Value::Null => {
            w.write_all(&[0u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1;
        }
        Value::Boolean(b) => {
            w.write_all(&[if *b { 2u8 } else { 1u8 }])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1;
        }
        Value::Int64(i) => {
            w.write_all(&[3u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&i.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 9;
        }
        Value::UInt64(u) => {
            w.write_all(&[4u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&u.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 9;
        }
        Value::Float64(f) => {
            w.write_all(&[5u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&f.to_bits().to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 9;
        }
        Value::String(s) => {
            w.write_all(&[6u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            let len = s.len() as u32;
            w.write_all(&len.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(s.as_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1 + 4 + s.len();
        }
        Value::Bytes(b) => {
            w.write_all(&[7u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            let len = b.len() as u32;
            w.write_all(&len.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(b)
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1 + 4 + b.len();
        }
    }
    Ok(written)
}

/// Deserializes a single `Value` from a binary reader.
fn deserialize_value<R: Read>(r: &mut R) -> Result<Value> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::ExternalAggregateRunCorrupted("unexpected EOF reading value tag".to_string())
        } else {
            Error::ExternalAggregateIo(e.to_string())
        }
    })?;

    match tag[0] {
        0 => Ok(Value::Null),
        1 => Ok(Value::Boolean(false)),
        2 => Ok(Value::Boolean(true)),
        3 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(Value::Int64(i64::from_le_bytes(buf)))
        }
        4 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(Value::UInt64(u64::from_le_bytes(buf)))
        }
        5 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(Value::Float64(f64::from_bits(u64::from_le_bytes(buf))))
        }
        6 => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut str_buf = vec![0u8; len];
            r.read_exact(&mut str_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let s = String::from_utf8(str_buf).map_err(|_| {
                Error::ExternalAggregateRunCorrupted("invalid UTF-8 in string value".to_string())
            })?;
            Ok(Value::String(s))
        }
        7 => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut bytes_buf = vec![0u8; len];
            r.read_exact(&mut bytes_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(Value::Bytes(bytes_buf))
        }
        other => Err(Error::ExternalAggregateRunCorrupted(format!(
            "invalid value type tag: {other}"
        ))),
    }
}

/// Serializes a single `AggregateState` accumulator to a binary writer.
fn serialize_aggregate_state<W: Write>(w: &mut W, state: &AggregateState) -> Result<usize> {
    let mut written = 0;
    match state {
        AggregateState::CountAll { count } => {
            w.write_all(&[0u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&count.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1 + 8;
        }
        AggregateState::Count { count, .. } => {
            w.write_all(&[1u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&count.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1 + 8;
        }
        AggregateState::SumInt64 { sum, seen, .. } => {
            w.write_all(&[2u8, if *seen { 1u8 } else { 0u8 }])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&sum.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 2 + 8;
        }
        AggregateState::SumUInt64 { sum, seen, .. } => {
            w.write_all(&[3u8, if *seen { 1u8 } else { 0u8 }])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&sum.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 2 + 8;
        }
        AggregateState::SumFloat64 { sum, seen, .. } => {
            w.write_all(&[4u8, if *seen { 1u8 } else { 0u8 }])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&sum.to_bits().to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 2 + 8;
        }
        AggregateState::Avg { sum, count, .. } => {
            w.write_all(&[5u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&sum.to_bits().to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            w.write_all(&count.to_le_bytes())
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1 + 8 + 8;
        }
        AggregateState::Min { value, .. } => {
            w.write_all(&[6u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1;
            match value {
                Some(val) => {
                    w.write_all(&[1u8])
                        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
                    written += 1;
                    written += serialize_value(w, val)?;
                }
                None => {
                    w.write_all(&[0u8])
                        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
                    written += 1;
                }
            }
        }
        AggregateState::Max { value, .. } => {
            w.write_all(&[7u8])
                .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
            written += 1;
            match value {
                Some(val) => {
                    w.write_all(&[1u8])
                        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
                    written += 1;
                    written += serialize_value(w, val)?;
                }
                None => {
                    w.write_all(&[0u8])
                        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
                    written += 1;
                }
            }
        }
    }
    Ok(written)
}

/// Deserializes a single `AggregateState` accumulator given its original `AggregateExpr`.
fn deserialize_aggregate_state<R: Read>(
    r: &mut R,
    agg_expr: &AggregateExpr,
) -> Result<AggregateState> {
    let mut tag = [0u8; 1];
    r.read_exact(&mut tag).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Error::ExternalAggregateRunCorrupted(
                "unexpected EOF reading aggregate state tag".to_string(),
            )
        } else {
            Error::ExternalAggregateIo(e.to_string())
        }
    })?;

    let expr = match agg_expr {
        AggregateExpr::CountAll => Expr::Literal(Value::Int64(1)),
        AggregateExpr::Function { expr, .. } => expr.clone(),
    };

    match tag[0] {
        0 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(AggregateState::CountAll {
                count: u64::from_le_bytes(buf),
            })
        }
        1 => {
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(AggregateState::Count {
                count: u64::from_le_bytes(buf),
                expr,
            })
        }
        2 => {
            let mut seen_buf = [0u8; 1];
            r.read_exact(&mut seen_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let mut sum_buf = [0u8; 8];
            r.read_exact(&mut sum_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(AggregateState::SumInt64 {
                seen: seen_buf[0] != 0,
                sum: i64::from_le_bytes(sum_buf),
                expr,
            })
        }
        3 => {
            let mut seen_buf = [0u8; 1];
            r.read_exact(&mut seen_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let mut sum_buf = [0u8; 8];
            r.read_exact(&mut sum_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(AggregateState::SumUInt64 {
                seen: seen_buf[0] != 0,
                sum: u64::from_le_bytes(sum_buf),
                expr,
            })
        }
        4 => {
            let mut seen_buf = [0u8; 1];
            r.read_exact(&mut seen_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let mut sum_buf = [0u8; 8];
            r.read_exact(&mut sum_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(AggregateState::SumFloat64 {
                seen: seen_buf[0] != 0,
                sum: f64::from_bits(u64::from_le_bytes(sum_buf)),
                expr,
            })
        }
        5 => {
            let mut sum_buf = [0u8; 8];
            r.read_exact(&mut sum_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let mut count_buf = [0u8; 8];
            r.read_exact(&mut count_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            Ok(AggregateState::Avg {
                sum: f64::from_bits(u64::from_le_bytes(sum_buf)),
                count: u64::from_le_bytes(count_buf),
                expr,
            })
        }
        6 => {
            let mut has_val_buf = [0u8; 1];
            r.read_exact(&mut has_val_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let value = if has_val_buf[0] != 0 {
                Some(deserialize_value(r)?)
            } else {
                None
            };
            Ok(AggregateState::Min { value, expr })
        }
        7 => {
            let mut has_val_buf = [0u8; 1];
            r.read_exact(&mut has_val_buf)
                .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
            let value = if has_val_buf[0] != 0 {
                Some(deserialize_value(r)?)
            } else {
                None
            };
            Ok(AggregateState::Max { value, expr })
        }
        other => Err(Error::ExternalAggregateRunCorrupted(format!(
            "invalid aggregate state discriminant: {other}"
        ))),
    }
}

/// Writes a group record `(group_values, states)` to a writer.
fn write_group_record<W: Write>(
    w: &mut W,
    group_values: &[Value],
    states: &[AggregateState],
) -> Result<usize> {
    let mut written = 0;
    let num_group_vals = group_values.len() as u32;
    w.write_all(&num_group_vals.to_le_bytes())
        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
    written += 4;
    for val in group_values {
        written += serialize_value(w, val)?;
    }

    let num_states = states.len() as u32;
    w.write_all(&num_states.to_le_bytes())
        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
    written += 4;
    for st in states {
        written += serialize_aggregate_state(w, st)?;
    }

    Ok(written)
}

/// Reads a group record `(group_values, states)` from a reader, returning `None` at EOF.
fn read_group_record<R: Read>(
    r: &mut R,
    aggregates: &[AggregateExpr],
) -> Result<Option<(Vec<Value>, Vec<AggregateState>)>> {
    let mut num_group_buf = [0u8; 4];
    match r.read_exact(&mut num_group_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(Error::ExternalAggregateIo(e.to_string())),
    }

    let num_group_vals = u32::from_le_bytes(num_group_buf) as usize;
    let mut group_values = Vec::with_capacity(num_group_vals);
    for _ in 0..num_group_vals {
        group_values.push(deserialize_value(r)?);
    }

    let mut num_states_buf = [0u8; 4];
    r.read_exact(&mut num_states_buf)
        .map_err(|e| Error::ExternalAggregateRunCorrupted(e.to_string()))?;
    let num_states = u32::from_le_bytes(num_states_buf) as usize;
    if num_states != aggregates.len() {
        return Err(Error::ExternalAggregateRunCorrupted(format!(
            "mismatch in aggregate states count: expected {}, found {}",
            aggregates.len(),
            num_states
        )));
    }

    let mut states = Vec::with_capacity(num_states);
    for agg_expr in aggregates {
        states.push(deserialize_aggregate_state(r, agg_expr)?);
    }

    Ok(Some((group_values, states)))
}

/// Helper representing an active partition file during external aggregation spilling.
struct PartitionWriter {
    file: TempAggregateFile,
    writer: BufWriter<File>,
    record_count: usize,
    bytes_written: usize,
}

impl PartitionWriter {
    fn create(temp_dir: &Path) -> Result<Self> {
        let (file, handle) = TempAggregateFile::create_in(temp_dir)?;
        let mut writer = BufWriter::new(handle);
        writer
            .write_all(TEMP_AGG_MAGIC)
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
        writer
            .write_all(&TEMP_AGG_VERSION.to_le_bytes())
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
        // Placeholder for record count (u64)
        writer
            .write_all(&0u64.to_le_bytes())
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
        writer
            .flush()
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;

        let bytes_written = TEMP_AGG_MAGIC.len() + 2 + 8;
        Ok(Self {
            file,
            writer,
            record_count: 0,
            bytes_written,
        })
    }

    fn write_record(&mut self, group_values: &[Value], states: &[AggregateState]) -> Result<usize> {
        let written = write_group_record(&mut self.writer, group_values, states)?;
        self.record_count += 1;
        self.bytes_written += written;
        Ok(written)
    }

    fn finish(mut self) -> Result<(TempAggregateFile, usize, usize)> {
        self.writer
            .flush()
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
        let mut file = self
            .writer
            .into_inner()
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;

        // Seek back and patch record count
        file.seek(SeekFrom::Start(
            (TEMP_AGG_MAGIC.len() + std::mem::size_of::<u16>()) as u64,
        ))
        .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
        file.write_all(&(self.record_count as u64).to_le_bytes())
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;
        file.flush()
            .map_err(|e| Error::ExternalAggregateIo(e.to_string()))?;

        Ok((self.file, self.record_count, self.bytes_written))
    }
}

/// External aggregation engine coordinating partitioned in-memory accumulation, spill-to-disk,
/// and multi-pass merge passes.
pub struct ExternalAggregator<'a> {
    input_schema: &'a Schema,
    group_by: &'a [Expr],
    aggregates: &'a [AggregateExpr],
    config: AggregateConfig,
}

impl<'a> ExternalAggregator<'a> {
    /// Creates a new `ExternalAggregator` with the specified schema, grouping/aggregate expressions, and configuration.
    pub fn new(
        input_schema: &'a Schema,
        group_by: &'a [Expr],
        aggregates: &'a [AggregateExpr],
        config: AggregateConfig,
    ) -> Self {
        Self {
            input_schema,
            group_by,
            aggregates,
            config,
        }
    }

    /// Executes aggregation over `rows`, spilling to disk if memory exceeds `config.memory_budget_bytes`.
    pub fn aggregate(
        self,
        rows: Vec<Row>,
    ) -> Result<(Schema, Vec<Row>, AggregateExecutionMetrics)> {
        let mut metrics = AggregateExecutionMetrics {
            rows_input: rows.len(),
            ..Default::default()
        };

        // Construct intermediate output schema: grouping columns first, then aggregate columns.
        let mut out_cols = Vec::with_capacity(self.group_by.len() + self.aggregates.len());
        for (idx, grp_expr) in self.group_by.iter().enumerate() {
            let dt = grp_expr.infer_type(self.input_schema)?;
            let name = match grp_expr {
                Expr::Column(col_name) => col_name.clone(),
                _ => format!("_group_{idx}"),
            };
            out_cols.push(Column::new(name, dt, true));
        }
        for agg_expr in self.aggregates {
            let dt = agg_expr.infer_type(self.input_schema)?;
            let name = agg_expr.display_name();
            out_cols.push(Column::new(name, dt, true));
        }
        let out_schema = Schema::new_projected(out_cols);

        // Case 1: Global aggregation (no GROUP BY)
        if self.group_by.is_empty() {
            let mut accumulators = Vec::with_capacity(self.aggregates.len());
            for agg in self.aggregates {
                accumulators.push(AggregateState::new(agg, self.input_schema)?);
            }

            for row in &rows {
                for acc in &mut accumulators {
                    acc.accumulate(row, self.input_schema)?;
                }
            }

            let agg_values: Vec<Value> = accumulators.into_iter().map(|acc| acc.finish()).collect();
            metrics.rows_output = 1;
            metrics.groups_created = 1;
            metrics.peak_memory_bytes = std::mem::size_of::<Value>() * agg_values.len();
            return Ok((out_schema, vec![Row::new(agg_values)], metrics));
        }

        // Case 2: Grouped aggregation on empty input -> 0 rows
        if rows.is_empty() {
            return Ok((out_schema, Vec::new(), metrics));
        }

        let temp_dir = self.config.effective_temp_dir();
        let num_partitions = self.config.max_temp_files.clamp(2, 32);

        let mut in_mem_groups: HashMap<GroupKey, (Vec<Value>, Vec<AggregateState>)> =
            HashMap::new();
        let mut group_order: Vec<GroupKey> = Vec::new();
        let mut current_memory_bytes: usize = 0;
        let mut peak_memory_bytes: usize = 0;

        let mut has_spilled = false;
        let mut partition_writers: Vec<Option<PartitionWriter>> =
            (0..num_partitions).map(|_| None).collect();
        let mut total_spilled_bytes: usize = 0;
        let mut total_runs_created: usize = 0;

        for row in rows {
            let mut key_values = Vec::with_capacity(self.group_by.len());
            let mut canonical_keys = Vec::with_capacity(self.group_by.len());

            for grp_expr in self.group_by {
                let val = grp_expr.evaluate(&row, self.input_schema)?;
                canonical_keys.push(CanonicalValue::from_value(&val));
                key_values.push(val);
            }

            let key = GroupKey(canonical_keys);

            if let Some(entry) = in_mem_groups.get_mut(&key) {
                for acc in &mut entry.1 {
                    acc.accumulate(&row, self.input_schema)?;
                }
            } else {
                let mut initial_states = Vec::with_capacity(self.aggregates.len());
                for agg in self.aggregates {
                    initial_states.push(AggregateState::new(agg, self.input_schema)?);
                }
                for acc in &mut initial_states {
                    acc.accumulate(&row, self.input_schema)?;
                }

                let entry_mem = estimate_group_entry_memory(&key, &key_values, &initial_states);
                current_memory_bytes += entry_mem;
                if current_memory_bytes > peak_memory_bytes {
                    peak_memory_bytes = current_memory_bytes;
                }

                group_order.push(key.clone());
                in_mem_groups.insert(key.clone(), (key_values, initial_states));
            }

            // Check if in-memory aggregation exceeded budget
            if current_memory_bytes >= self.config.memory_budget_bytes && in_mem_groups.len() > 1 {
                has_spilled = true;
                // Flush current in-memory groups to partition writers
                for (grp_key, (grp_vals, grp_states)) in in_mem_groups.drain() {
                    let hash = hash_group_key(&grp_key, 0);
                    let part_idx = (hash as usize) % num_partitions;

                    if partition_writers[part_idx].is_none() {
                        let pw = PartitionWriter::create(&temp_dir)?;
                        total_runs_created += 1;
                        partition_writers[part_idx] = Some(pw);
                    }

                    let pw = partition_writers[part_idx].as_mut().unwrap();
                    let written = pw.write_record(&grp_vals, &grp_states)?;
                    total_spilled_bytes += written;

                    if total_spilled_bytes > self.config.max_temp_bytes {
                        return Err(Error::ExternalAggregateQuotaExceeded {
                            bytes_spilled: total_spilled_bytes,
                            limit: self.config.max_temp_bytes,
                        });
                    }
                }
                group_order.clear();
                current_memory_bytes = 0;
            }
        }

        // Fast path: If we never spilled, finalize in-memory groups directly in insertion order.
        if !has_spilled {
            let mut out_rows = Vec::with_capacity(group_order.len());
            for key in group_order {
                if let Some((grp_vals, accs)) = in_mem_groups.remove(&key) {
                    let mut row_values = grp_vals;
                    for acc in accs {
                        row_values.push(acc.finish());
                    }
                    out_rows.push(Row::new(row_values));
                }
            }

            metrics.rows_output = out_rows.len();
            metrics.groups_created = out_rows.len();
            metrics.peak_memory_bytes = peak_memory_bytes;
            return Ok((out_schema, out_rows, metrics));
        }

        // Spilled path: Flush any remaining in-memory groups to partitions
        if !in_mem_groups.is_empty() {
            for (grp_key, (grp_vals, grp_states)) in in_mem_groups.drain() {
                let hash = hash_group_key(&grp_key, 0);
                let part_idx = (hash as usize) % num_partitions;

                if partition_writers[part_idx].is_none() {
                    let pw = PartitionWriter::create(&temp_dir)?;
                    total_runs_created += 1;
                    partition_writers[part_idx] = Some(pw);
                }

                let pw = partition_writers[part_idx].as_mut().unwrap();
                let written = pw.write_record(&grp_vals, &grp_states)?;
                total_spilled_bytes += written;

                if total_spilled_bytes > self.config.max_temp_bytes {
                    return Err(Error::ExternalAggregateQuotaExceeded {
                        bytes_spilled: total_spilled_bytes,
                        limit: self.config.max_temp_bytes,
                    });
                }
            }
        }

        // Finalize all active partition writers
        let mut finished_partitions: Vec<TempAggregateFile> = Vec::new();
        let mut active_partition_count = 0;
        for pw in partition_writers.into_iter().flatten() {
            let (file, rec_count, _) = pw.finish()?;
            if rec_count > 0 {
                active_partition_count += 1;
                finished_partitions.push(file);
            }
        }

        metrics.spill_partitions = active_partition_count;
        metrics.runs_created = total_runs_created;
        metrics.bytes_spilled = total_spilled_bytes;
        metrics.merge_passes = 1;

        // Process each partition and produce final aggregated rows
        let mut out_rows = Vec::new();
        let mut total_groups = 0;

        for part_file in finished_partitions {
            let (part_rows, part_groups, part_peak_mem) =
                self.process_partition(part_file, 1, &mut metrics)?;
            total_groups += part_groups;
            if part_peak_mem > peak_memory_bytes {
                peak_memory_bytes = part_peak_mem;
            }
            out_rows.extend(part_rows);
        }

        metrics.rows_output = out_rows.len();
        metrics.groups_created = total_groups;
        metrics.peak_memory_bytes = peak_memory_bytes;

        Ok((out_schema, out_rows, metrics))
    }

    /// Recursively processes a temporary partition file, merging partial aggregate states.
    /// If loading a partition exceeds `memory_budget_bytes`, it is re-partitioned into sub-partitions.
    pub fn process_partition(
        &self,
        partition_file: TempAggregateFile,
        pass: usize,
        metrics: &mut AggregateExecutionMetrics,
    ) -> Result<(Vec<Row>, usize, usize)> {
        if pass > self.config.max_merge_passes {
            return Err(Error::ExternalAggregateMaxPassesExceeded {
                passes: pass,
                limit: self.config.max_merge_passes,
            });
        }

        let file = File::open(partition_file.path()).map_err(|e| {
            Error::ExternalAggregateIo(format!(
                "failed to open temporary partition run {}: {e}",
                partition_file.path().display()
            ))
        })?;

        let mut reader = BufReader::new(file);

        // Validate header
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic).map_err(|e| {
            Error::ExternalAggregateRunCorrupted(format!(
                "failed to read magic header from temporary aggregation run: {e}"
            ))
        })?;
        if &magic != TEMP_AGG_MAGIC {
            return Err(Error::ExternalAggregateRunCorrupted(
                "invalid magic bytes in temporary aggregation run header".to_string(),
            ));
        }

        let mut ver_buf = [0u8; 2];
        reader.read_exact(&mut ver_buf).map_err(|e| {
            Error::ExternalAggregateRunCorrupted(format!(
                "failed to read version from temporary aggregation run: {e}"
            ))
        })?;
        let ver = u16::from_le_bytes(ver_buf);
        if ver != TEMP_AGG_VERSION {
            return Err(Error::ExternalAggregateRunCorrupted(format!(
                "unsupported temporary aggregation run version: {ver}"
            )));
        }

        let mut count_buf = [0u8; 8];
        reader.read_exact(&mut count_buf).map_err(|e| {
            Error::ExternalAggregateRunCorrupted(format!(
                "failed to read record count from temporary aggregation run: {e}"
            ))
        })?;
        let total_records = u64::from_le_bytes(count_buf) as usize;

        let mut in_mem_map: HashMap<GroupKey, (Vec<Value>, Vec<AggregateState>)> = HashMap::new();
        let mut in_mem_keys: Vec<GroupKey> = Vec::new();
        let mut current_mem = 0;
        let mut peak_mem = 0;

        let num_sub_partitions = self.config.max_temp_files.clamp(2, 32);
        let temp_dir = self.config.effective_temp_dir();
        let mut sub_partition_writers: Option<Vec<Option<PartitionWriter>>> = None;

        let mut records_read = 0;
        while let Some((group_values, incoming_states)) =
            read_group_record(&mut reader, self.aggregates)?
        {
            records_read += 1;
            let mut canonical_keys = Vec::with_capacity(group_values.len());
            for val in &group_values {
                canonical_keys.push(CanonicalValue::from_value(val));
            }
            let key = GroupKey(canonical_keys);

            if let Some(writers) = sub_partition_writers.as_mut() {
                // Already spilled this partition into sub-partitions
                let hash = hash_group_key(&key, pass);
                let part_idx = (hash as usize) % num_sub_partitions;
                if writers[part_idx].is_none() {
                    let pw = PartitionWriter::create(&temp_dir)?;
                    metrics.runs_created += 1;
                    writers[part_idx] = Some(pw);
                }
                let pw = writers[part_idx].as_mut().unwrap();
                let written = pw.write_record(&group_values, &incoming_states)?;
                metrics.bytes_spilled += written;
                if metrics.bytes_spilled > self.config.max_temp_bytes {
                    return Err(Error::ExternalAggregateQuotaExceeded {
                        bytes_spilled: metrics.bytes_spilled,
                        limit: self.config.max_temp_bytes,
                    });
                }
                continue;
            }

            if let Some(existing) = in_mem_map.get_mut(&key) {
                for (st_idx, inc_st) in incoming_states.into_iter().enumerate() {
                    existing.1[st_idx].merge_from(inc_st)?;
                }
            } else {
                let entry_mem = estimate_group_entry_memory(&key, &group_values, &incoming_states);
                current_mem += entry_mem;
                if current_mem > peak_mem {
                    peak_mem = current_mem;
                }
                in_mem_keys.push(key.clone());
                in_mem_map.insert(key.clone(), (group_values, incoming_states));

                // Check if reload exceeded memory budget -> must re-partition into sub-partitions!
                if current_mem >= self.config.memory_budget_bytes && in_mem_map.len() > 1 {
                    if pass + 1 > metrics.merge_passes {
                        metrics.merge_passes = pass + 1;
                    }
                    let mut writers: Vec<Option<PartitionWriter>> =
                        (0..num_sub_partitions).map(|_| None).collect();

                    // Spill all groups loaded so far
                    for (spill_key, (spill_vals, spill_states)) in in_mem_map.drain() {
                        let hash = hash_group_key(&spill_key, pass);
                        let part_idx = (hash as usize) % num_sub_partitions;
                        if writers[part_idx].is_none() {
                            let pw = PartitionWriter::create(&temp_dir)?;
                            metrics.runs_created += 1;
                            writers[part_idx] = Some(pw);
                        }
                        let pw = writers[part_idx].as_mut().unwrap();
                        let written = pw.write_record(&spill_vals, &spill_states)?;
                        metrics.bytes_spilled += written;
                        if metrics.bytes_spilled > self.config.max_temp_bytes {
                            return Err(Error::ExternalAggregateQuotaExceeded {
                                bytes_spilled: metrics.bytes_spilled,
                                limit: self.config.max_temp_bytes,
                            });
                        }
                    }
                    in_mem_keys.clear();
                    current_mem = 0;
                    sub_partition_writers = Some(writers);
                }
            }
        }

        if records_read != total_records {
            return Err(Error::ExternalAggregateRunCorrupted(format!(
                "record count mismatch in partition run: expected {total_records}, read {records_read}"
            )));
        }

        // If we had to re-partition into sub-partitions:
        if let Some(writers) = sub_partition_writers {
            let mut sub_files = Vec::new();
            for pw in writers.into_iter().flatten() {
                let (sub_file, sub_count, _) = pw.finish()?;
                if sub_count > 0 {
                    sub_files.push(sub_file);
                }
            }

            let mut out_rows = Vec::new();
            let mut total_groups = 0;
            for sub_file in sub_files {
                let (sub_rows, sub_groups, sub_peak) =
                    self.process_partition(sub_file, pass + 1, metrics)?;
                total_groups += sub_groups;
                if sub_peak > peak_mem {
                    peak_mem = sub_peak;
                }
                out_rows.extend(sub_rows);
            }
            return Ok((out_rows, total_groups, peak_mem));
        }

        // Otherwise, this partition completed cleanly in memory. Finalize output rows.
        let mut out_rows = Vec::with_capacity(in_mem_keys.len());
        for key in in_mem_keys {
            if let Some((grp_vals, accs)) = in_mem_map.remove(&key) {
                let mut row_values = grp_vals;
                for acc in accs {
                    row_values.push(acc.finish());
                }
                out_rows.push(Row::new(row_values));
            }
        }

        let num_groups = out_rows.len();
        Ok((out_rows, num_groups, peak_mem))
    }
}
