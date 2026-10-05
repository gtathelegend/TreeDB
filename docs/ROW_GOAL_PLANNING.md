# Row-Goal Planning & Selectivity-Aware Ordered Access (Phase 56)

## 1. Executive Summary

Phase 56 introduces a **first-class physical row-goal / bounded-output planning abstraction** and **selectivity-aware ordered access costing** into TreeDB's query optimizer.

Prior to Phase 56, an ordered index scan satisfying `ORDER BY indexed_col LIMIT K` with a residual filter `WHERE selective_predicate` assumed the engine would only need to examine $\approx K$ index entries. However, if the residual predicate has very low selectivity (e.g. 0.001%), the engine might have to examine millions of index entries and fetch millions of base records before finding $K$ qualifying rows.

With Phase 56, the physical planner:
1. Expresses the parent's candidate row requirements explicitly via `RowGoal` and `RequiredPhysicalProperties`.
2. Computes the expected entries to examine using existing histogram and correlation statistics:
   $$\text{expected\_entries} = \min\left(\left\lceil \frac{K}{\text{selectivity}} \right\rceil, N\right)$$
3. Accurately costs ordered index scans vs candidate alternatives (selective index + TopN, full scan + TopN, covering index scan + early termination).
4. Tracks runtime diagnostics (`rows_examined`, `rows_qualified`, `early_terminated`, `row_goal_satisfied`) in EXPLAIN / EXPLAIN ANALYZE and feeds observed selectivity into adaptive replanning.

---

## 2. Architecture & Concept Overview

### 2.1 What is a Row Goal?

A **Row Goal** is a physical planning requirement that represents:
> *"The parent execution node only needs up to $K$ qualifying rows from this child subtree to complete its computation."*

where:
$$K = \text{OFFSET} + \text{LIMIT}$$
when both are bounded.

```
SQL LIMIT / OFFSET
       ↓
   Row Goal (K = Offset + Limit)
       ↓
Required Physical Properties (Ordering + Row Goal)
       ↓
Optimizer Access Path Selection
   ↙                   ↓                    ↘
Ordered Index Scan    Selective Index + TopN   Full Scan + TopN
   ↓                   ↓                    ↓
Residual Filter       Residual Filter       Residual Filter
   ↓                   ↓                    ↓
Early Termination     TopN Buffer           TopN Buffer
```

### 2.2 Difference Between SQL LIMIT and Physical Row Goal

* **SQL `LIMIT L OFFSET O`**: A relational operator specifying that the query result should skip $O$ rows and return at most $L$ rows from the ordered stream.
* **Physical `RowGoal { candidate_rows: K, required_output_rows: L, offset: O }`**: Planning metadata attached to `RequiredPhysicalProperties` informing candidate access paths how many rows are expected to be consumed, enabling early-termination cost scaling without altering relational semantics.

---

## 3. Safe vs Unsafe Propagation Rules

### 3.1 Safe Propagation Boundaries

A row goal may safely propagate through operators where consuming fewer rows from the child does not change the parent's semantic result:

1. **Filter**: Receives the row goal $K$ from the parent. Because filtering reduces cardinality with selectivity $S$, the child must produce $\approx K / S$ candidate rows. The optimizer estimates entries examined accordingly, clamped to total relation size $N$.
2. **Projection**: Preserves the row goal directly (cardinality is unchanged).
3. **Limit / Offset**: Directly generates or scales the row goal: $K = O + L$.
4. **TopN**: Consumes the bounded row goal and operates within a bounded priority queue of size $K$.
5. **Ordered Scans**: Consume the row goal and evaluate expected entries examined and early termination.

### 3.2 Unsafe / Conservative Boundaries (Semantic Barriers)

