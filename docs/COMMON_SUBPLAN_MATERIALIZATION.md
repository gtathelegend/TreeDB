# Common Subplan Materialization and Reuse

## Overview

In relational query engines, complex queries often evaluate the exact same subplan multiple times. Common examples include non-recursive Common Table Expressions (CTEs) referenced by multiple joins, identical derived tables in `FROM` clauses, and identical uncorrelated subqueries in filters or projections. 

Without subplan reuse, each consumer must redundantly re-execute the child subtree (scans, filters, joins, aggregations, windows, sorts). This leads to redundant CPU overhead, excessive I/O, and duplicated resource contention.

TreeDB's **Cost-Based Common Subplan Reuse & Materialization** engine provides a deterministic, transaction-correct, memory-budget-aware physical planning layer that recognizes reusable intermediate subplans and makes cost-optimal decisions:
- **`Inline`**: Re-execute the subplan per consumer when subplan execution is cheaper than writing to and reading from intermediate storage (or when there is only a single consumer).
- **`Materialize`**: Execute the subplan once, store the intermediate tuple stream in-memory, and serve multiple consumers via zero-copy / shared references.
- **`ExternalMaterialize`**: When estimated intermediate rows exceed the memory budget, stream tuples to a temporary binary disk storage (`.tmp` file) with deterministic binary encoding and RAII cleanup.

---

## Architecture

```text
                     Logical Subplan (CTE / Subquery)
                                    ↓
                       Subplan Fingerprinting
                                    ↓
                    Cost Comparison & Resource Budget
                       ├── Inline (Re-evaluate per consumer)
                       └── Materialize / ExternalMaterialize
                                    ↓
                       MaterializeOperator
                     (Execute child once, store)
                                    ↓
                         Shared Storage
                      (Memory / Spilled File)
                        ↙        ↓        ↘
                  Consumer #1  Consumer #2  Consumer #3
```

---

## 1. Why Repeated Subplans Are Expensive

When a CTE or derived table is evaluated independently by $N$ consumers:
1. **CPU & I/O Multiplication**: A 100,000-row table scan with an expensive predicate executed 3 times requires $3\times$ CPU evaluations and table lookups.
2. **Buffer Pool Pressure**: Repeated scans evict other hot pages from the buffer cache.
3. **Execution Latency**: Serial query execution scales linearly with consumer count $O(N \cdot \text{Cost})$.

By evaluating once and materializing into a compact temporary representation, execution time scales as $O(\text{Cost} + \text{WriteCost} + N \cdot \text{ReadCost})$, yielding substantial speedups when $N \ge 2$ and subplan cost is significant.

---

## 2. Canonical Subplan Identity & Fingerprinting

Subplan reuse requires proving that two subqueries produce identical tuples under identical environments.

`SubplanFingerprint` generates a canonical, deterministic hash of the plan structure:
- Base table name and access path (e.g. `SeqScan`, `IndexScan(idx)`).
- Residual filter expressions.
- Joins (join type, join table name, ON conditions).
- Post-join filters.
- GROUP BY expressions, aggregate functions, and HAVING clauses.
- Window expressions and functions.
- Output projection expressions and aliases.
- ORDER BY keys, directions, and nulls ordering.
- Limit and Offset pagination parameters.
- Physical set operations (`UNION`, `INTERSECT`, `EXCEPT`).

Deterministic hashing prevents memory address or map iteration nondeterminism from affecting fingerprint identity.

---

## 3. Cost-Based Decision Model

The optimizer compares the total estimated cost of inlining against materialization:

$$\text{Inline Total Cost} = \text{Subplan Cost} \times \text{Consumer Count}$$

$$\text{Materialize Total Cost} = \text{Subplan Cost} + \text{Write Cost} + (\text{Read Cost} \times \text{Consumer Count})$$

Where:
- $\text{Write Cost} = \text{Rows} \times (\text{MATERIALIZE\_WRITE\_CPU\_COST} + \text{Spill IO Cost if External})$
- $\text{Read Cost} = \text{Rows} \times (\text{MATERIALIZE\_READ\_CPU\_COST} + \text{Spill IO Cost if External})$

