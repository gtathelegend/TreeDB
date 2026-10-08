# TreeDB — Execution Pipelines, Backpressure & Operator Lifecycle

## 1. Overview & Execution Model

TreeDB query execution is structured as a deterministic, pull-based iterator tree operating under strict resource boundaries and transaction visibility.

> **Physical plans are reusable descriptions of execution; operator instances are per-execution state.**

A `PhysicalPlan` or `ValidatedPhysicalPlan` represents an immutable, cost-optimized specification of execution. When a query executes—whether directly or via prepared statement / cached plan execution—a completely fresh tree of `ExecutionOperator` instances is constructed together with a fresh, isolated `ExecutionContext`. Mutable operator state, cursors, and memory allocations are never retained across executions.

```text
ValidatedPhysicalPlan
        ↓
ExecutionContract
        ↓
ExecutionContext (ExecutionResourceManager, TxnId)
        ↓
Operator Initialization
        ↓
Open / Start (open())
        ↓
Pull / Next (next())  <--- Backpressure & Row Demand
        ↓
Child Operators
        ↓
Close / Cleanup (close())  <--- Deterministic Safe Cleanup
        ↓
Execution Metrics
        ↓
Adaptive Feedback
```

---

## 2. Canonical Operator Lifecycle

Every physical operator implements the `ExecutionOperator` contract and follows an explicit, deterministic state machine:

```text
Created ──open()──► Opened ──next()──► Running ──exhausted/limit──► Exhausted ──close()──► Closed
   │                   │                  │                                                   ▲
   │                   │                  ├──error / cancellation──► Failed ──safe_close()────┘
   └───────────────────┴──────────────────┘
```

### Lifecycle Guarantees:

1. **`open(&mut self, ctx: &mut ExecutionContext) -> Result<()>`**:
   Transitions operator from `Created` to `Opened`. Initializes resources, accounts memory with `ctx.resource_manager`, and recursively opens child operators. Calling `next()` before `open()`, or calling `open()` multiple times, yields a deterministic `Error::InvalidOperatorLifecycleState`.
2. **`next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>>`**:
   Pulls the next qualifying tuple. Transitions to `Running` while tuples are produced, or `Exhausted` when the input is depleted or demand has been satisfied.
3. **`close(&mut self, ctx: &mut ExecutionContext) -> Result<()>`**:
   Releases all allocated memory and temporary disk files to `ctx.resource_manager`, and recursively closes child operators. Transitions state to `Closed`. Calling `close()` multiple times is idempotent and deterministic.
4. **`safe_close(op, ctx, primary_err)`**:
   Ensures that if execution fails with an error (e.g. `ResourceLimitExceeded` or `InternalExecutionError`), child operators are still deterministically closed, while preserving the original primary error. Secondary cleanup errors are collected in `ctx.cleanup_errors` rather than masking the primary failure.

---

## 3. Pull Execution, Backpressure & Row Demand

> **Backpressure flows from the consumer toward the producer through bounded row demand.**

Execution follows a strict pull model: parent operators request rows only as demanded from their children via `next()`.

### Backpressure Mechanisms:
- **LIMIT & OFFSET (`OffsetLimitExecutionOperator`)**:
  Signals row demand (`ctx.row_demand = offset + limit`). Once the limit has been satisfied, the operator transitions to `Exhausted` and immediately halts pulling from its child. For example, a `LIMIT 1` query over a 100,000-row table consumes exactly 1 tuple from its child.
- **Top-N Bounded Priority Queue (`TopNExecutionOperator`)**:
  Maintains a bounded max-heap of size $M + K$ (`OFFSET M LIMIT K`). If the memory budget is exceeded, it gracefully transitions to external sort without retaining unbounded memory.
- **Ordered Index Scan Early Termination**:
  When an index provides the required physical ordering for `ORDER BY col LIMIT K`, the scan checks residual predicates incrementally and stops index leaf traversal immediately once $K$ qualifying rows have been emitted.
