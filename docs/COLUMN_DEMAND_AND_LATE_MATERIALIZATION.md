# Phase 59: Cost-Based Column Demand, Projection Pruning & Late Materialization

## Overview

Phase 59 of TreeDB introduces **global column demand (`RequiredColumns`)** into the physical properties framework (`PhysicalProperties` / `RequiredPhysicalProperties`), enabling:
1. **Projection Pruning**: Safe elimination of unreferenced table columns from physical scan streams and intermediate operators.
2. **Demand-Aware Covering Index Selection**: Multi-column index eligibility evaluating complete query column demand (projections, filters, joins, groupings, window specs, and ordering).
3. **Cost-Based Late Materialization**: Safe two-stage execution for single-table secondary index scans that evaluates index-covered filters and applies row-goal / limit bounds on index candidate entries before fetching full base records from the `RecordStore`.
4. **Column-Aware Materialization & Spilling**: Reducing materialization tuple width and disk spill volume for common subplans, sort, hash join, and aggregation.

---

## 1. Global Column Demand (`RequiredColumns`)

`RequiredColumns` is a canonical, deterministic representation of column requirements built upon `BTreeSet<String>` to prevent non-deterministic EXPLAIN outputs:

```rust
pub struct RequiredColumns {
    pub columns: BTreeSet<String>,
}
```

### Key Operations & Semantics
- `empty()` / `all()`: Constructs empty demand or wildcard `*` (all columns).
- `from_expr(&Expr)` / `from_exprs(...)`: Autoritative column reference walker across `Column`, `OuterColumn`, `Binary`, `Unary`, `InList`, and correlated `Subquery` outer references.
- `from_sort_keys(&[SortKey])`, `from_aggregates(...)`, `from_window_exprs(...)`.
- `union(&self, other: &Self)` / `intersect(&self, other: &Self)`.
- `is_required_for_table(&self, qualifier: &str, table_name: &str, col_name: &str) -> bool`: Safe column lookup supporting both qualified (`users.name`) and unqualified (`name`) references.

---

## 2. Operator Column Demand Propagation

Every physical operator participates in column demand propagation:

| Operator | Child Demand Formula |
|---|---|
| **TableScan** | Requests only $C_{\text{required}} = C_{\text{parent}} \cap \text{Schema}(T)$ |
| **Filter** | $C_{\text{child}} = C_{\text{parent}} \cup \text{Columns}(\text{Predicate})$ |
| **Projection** | $C_{\text{child}} = \bigcup_{e \in \text{RequiredOutputs}} \text{Columns}(e)$ |
| **Sort / TopN** | $C_{\text{child}} = C_{\text{parent}} \cup \bigcup_{k \in \text{SortKeys}} \text{Columns}(k)$ |
| **Limit / Offset** | $C_{\text{child}} = C_{\text{parent}}$ |
| **Aggregation** | $C_{\text{child}} = \text{Columns}(\text{GroupBy}) \cup \text{Columns}(\text{AggArgs}) \cup \text{Columns}(\text{Having})$ |
| **Window** | $C_{\text{child}} = C_{\text{parent}} \cup \text{Columns}(\text{PartitionBy}) \cup \text{Columns}(\text{OrderBy}) \cup \text{Columns}(\text{WindowArgs})$ |
| **Joins** | $C_{\text{left}} = C_{\text{parent, left}} \cup \text{Columns}(\text{JoinOn})_{\text{left}}$; $C_{\text{right}} = C_{\text{parent, right}} \cup \text{Columns}(\text{JoinOn})_{\text{right}}$ |
| **Semi / Anti Join** | $C_{\text{right}} = \text{Columns}(\text{JoinKeys})_{\text{right}} \cup \text{Columns}(\text{ResidualFilter})_{\text{right}}$ (no payload columns) |

---

## 3. Covering Index Integration

Phase 41 introduced index-only scans; Phase 59 generalizes covering index eligibility to verify against the complete query column demand:

$$\text{Index-Only Eligible} \iff C_{\text{required}}(T) \subseteq \text{Columns}(\text{Index}) \cup \{\text{PrimaryKey}\}$$

If all required columns, filter predicates, and sort columns exist in the secondary or composite index, TreeDB selects `IndexOnlyScan`, avoiding all base `RecordStore` page lookups.

---

## 4. Late Materialization Execution Flow

When an index does not cover all required output columns but covers selective predicates:

```text
1. Secondary Index Leaf Scan
       ↓
2. Decode partial index row (covered columns only)
       ↓
3. Evaluate index-covered predicate(s)
       ↓
4. Retain surviving RecordIds (candidate rows reduced)
       ↓
5. Apply Row-Goal / Early Termination bound
       ↓
6. Fetch base RecordStore tuples ONLY for surviving RecordIds
       ↓
7. Evaluate base-record residual predicates
       ↓
8. Produce projected output
```

### Safety Barriers
Late materialization is conservatively applied only when:
- Single-table SELECT access path.
- Deterministic predicates with no volatile expressions.
- Safe boundaries (not across outer join null-padding, set operations, or recursive CTE steps).

---

## 5. Cost Model Enhancements

1. **Estimated Row Width**:
   $$\text{RowWidth}(S, C_{\text{req}}) = \sum_{c \in C_{\text{req}}} \text{EstimatedWidth}(c.dataType)$$
2. **Late Materialization Cost**:
   $$\text{Cost}_{\text{late}} = \text{Cost}_{\text{index\_scan}} + (N_{\text{survivors}} \times \text{FETCH\_COST}) + (\text{RowWidth} \times N_{\text{survivors}} \times \text{BYTE\_DECODE\_COST})$$
3. **Column-Aware Materialization**:
   Materialization size and spill decisions scale directly with projected column width rather than full table width.

---

## 6. EXPLAIN & EXPLAIN ANALYZE Output

### EXPLAIN Output
```text
TableScan users
  Access Path: SecondaryIndexScan (idx_users_status)
  Required Columns: [users.id, users.name, users.status, users.age]
  Pruned Columns: [users.score, users.bio, users.created_at, users.extra_1, users.extra_2]
  Covered Columns: [status]
  Late Materialization: true
```

### EXPLAIN ANALYZE Counters
```text
TableScan users (SecondaryIndexScan idx_users_status)
  columns_required: 4
  columns_pruned: 5
  late_materialization_enabled: true
  candidate_rows: 50
  rows_fetched: 5
  base_records_fetched: 5
  base_records_avoided: 45
```

---

## 7. Scope & Non-Goals

Phase 59 does NOT introduce:
- Columnar physical storage format on disk.
- Changes to WAL, page layout, or tuple binary serialization.
- Arbitrary late materialization through general outer joins.
- Parallel execution.
