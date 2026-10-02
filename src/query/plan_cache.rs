use std::collections::{HashMap, HashSet};

use crate::engine::types::Value;
use crate::query::operator::CanonicalValue;
use crate::query::optimizer::{AccessPath, PhysicalPlan};

/// Default maximum number of physical plans retained in the plan cache.
pub const DEFAULT_PLAN_CACHE_CAPACITY: usize = 128;

/// Parameter planning signature capturing the canonical parameter values that affect query planning.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParamPlanningSignature(pub Vec<CanonicalValue>);

impl ParamPlanningSignature {
    /// Constructs a parameter planning signature from a slice of runtime `Value`s.
    pub fn from_values(values: &[Value]) -> Self {
        Self(values.iter().map(CanonicalValue::from_value).collect())
    }

    /// Constructs an empty parameter planning signature.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Returns `true` if this signature has no parameters.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of parameter entries in this signature.
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Normalized resource planning signature capturing execution-memory and spill configurations.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourcePlanningSignature {
    /// In-memory execution budget in bytes.
    pub memory_budget_bytes: usize,
    /// Maximum number of temporary files open simultaneously.
    pub max_temp_files: usize,
    /// Maximum disk space in bytes for temporary runs.
    pub max_temp_bytes: usize,
    /// Maximum number of merge/partition passes allowed.
    pub max_merge_passes: usize,
    /// Specific sort buffer budget in bytes.
    pub sort_budget: usize,
    /// Specific aggregation buffer budget in bytes.
    pub agg_budget: usize,
    /// Specific hash join buffer budget in bytes.
    pub join_budget: usize,
    /// Specific multi-index buffer budget in bytes.
    pub multi_index_budget: usize,
}

impl ResourcePlanningSignature {
    /// Creates a uniform resource planning signature.
    pub const fn new(
        memory_budget_bytes: usize,
        max_temp_files: usize,
        max_temp_bytes: usize,
        max_merge_passes: usize,
    ) -> Self {
        Self {
            memory_budget_bytes,
            max_temp_files,
            max_temp_bytes,
            max_merge_passes,
            sort_budget: memory_budget_bytes,
            agg_budget: memory_budget_bytes,
            join_budget: memory_budget_bytes,
            multi_index_budget: memory_budget_bytes,
        }
    }

    /// Constructs a resource planning signature from specific subsystem configurations.
    pub fn from_configs(
        sort_cfg: &crate::query::external_sort::SortConfig,
        agg_cfg: &crate::query::external_aggregate::AggregateConfig,
        join_cfg: &crate::query::external_hash_join::JoinConfig,
        multi_idx_cfg: &crate::query::external_multi_index::MultiIndexConfig,
    ) -> Self {
        let min_budget = sort_cfg
            .memory_budget_bytes
            .min(agg_cfg.memory_budget_bytes)
            .min(join_cfg.memory_budget_bytes)
            .min(multi_idx_cfg.memory_budget_bytes);
        Self {
            memory_budget_bytes: min_budget,
            max_temp_files: sort_cfg.max_temp_files,
            max_temp_bytes: sort_cfg.max_temp_bytes,
            max_merge_passes: sort_cfg.max_merge_passes,
            sort_budget: sort_cfg.memory_budget_bytes,
            agg_budget: agg_cfg.memory_budget_bytes,
            join_budget: join_cfg.memory_budget_bytes,
            multi_index_budget: multi_idx_cfg.memory_budget_bytes,
        }
    }
}

/// Strongly typed, deterministic cache key for prepared statement physical execution plans.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PlanCacheKey {
    /// Normalized SQL text of the prepared statement.
    pub sql: String,
    /// Parameter-sensitive planning signature.
    pub param_signature: ParamPlanningSignature,
    /// Catalog/schema generation at plan compile time.
    pub catalog_generation: u64,
    /// Index state generation at plan compile time.
    pub index_generation: u64,
    /// Statistics registry generation at plan compile time.
    pub statistics_generation: u64,
    /// Resource planning signature (isolating memory-budget and spill configurations).
    pub resource_signature: Option<ResourcePlanningSignature>,
}

impl PlanCacheKey {
    /// Creates a new `PlanCacheKey`.
    pub fn new(
        sql: impl Into<String>,
        param_signature: ParamPlanningSignature,
        catalog_generation: u64,
        index_generation: u64,
        statistics_generation: u64,
    ) -> Self {
        Self {
            sql: sql.into(),
            param_signature,
            catalog_generation,
            index_generation,
            statistics_generation,
            resource_signature: None,
        }
    }

    /// Sets the resource planning signature for this cache key.
    pub fn with_resource_signature(
        mut self,
        resource_signature: Option<ResourcePlanningSignature>,
    ) -> Self {
        self.resource_signature = resource_signature;
        self
    }
}

/// Validity metadata associated with a cached physical execution plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanValidity {
    /// Schema/catalog generation at compile time.
    pub catalog_generation: u64,
    /// Index state generation at compile time.
    pub index_generation: u64,
    /// Statistics registry generation at compile time.
    pub statistics_generation: u64,
    /// Table relations referenced by the physical plan.
    pub referenced_tables: Vec<String>,
    /// Secondary indexes referenced by the physical plan.
    pub referenced_indexes: Vec<String>,
    /// Resource planning signature valid for this plan.
    pub resource_signature: Option<ResourcePlanningSignature>,
}