### Decision Rules:
1. **Single Consumer ($N \le 1$)**: Always choose `Inline` (materialization write/read overhead provides no benefit).
2. **Multiple Consumers ($N \ge 2$)**: Choose `Materialize` (or `ExternalMaterialize`) when $\text{Materialize Total Cost} < \text{Inline Total Cost}$.
3. **Memory Budget**: If $\text{Estimated Rows} \times \text{Row Width} > \text{Memory Budget}$, select `ExternalMaterialize` to spill safely to disk.

---

## 4. Correctness & Semantic Invariants

Common subplan materialization preserves full SQL semantics:
- **Row Multiplicity**: Duplicate rows are strictly preserved (no implicit `DISTINCT` deduplication).
- **NULL Semantics**: NULL values and 3-valued logic (3VL) are preserved across all columns.
- **Column Types & Order**: Projected schema, column names, and type definitions match original child outputs.
- **Statement-Local Scope**: Materialized results exist strictly within a single statement execution.
- **Transaction Boundaries**: Results are never shared across transactions, connections, or database handles.

---

## 5. Ordering & Physical Properties

Materialization acts as a pipeline barrier. 
- In-memory materialization preserves insertion order produced by the child plan.
- However, the optimizer remains conservative: if a consumer requires a specific `ORDER BY`, it will only omit an explicit sort if the materialized producer explicitly guaranteed that physical ordering property.
- Consumer-specific sorting requirements are never pushed down into a shared producer if other consumers require different orderings.

---

## 6. Row Goals & Limit Interaction

When a query has a row goal (such as `LIMIT K`):
- If there is a single consumer, row goals can push down through inline subplans for early termination.
- When multiple consumers exist with different row goals (e.g. Consumer A needs `LIMIT 10`, Consumer B needs all 10,000 rows), the shared producer materializes the complete relation so all consumers can read their required subset safely.

---

## 7. Resource-Aware Materialization & Disk Spill

TreeDB integrates materialization with the resource planning engine:
- `MaterializeConfig`: Configures `memory_budget_bytes`, `max_temp_bytes`, and `temp_dir`.
- `TempMaterializeFile`: RAII temporary disk file with magic header `b"TMAT"` and format version `1`. Temporary files are guaranteed to be cleaned up on query completion, error, transaction abort, or panic.

---

## 8. Adaptive Runtime Feedback

Runtime materialization metrics are captured in `FeedbackContext::Materialization`:
- `estimated_rows` vs `actual_rows`
- `estimated_bytes` vs `actual_bytes`
- `spill_occurred` (memory vs disk spill)
- `consumer_count` and `reuse_reads`

If cardinality feedback reveals that a subplan produces orders of magnitude more or fewer rows than estimated, subsequent planning adapts between `Inline`, `Materialize`, and `ExternalMaterialize`.

---

## 9. EXPLAIN & EXPLAIN ANALYZE Diagnostics

### EXPLAIN Output:
```text
CTE: expensive_cte
  Consumers: 2
  Estimated Rows: 10000
  Estimated Bytes: 640 KB
  Decision: Materialize
  Estimated Inline Cost: 5000.00
  Estimated Materialize Cost: 2850.00
```

### EXPLAIN ANALYZE Output:
```text
-> Materialize #1 [expensive_cte] (actual rows=10000, bytes=640000, consumers=2, reads=2, reuse_reads=1, memory_peak=640000, time=4.2ms)
    -> SeqScan on large_table (actual rows=10000)
-> MaterializedScan #1 [expensive_cte] (actual rows=10000, reads=2)
```

---

## 10. Why Persistent Materialized Views Are Out of Scope

Persistent materialized views require background incremental maintenance, transaction log tailing, cross-statement invalidation, catalog persistence, and crash recovery. Phase 58 focuses exclusively on **in-memory, statement-local, cost-based common subplan reuse**, leaving persistent views decoupled from the core query planner.
