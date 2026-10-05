# Phase 57 — Cost-Based Row-Goal Join Planning & Early-Stop Join Execution

## 1. Overview and Architecture

Phase 57 extends TreeDB's cost-based optimizer and physical execution framework with **row-goal-aware join planning** and **semantically safe early-stop join execution**.

In earlier phases, queries containing `LIMIT K` / `OFFSET M` could propagate row goals down to single-table scan access paths (Phase 56) and bounded sort operators (`TopNOperator`, Phase 55), but joins acted as conservative row-goal barriers.

Phase 57 introduces join-aware row goal reasoning:

```text
SQL (JOIN + ORDER BY + LIMIT)
              ↓
      Parent Row Goal (K = LIMIT + OFFSET)
              ↓
      Join Output Row Goal
              ↓
┌─────────────────────────────────────────────────────────────┐
│                 Cost-Based Join Planner                     │
│  ├── Nested Loop: Bounded outer probe costing & early stop  │
│  ├── Hash Join: Full build phase + bounded probe costing    │
│  ├── External Hash: Fixed spill/build + partial probe       │
│  └── Merge Join: Ordering-compatible early termination      │
└─────────────────────────────────────────────────────────────┘
              ↓
  Early Stop when Semantically Safe
```

---

## 2. Join Output Row Goal vs. Child Input Row Goals

A fundamental principle of relational join row-goal semantics is:

> **A join output row goal $K$ specifies that the parent needs at most $K$ rows from the join output. It does NOT imply that each child relation only needs $K$ rows.**

### Why Child Row Goals Are Not Automatically $K$
1. **Multiplication Effect (One-to-Many / Many-to-Many)**: A single left row could match 100 right rows. If $K = 10$, only 1 left row is needed to satisfy the goal.
2. **Filtering / Low Match Rate**: If the join condition or residual filters are selective, 1,000 left rows might be needed to find even 1 matching join output row.
3. **Build-Side Completeness**: Hash joins require building a complete hash table on the inner relation before probing can begin.

Therefore, the optimizer distinguishes:
- `join_output_row_goal`: The target number of joined rows requested by the parent.
- `expected_outer_probes`: $\min\left(N_{\text{outer}}, \frac{K}{\text{expected\_rows\_per\_probe}}\right)$, where $\text{expected\_rows\_per\_probe} = \frac{\text{estimated\_join\_rows}}{N_{\text{outer}}}$.

---

## 3. Physical Join Strategy Row-Goal Semantics

### Nested Loop Join
For `INNER` and `SEMI` joins:
- Outer relation stream is iterated.
- Inner relation is probed / scanned.
- Once the number of emitted qualifying joined rows reaches the row goal $K$, execution terminates immediately.
- Cost model:
  $$\text{Cost} = \text{cost}(\text{outer\_scan}) + \text{expected\_outer\_probes} \times \text{cost}(\text{inner\_scan}) + K \times C_{\text{emit}}$$

### Hash Join
- **Build Phase**: Fixed, must completely build the hash table over the right relation in memory.
- **Probe Phase**: Outer relation rows are probed against the hash table. Once $K$ qualifying output rows are emitted, the probe loop terminates early.
- Cost model:
  $$\text{Cost} = \text{cost}(\text{build}) + C_{\text{build\_hash}} + \text{expected\_probe\_rows} \times C_{\text{probe\_hash}} + K \times C_{\text{emit}}$$

### External Hash Join
- If the build relation exceeds the join memory budget, partitioning and temporary run files are created on disk.
- Fixed partition and build costs remain necessary.
- Bounded probe work applies only during the streaming probe phase of matching partition buckets.

### Merge Join
- When both input relations are sorted on their join keys, and the resulting join output order satisfies the query's required `ORDER BY` (or when no downstream sort is required):
  - Merge join streams both relations simultaneously.
  - When $K$ output rows have been emitted, merge join safely halts.
  - Preserves exact ordering and many-to-many duplicate runs without needing an intermediate sort or bounded heap.

---

## 4. Duplicate Run Correctness in Merge Join

When duplicate join keys exist in both relations ($M \times N$ cross-product combinations for identical keys):
- TreeDB's `MergeJoinOperator` materializes the contiguous duplicate run of the right relation.
- For each duplicate left row, it iterates through the right duplicate buffer.
- When output row count reaches the row goal $K$ in the middle of a duplicate run, it cleanly breaks, returning exactly the first $K$ joined rows according to the required ordering.
- No duplicate combinations before the limit are skipped or reordered.

---

## 5. Outer Join Safety Barriers

For `LEFT`, `RIGHT`, and `FULL` outer joins:
- Early stopping before all input rows are evaluated is unsafe in general because unmatched outer rows must be emitted with `NULL` padding.
- Under an arbitrary downstream `ORDER BY`, an unmatched row with `NULL` values could sort ahead of or after matched rows.
- TreeDB adheres to conservative safety barriers: outer joins evaluate fully unless ordering and nullability properties mathematically prove safety.

---

## 6. SEMI and ANTI Joins

- **SEMI Join**: Probing stops on the inner relation at the first qualifying match. With a row goal $K$, the outer stream also halts once $K$ qualifying outer rows have been emitted.
- **ANTI Join**: Requires proving absence of any matching inner row for an outer row before emitting it. Once $K$ non-matching outer rows are emitted, the outer stream halts.

---

## 7. Interaction with Ordering and TopN

The optimizer compares three execution plans for `JOIN + ORDER BY + LIMIT K`:

1. **Ordered Merge Join**:
   Inputs sorted on join keys $\rightarrow$ Merge Join emits in required order $\rightarrow$ Early stop at $K$ rows. Avoids downstream Sort/TopN.
2. **Hash Join + TopN**:
   Hash Join (full build + bounded probe) $\rightarrow$ Bounded priority queue heap of size $K$.
3. **Full Join + Sort**:
   Standard join $\rightarrow$ Full external or in-memory sort $\rightarrow$ LimitOffset.

---

## 8. Runtime Feedback and Plan Cache

### Feedback Context
TreeDB records join row-goal feedback under:
```rust
FeedbackContext::JoinRowGoal {
    join_type,
    strategy,
    left_table,
    right_table,
    on_clause,
    row_goal,
}
```
- Tracks actual rows examined vs. estimated rows examined.
- Smooths estimates using EWMA.
- Detects severe estimation errors (e.g. unexpected key skew or selective residuals) and invalidates cached plans when thresholds exceed $2\times$.

### Plan Cache Parameter Sensitivity
Parameterized queries (`LIMIT ? OFFSET ?`) generate plan cache keys sensitive to parameter planning signatures (`ParamPlanningSignature`). Plans optimized for $K=1$ are not reused for $K=1,000,000$ when the optimal join strategy changes.
