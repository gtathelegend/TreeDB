# TreeDB — Execution Correctness Under Pathological Workloads & Differential Stress Testing

> **Every optimizer transformation is considered correct only when it preserves the reference relational semantics.**

Phase 66 establishes the formal verification and differential stress testing framework for TreeDB's query optimization and execution engine. As TreeDB's optimizer memo, adaptive plan selection, physical properties, cost models, and spilling strategies grow in sophistication, execution semantics must remain inviolable:

$$\text{Optimized Physical Execution} \equiv \text{Reference Relational Execution}$$

across schema, projected names, types, NULL semantics, duplicates, multiset multiplicity, ordering constraints, LIMIT/OFFSET slicing, and transaction visibility.

---

## 1. Reference Execution Architecture

TreeDB provides a dedicated `ReferenceEvaluator` (`src/query/reference.rs`) designed intentionally as an independent, unoptimized, canonical relational oracle:
* **Decoupled from Optimizer Memo:** Completely bypasses memo exploration, alternative enumeration, cost-based access path selection, skip scans, and multi-index scans.
* **Straightforward Relational Semantics:**
  - Evaluates table scans via sequential base table scans (`table.scan(None, None)`).
  - Evaluates all joins via nested-loop algorithms conforming strictly to relational algebra and SQL three-valued logic (3VL).
  - Evaluates aggregations and groupings using simple in-memory hash buckets without streaming or spill optimizations.
  - Slices `LIMIT` and `OFFSET` in memory after full relation evaluation.
* **Reuses Shared Infrastructure:** Reuses the SQL parser, AST representations, binder type inference, transaction visibility, and catalog state, guaranteeing that binder rules remain identical while execution pathways diverge completely.

---

## 2. Differential Testing Framework

Differential testing compares the output of TreeDB's cost-based physical optimizer against the output of the reference evaluator for identical queries and transactional state:
1. **Schema Invariant:** Column count, inferred data types, and projected column names must match exactly.
2. **Multiplicity Invariant:** The exact multiset row counts and duplicate counts must be equal.
3. **Ordering Invariant:**
   - When an explicit `ORDER BY` clause is present, row sequences must match element-by-element in exact sort order.
   - For queries without `ORDER BY`, results are canonicalized into sorted multisets of `CanonicalRow` before asserting exact equality, preventing false failures caused by non-deterministic physical iteration order.

---

## 3. Canonical Row Representation & Float Semantics

Float and NULL comparison rules must not produce oracle inconsistencies:
* **CanonicalValue / CanonicalRow:** Encapsulates all TreeDB data types (`Null`, `Boolean`, `Int64`, `UInt64`, `Float64`, `String`, `Bytes`).
* **Deterministic Float Equivalence:** Implements total ordering for floating-point numbers:
  - `NaN == NaN` (canonical quiet NaN representation).
  - `-0.0 == +0.0` equivalence.
  - Subnormal, infinite, and standard float representations are mapped deterministically.
* **NULL Ordering:** Follows deterministic sort semantics where `NULL` is consistently grouped and ordered.

---

## 4. Randomized Workloads & Seed Reproducibility

Differential test suites employ deterministic pseudo-random generators (`seed = fixed`).
When a failure occurs, the framework captures and formats structured diagnostic reports (`DifferentialFailureReport`):
* **Seed Value:** Fixed seed allowing exact single-command reproduction.
* **Step Number:** Sequential operation index at failure.
* **Failing SQL Statement:** Exact SQL query executed.
* **Physical Execution Strategy:** Strategy chosen by the optimizer at execution time.
* **Error Message / Diff:** Detailed discrepancy between physical and reference results.

---

## 5. Pathological Data Distribution Families

TreeDB tests relational operators against pathological data distributions (`tests/execution_pathological.rs`):
* **Uniform:** High-cardinality, evenly distributed distinct keys across the domain.
* **Constant:** Identical values across all rows to stress degenerate hash tables and grouping buckets.
* **Zipf-like / Heavy-Tailed:** Few hyper-frequent values and a long tail of rare values, testing hash skew and cardinality estimation boundaries.
* **Alternating:** High-frequency alternating values (`'A'`, `'B'`, `'A'`, `'B'`).
* **Sparse / Mostly-NULL:** Columns containing $>90\%$ NULL values to stress NULL bitmap filtering, sparse indexes, and aggregate null handling.
* **Anti-Correlated:** Columns where $a$ increases while $b$ decreases, stressing multi-column correlation statistics and compound index scans.
* **Perfect Correlation:** Columns where $a = b$ to stress correlation estimators and redundant predicate elimination.

---

## 6. SQL Three-Valued Logic (3VL) & NULL Stress

SQL 3VL requires truth evaluations across `TRUE`, `FALSE`, and `NULL`:
* **Truth Tables:** Systematic stress of `AND`, `OR`, `NOT`, `IS NULL`, and `IS NOT NULL` matching ANSI SQL standard specifications:
  - `NULL AND FALSE == FALSE`, `NULL AND TRUE == NULL`, `NULL OR TRUE == TRUE`, `NULL OR FALSE == NULL`.
  - `NOT NULL == NULL`.
* **IN and NOT IN with NULLs:**
  - `x IN (...)` returns `NULL` (falsy in WHERE) when no match is found but a NULL is present in the set.
  - `x NOT IN (...)` returns `NULL` (falsy in WHERE) if any element in the subquery or list is NULL and no match exists.
