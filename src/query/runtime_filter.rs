use std::collections::HashSet;
use std::fmt;

use crate::query::cost::CostModel;
use crate::query::expr::Expr;
use crate::query::external_hash_join::{
    estimate_hash_entry_memory, hash_join_key, JoinConfig, JoinKey,
};
use crate::sql::ast::JoinType;

/// Scope of applicability for a runtime join filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFilterScope {
    /// Valid across all rows of the relation globally.
    Global,
    /// Valid only for a specific partition index in an external partitioned join.
    Partition(u32),
}

impl fmt::Display for RuntimeFilterScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => write!(f, "Global"),
            Self::Partition(p) => write!(f, "Partition({p})"),
        }
    }
}

/// Representation type of runtime filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFilterType {
    /// Exact hash set containing observed build-side keys. No false positives, no false negatives.
    ExactSet,
    /// Bitwise probabilistic Bloom filter with bounded memory and controlled false positive rate.
    Bloom,
}

impl fmt::Display for RuntimeFilterType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExactSet => write!(f, "ExactSet"),
            Self::Bloom => write!(f, "Bloom"),
        }
    }
}

/// Exact in-memory membership filter for join keys observed on the build side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactRuntimeFilter {
    keys: HashSet<JoinKey>,
    scope: RuntimeFilterScope,
    estimated_memory_bytes: usize,
}

impl ExactRuntimeFilter {
    /// Creates a new empty `ExactRuntimeFilter` with the specified scope.
    pub fn new(scope: RuntimeFilterScope) -> Self {
        Self {
            keys: HashSet::new(),
            scope,
            estimated_memory_bytes: std::mem::size_of::<Self>(),
        }
    }

    /// Inserts a join key into the runtime filter.
    ///
    /// SQL 3VL Semantics: Keys containing `NULL` are rejected and not stored,
    /// because `NULL = NULL` is UNKNOWN in equi-joins and cannot match.
    pub fn insert(&mut self, key: JoinKey) -> bool {
        if key.has_null() {
            return false;
        }
        let mem = estimate_hash_entry_memory(&key, &[]);
        if self.keys.insert(key) {
            self.estimated_memory_bytes += mem;
            true
        } else {
            false
        }
    }

    /// Tests if a probe key might match any build row.
    ///
    /// Returns `false` if the key is definitely absent (or contains `NULL`).
    /// For `ExactRuntimeFilter`, never produces false positives.
    pub fn contains(&self, key: &JoinKey) -> bool {
        if key.has_null() {
            return false;
        }
        self.keys.contains(key)
    }

    /// Number of distinct keys present in the filter.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the filter contains zero keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Estimated memory consumption in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.estimated_memory_bytes
    }

    /// Scope of this filter.
    pub fn scope(&self) -> RuntimeFilterScope {
        self.scope
    }
}

/// Bounded probabilistic Bloom filter for join keys with deterministic hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BloomRuntimeFilter {
    bits: Vec<u64>,
    num_bits: usize,
    num_hashes: usize,
    num_elements: usize,
    scope: RuntimeFilterScope,
}

impl BloomRuntimeFilter {
    /// Creates a new `BloomRuntimeFilter` with capacity sized for `expected_elements` and target false positive rate.
    pub fn new(expected_elements: usize, scope: RuntimeFilterScope) -> Self {
        // Aim for ~1% false positive rate -> m ≈ n * 10 bits, k ≈ 7 (or 4 for speed)
        let n = expected_elements.max(16);
        let num_bits = (n * 10).max(64);
        let num_words = num_bits.div_ceil(64);
        let num_hashes = 4;

        Self {
            bits: vec![0u64; num_words],
            num_bits: num_words * 64,
            num_hashes,
            num_elements: 0,
            scope,
        }
    }

    /// Inserts a join key into the Bloom filter.
    /// Keys containing `NULL` are ignored.
    pub fn insert(&mut self, key: &JoinKey) -> bool {
        if key.has_null() {
            return false;
        }
        for i in 0..self.num_hashes {
            let h = hash_join_key(key, i);
            let bit_idx = (h as usize) % self.num_bits;
            let word_idx = bit_idx / 64;
            let bit_offset = bit_idx % 64;
            self.bits[word_idx] |= 1u64 << bit_offset;
        }
        self.num_elements += 1;
        true
    }

    /// Tests if a probe key might match any build row.
    /// May produce false positives, but never false negatives for non-null keys.
    pub fn contains(&self, key: &JoinKey) -> bool {
        if key.has_null() {
            return false;
        }
        for i in 0..self.num_hashes {
            let h = hash_join_key(key, i);
            let bit_idx = (h as usize) % self.num_bits;
            let word_idx = bit_idx / 64;
            let bit_offset = bit_idx % 64;
            if (self.bits[word_idx] & (1u64 << bit_offset)) == 0 {
                return false;
            }
        }
        true
    }

    /// Number of elements inserted into the filter.
    pub fn len(&self) -> usize {
        self.num_elements
    }

    /// Whether the filter is empty.
    pub fn is_empty(&self) -> bool {
        self.num_elements == 0
    }

    /// Estimated memory consumption in bytes.
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.bits.len() * 8
    }

    /// Scope of this filter.
    pub fn scope(&self) -> RuntimeFilterScope {
        self.scope
    }
}

/// Unified runtime join filter enum supporting exact and probabilistic strategies.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeFilter {
    /// Exact hash set filter.
    ExactSet(ExactRuntimeFilter),
    /// Probabilistic Bloom filter.
    Bloom(BloomRuntimeFilter),
}