- **SEMI / ANTI Join Short-Circuiting**:
  For SEMI joins (`EXISTS`), probe execution halts on the first qualifying match. For ANTI joins (`NOT EXISTS`), probing halts once a match disproves existence.

---

## 4. Execution Context & Resource Management

The `ExecutionContext` carries execution-wide state explicitly without relying on process-global mutable variables:

- **`ExecutionResourceManager`**:
  Tracks active memory reservations, peak memory, active temporary spill files, and active temporary disk bytes.
- **Leak Detection**:
  Upon execution completion (normal termination, early termination, cancellation, or error), `ctx.resource_manager.is_clean()` verifies that:
  - `active_memory_reservations == 0`
  - `active_temp_files == 0`
  - `active_temp_bytes == 0`
  Any dangling allocation triggers `Error::InternalExecutionError`.
- **Resource Admission & Enforcement**:
  Every operator requesting execution memory or creating temporary spill files must allocate via `ctx.resource_manager`. Exceeding configured limits immediately fails with `Error::ResourceLimitExceeded`.

---

## 5. Execution Cancellation

TreeDB provides deterministic, threadless cancellation via cooperative loop polling:

- `ctx.request_cancellation()` marks the execution context as cancelled.
- Operators check `ctx.check_cancellation()?` at safe execution boundaries:
  - Table and index scan loops
  - Recursive CTE iteration loops
  - Sort run merge loops
  - Hash join partition loops
- Cancellation triggers immediate clean termination:
  - `ctx.termination_status = ExecutionTerminationStatus::Cancelled`
  - Returns `Error::ExecutionCancelled`
  - Runs `safe_close()` to reclaim all active reservations and temporary disk runs.

---

## 6. Blocking, Streaming, and Spill Operator Classifications

| Operator | Category | Memory Management | Backpressure Behavior |
| :--- | :--- | :--- | :--- |
| **`PhysicalScanOperator`** | Streaming | Bounded buffer accounted in resource manager | Obeys `row_demand` and `early_stop_bound` |
| **`FilterExecutionOperator`** | Streaming | Negligible (stateless predicate evaluation) | Passes row demands upward |
| **`ProjectExecutionOperator`**| Streaming | Negligible (projection expressions) | Passes row demands upward |
| **`OffsetLimitExecutionOperator`** | Streaming | Negligible (counts offset and limit) | Enforces hard demand boundary |
| **`StreamingAggregateExecutionOperator`** | Streaming | Bounded by group state | Emits group on key transition |
| **`SortExecutionOperator`** | Blocking / Spillable | Memory budgeted; spills runs to disk | Consumes child fully before emitting |
| **`TopNExecutionOperator`** | Blocking / Spillable | Bounded heap $M+K$; spills if budget exceeded | Bounds candidate heap |
| **`AggregateExecutionOperator`** | Blocking / Spillable | Hash table partitioned / spilled on budget | Consumes child fully before emitting |
| **`JoinExecutionOperator`** | Blocking Build / Streaming Probe | Build side memory budgeted; probe is streaming | Probe side obeys backpressure |
| **`WindowExecutionOperator`** | Partition-Blocking | Partitions buffered in memory | Emits partition rows in order |
| **`SetOperationExecutionOperator`** | Streaming / Blocking | UNION ALL is streaming; UNION is distinct buffered | UNION ALL passes backpressure directly |
| **`MaterializedResultOperator`** | Materialized Buffer | Cached subplan rows accounted in resource manager | Replays materialized tuples |

---

## 7. Transaction Boundaries & Adaptive Feedback

- **Transaction Atomicity**:
  Execution failure or cancellation leaves the transaction safely abortable. DML statements never commit partial modifications if an execution fails midway.
- **Adaptive Feedback Hygiene**:
  Only executions with status `CompletedNormally` or `CompletedEarly` feed cardinality observations into the adaptive query optimization registry (`FeedbackRegistry`). Failed executions, resource abortions, and cancelled queries are strictly prevented from contaminating EWMA cost models.
