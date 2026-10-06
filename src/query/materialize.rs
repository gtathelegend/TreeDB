use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::engine::types::{Row, Schema, Value};
use crate::error::{Error, Result};

/// Default in-memory materialization memory budget: 64 KB.
pub const DEFAULT_MATERIALIZE_MEMORY_BUDGET_BYTES: usize = 64 * 1024;

/// Default maximum total disk space permitted for temporary materialization files: 1 GB.
pub const DEFAULT_MAX_MATERIALIZE_TEMP_BYTES: usize = 1024 * 1024 * 1024;

/// Magic bytes identifying a TreeDB temporary materialized subplan file: `b"TMAT"`.
pub const TEMP_MATERIALIZE_MAGIC: &[u8; 4] = b"TMAT";

/// Version identifier for the temporary materialized file format.
pub const TEMP_MATERIALIZE_VERSION: u16 = 1;

static MATERIALIZE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Test-only override mechanism for forcing a specific materialization decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaterializeOverride {
    /// Cost-based automatic decision (default).
    #[default]
    Auto,
    /// Force inlining (recomputation for each consumer).
    ForceInline,
    /// Force in-memory materialization once.
    ForceMaterialize,
    /// Force external spilled materialization to temporary disk files.
    ForceExternalMaterialize,
}

/// Runtime configuration parameters controlling common subplan materialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeConfig {
    /// In-memory buffer budget in bytes before spilling to disk.
    pub memory_budget_bytes: usize,
    /// Maximum disk space in bytes that can be consumed by temporary materialization files.
    pub max_temp_bytes: usize,
    /// Custom directory for temporary materialization files. If `None`, `std::env::temp_dir()` is used.
    pub temp_dir: Option<PathBuf>,
    /// Test-only planning override.
    pub test_override: MaterializeOverride,
}

impl Default for MaterializeConfig {
    fn default() -> Self {
        Self {
            memory_budget_bytes: DEFAULT_MATERIALIZE_MEMORY_BUDGET_BYTES,
            max_temp_bytes: DEFAULT_MAX_MATERIALIZE_TEMP_BYTES,
            temp_dir: None,
            test_override: MaterializeOverride::Auto,
        }
    }
}

impl MaterializeConfig {
    /// Creates a new `MaterializeConfig` with default parameters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the in-memory materialization budget in bytes.
    pub fn with_memory_budget_bytes(mut self, budget: usize) -> Self {
        self.memory_budget_bytes = budget.max(1);
        self
    }

    /// Sets the maximum allowable disk space in bytes for temporary materialization files.
    pub fn with_max_temp_bytes(mut self, max_bytes: usize) -> Self {
        self.max_temp_bytes = max_bytes;
        self
    }

    /// Sets the directory path for temporary materialization files.
    pub fn with_temp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.temp_dir = Some(path.into());
        self
    }

    /// Sets a test-only planning override.
    pub fn with_override(mut self, test_override: MaterializeOverride) -> Self {
        self.test_override = test_override;
        self
    }

    /// Returns the effective temporary directory path.
    pub fn effective_temp_dir(&self) -> PathBuf {
        self.temp_dir.clone().unwrap_or_else(std::env::temp_dir)
    }
}

/// Cost-based physical decision on how to evaluate an intermediate relational subplan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MaterializationDecision {
    /// Recompute the subplan per consumer reference inline.
    #[default]
    Inline,
    /// Compute once, store in memory, and reuse across consumers.
    Materialize,
    /// Compute once, spill to temporary file if exceeding memory budget, and stream to consumers.
    ExternalMaterialize,
}

impl std::fmt::Display for MaterializationDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline => write!(f, "Inline"),
            Self::Materialize => write!(f, "Materialize"),
            Self::ExternalMaterialize => write!(f, "ExternalMaterialize"),
        }
    }
}

/// Deterministic identity fingerprint representing a logically reusable subplan.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubplanFingerprint(pub String);

impl SubplanFingerprint {
    /// Creates a new fingerprint from a canonicalized string representation.
    pub fn new(canonical: impl Into<String>) -> Self {
        Self(canonical.into())
    }

