use std::collections::HashMap;
use std::fmt;

use crate::engine::types::{Schema, Value};
use crate::query::operator::CanonicalValue;

/// Maximum number of columns in a correlated column group.
pub const MAX_COLUMN_GROUP_WIDTH: usize = 2;
/// Maximum number of column groups collected per table.
pub const MAX_COLUMN_GROUPS_PER_TABLE: usize = 16;
/// Maximum number of Top-K joint value combinations retained per column group.
pub const MAX_TOP_K_PER_GROUP: usize = 50;
/// Maximum number of equi-depth buckets retained per column histogram.
pub const MAX_HISTOGRAM_BUCKETS: usize = 32;
/// Maximum memory byte limit per column histogram.
pub const MAX_HISTOGRAM_MEMORY_BYTES: usize = 4096;

/// Functional classification of a histogram algorithm strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistogramKind {
    /// Equi-depth (equal row frequency per bucket) histogram.
    EquiDepth,
}

/// A single bucket within a column histogram.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramBucket {
    /// Lower bound (inclusive) of non-null values in this bucket.
    pub lower_bound: Value,
    /// Upper bound of non-null values in this bucket.
    pub upper_bound: Value,
    /// Total number of rows falling inside this bucket.
    pub row_count: usize,
    /// Estimated or actual distinct non-null values inside this bucket.
    pub distinct_count: usize,
}

impl HistogramBucket {
    /// Creates a new `HistogramBucket` record.
    pub fn new(
        lower_bound: Value,
        upper_bound: Value,
        row_count: usize,
        distinct_count: usize,
    ) -> Self {
        Self {
            lower_bound,
            upper_bound,
            row_count,
            distinct_count,
        }
    }
}

/// Bounded single-column histogram statistics for distribution-aware range estimation.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    /// Histogram strategy kind.
    pub kind: HistogramKind,
    /// Bounded sequence of histogram buckets ordered by value.
    pub buckets: Vec<HistogramBucket>,
    /// Total number of NULL values excluded from the histogram.
    pub null_count: usize,
}

impl Histogram {
    /// Creates a new equi-depth `Histogram`.
    pub fn new(buckets: Vec<HistogramBucket>, null_count: usize) -> Self {
        Self {
            kind: HistogramKind::EquiDepth,
            buckets,
            null_count,
        }
    }
}

/// Key representing a bounded group of columns in a table.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnGroupKey {
    /// Sorted 0-based column indices in the table schema.
    pub column_indices: Vec<usize>,
}

impl ColumnGroupKey {
    /// Creates a new `ColumnGroupKey` with sorted, deduplicated column indices.
    pub fn new(mut indices: Vec<usize>) -> Self {
        indices.sort_unstable();
        indices.dedup();
        Self {
            column_indices: indices,
        }
    }
}

/// A frequent joint combination of canonical values for a column group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopKCombination {
    /// Canonical values for each column in the group (in order of `ColumnGroupKey.column_indices`).
    pub values: Vec<CanonicalValue>,
    /// Number of rows matching this exact joint combination.
    pub frequency: usize,
}

/// Bounded multi-column statistics for a column group.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnGroupStatistics {
    /// Column indices included in this group (sorted).
    pub column_indices: Vec<usize>,
    /// Column names corresponding to `column_indices` in the same order.
    pub column_names: Vec<String>,
    /// Total number of distinct joint value combinations observed.
    pub distinct_count: usize,
    /// Number of rows where ALL columns in the group are NULL.
    pub all_null_count: usize,
    /// Number of rows where ANY column in the group is NULL.
    pub any_null_count: usize,
    /// Top-K most frequent canonical value combinations, sorted deterministically.
    pub top_k: Vec<TopKCombination>,
}