Row goals are **strictly prohibited** from propagating through:
* `INNER JOIN`, `LEFT JOIN`, `RIGHT JOIN`, `FULL JOIN`, `SEMI JOIN`, `ANTI JOIN` (no arbitrary LIMIT pushdown through joins).
* `GROUP BY` and Aggregations (all input rows must be grouped before ranking/limiting).
* `WINDOW` functions (window frames and partitions depend on full partition scans).
* `CTE` materialization barriers and Recursive CTEs.
* `SET` operations (`UNION`, `INTERSECT`, `EXCEPT`).

---

## 4. Selectivity-Aware Cost Model

### 4.1 Expected Entries Examined

For an ordered scan with row goal $K$, residual predicate selectivity $S \in [0, 1]$, and table population $N$:

$$\text{expected\_entries}(K, S, N) = \begin{cases} 
\min(K, N) & \text{if } S \ge 1.0 \\
N & \text{if } S \le 0.0 \\
\min\left(\left\lceil \frac{K}{S} \right\rceil, N\right) & \text{if } 0 < S < 1.0 
\end{cases}$$

### 4.2 Ordered Scan Cost Formula

$$\text{Cost}_{\text{ordered}} = \text{TraverseCost} + \text{entries} \times (\text{IndexDecodeCost} + \text{BaseFetchCost} + \text{PredicateCost})$$

* For **Covering Indexes** (`IndexOnlyScan`), $\text{BaseFetchCost} = 0$, significantly reducing execution cost.
* For **Primary Key Scans**, $\text{BaseFetchCost} = 0$ since records are co-located in the primary B+ tree.
* For **Secondary Index Scans**, $\text{BaseFetchCost} = \text{entries} \times \text{TUPLE\_FETCH\_COST}$.

---

## 5. Candidate Access Path Comparison

When optimizing `SELECT ... WHERE pred ORDER BY indexed_col LIMIT K`, the optimizer compares:

1. **Path A: Ordered Index Scan**
   * Cost $\approx \text{IndexTraverse} + \text{expected\_entries} \times (\text{Decode} + \text{Fetch} + \text{Filter})$.
   * Sort cost = $0$.
   * **Best when**: $S$ is high (e.g. $> 10\%$).

2. **Path B: Selective Index + TopN**
   * Cost $\approx \text{IndexTraverse} + (N \times S) \times (\text{Decode} + \text{Fetch}) + \text{TopN}(N \times S, K)$.
   * **Best when**: Predicate has a dedicated selective index (e.g. point lookup $S = 0.001$).

3. **Path C: Full Table Scan + TopN**
   * Cost $\approx N \times (\text{SequentialPageRead} + \text{TupleCPU} + \text{Filter}) + \text{TopN}(N \times S, K)$.
   * **Best when**: $S$ is very low, no secondary index exists on the predicate, avoiding millions of random secondary index lookups.

4. **Path D: Covering Index Scan**
   * Evaluates residual filters directly from the included index payload, avoiding all base tuple fetches.

---

## 6. Runtime Feedback & Diagnostics

### 6.1 EXPLAIN & EXPLAIN ANALYZE

* **EXPLAIN** outputs explicit row-goal planning properties:
  ```text
  TableScan (table: orders, access: SecondaryIndexScan(idx_created_at), filter: [status = 999])
    cost: 15.20, estimated_rows: 10, Row Goal: 10, Estimated Rows Examined: 1000, Estimated Qualifying Rows: 10, Early Termination: possible
  ```
* **EXPLAIN ANALYZE** outputs runtime counters:
  ```text
  actual_rows: 5, row_goal: 5, rows_examined: 50, rows_qualified: 5, index_entries_examined: 50, base_records_fetched: 50, early_terminated: true, row_goal_satisfied: true
  ```

### 6.2 Adaptive Replanning

* `FeedbackKey::ordered_scan(table_name, index_name, filter, row_goal, param_sig, stats_gen)` records runtime observations.
* When actual entries examined exceed optimizer estimates by $> 2\times$ under hysteresis rules, the plan cache invalidates the entry and triggers re-optimization.