impl PlanValidity {
    /// Creates a new `PlanValidity` descriptor.
    pub fn new(
        catalog_generation: u64,
        index_generation: u64,
        statistics_generation: u64,
        referenced_tables: Vec<String>,
        referenced_indexes: Vec<String>,
    ) -> Self {
        Self {
            catalog_generation,
            index_generation,
            statistics_generation,
            referenced_tables,
            referenced_indexes,
            resource_signature: None,
        }
    }

    /// Sets the resource planning signature for this validity descriptor.
    pub fn with_resource_signature(
        mut self,
        resource_signature: Option<ResourcePlanningSignature>,
    ) -> Self {
        self.resource_signature = resource_signature;
        self
    }

    /// Validates whether this cached plan remains valid against the active database generations.
    pub fn is_valid(
        &self,
        current_catalog_gen: u64,
        current_index_gen: u64,
        current_stats_gen: u64,
    ) -> bool {
        self.catalog_generation == current_catalog_gen
            && self.index_generation == current_index_gen
            && self.statistics_generation == current_stats_gen
    }

    /// Validates whether this cached plan remains valid against active generations and resource configuration.
    pub fn is_valid_with_resources(
        &self,
        current_catalog_gen: u64,
        current_index_gen: u64,
        current_stats_gen: u64,
        current_resource_sig: Option<&ResourcePlanningSignature>,
    ) -> bool {
        if !self.is_valid(current_catalog_gen, current_index_gen, current_stats_gen) {
            return false;
        }
        match (&self.resource_signature, current_resource_sig) {
            (Some(expected), Some(actual)) => expected == actual,
            (None, None) => true,
            (Some(_), None) => false,
            (None, Some(_)) => true,
        }
    }
}

/// An entry in the physical plan cache.
#[derive(Debug, Clone)]
pub struct PlanCacheEntry {
    /// Cached physical execution plan.
    pub plan: PhysicalPlan,
    /// Validity assumptions recorded when the plan was compiled.
    pub validity: PlanValidity,
    /// Logical access sequence counter used for deterministic LRU replacement.
    pub last_accessed: u64,
}

/// Diagnostic telemetry recorded by the plan cache subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanCacheStats {
    /// Total number of successful cache lookups.
    pub hits: usize,
    /// Total number of cache misses.
    pub misses: usize,
    /// Total number of entries evicted due to capacity limits.
    pub evictions: usize,
    /// Current number of active entries in the cache.
    pub entries: usize,
    /// Maximum capacity of the cache.
    pub capacity: usize,
}

/// A runtime-only, bounded, invalidation-aware physical plan cache.
#[derive(Debug, Clone)]
pub struct PlanCache {
    capacity: usize,
    entries: HashMap<PlanCacheKey, PlanCacheEntry>,
    access_counter: u64,
    stats: PlanCacheStats,
}