impl ColumnGroupStatistics {
    /// Creates a new `ColumnGroupStatistics` record.
    pub fn new(
        column_indices: Vec<usize>,
        column_names: Vec<String>,
        distinct_count: usize,
        all_null_count: usize,
        any_null_count: usize,
        top_k: Vec<TopKCombination>,
    ) -> Self {
        Self {
            column_indices,
            column_names,
            distinct_count,
            all_null_count,
            any_null_count,
            top_k,
        }
    }
}

/// Summary statistics for an individual table column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStatistics {
    /// Total number of rows evaluated.
    pub row_count: usize,
    /// Number of `NULL` values in the column.
    pub null_count: usize,
    /// Number of distinct non-null values.
    pub distinct_count: usize,
    /// Minimum non-null value observed.
    pub min: Option<Value>,
    /// Maximum non-null value observed.
    pub max: Option<Value>,
    /// Distribution histogram statistics for range estimation, if available.
    pub histogram: Option<Histogram>,
}

impl ColumnStatistics {
    /// Creates a new `ColumnStatistics` record.
    pub fn new(
        row_count: usize,
        null_count: usize,
        distinct_count: usize,
        min: Option<Value>,
        max: Option<Value>,
    ) -> Self {
        Self {
            row_count,
            null_count,
            distinct_count,
            min,
            max,
            histogram: None,
        }
    }

    /// Attaches histogram statistics to this column record.
    pub fn with_histogram(mut self, histogram: Histogram) -> Self {
        self.histogram = Some(histogram);
        self
    }

    /// Computes the fraction of rows containing `NULL` values in `[0.0, 1.0]`.
    pub fn null_fraction(&self) -> f64 {
        if self.row_count == 0 {
            0.0
        } else {
            (self.null_count as f64) / (self.row_count as f64)
        }
    }

    /// Returns the number of non-null rows in this column.
    pub fn non_null_count(&self) -> usize {
        self.row_count.saturating_sub(self.null_count)
    }
}

/// Summary statistics for a secondary index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexStatistics {
    /// Total number of indexed entries.
    pub indexed_row_count: usize,
    /// Number of distinct scalar prefix index keys (scalar column NDV).
    pub distinct_key_count: usize,
    /// Number of `NULL` index entries.
    pub null_count: usize,
    /// Number of columns in this index.
    pub column_count: usize,
    /// Number of distinct full composite index keys (composite key NDV).
    pub composite_ndv: usize,
    /// Estimated index height (levels in B+ Tree).
    pub index_height: usize,
    /// Estimated leaf/page count.
    pub estimated_leaf_pages: usize,
}

impl IndexStatistics {
    /// Creates a new `IndexStatistics` record for a single-column index.
    pub fn new(indexed_row_count: usize, distinct_key_count: usize, null_count: usize) -> Self {
        Self {
            indexed_row_count,
            distinct_key_count,
            null_count,
            column_count: 1,
            composite_ndv: distinct_key_count,
            index_height: 1,
            estimated_leaf_pages: 1,
        }
    }

    /// Creates a new `IndexStatistics` record for a composite secondary index.
    pub fn new_composite(
        indexed_row_count: usize,
        distinct_key_count: usize,
        null_count: usize,
        column_count: usize,
        composite_ndv: usize,
        index_height: usize,
        estimated_leaf_pages: usize,
    ) -> Self {
        Self {
            indexed_row_count,
            distinct_key_count,
            null_count,
            column_count,
            composite_ndv,
            index_height,
            estimated_leaf_pages,
        }
    }
}

/// Comprehensive relational statistics for a named database table and its attached secondary indexes.
#[derive(Debug, Clone, PartialEq)]
pub struct TableStatistics {
    /// Canonical table name.
    pub table_name: String,
    /// Total row count in the table relation.
    pub row_count: usize,
    /// Per-column statistics keyed by column name.
    pub columns: HashMap<String, ColumnStatistics>,
    /// Per-index statistics keyed by index name.
    pub indexes: HashMap<String, IndexStatistics>,
    /// Column group statistics keyed by ColumnGroupKey.
    pub column_groups: HashMap<ColumnGroupKey, ColumnGroupStatistics>,
}