    /// Constructs a canonical deterministic fingerprint from a physical plan.
    pub fn from_plan(plan: &crate::query::optimizer::PhysicalPlan) -> Self {
        let mut s = String::new();
        s.push_str("TABLE:");
        s.push_str(&plan.base_table.table_name);
        s.push_str(";PATH:");
        s.push_str(&format!("{:?}", plan.base_table.access_path));
        if let Some(f) = &plan.base_table.residual_filter {
            s.push_str(&format!(";RES:{f}"));
        }
        for j in &plan.joins {
            s.push_str(&format!(
                ";JOIN:{:?}:{}:{:?}",
                j.join_type, j.right.table_name, j.on
            ));
        }
        if let Some(pj) = &plan.post_join_filter {
            s.push_str(&format!(";PJF:{pj}"));
        }
        for g in &plan.group_by {
            s.push_str(&format!(";GRP:{g}"));
        }
        for a in &plan.aggregates {
            s.push_str(&format!(";AGG:{a:?}"));
        }
        if let Some(h) = &plan.having {
            s.push_str(&format!(";HAV:{h}"));
        }
        for (p, alias) in &plan.projections {
            s.push_str(&format!(";PROJ:{p}:{:?}", alias));
        }
        for o in &plan.order_by {
            s.push_str(&format!(";ORD:{:?}:{:?}", o.expression, o.direction));
        }
        if let Some(off) = plan.offset {
            s.push_str(&format!(";OFF:{off}"));
        }
        if let Some(lim) = plan.limit {
            s.push_str(&format!(";LIM:{lim}"));
        }
        if let Some(so) = &plan.set_op {
            s.push_str(&format!(";SETOP:{:?}", so.op));
        }
        Self(s)
    }

    /// Constructs a canonical deterministic fingerprint from a bound select logical expression.
    pub fn from_bound(bound: &crate::sql::binder::BoundSelect) -> Self {
        Self(LogicalSubplanFingerprint::from_bound(bound).0)
    }

    /// Returns the raw canonical fingerprint string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns a deterministic 16-character hexadecimal hash of this fingerprint.
    pub fn to_hex(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.0.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }
}

/// Canonical logical fingerprint identifying a reusable subplan independently of physical access paths.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalSubplanFingerprint(pub String);

impl LogicalSubplanFingerprint {
    /// Creates a new logical subplan fingerprint.
    pub fn new(canonical: impl Into<String>) -> Self {
        Self(canonical.into())
    }

    /// Constructs a logical fingerprint from a BoundSelect AST representation.
    pub fn from_bound(bound: &crate::sql::binder::BoundSelect) -> Self {
        let mut s = String::new();
        s.push_str("LOGICAL_TABLE:");
        s.push_str(&bound.table_name);
        if let Some(alias) = &bound.table_alias {
            s.push_str(&format!(";ALIAS:{alias}"));
        }
        if let Some(f) = &bound.filter {
            s.push_str(&format!(";WHERE:{f}"));
        }
        for j in &bound.joins {
            s.push_str(&format!(
                ";JOIN:{:?}:{}:{:?}:{}",
                j.join_type, j.table_name, j.table_alias, j.on
            ));
        }
        for g in &bound.group_by {
            s.push_str(&format!(";GRP:{g}"));
        }
        for a in &bound.aggregates {
            s.push_str(&format!(";AGG:{a:?}"));
        }
        if let Some(h) = &bound.having {
            s.push_str(&format!(";HAV:{h}"));
        }
        for (p, alias) in &bound.projections {
            s.push_str(&format!(";PROJ:{p}:{:?}", alias));
        }
        for o in &bound.order_by {
            s.push_str(&format!(";ORD:{:?}:{:?}", o.expression, o.direction));
        }
        if let Some(off) = bound.offset {
            s.push_str(&format!(";OFF:{off}"));
        }
        if let Some(lim) = bound.limit {
            s.push_str(&format!(";LIM:{lim}"));
        }
        Self(s)
    }

    /// Returns the raw string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LogicalSubplanFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Display for SubplanFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// RAII-managed temporary materialization disk file that is automatically removed upon Drop.
#[derive(Debug)]
pub struct TempMaterializeFile {
    path: PathBuf,
}

impl TempMaterializeFile {
    /// Creates a new temporary materialization file in the specified directory.
    pub fn create_in(temp_dir: &Path) -> Result<(Self, File)> {
        let seq = MATERIALIZE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let pid = std::process::id();
        let file_name = format!("treedb_mat_{pid}_{seq}.tmp");
        let path = temp_dir.join(file_name);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(Error::Io)?;

        Ok((Self { path }, file))
    }

