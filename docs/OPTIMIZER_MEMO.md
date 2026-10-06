# Property-Aware Optimizer Memo & Global Physical Alternative Management

## 1. Overview and Rationale

TreeDB has evolved a mature cost-based query optimization architecture featuring:
* Primary B+ Tree range and point scans
* Persistent secondary, composite, and covering indexes
* Cost-based access-path selection and index skip-scans
* Multi-index scans with intersection/union bitmap combining
* Histograms, correlation statistics, and adaptive runtime feedback
* Required physical property propagation (Ordering, Required Columns, Row Goals, Resource Signatures)
* Dynamic programming / greedy join enumeration with outer and semi/anti barriers
* Query rewriting, subquery decorrelation, and predicate inference
* Streaming and hash aggregations, external sorting, and common subplan materialization

Prior to Phase 60, physical alternatives (such as Full Scan vs Index Scan vs Covering Index Scan vs Skip Scan vs Hash Join vs Merge Join) were generated and evaluated through separate, disparate optimizer pathways. Furthermore, common subplan materialization previously used access-path details inside its logical fingerprint.

Phase 60 introduces a **bounded, deterministic property-aware optimizer memo** (`OptimizerMemo`) that unifies physical alternative management while maintaining clean separation between logical expressions and physical implementations.

```text
SQL
 ↓
Binder
 ↓
Rewrite / Inference
 ↓
Logical Plan
 ↓
┌─────────────────────────────┐
│        Optimizer Memo       │
│                             │
│ Logical Groups              │
│      ↓                      │
│ Physical Alternatives       │
│      ↓                      │
│ Required Properties         │
│      ↓                      │
│ Cost / Cardinality          │
└─────────────────────────────┘
 ↓
Physical Plan
 ↓
Execution
```

---

## 2. Logical Identity vs Physical Identity

A core architectural principle established in Phase 60 is the strict separation between:
1. **Logical Identity (`LogicalOp`, `LogicalSubplanFingerprint`)**:
   - Represents *what* relational operation is being performed.
   - Characterized by table relations, filters/predicates, projections, joins, grouping expressions, aggregates, window functions, and set operations.
   - Independent of chosen index, access path, sort algorithm, join strategy, or materialization mode.
2. **Physical Identity (`PhysicalAlternative`, `BestPlan`)**:
   - Represents *how* the operation is executed given specific physical requirements.
   - Encapsulates exact access paths (e.g. `IndexScan(idx_users_age)` vs `FullTableScan`), join strategies (`HashJoin`, `MergeJoin`, `NestedLoopJoin`), sort enforcement (`Sort`, `TopN`), and materialization modes (`Inline`, `Materialize`, `ExternalMaterialize`).

---

## 3. Memo Groups and Equivalence Classes

The memo models relational expressions as equivalence classes (`MemoGroup`):

```rust
pub struct GroupId(pub u32);

pub struct MemoGroup {
    pub id: GroupId,
    pub logical_op: LogicalOp,
    pub alternatives: Vec<PhysicalAlternative>,
    pub alternative_fingerprints: BTreeSet<String>,
    pub best_plans: BTreeMap<PhysicalPropertyKey, BestPlan>,
}
```

* **Group Identification**: Each distinct logical operation is assigned a unique `GroupId`. Deduplication is performed deterministically via `LogicalOp::fingerprint()`.
* **Physical Alternative Pool**: Each group holds all valid physical operator alternatives discovered during access-path generation and subplan planning.
* **Property-Indexed Best Plans**: Optimal plans are cached per `PhysicalPropertyKey`.

---

## 4. Physical Properties and Property-Aware Optimization

Optimization requests carry a tuple of physical constraints:
* **Required Ordering (`OrderingProperty`)**: Column sort keys, ascending/descending order, nulls first/last.
* **Required Columns (`RequiredColumns`)**: The set of column names needed by parent operators or consumers.
* **Row Goal (`RowGoal`)**: Candidate bounded row limit and offset for early termination planning.
* **Resource Signature (`ResourcePlanningSignature`)**: Memory budgets and spill parameters.

### Property Satisfaction & Enforcement
When evaluating physical alternatives in a group:
1. **Direct Satisfaction**: If an alternative naturally provides the required ordering (e.g., secondary index scan on `(created_at)`) or required columns (e.g., covering index), it is costed without extra operators.
2. **Enforcement**: If an alternative does not provide the required property, an enforcer operator (e.g., `Sort` or `TopN`) is added and its cost is incorporated into the candidate total.
3. **Pruning**: Alternatives that cannot supply the required columns or violate hard resource budgets are filtered out.

---

## 5. Bounded Search & Determinism

The memo is strictly bounded to guarantee safety and deterministic latency:

```rust
pub struct MemoConfig {
    pub max_groups: usize,
    pub max_alternatives_per_group: usize,
    pub max_property_variants: usize,
    pub max_optimization_iterations: usize,
}
```

### Deterministic Tie-Breaking
When comparing candidate implementations, TreeDB uses a stable, multi-tier tie-breaker:
1. Lower estimated cost (with epsilon tolerance $10^{-6}$)
2. Lower estimated output cardinality
3. Lower examined candidate row count
4. Covering / Index-only scan preference
5. Lexicographical alternative description ordering

---

## 6. Cost Dominance & Deduplication

* **Alternative Deduplication**: Physical alternatives are fingerprinted canonically (`PhysicalAlternative::fingerprint`) to prevent redundant duplicate evaluations.
* **Pareto Dominance**: Plans that satisfy identical physical properties with strictly higher costs and resource requirements are dominated and pruned.

---

## 7. Common Subplan & Plan Cache Integration

* **Subplan Materialization**: `LogicalSubplanFingerprint` identifies reusable common subplans (CTEs) based entirely on relational structure, resolving the Phase 58 caveat. The memo evaluates `Inline` vs `Materialize` vs `ExternalMaterialize` per consumer property request.
* **Outer Plan Cache**: Prepared statements use the outer plan cache across statement executions. The optimizer memo is instantiated per query planning session and is purely in-memory.

---

## 8. Safe Fallback

If any configured threshold (`max_groups`, `max_alternatives_per_group`, `max_property_variants`, or `max_optimization_iterations`) is exceeded during query optimization:
1. `memo.stats.limits_reached` is flagged as `true`.
2. The optimizer safely falls back to the deterministic baseline physical planner.
3. Query execution succeeds without failure, prioritizing correctness over exhaustive search.

---

## 9. Limitations & Future Work

* **Cascades Transformations**: Optimization is rule-directed and property-driven rather than an open unbounded Cascades search tree.
* **Bushy Join Trees**: DP search remains capped at $\le 8$ relations with greedy fallback for larger joins.
* **No Format Changes**: In-memory only; no WAL, page, or catalog disk format changes.