impl TableStatistics {
    /// Creates a new `TableStatistics` record.
    pub fn new(table_name: impl Into<String>, row_count: usize) -> Self {
        Self {
            table_name: table_name.into(),
            row_count,
            columns: HashMap::new(),
            indexes: HashMap::new(),
            column_groups: HashMap::new(),
        }
    }

    /// Adds column statistics for the given column name.
    pub fn with_column(mut self, column_name: impl Into<String>, stats: ColumnStatistics) -> Self {
        self.columns.insert(column_name.into(), stats);
        self
    }

    /// Adds index statistics for the given index name.
    pub fn with_index(mut self, index_name: impl Into<String>, stats: IndexStatistics) -> Self {
        self.indexes.insert(index_name.into(), stats);
        self
    }

    /// Adds column group statistics.
    pub fn with_column_group(mut self, stats: ColumnGroupStatistics) -> Self {
        let key = ColumnGroupKey::new(stats.column_indices.clone());
        self.column_groups.insert(key, stats);
        self
    }

    /// Retrieves statistics for a specific column.
    pub fn column(&self, name: &str) -> Option<&ColumnStatistics> {
        self.columns.get(name)
    }

    /// Retrieves statistics for a specific secondary index.
    pub fn index(&self, name: &str) -> Option<&IndexStatistics> {
        self.indexes.get(name)
    }

    /// Retrieves statistics for a specific column group key.
    pub fn column_group(&self, key: &ColumnGroupKey) -> Option<&ColumnGroupStatistics> {
        self.column_groups.get(key)
    }

    /// Retrieves column group statistics matching the given column indices.
    pub fn find_column_group_for_indices(
        &self,
        indices: &[usize],
    ) -> Option<&ColumnGroupStatistics> {
        let key = ColumnGroupKey::new(indices.to_vec());
        self.column_groups.get(&key)
    }

    /// Retrieves column group statistics matching the given column names within a schema.
    pub fn find_column_group_for_names(
        &self,
        names: &[&str],
        schema: &Schema,
    ) -> Option<&ColumnGroupStatistics> {
        let mut indices = Vec::with_capacity(names.len());
        for &name in names {
            let col_raw = name.split('.').next_back().unwrap_or(name);
            let idx = schema.columns.iter().position(|c| c.name == col_raw)?;
            indices.push(idx);
        }
        self.find_column_group_for_indices(&indices)
    }
}

/// Trait defining access to table-level and column-level statistics.
pub trait StatisticsProvider: fmt::Debug {
    /// Returns advisory statistics for a table if available.
    fn get_table_stats(&self, table_name: &str) -> Option<&TableStatistics>;
}

/// In-memory registry and cache of table statistics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsRegistry {
    tables: HashMap<String, TableStatistics>,
}

impl StatsRegistry {
    /// Creates an empty `StatsRegistry`.
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
        }
    }

    /// Registers or updates statistics for a table.
    pub fn set_table_stats(&mut self, stats: TableStatistics) {
        self.tables.insert(stats.table_name.clone(), stats);
    }

    /// Removes statistics for a table.
    pub fn remove_table_stats(&mut self, table_name: &str) -> Option<TableStatistics> {
        self.tables.remove(table_name)
    }

    /// Clears all registered statistics.
    pub fn clear(&mut self) {
        self.tables.clear();
    }

    /// Retrieves statistics for a table if registered.
    pub fn get_table_stats(&self, table_name: &str) -> Option<&TableStatistics> {
        self.tables.get(table_name)
    }

    /// Returns a map of all registered table statistics.
    pub fn tables(&self) -> &HashMap<String, TableStatistics> {
        &self.tables
    }
}

impl StatisticsProvider for StatsRegistry {
    fn get_table_stats(&self, table_name: &str) -> Option<&TableStatistics> {
        self.tables.get(table_name)
    }
}