impl RuntimeFilter {
    /// Creates an exact set filter.
    pub fn exact(scope: RuntimeFilterScope) -> Self {
        Self::ExactSet(ExactRuntimeFilter::new(scope))
    }

    /// Creates a Bloom filter.
    pub fn bloom(expected_elements: usize, scope: RuntimeFilterScope) -> Self {
        Self::Bloom(BloomRuntimeFilter::new(expected_elements, scope))
    }

    /// Inserts a join key into the runtime filter.
    pub fn insert(&mut self, key: JoinKey) -> bool {
        match self {
            Self::ExactSet(e) => e.insert(key),
            Self::Bloom(b) => b.insert(&key),
        }
    }

    /// Checks if a probe join key could match the build side.
    pub fn contains(&self, key: &JoinKey) -> bool {
        match self {
            Self::ExactSet(e) => e.contains(key),
            Self::Bloom(b) => b.contains(key),
        }
    }

    /// Returns the active filter type.
    pub fn filter_type(&self) -> RuntimeFilterType {
        match self {
            Self::ExactSet(_) => RuntimeFilterType::ExactSet,
            Self::Bloom(_) => RuntimeFilterType::Bloom,
        }
    }

    /// Returns the scope of this filter.
    pub fn scope(&self) -> RuntimeFilterScope {
        match self {
            Self::ExactSet(e) => e.scope(),
            Self::Bloom(b) => b.scope(),
        }
    }

    /// Returns the estimated in-memory footprint in bytes.
    pub fn memory_bytes(&self) -> usize {
        match self {
            Self::ExactSet(e) => e.memory_bytes(),
            Self::Bloom(b) => b.memory_bytes(),
        }
    }

    /// Returns the number of distinct or inserted keys.
    pub fn distinct_keys(&self) -> usize {
        match self {
            Self::ExactSet(e) => e.len(),
            Self::Bloom(b) => b.len(),
        }
    }
}

/// Physical plan metadata for a planned runtime filter on a join operator.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeFilterPlan {
    /// Type of runtime filter (ExactSet or Bloom).
    pub filter_type: RuntimeFilterType,
    /// Scope of validity (Global or Partition).
    pub scope: RuntimeFilterScope,
    /// Join key expressions to be evaluated into the filter.
    pub key_exprs: Vec<Expr>,
}

impl fmt::Display for RuntimeFilterPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys_str = self
            .key_exprs
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "RuntimeFilter(type={}, scope={}, keys=[{}])",
            self.filter_type, self.scope, keys_str
        )
    }
}

/// Centralized decision engine evaluating whether to construct and execute a runtime filter.
pub fn should_build_runtime_filter(
    join_type: JoinType,
    equi_keys: &[Expr],
    build_rows: f64,
    probe_rows: f64,
    build_ndv: Option<f64>,
    memory_budget_bytes: usize,
    config: &JoinConfig,
) -> Option<RuntimeFilterPlan> {
    if !config.enable_runtime_filter {
        return None;
    }

    // Safety Invariant: Runtime filters are safe for INNER JOIN and SEMI JOIN.
    // For LEFT / RIGHT / FULL OUTER JOIN and ANTI JOIN, disabled to prevent incorrect preserved-row rejection.
    if join_type != JoinType::Inner && join_type != JoinType::Semi {
        return None;
    }

    if equi_keys.is_empty() || build_rows <= 0.0 || probe_rows <= 0.0 {
        return None;
    }

    // Estimate rejection ratio
    let rejection_ratio = if let Some(ndv) = build_ndv {
        if probe_rows > 0.0 {
            (1.0 - (ndv / probe_rows).clamp(0.01, 1.0)).max(0.0)
        } else {
            0.5
        }
    } else if build_rows < probe_rows {
        (1.0 - (build_rows / probe_rows)).clamp(0.1, 0.9)
    } else {
        0.1
    };

    let target_type = config.runtime_filter_type;

    if target_type == RuntimeFilterType::ExactSet {
        let estimated_exact_mem = (build_rows * 64.0) as usize;
        if estimated_exact_mem <= memory_budget_bytes {
            let build_cost = CostModel::runtime_filter_exact_build_cost(build_rows);
            let probe_cost = CostModel::runtime_filter_exact_probe_cost(probe_rows);
            let savings = CostModel::runtime_filter_expected_savings(probe_rows, rejection_ratio);

            if savings >= (build_cost + probe_cost) * 0.5 {
                return Some(RuntimeFilterPlan {
                    filter_type: RuntimeFilterType::ExactSet,
                    scope: RuntimeFilterScope::Global,
                    key_exprs: equi_keys.to_vec(),
                });
            }
        }
    }

    // If exact filter is too large or Bloom is explicitly selected
    let bloom_bits = (build_rows * 10.0).max(64.0);
    let bloom_mem = ((bloom_bits / 8.0) as usize) + 64;

    if bloom_mem <= memory_budget_bytes {
        let build_cost = CostModel::runtime_filter_bloom_build_cost(build_rows);
        let probe_cost = CostModel::runtime_filter_bloom_probe_cost(probe_rows);
        let savings = CostModel::runtime_filter_expected_savings(probe_rows, rejection_ratio);

        if savings >= (build_cost + probe_cost) * 0.5 {
            return Some(RuntimeFilterPlan {
                filter_type: if target_type == RuntimeFilterType::Bloom {
                    RuntimeFilterType::Bloom
                } else {
                    RuntimeFilterType::ExactSet
                },
                scope: RuntimeFilterScope::Global,
                key_exprs: equi_keys.to_vec(),
            });
        }
    }

    None
}