    /// Returns the path to this temporary file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempMaterializeFile {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Serialized storage representation for materialized results (either in-memory or on-disk).
#[derive(Debug)]
pub enum MaterializedStorage {
    /// Entire result stored in memory.
    InMemory(Vec<Row>),
    /// Result spilled to an RAII-managed temporary file.
    Spilled {
        file: TempMaterializeFile,
        row_count: usize,
        byte_count: usize,
    },
}

impl MaterializedStorage {
    /// Materializes a collection of rows according to the specified memory budget and configuration.
    pub fn materialize(
        rows: Vec<Row>,
        config: &MaterializeConfig,
        force_spill: bool,
    ) -> Result<(Self, usize, usize)> {
        let mut total_bytes = 0usize;
        for row in &rows {
            total_bytes += estimate_row_memory(row);
        }

        let should_spill = force_spill
            || (total_bytes > config.memory_budget_bytes)
            || config.test_override == MaterializeOverride::ForceExternalMaterialize;

        if !should_spill {
            let row_count = rows.len();
            return Ok((Self::InMemory(rows), row_count, total_bytes));
        }

        // Spill to disk
        let temp_dir = config.effective_temp_dir();
        let (temp_file, file) = TempMaterializeFile::create_in(&temp_dir)?;
        let mut writer = BufWriter::new(file);

        let mut written_bytes = 0;
        writer.write_all(TEMP_MATERIALIZE_MAGIC)?;
        written_bytes += 4;

        writer.write_all(&TEMP_MATERIALIZE_VERSION.to_le_bytes())?;
        written_bytes += 2;

        writer.write_all(&[0u8; 2])?;
        written_bytes += 2;

        let row_count_u64 = rows.len() as u64;
        writer.write_all(&row_count_u64.to_le_bytes())?;
        written_bytes += 8;

        for row in &rows {
            written_bytes += serialize_row(&mut writer, row)?;
        }

        writer.flush()?;

        if written_bytes > config.max_temp_bytes {
            return Err(Error::ExternalSortQuotaExceeded {
                bytes_spilled: written_bytes,
                limit: config.max_temp_bytes,
            });
        }

        let row_count = rows.len();
        Ok((
            Self::Spilled {
                file: temp_file,
                row_count,
                byte_count: written_bytes,
            },
            row_count,
            written_bytes,
        ))
    }