* **Join Equivalence with NULLs:** Outer joins and inner joins never match `NULL = NULL`, properly preserving unmatched rows into outer null-padded positions.

---

## 7. JOIN Strategy Differential Testing

TreeDB validates all supported join topologies and physical strategies:
* **Join Topologies:** `INNER`, `LEFT OUTER`, `RIGHT OUTER`, `FULL OUTER`, `SEMI`, and `ANTI` joins.
* **Pathological Topologies:** Empty left relation, empty right relation, both empty, 1:1, 1:N, N:1, and N:M duplicate keys.
* **Outer Join Safety:** Predicate pushdown barriers ensure predicates in `ON` clauses vs `WHERE` clauses are never conflated, preventing incorrect outer-to-inner conversions when filtering null-padded rows.
* **Strategy Invariance:** Results remain identical regardless of whether the optimizer selects Nested Loop, Hash Join, External Hash Join, Merge Join, or Index Join.

---

## 8. Aggregation & Window Function Semantics

* **Aggregate Semantics:** Verified for `COUNT(*)`, `COUNT(col)`, `SUM`, `AVG`, `MIN`, and `MAX` across empty tables, all-NULL inputs, large integers, and mixed datasets.
* **Aggregate Strategies:** In-memory `HashAggregate`, streaming `StreamingAggregate`, and spilled `ExternalAggregate` produce identical results.
* **Window Functions:** `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, and `LAST_VALUE` tested across partitions, single-row partitions, and duplicate order keys.
* **Window Ordering Independence:** Window internal `ORDER BY` is verified to operate independently of query root `ORDER BY`.

---

## 9. Recursive CTE Stress & Cycle Detection

* **Termination Boundaries:** Recursive queries evaluated near iteration limits ($0, 1, 5, 1000$).
* **Cycle Prevention:** Self-referential infinite loops and cycles are deterministically detected and rejected via recursion iteration limits without stack overflows or memory exhaustion.
* **Resource Cleanup:** Temporary state allocated across recursive iterations is cleaned up deterministically upon query completion or error.

---

## 10. Set Operations & Multiset Semantics

* Relational set operations `UNION`, `UNION ALL`, `INTERSECT`, `INTERSECT ALL`, `EXCEPT`, and `EXCEPT ALL` verified against multiset definitions.
* Handled correctly across identical inputs, completely disjoint inputs, duplicate rows, and NULL values.

---

## 11. Access Path Equivalence

For any query accessing indexed tables:
$$\text{FullTableScan} \equiv \text{PrimaryKeyScan} \equiv \text{SecondaryIndexScan} \equiv \text{CompositeIndexScan} \equiv \text{IndexOnlyScan} \equiv \text{IndexSkipScan}$$
All index mutations (INSERT, UPDATE, DELETE, CREATE INDEX, DROP INDEX) maintain index synchronization with base heap records.

---

## 12. Spill & Resource Stress Differential Testing

Execution correctness is invariant to runtime memory configurations:
* Queries executed under large memory budgets ($64\text{ MB}$), moderate budgets ($1\text{ MB}$), and constrained budgets ($256\text{ bytes}$) produce identical canonical output.
* Forced external spilling in:
  - External Merge Sort (`SortExecutionOperator`)
  - External Hash Join (`ExternalHashJoiner`)
  - External Hash Aggregation (`ExternalAggregator`)
  - Common Subplan Materialization (`MaterializeOperator`)
  produces identical results to in-memory execution.

---

## 13. Failure Injection & Cancellation Safety

* **Failure Injection:** Simulated resource limit violations (memory reservation exceedance, temporary disk quota exceedance) fail cleanly without memory leaks or state corruption.
* **Cooperative Cancellation:** Pull-based operators honor `ExecutionContext::cancel()`, aborting downstream iteration immediately, unwinding pipelines, releasing resource reservations, and leaving the transaction in a consistent state.

---

## 14. Plan Cache & Adaptive Replanning Invariance

* **Plan Cache Validation:** Cached plans are validated against catalog generation and resource signatures. Schema or index changes immediately invalidate stale cached plans.
* **Adaptive Replanning:** Selective feedback updates and EWMA statistics shifts alter candidate physical costs over successive executions without changing relational output semantics.
* **Plan Fingerprint Stability:** Identical queries with identical catalog generations and statistics produce deterministic physical plan fingerprints.

---

## 15. Benchmark Suite & Methodology

TreeDB maintains standard micro-benchmarks (`tests/execution_failure_injection.rs`) evaluating operator throughput under unoptimized and optimized profiles:
* **Operators Covered:** Full Table Scan, Filtered Scan, Primary/Secondary Index Scan, Top-N Heap Selection, In-Memory/External Sort, Hash Aggregation.
* **Metrics Tracked:** Median execution latency, processed row count, throughput (rows/sec), memory reservation, and spill bytes.

---

## Summary of Acceptance Criteria

All tests in `tests/execution_differential.rs`, `tests/execution_pathological.rs`, and `tests/execution_failure_injection.rs` pass with zero failures and zero warnings, confirming that TreeDB's optimization pipeline satisfies the core invariant: **Optimizer sophistication never changes SQL semantics.**
