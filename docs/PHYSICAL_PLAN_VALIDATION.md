# Physical Plan Validation & Optimizer/Executor Contract

> **Architectural Invariant:**
> The validator verifies a physical plan. It does not optimize, rewrite, repair, or execute the plan.
> No physical plan produced by the optimizer or reused from the plan cache may reach execution without satisfying its declared physical, schema, resource, and semantic contracts.

---

## 1. Why Physical-Plan Validation Exists

In database architectures with sophisticated query optimizers (such as TreeDB's cost-based optimizer, property-aware memo, multi-index intersection/union, and late materialization engines), physical optimization generates complex alternatives.

While the optimizer aims to construct valid plans satisfying required properties (such as ordering, column demand, row goals, and resource limits), the execution engine should **never blindly trust** physical plans. A software bug, stale catalog entry, misconfigured resource signature, or invalid cache reuse could otherwise cause:
* Segmentation faults, panic crashes, or out-of-bounds column accesses
* Silent data corruption or incorrect query results
* Flawed ordering assumptions in streaming aggregations or merge joins
* Infinite loops or broken early-termination logic
* Resource starvation and memory overcommit

`PhysicalPlanValidator` acts as an immutable safety barrier between optimization (and plan caching) and execution. If a plan violates its declared contract, validation rejects it with deterministic, structured diagnostics.

```text
SQL
 ↓
Lexer / Parser
 ↓
Binder
 ↓
Rewrite / Predicate Inference
 ↓
Optimizer Memo
 ↓
Physical Plan
 ↓
Physical Plan Validation   ← SAFETY BARRIER
 ↓
ValidatedPhysicalPlan
 ↓
Execution
 ↓
Buffer Pool / WAL / Storage
```

---

## 2. Optimizer/Executor Contract

The relationship between the optimizer and the execution engine is governed by a formal contract:

1. **Declared Property Truthfulness**: Every property advertised by a `PhysicalPlan` node (such as `provided_ordering`, `required_columns`, `row_goal`, `covered_columns`) must be mathematically true and guaranteed by the underlying operator semantics.
2. **Schema Compatibility**: For every operator in the execution tree, child output schemas must satisfy parent expected input schemas. Every column referenced by projections, filters, join conditions, aggregations, windows, and sorts must be available and type-compatible.
3. **Immutability of Proof**: Successful validation returns a `ValidatedPhysicalPlan<'a>`, wrapping `&'a PhysicalPlan`. Execution functions require a `ValidatedPhysicalPlan` or validate dynamically before running.
4. **No Plan Mutation**: The validator never rewrites predicates, picks alternative access paths, estimates cardinalities, or silently patches invalid plans. It only accepts or rejects.

---

## 3. Validation Context

Plan verification is parameterized by `PlanValidationContext`:

```rust
pub struct PlanValidationContext {
    pub tables: BTreeMap<String, TableValidationInfo>,
    pub resource_signature: Option<ResourcePlanningSignature>,
    pub plan_validity: Option<PlanValidity>,
    pub catalog_generation: u64,
    pub index_generation: u64,
    pub statistics_generation: u64,
    pub param_signature: Option<ParamPlanningSignature>,
    pub is_catalog_authoritative: bool,
}
```

* **Table Information**: Maps table names and aliases to schemas, primary keys, and secondary index metadata (`TableValidationInfo`).
* **Authoritativeness Boundary**:
  - In an authoritative context (such as transaction-level plan execution), any table referenced that is missing from the catalog is rejected immediately.
  - In a local optimizer context (where only query-block tables are populated), subplan validation verifies local schemas and properties without rejecting unlisted external tables.
* **Generations**: Carries active catalog, index, and statistics generation numbers to detect cache staleness.
* **Resource Signature**: Carries memory budget, temp files, temp bytes, and merge pass limits to reject impossible runtime configurations.

---

## 4. Operator Schema Validation

Every physical operator undergoes recursive structural validation:

1. **Child Count & Shape**:
   - Leaf scans: validated against catalog table schemas.
   - Joins: left input validated, right scan validated; schemas combined into join output schema.
   - Set operations: left and right subplans validated recursively; column counts and data types must match.
2. **Expression Type Checking**:
   - `WHERE`, `HAVING`, and residual filters must evaluate to `DataType::Boolean` (or nullable boolean).
   - Equi-join predicates must have type-compatible left and right operands.
3. **Column Scoping**:
   - Projections can only reference columns exposed by the underlying select pipeline.
   - Unqualified column names resolve against single table scopes; qualified names resolve against matching qualifiers.

---

## 5. Ordering Property Validation

TreeDB relies on Phase 54's physical properties infrastructure (`OrderingProperty`). The validator enforces:

1. **Primary Key Scans**:
   - `PrimaryPointLookup` provides empty ordering (single row).
   - `PrimaryRangeScan` or PK-ordered full scans provide ascending primary key ordering.
2. **Secondary Index Scans**:
   - Single-column index scans provide index key ordering.
   - Composite index scans over `(c1, c2, c3)` provide prefixes `(c1)`, `(c1, c2)`, or `(c1, c2, c3)`. They cannot provide `(c2)` or `(c1, c3)` without prefix constraints.
   - `SkipScan` cannot falsely advertise global ordering over skipped columns.
   - `MultiIndexScan` (index intersection or union) yields unsorted row ID streams and cannot advertise index ordering.
3. **Merge Join**:
   - Both left and right inputs must provide the exact ordering required by the equi-join keys, or explicit sorting flags (`left_sort`, `right_sort`) must be enabled.
4. **Streaming Aggregate**:
   - When `AggregateStrategy::Streaming { sort_required: false }` is chosen, the input stream ordering must prove that rows are already grouped by the `GROUP BY` expressions.
5. **Sort / Top-N**:
   - Sort operators provide the exact `order_by` ordering.
   - Set operations conservatively advertise `None` unless an explicit post-set sort is planned.

---

## 6. Required-Column & Column-Demand Validation

TreeDB integrates Phase 59 column demand:

1. **Subset Invariant**: `required_columns ⊆ available_columns`.
2. **Disjointness**: A column cannot be marked as both `required` and `pruned`.
3. **Index-Only Scans**:
   - Every column in `required_columns` must be a subset of the index's covered payload (key columns + `INCLUDE` columns).
   - If any required column is missing from the payload, validation fails with `RequiredColumnsViolation`.
4. **Late Materialization**:
   - Columns evaluated during index pre-filtering must exist in the index payload.
   - Residual base columns must exist in the base table schema for delayed row retrieval.

---

## 7. Row-Goal Validation

TreeDB integrates Phase 56/57 bounded row goals:

1. **Candidate Consistency**: `candidate_rows >= required_output_rows`.
2. **Pagination Relationship**: When LIMIT and OFFSET are specified, `candidate_rows = offset + limit`.
3. **Early Termination Invariants**:
   - An operator claiming `early_termination_possible = true` must have an active `row_goal`.
   - Early termination is forbidden on outer joins (`LEFT`, `RIGHT`, `FULL`) where unmatched rows must still be preserved.
   - Unordered scans, multi-index scans, and skip scans cannot claim early termination.

---

## 8. Access Path Validation

Every access path variant is verified against catalog definitions:

* **Table Existence**: Target table must exist in the active catalog or CTE map.
* **Index Ownership**: Secondary indexes referenced by `SecondaryIndexScan`, `IndexOnlyScan`, `IndexProbe`, or `MultiIndexScan` must belong to the target table.
* **Key Type Compatibility**: Point lookups and range boundaries must match the data type of the indexed columns.
* **Multi-Index Scans**:
  - Maximum index count cannot exceed `MAX_MULTI_INDEXES` (8).
  - All constituent indexes must belong to the same underlying table.

---

## 9. Join Validation

Physical joins (`PhysicalJoin`) are verified across all algorithms:

1. **Relational Types**:
   - `INNER`: Validated equi-keys and residual conditions.
   - `LEFT` / `RIGHT` / `FULL`: Correct nullability propagation on outer columns.
   - `SEMI` / `ANTI`: Output schema strictly contains only the outer/left-side columns. Any inner columns in output are rejected.
2. **Join Strategies**:
   - `NestedLoop`: ON expression resolvable over combined schema.
   - `Hash`: Equi-join keys present, resolvable, and type-compatible.
   - `IndexProbe`: Right table must have the referenced index, or primary key (`"__primary__"`).
   - `Merge`: Equi-keys present, sort direction and ordering verified on both children.
3. **Runtime Filters**:
   - Permitted only on `INNER` and `SEMI` joins.
   - Strictly forbidden on `LEFT`, `RIGHT`, `FULL`, and `ANTI` joins to prevent erroneous row elimination.

---

## 10. Aggregation & Window Validation

1. **Aggregations**:
   - Grouping expressions must reference available pre-aggregate columns.
   - Aggregate function arguments must exist in input schema.
   - Return types must conform to function specification (`COUNT(*)` -> `UInt64`, `SUM` -> numeric, etc.).
   - `HAVING` predicates must reference valid grouping expressions or aggregates.
2. **Window Functions**:
   - Partition expressions and ORDER BY expressions must resolve against the input schema.
   - Frame bounds (`ROWS BETWEEN ... AND ...`) must be well-formed and non-negative.
   - Window expressions cannot be nested inside each other.

---

## 11. Materialization & CTE Validation

1. **CTE Schemas**:
   - Subplan producer output schema must match the consumer's expected CTE schema in column count, types, and names.
2. **Recursive CTEs**:
   - `max_iterations > 0` and `max_rows > 0`.
   - Anchor plan and recursive member plan validated independently.
   - Output schema of recursive member must match anchor schema.
3. **Common Subplans**:
   - Materialization decision (Inline vs Materialize) and materialized ID verified.

---

## 12. Resource Validation

Hardware and memory constraints are checked for physical realism:

* `memory_budget_bytes > 0`
* `max_temp_files > 0`
* `max_temp_bytes > 0`
* `max_merge_passes > 0`
* Operator-specific budgets (`sort_budget`, `agg_budget`, `join_budget`, `multi_index_budget`) cannot exceed the global memory budget.

---

## 13. Plan-Cache Validation

When a prepared statement execution retrieves a physical plan from the plan cache:

1. `catalog_generation` must match the current catalog generation.
2. `index_generation` must match the current index generation.
3. `statistics_generation` must match the current statistics generation.
4. `resource_signature` must be compatible with active engine configuration.

If any generation or signature differs, the cached plan is rejected with `InvalidPlanCacheEntry`, prompting recompilation.

---

## 14. Memo & Execution Boundaries

* **Memo Boundary**: During cost-based optimization (`optimize_with_memo`), the selected best physical plan is validated before being returned. Invalid alternatives are rejected.
* **Execution Boundary**: In `Transaction::execute_physical_plan`, `execute_physical_plan_with_outer`, and `execute_physical_plan_with_metrics`, plans are validated against an authoritative context before operator evaluation commences.
* **Validated Proof Type**: `ValidatedPhysicalPlan<'a>` provides the type-safe contract boundary ensuring validation cannot be silently bypassed.

---

## 15. Error Taxonomy

`PlanValidationError` provides deterministic, structured errors with detailed diagnostics:

```rust
pub enum PlanValidationError {
    MissingColumn { operator, column, available, reason },
    InvalidColumnType { operator, column, expected, actual, reason },
    SchemaMismatch { operator, expected, actual, reason },
    OrderingViolation { operator, expected, actual, reason },
    RequiredColumnsViolation { operator, missing_columns, available_columns, reason },
    RowGoalViolation { operator, expected, actual, reason },
    InvalidAccessPath { table, access_path, reason },
    InvalidIndexReference { table, index, reason },
    InvalidCoveringPayload { table, index, missing_columns, covered_columns, reason },
    InvalidLateMaterialization { table, reason },
    InvalidJoin { operator, join_type, reason },
    InvalidAggregate { operator, reason },
    InvalidWindow { operator, reason },
    InvalidSetOperation { operator, reason },
    InvalidMaterialization { cte_name, reason },
    InvalidRuntimeFilter { operator, reason },
    InvalidResourceConfiguration { reason },
    InvalidPlanCacheEntry { reason },
    InvalidOperatorContract { operator, reason },
}
```

Format example:
```text
Plan validation failed:
Operator: MergeJoin
Property: Ordering
Expected: Left input ordered by [user_id ASC]
Actual: Left input ordered by [created_at ASC]
Reason: Merge join equi-keys require matching child stream ordering
```

---

## 16. Testing Strategy

TreeDB maintains a comprehensive validation test suite in `tests/plan_validation.rs`:

1. **Basic Access Paths**: Valid full scans, primary key scans, secondary index scans, composite scans, and covering scans are accepted.
2. **Column Demand Violations**: Missing columns, invalid projections, and covering payload omissions are rejected.
3. **Ordering Violations**: False ordering claims, unsorted streaming aggregates, and broken merge joins are rejected.
4. **Row-Goal Violations**: Contradictory candidate bounds, mismatched pagination limits, and early termination on outer joins are rejected.
5. **Join Violations**: Foreign index references, runtime filters on left joins, and merge join ordering mismatches are rejected.
6. **Aggregate & Window Violations**: Unresolvable grouping, aggregate, partition, or frame expressions are rejected.
7. **Set Operations & CTE Violations**: Schema mismatches, column count differences, and invalid recursive CTE configurations are rejected.
8. **Resource & Cache Violations**: Zero memory budgets, stale catalog generations, and dropped tables are rejected.
9. **Mutation Matrix & Randomized Property Testing**: Systematically corrupts valid plans across schema, ordering, row goal, access path, and columns to verify 100% rejection with zero false negatives.
10. **Differential Execution Equivalence**: Validates that memo optimization and validation produce identical query results, ordering, NULL behavior, and DML effects.
11. **Persistence & Recovery**: Verifies schema validation survives database reopening and crash recovery.

---

## 17. What the Validator Deliberately Does NOT Do

The validator is strictly a **safety verifier**:

* ❌ It does **NOT** pick a better plan or access path.
* ❌ It does **NOT** rewrite predicates or push down filters.
* ❌ It does **NOT** estimate cardinalities or recalculate costs.
* ❌ It does **NOT** invent optimizer rules.
* ❌ It does **NOT** silently repair or patch invalid plans.
* ❌ It does **NOT** scan tables, read buffer pages, or execute queries.

If a plan is invalid, the validator rejects it deterministically.