    /// Reads all rows stored in this materialized representation.
    pub fn read_rows(&self) -> Result<Vec<Row>> {
        match self {
            Self::InMemory(rows) => Ok(rows.clone()),
            Self::Spilled { file, .. } => {
                let f = File::open(file.path()).map_err(Error::Io)?;
                let mut reader = BufReader::new(f);

                let mut magic = [0u8; 4];
                reader.read_exact(&mut magic).map_err(Error::Io)?;
                if &magic != TEMP_MATERIALIZE_MAGIC {
                    return Err(Error::QueryExecutionError(
                        "invalid magic header in materialized temporary file".into(),
                    ));
                }

                let mut version_bytes = [0u8; 2];
                reader.read_exact(&mut version_bytes).map_err(Error::Io)?;
                let version = u16::from_le_bytes(version_bytes);
                if version != TEMP_MATERIALIZE_VERSION {
                    return Err(Error::UnsupportedFormatVersion {
                        found: version as u32,
                        supported: TEMP_MATERIALIZE_VERSION as u32,
                    });
                }

                let mut reserved = [0u8; 2];
                reader.read_exact(&mut reserved).map_err(Error::Io)?;

                let mut count_bytes = [0u8; 8];
                reader.read_exact(&mut count_bytes).map_err(Error::Io)?;
                let count = u64::from_le_bytes(count_bytes) as usize;

                let mut rows = Vec::with_capacity(count);
                for _ in 0..count {
                    let row = deserialize_row(&mut reader)?;
                    rows.push(row);
                }

                Ok(rows)
            }
        }
    }
}

/// Observed metrics during subplan materialization and reuse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MaterializationMetrics {
    /// Deterministic identifier for this materialization node.
    pub materialize_id: usize,
    /// Total rows produced and stored.
    pub rows_materialized: usize,
    /// Total bytes stored in memory or on disk.
    pub bytes_materialized: usize,
    /// Number of distinct consumers planned for reuse.
    pub consumers: usize,
    /// Total reads performed by consumers.
    pub reads: usize,
    /// Peak in-memory byte footprint.
    pub peak_memory_bytes: usize,
    /// Bytes spilled to disk (if external materialization).
    pub spill_bytes: usize,
    /// Number of temporary files created (if external materialization).
    pub spill_files: usize,
    /// Number of reuse reads (reads occurring after producer execution).
    pub reuse_reads: usize,
}

/// Executes a subplan once and provides reusable reading capability.
pub struct MaterializeOperator;

impl MaterializeOperator {
    /// Materializes rows produced by an executed subplan and initializes tracking metrics.
    pub fn materialize_rows(
        id: usize,
        rows: Vec<Row>,
        _schema: &Schema,
        consumers: usize,
        config: &MaterializeConfig,
        force_spill: bool,
    ) -> Result<(MaterializedStorage, MaterializationMetrics)> {
        let (storage, row_count, byte_count) =
            MaterializedStorage::materialize(rows, config, force_spill)?;

        let (peak_memory, spill_bytes, spill_files) = match &storage {
            MaterializedStorage::InMemory(_) => (byte_count, 0, 0),
            MaterializedStorage::Spilled { .. } => (0, byte_count, 1),
        };

        let metrics = MaterializationMetrics {
            materialize_id: id,
            rows_materialized: row_count,
            bytes_materialized: byte_count,
            consumers,
            reads: 0,
            peak_memory_bytes: peak_memory,
            spill_bytes,
            spill_files,
            reuse_reads: 0,
        };

        Ok((storage, metrics))
    }
}

/// Statement-local registry tracking materialized subplans during query execution.
#[derive(Debug, Default)]
pub struct StatementMaterializeStore {
    entries:
        std::collections::HashMap<usize, (Schema, MaterializedStorage, MaterializationMetrics)>,
    named_map: std::collections::HashMap<String, usize>,
}

impl StatementMaterializeStore {
    /// Creates a new empty statement-local materialize store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a newly materialized result for a given materialize ID and optional relation name.
    pub fn insert(
        &mut self,
        id: usize,
        name: Option<String>,
        schema: Schema,
        storage: MaterializedStorage,
        metrics: MaterializationMetrics,
    ) {
        if let Some(n) = name {
            self.named_map.insert(n, id);
        }
        self.entries.insert(id, (schema, storage, metrics));
    }

    /// Checks whether a materialized result exists by ID.
    pub fn contains_id(&self, id: usize) -> bool {
        self.entries.contains_key(&id)
    }

    /// Checks whether a materialized result exists by name.
    pub fn contains_name(&self, name: &str) -> bool {
        self.named_map.contains_key(name)
    }

    /// Reads rows for a materialized result by ID, updating read metrics.
    pub fn read_by_id(&mut self, id: usize) -> Result<Option<(Schema, Vec<Row>)>> {
        if let Some((schema, storage, metrics)) = self.entries.get_mut(&id) {
            metrics.reads += 1;
            if metrics.reads > 1 {
                metrics.reuse_reads += 1;
            }
            let rows = storage.read_rows()?;
            Ok(Some((schema.clone(), rows)))
        } else {
            Ok(None)
        }
    }

    /// Reads rows for a materialized result by name, updating read metrics.
    pub fn read_by_name(&mut self, name: &str) -> Result<Option<(Schema, Vec<Row>)>> {
        if let Some(&id) = self.named_map.get(name) {
            self.read_by_id(id)
        } else {
            Ok(None)
        }
    }

    /// Returns the metrics recorded for a given materialize ID.
    pub fn get_metrics(&self, id: usize) -> Option<&MaterializationMetrics> {
        self.entries.get(&id).map(|(_, _, m)| m)
    }

