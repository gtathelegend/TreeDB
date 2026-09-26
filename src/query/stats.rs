use std::collections::HashMap;
use std::fmt;

use crate::engine::types::Value;

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
        }
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
    /// Number of distinct index keys.
    pub distinct_key_count: usize,
    /// Number of `NULL` index entries.
    pub null_count: usize,
}

impl IndexStatistics {
    /// Creates a new `IndexStatistics` record.
    pub fn new(indexed_row_count: usize, distinct_key_count: usize, null_count: usize) -> Self {
        Self {
            indexed_row_count,
            distinct_key_count,
            null_count,
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
}

impl TableStatistics {
    /// Creates a new `TableStatistics` record.
    pub fn new(table_name: impl Into<String>, row_count: usize) -> Self {
        Self {
            table_name: table_name.into(),
            row_count,
            columns: HashMap::new(),
            indexes: HashMap::new(),
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

    /// Retrieves statistics for a specific column.
    pub fn column(&self, name: &str) -> Option<&ColumnStatistics> {
        self.columns.get(name)
    }

    /// Retrieves statistics for a specific secondary index.
    pub fn index(&self, name: &str) -> Option<&IndexStatistics> {
        self.indexes.get(name)
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
