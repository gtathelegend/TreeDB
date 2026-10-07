# Physical Plan Canonicalization, Fingerprinting & Deterministic Plan Identity

> **A physical fingerprint identifies execution strategy, not execution history.**

> **Cost, statistics, feedback observations, and temporary runtime state are not physical identity unless they change the physical strategy itself.**

---

## 1. Overview & Core Philosophy

TreeDB separates three distinct relational identity concepts across its compilation, optimization, and execution pipeline:

```text
Logical Identity
      ↓
Physical Identity (Fingerprint)
      ↓
Execution Instance Identity
```

1. **Logical Identity**: Answers whether two relational expressions represent the same logical relation (used by logical equivalence memo groups, rewrite rules, and common subexpression deduplication).
2. **Physical Identity**: Answers whether two plans represent the same physical execution strategy and runtime semantics (used by physical plan cache diagnostics, memo alternative deduplication, plan validation, runtime feedback association, and reproducible `EXPLAIN`).
3. **Execution Instance Identity**: Identifies a specific execution of a physical plan at runtime (encompassing concrete parameter values, buffer pool hit rates, elapsed wall-clock microseconds, temporary file paths, and execution telemetry).

---

## 2. Canonical Physical Plan Representation

The canonicalizer serializes an arbitrary `PhysicalPlan` into a stable, deterministic textual encoding:

```text
PhysicalPlan
     │
     ▼
Canonicalizer ───► Canonical String ───► FNV-1a 128 Digest ───► PhysicalPlanFingerprint
```

### Invariants:
* **Independence of Debug Output**: Formats never rely on Rust `Debug` or `Display` implementations that could change incidentally.
* **Independence of Memory Addresses**: Pointer addresses, heap allocations, and `Rc`/`Arc` identities are strictly excluded.
* **Independence of Hash Order**: Collections with set semantics (e.g. required columns, covered columns, pruned columns) are lexicographically sorted and deduplicated.
* **Commutativity Bounds**: Set intersection and union across index child scans are commutative and deterministically sorted; sequence-dependent pipelines (e.g., join trees, projections, sort keys) strictly preserve execution order.

---

## 3. Fingerprint Representation & Versioning

```rust
pub const PHYSICAL_PLAN_FINGERPRINT_VERSION: u32 = 1;

#[derive(Clone, Eq)]
pub struct PhysicalPlanFingerprint {
    version: u32,
    digest: u128,
    canonical: String,
}
```

* **Version**: Guard constant `PHYSICAL_PLAN_FINGERPRINT_VERSION = 1`. If canonical encoding changes in future TreeDB releases, the version increments to prevent accidental conflation.
* **Digest**: 128-bit FNV-1a hash (`fnv1a_128`) providing collision resistance and fast `O(1)` comparisons (`Eq`, `Hash`, `Ord`).
* **Canonical String**: The complete canonical serialized plan representation retained in-memory for zero-collision verification, logging, and EXPLAIN diagnostics.
* **Persistence Policy**: Fingerprints are transient runtime data structures and are never persisted to database disk files (`.treedb`).

---

## 4. Canonicalization Rules by Subsystem

### 4.1 Expressions & Literals
* **Literals**: Deterministically typed:
  * Signed (`i64(x)`) and unsigned (`u64(x)`) integers remain distinct.
  * Floating-point numbers:
    * `+0.0` is serialized as `f64(+0.0)`.
    * `-0.0` is serialized as `f64(-0.0)`.
    * Infinities: `f64(+Inf)` and `f64(-Inf)`.
    * NaNs: All NaN bit patterns normalize to canonical `f64(NaN)`.
    * Finite numbers: Serialized via `f64::to_bits()` into a 16-character hexadecimal string, ensuring portable bitwise invariance.
  * Strings: Escaped character representations (`str("...")`).
  * Bytes: Hexadecimal byte streams (`bytes(...)`).
  * Null: Canonical `null`.
* **Parameters**: Represented by positional placeholder identifier (e.g. `PARAM($1)` vs `PARAM($2)`). Concrete parameter bound values are strictly excluded from the structural fingerprint.
* **Operations**: Binary and unary operators preserve exact operand sequence; algebraic transformations (such as `a - b` vs `b - a`) are never applied.
* **Subqueries**: Canonicalized recursively via subquery kind and child physical plan. Pre-evaluated materialized cache results are excluded.

### 4.2 Table Scans & Access Paths
* **Table Scans**: Encodes table name, optional table alias, access path, late materialization flag, index covered filter, base record filter, residual filter, row goal, and early termination flag.
* **Access Paths**:
  * `FullTableScan`: Unindexed sequential scan.
  * `PrimaryPointLookup`: Key value lookup.
  * `PrimaryRangeScan`: Start and end bound values.
  * `SecondaryIndexScan`: Stable index name, index ID, and `SecondaryScanType`.
  * `IndexOnlyScan`: Index name, index ID, sorted covered columns, and scan type.
  * `MultiIndexScan`: Strategy (`Intersection` or `Union`), execution mode (`InMemory` or `External`), and deterministically sorted child scans.
  * `EmptyResult`: Contradiction / zero-row shortcut.
* **Secondary Scan Types**: `PointLookup`, `CompositePointLookup`, `PrefixScan`, `RangeScan`, and `SkipScan` (with sorted constrained columns).
* **Column Demand**: `required_columns`, `covered_columns`, and `pruned_columns` are sorted and deduplicated to enforce set equivalence.