    /// Returns the metrics recorded for a given relation name.
    pub fn get_metrics_by_name(&self, name: &str) -> Option<&MaterializationMetrics> {
        self.named_map
            .get(name)
            .and_then(|id| self.entries.get(id).map(|(_, _, m)| m))
    }
}

/// Helper function estimating in-memory byte size of a row.
pub fn estimate_row_memory(row: &Row) -> usize {
    let mut bytes = std::mem::size_of::<Row>() + row.values.len() * std::mem::size_of::<Value>();
    for val in &row.values {
        match val {
            Value::String(s) => bytes += s.len(),
            Value::Bytes(b) => bytes += b.len(),
            _ => {}
        }
    }
    bytes
}

fn serialize_row<W: Write>(writer: &mut W, row: &Row) -> Result<usize> {
    let mut written = 0;
    let num_vals = row.values.len() as u32;
    writer.write_all(&num_vals.to_le_bytes())?;
    written += 4;

    for val in &row.values {
        written += serialize_value(writer, val)?;
    }
    Ok(written)
}

fn deserialize_row<R: Read>(reader: &mut R) -> Result<Row> {
    let mut num_vals_bytes = [0u8; 4];
    reader.read_exact(&mut num_vals_bytes)?;
    let num_vals = u32::from_le_bytes(num_vals_bytes) as usize;

    let mut values = Vec::with_capacity(num_vals);
    for _ in 0..num_vals {
        values.push(deserialize_value(reader)?);
    }
    Ok(Row::new(values))
}

fn serialize_value<W: Write>(writer: &mut W, val: &Value) -> Result<usize> {
    let mut written = 0;
    match val {
        Value::Null => {
            writer.write_all(&[0u8])?;
            written += 1;
        }
        Value::Boolean(b) => {
            writer.write_all(&[1u8, if *b { 1 } else { 0 }])?;
            written += 2;
        }
        Value::Int64(i) => {
            writer.write_all(&[2u8])?;
            writer.write_all(&i.to_le_bytes())?;
            written += 9;
        }
        Value::Float64(f) => {
            writer.write_all(&[3u8])?;
            writer.write_all(&f.to_bits().to_le_bytes())?;
            written += 9;
        }
        Value::String(s) => {
            writer.write_all(&[4u8])?;
            let len = s.len() as u32;
            writer.write_all(&len.to_le_bytes())?;
            writer.write_all(s.as_bytes())?;
            written += 1 + 4 + s.len();
        }
        Value::Bytes(b) => {
            writer.write_all(&[5u8])?;
            let len = b.len() as u32;
            writer.write_all(&len.to_le_bytes())?;
            writer.write_all(b)?;
            written += 1 + 4 + b.len();
        }
        Value::UInt64(u) => {
            writer.write_all(&[6u8])?;
            writer.write_all(&u.to_le_bytes())?;
            written += 9;
        }
    }
    Ok(written)
}

fn deserialize_value<R: Read>(reader: &mut R) -> Result<Value> {
    let mut tag = [0u8; 1];
    reader.read_exact(&mut tag)?;
    match tag[0] {
        0 => Ok(Value::Null),
        1 => {
            let mut b = [0u8; 1];
            reader.read_exact(&mut b)?;
            Ok(Value::Boolean(b[0] != 0))
        }
        2 => {
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            Ok(Value::Int64(i64::from_le_bytes(buf)))
        }
        3 => {
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            Ok(Value::Float64(f64::from_bits(u64::from_le_bytes(buf))))
        }
        4 => {
            let mut len_bytes = [0u8; 4];
            reader.read_exact(&mut len_bytes)?;
            let len = u32::from_le_bytes(len_bytes) as usize;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            let s =
                String::from_utf8(buf).map_err(|e| Error::SerializationFailure(e.to_string()))?;
            Ok(Value::String(s))
        }
        5 => {
            let mut len_bytes = [0u8; 4];
            reader.read_exact(&mut len_bytes)?;
            let len = u32::from_le_bytes(len_bytes) as usize;
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            Ok(Value::Bytes(buf))
        }
        6 => {
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            Ok(Value::UInt64(u64::from_le_bytes(buf)))
        }
        other => Err(Error::SerializationFailure(format!(
            "invalid value tag {other}"
        ))),
    }
}