impl Default for PlanCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanCache {
    /// Creates a new `PlanCache` with default capacity (`DEFAULT_PLAN_CACHE_CAPACITY = 128`).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_PLAN_CACHE_CAPACITY)
    }

    /// Creates a new `PlanCache` with a configurable capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = if capacity == 0 { 1 } else { capacity };
        Self {
            capacity,
            entries: HashMap::with_capacity(capacity),
            access_counter: 0,
            stats: PlanCacheStats {
                hits: 0,
                misses: 0,
                evictions: 0,
                entries: 0,
                capacity,
            },
        }
    }

    /// Looks up a cached physical plan matching the key and verifies validity against active generations.
    pub fn get(
        &mut self,
        key: &PlanCacheKey,
        current_catalog_gen: u64,
        current_index_gen: u64,
        current_stats_gen: u64,
    ) -> Option<PhysicalPlan> {
        self.access_counter += 1;
        let counter = self.access_counter;

        if let Some(entry) = self.entries.get_mut(key) {
            if entry.validity.is_valid_with_resources(
                current_catalog_gen,
                current_index_gen,
                current_stats_gen,
                key.resource_signature.as_ref(),
            ) {
                entry.last_accessed = counter;
                self.stats.hits += 1;
                return Some(entry.plan.clone());
            } else {
                self.entries.remove(key);
                self.stats.entries = self.entries.len();
            }
        }
        self.stats.misses += 1;
        None
    }

    /// Inserts a newly compiled physical plan into the cache.
    pub fn insert(&mut self, key: PlanCacheKey, plan: PhysicalPlan, validity: PlanValidity) {
        self.access_counter += 1;

        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.evict_lru();
        }

        self.entries.insert(
            key,
            PlanCacheEntry {
                plan,
                validity,
                last_accessed: self.access_counter,
            },
        );
        self.stats.entries = self.entries.len();
    }

    /// Performs deterministic LRU eviction of the oldest accessed entry.
    fn evict_lru(&mut self) {
        if let Some((oldest_key, _)) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_accessed)
            .map(|(k, e)| (k.clone(), e.clone()))
        {
            self.entries.remove(&oldest_key);
            self.stats.evictions += 1;
            self.stats.entries = self.entries.len();
        }
    }

    /// Removes an entry from the plan cache.
    pub fn remove(&mut self, key: &PlanCacheKey) -> Option<PlanCacheEntry> {
        let entry = self.entries.remove(key);
        self.stats.entries = self.entries.len();
        entry
    }

    /// Clears all entries from the plan cache.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.stats.entries = 0;
    }

    /// Resets diagnostic statistics counters.
    pub fn reset_stats(&mut self) {
        self.stats = PlanCacheStats {
            hits: 0,
            misses: 0,
            evictions: 0,
            entries: self.entries.len(),
            capacity: self.capacity,
        };
    }

    /// Adjusts the maximum capacity of the plan cache, evicting excess entries if needed.
    pub fn set_capacity(&mut self, new_capacity: usize) {
        let new_capacity = if new_capacity == 0 { 1 } else { new_capacity };
        self.capacity = new_capacity;
        self.stats.capacity = new_capacity;
        while self.entries.len() > self.capacity {
            self.evict_lru();
        }
    }

    /// Invalidates all cached plans referencing the specified table name.
    pub fn invalidate_table(&mut self, table_name: &str) {
        self.entries.retain(|_, entry| {
            !entry
                .validity
                .referenced_tables
                .iter()
                .any(|t| t == table_name)
        });
        self.stats.entries = self.entries.len();
    }

    /// Invalidates all cached plans referencing the specified index name.
    pub fn invalidate_index(&mut self, index_name: &str) {
        self.entries.retain(|_, entry| {
            !entry
                .validity
                .referenced_indexes
                .iter()
                .any(|i| i == index_name)
        });
        self.stats.entries = self.entries.len();
    }

    /// Invalidates all cached plans referencing the specified table's statistics.
    pub fn invalidate_statistics(&mut self, table_name: &str) {
        self.invalidate_table(table_name);
    }

    /// Invalidates all schema-dependent plans.
    pub fn invalidate_schema(&mut self) {
        self.clear();
    }

    /// Invalidates all plans in the cache.
    pub fn invalidate_all(&mut self) {
        self.clear();
    }

    /// Returns diagnostic cache statistics.
    pub fn stats(&self) -> PlanCacheStats {
        self.stats
    }

    /// Returns the current number of cached plan entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the cache contains zero entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the maximum capacity of the cache.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Extracts all table and secondary index names referenced by a `PhysicalPlan`.
pub fn extract_plan_references(plan: &PhysicalPlan) -> (Vec<String>, Vec<String>) {
    let mut tables = HashSet::new();
    let mut indexes = HashSet::new();
    collect_references_from_plan(plan, &mut tables, &mut indexes);
    let mut tables_vec: Vec<String> = tables.into_iter().collect();
    let mut indexes_vec: Vec<String> = indexes.into_iter().collect();
    tables_vec.sort();
    indexes_vec.sort();
    (tables_vec, indexes_vec)
}

fn collect_references_from_plan(
    plan: &PhysicalPlan,
    tables: &mut HashSet<String>,
    indexes: &mut HashSet<String>,
) {
    if let Some(set_op) = &plan.set_op {
        collect_references_from_plan(&set_op.left, tables, indexes);
        collect_references_from_plan(&set_op.right, tables, indexes);
        return;
    }

    if !plan.base_table.table_name.is_empty() {
        tables.insert(plan.base_table.table_name.clone());
    }
    match &plan.base_table.access_path {
        AccessPath::SecondaryIndexScan { index_name, .. }
        | AccessPath::IndexOnlyScan { index_name, .. } => {
            indexes.insert(index_name.clone());
        }
        AccessPath::MultiIndexScan { child_scans, .. } => {
            for child in child_scans {
                indexes.insert(child.index_name.clone());
            }
        }
        _ => {}
    }

    for join in &plan.joins {
        if !join.right.table_name.is_empty() {
            tables.insert(join.right.table_name.clone());
        }
        match &join.right.access_path {
            AccessPath::SecondaryIndexScan { index_name, .. }
            | AccessPath::IndexOnlyScan { index_name, .. } => {
                indexes.insert(index_name.clone());
            }
            AccessPath::MultiIndexScan { child_scans, .. } => {
                for child in child_scans {
                    indexes.insert(child.index_name.clone());
                }
            }
            _ => {}
        }
        if let Some(idx_name) = &join.index_name {
            if idx_name != "__primary__" {
                indexes.insert(idx_name.clone());
            }
        }
    }

    for cte in &plan.ctes {
        match &cte.kind {
            crate::sql::BoundCteKind::NonRecursive { plan } => {
                collect_references_from_plan(plan, tables, indexes);
            }
            crate::sql::BoundCteKind::Recursive {
                anchor_plan,
                recursive_plan,
                ..
            } => {
                collect_references_from_plan(anchor_plan, tables, indexes);
                collect_references_from_plan(recursive_plan, tables, indexes);
            }
        }
    }
}