### 4.3 Joins
* **Join Strategies**: `NestedLoop`, `Hash`, `SpilledHash`, `Merge`, and `IndexExistenceProbe`.
* **Join Types**: `Inner`, `Left`, `Right`, `Full`, `Semi`, `Anti`, and `Cross` remain distinguishable.
* **Join Ordering**: Left and right child structures, probe/build sides, sort requirements, row goals, and early stop phase are strictly preserved.
* **Runtime Filters**: Filter type (`ExactSet` vs `Bloom`), scope (`Global` vs `Partition`), and join key expressions are encoded; runtime bit arrays and observed key counts are excluded.

### 4.4 Aggregation & Windows
* **Aggregation Strategies**: `Hash`, `Streaming(sort=...)`, and `External`.
* **Group By & Aggregates**: Grouping expressions and aggregate function forms (`COUNT(*)`, `SUM(x)`, etc.) are canonicalized in evaluation sequence.
* **Window Expressions**: Encodes window function kind, argument expressions, partition by expressions, order by sort keys, frame bounds (`ROWS BETWEEN start AND end`), offset, and default expressions.

### 4.5 Ordering, Projections, and Row Goals
* **Order By**: List of sort keys preserving expression, direction (`Ascending` vs `Descending`), and NULL placement (`NULLS FIRST` vs `NULLS LAST`).
* **Projections**: Exact projection expression sequence and column aliases.
* **Pagination**: `LIMIT` and `OFFSET` constants.
* **Row Goals**: Candidate rows examined, required output rows, and offset.

### 4.6 CTEs & Materialization
* **CTEs**: CTE name, dependencies, and kind:
  * `NonRecursive`: Decision (`Inline`, `Materialize`, `ExternalMaterialize`), materialize ID, and inner plan.
  * `Recursive`: Anchor plan, recursive member plan, maximum iterations, and maximum rows.
* **Temporary Files**: Ephemeral temporary file paths and runtime file descriptors are strictly excluded.

### 4.7 Resource Planning Signatures
* When planning is resource-aware, the resource signature is incorporated:
  * `memory_budget_bytes`, `max_temp_files`, `max_temp_bytes`, `max_merge_passes`
  * `sort_budget`, `agg_budget`, `join_budget`, `multi_index_budget`

---

## 5. What is Deliberately Excluded

The physical plan fingerprint identifies **execution strategy**, not execution metrics or costs. The following are intentionally omitted:

* **Estimated Cost**: `total_cost` and per-operator cost estimates.
* **Estimated Cardinalities**: `estimated_rows`, `estimated_rows_examined`, `estimated_qualifying_rows`.
* **Optimizer Diagnostics**: `logical_rewrites` and `inferred_predicates`.
* **Runtime Execution State**: Buffer pool frame hits, temporary filenames, wall-clock time, rows processed, Bloom filter bit contents.
* **Bound Parameter Values**: Runtime values bound to `$1`, `$2`.

---

## 6. System Integrations

### 6.1 Optimizer Memo
* `PhysicalAlternative` stores `plan_fingerprint: PhysicalPlanFingerprint`.
* `MemoGroup::add_alternative` detects duplicate alternatives within a logical group using the canonical physical plan fingerprint and metadata description, preventing redundant memo slot occupation.

### 6.2 Plan Cache
* `PlanCacheEntry` records `fingerprint: PhysicalPlanFingerprint`.
* Plan retrieval continues to enforce monotonic generation counters (`catalog_generation`, `index_generation`, `statistics_generation`) and resource signatures for cache invalidation. The fingerprint serves as an immutable identity and diagnostic mechanism.

### 6.3 Runtime Adaptive Feedback
* `FeedbackKey` incorporates an optional `physical_fingerprint: Option<PhysicalPlanFingerprint>`, enabling runtime observations to be attributed to the specific physical strategy that produced them.
* Re-accumulated runtime observations do not alter the physical fingerprint of the underlying plan.

### 6.4 EXPLAIN Diagnostics
* `Transaction::explain_plan_fingerprint` and `TreeDb::explain_plan_fingerprint` return the deterministic fingerprint for any query.
* `PhysicalPlanValidator::format_validation_diagnostics_with_fingerprint` and `explain_sql_with_fingerprint` format output with a prominent header:
  ```text
  Physical Plan Fingerprint: <hex_digest>
  <formatted plan>
  Validation: OK
  ```

---

## 7. Collision Handling

1. **128-Bit FNV-1a Hash**: The 128-bit digest provides a negligible collision probability across trillions of distinct plans.
2. **Canonical String Fallback**: In `PartialEq` and `Ord`, both the `digest` and the full `canonical` string are compared:
   ```rust
   self.version == other.version
       && self.digest == other.digest
       && self.canonical == other.canonical
   ```
3. Semantic equality never relies on the digest alone when exact structural equality is required.

---

## 8. Testing Strategy

TreeDB validates physical plan fingerprinting through comprehensive unit, integration, and differential tests in `tests/physical_plan_fingerprint.rs`:

1. **Basic Identity & Determinism**: Verifies idempotence and repeatability across successive generations.
2. **Safe Metadata Commutativity**: Verifies that reordering required/covered/pruned columns or multi-index child scans does not change the fingerprint.
3. **Execution Distinction Coverage**: Proves that modifying access paths, sort directions, null placement, pagination, join types, join strategies, aggregation algorithms, window frames, or parameter positions changes the fingerprint.
4. **Edge Case Floating-Point Determinism**: Tests signed zero (`-0.0` vs `+0.0`), infinity (`-Inf` vs `+Inf`), and NaN normalization.
5. **Cost/Cardinality Invariance**: Verifies that changing estimated cost or row estimates does not alter physical plan identity.
6. **Integration Tests**: Tests memo deduplication, plan cache entries, adaptive feedback keys, EXPLAIN diagnostics, and post-recovery stability.
7. **Randomized Differential Mutations**: Executes 250+ randomized plan variations verifying determinism, safe commutativity, and sensitivity to semantic mutations.
