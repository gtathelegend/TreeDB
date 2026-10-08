//! Canonical Execution Pipelines, Operator Lifecycle & Backpressure.
//!
//! Formalizes TreeDB's physical query execution pipeline with:
//! 1. Explicit, deterministic operator lifecycles (`Created -> Opened -> Running -> Exhausted -> Closed`).
//! 2. Pull-based backpressure and row-demand flowing upward from consumers toward producers.
//! 3. Unified execution context and resource manager tracking active memory reservations,
//!    temporary disk files, and temporary bytes with leak detection.
//! 4. Deterministic child ownership and error propagation with primary error preservation.
//! 5. Execution cancellation checked at safe loop boundaries.
//! 6. Complete state isolation: physical plans are reusable definitions; execution trees are fresh per execution.

use std::fmt;
use std::time::Instant;

use crate::engine::types::{Column, Row, Schema, Value};
use crate::error::{Error, Result};
use crate::query::expr::{AggregateExpr, Expr};
use crate::query::external_aggregate::AggregateConfig;
use crate::query::external_hash_join::JoinConfig;
use crate::query::external_sort::SortConfig;
use crate::query::feedback::FeedbackContext;
use crate::query::materialize::MaterializeConfig;
use crate::query::metrics::{OperatorKind, OperatorMetrics};
use crate::query::operator::{
    AggregateOperator, QueryResult, SortKey, SortOperator, WindowOperator,
};
use crate::query::streaming_aggregate::StreamingAggregateOperator;
use crate::query::top_n::TopNOperator;
use crate::wal::lsn::TxnId;

/// Explicit states representing a physical operator's lifecycle progression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OperatorLifecycleState {
    /// Operator instance created; children attached; uninitialized.
    Created,
    /// Resources acquired, child operators opened, ready to produce rows.
    Opened,
    /// Actively iterating and producing rows via `next()`.
    Running,
    /// All rows emitted; child operators exhausted or demand satisfied.
    Exhausted,
    /// Cleaned up; memory and disk resources released; terminal state.
    Closed,
    /// Failed due to an unrecoverable runtime error before or during cleanup.
    Failed,
}

impl fmt::Display for OperatorLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "Created"),
            Self::Opened => write!(f, "Opened"),
            Self::Running => write!(f, "Running"),
            Self::Exhausted => write!(f, "Exhausted"),
            Self::Closed => write!(f, "Closed"),
            Self::Failed => write!(f, "Failed"),
        }
    }
}

/// Helper verifying valid lifecycle state transitions.
pub fn validate_transition(
    operator: &str,
    current: OperatorLifecycleState,
    expected: &[OperatorLifecycleState],
) -> Result<()> {
    if !expected.contains(&current) {
        let exp_str = expected
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        Err(Error::InvalidOperatorLifecycleState {
            operator: operator.to_string(),
            expected: exp_str,
            actual: current.to_string(),
        })
    } else {
        Ok(())
    }
}

/// Execution termination classifications for diagnostic reporting and feedback filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionTerminationStatus {
    /// Completed normally, emitting all demanded rows without error.
    CompletedNormally,
    /// Terminated early due to row goal, LIMIT, or SEMI/ANTI match.
    CompletedEarly,
    /// Terminated due to hard memory or temporary disk resource quota.
    FailedResourceLimit,
    /// Terminated due to evaluation error, schema mismatch, or child error.
    FailedExecutionError,
    /// Terminated due to explicit execution cancellation request.
    Cancelled,
}

impl fmt::Display for ExecutionTerminationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompletedNormally => write!(f, "CompletedNormally"),
            Self::CompletedEarly => write!(f, "CompletedEarly"),
            Self::FailedResourceLimit => write!(f, "FailedResourceLimit"),
            Self::FailedExecutionError => write!(f, "FailedExecutionError"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

/// Configured resource boundaries governing physical operator execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResourceLimits {
    /// Maximum allowed memory reservation in bytes for execution operators.
    pub max_memory_bytes: usize,
    /// Maximum allowed disk space in bytes for temporary spill runs and partitions.
    pub max_temp_bytes: usize,
    /// Maximum number of simultaneously open temporary disk files.
    pub max_temp_files: usize,
}

impl Default for ExecutionResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024, // 64 MB
            max_temp_bytes: 1024 * 1024 * 1024, // 1 GB
            max_temp_files: 64,
        }
    }
}

impl ExecutionResourceLimits {
    /// Creates unlimited resource boundaries.
    pub fn unlimited() -> Self {
        Self {
            max_memory_bytes: usize::MAX,
            max_temp_bytes: usize::MAX,
            max_temp_files: usize::MAX,
        }
    }

    /// Derives resource limits from engine configurations.
    pub fn from_configs(
        sort_config: &SortConfig,
        join_config: &JoinConfig,
        agg_config: &AggregateConfig,
        mat_config: &MaterializeConfig,
    ) -> Self {
        let max_mem = sort_config
            .memory_budget_bytes
            .max(join_config.memory_budget_bytes)
            .max(agg_config.memory_budget_bytes)
            .max(mat_config.memory_budget_bytes)
            .max(64 * 1024);

        let max_disk = sort_config
            .max_temp_bytes
            .max(join_config.max_temp_bytes)
            .max(agg_config.max_temp_bytes)
            .max(mat_config.max_temp_bytes)
            .max(1024 * 1024);

        let max_files = sort_config
            .max_temp_files
            .max(join_config.max_temp_files)
            .max(agg_config.max_temp_files)
            .max(16);

        Self {
            max_memory_bytes: max_mem,
            max_temp_bytes: max_disk,
            max_temp_files: max_files,
        }
    }
}

/// Runtime resource manager providing centralized accounting, limit enforcement,
/// and post-execution leak detection across all operators in an execution tree.
#[derive(Debug, Clone)]
pub struct ExecutionResourceManager {
    limits: ExecutionResourceLimits,
    active_memory_reservations: usize,
    peak_memory_bytes: usize,
    active_temp_bytes: usize,
    peak_temp_bytes: usize,
    active_temp_files: usize,
    peak_temp_files: usize,
}

impl ExecutionResourceManager {
    /// Creates a new `ExecutionResourceManager` governed by the specified limits.
    pub fn new(limits: ExecutionResourceLimits) -> Self {
        Self {
            limits,
            active_memory_reservations: 0,
            peak_memory_bytes: 0,
            active_temp_bytes: 0,
            peak_temp_bytes: 0,
            active_temp_files: 0,
            peak_temp_files: 0,
        }
    }

    /// Attempts to reserve `bytes` of memory. Returns `ResourceLimitExceeded` if budget is exceeded.
    pub fn reserve_memory(&mut self, bytes: usize) -> Result<()> {
        let new_total = self.active_memory_reservations.saturating_add(bytes);
        if new_total > self.limits.max_memory_bytes {
            return Err(Error::ResourceLimitExceeded {
                resource: "memory".to_string(),
                requested: bytes,
                limit: self.limits.max_memory_bytes,
            });
        }
        self.active_memory_reservations = new_total;
        self.peak_memory_bytes = self.peak_memory_bytes.max(self.active_memory_reservations);
        Ok(())
    }

    /// Releases a previously reserved memory allocation.
    pub fn release_memory(&mut self, bytes: usize) {
        self.active_memory_reservations = self.active_memory_reservations.saturating_sub(bytes);
    }

    /// Attempts to reserve temporary disk space. Returns `ResourceLimitExceeded` if quota is exceeded.
    pub fn reserve_temp_bytes(&mut self, bytes: usize) -> Result<()> {
        let new_total = self.active_temp_bytes.saturating_add(bytes);
        if new_total > self.limits.max_temp_bytes {
            return Err(Error::ResourceLimitExceeded {
                resource: "temp_disk_bytes".to_string(),
                requested: bytes,
                limit: self.limits.max_temp_bytes,
            });
        }
        self.active_temp_bytes = new_total;
        self.peak_temp_bytes = self.peak_temp_bytes.max(self.active_temp_bytes);
        Ok(())
    }

    /// Releases temporary disk space reservation.
    pub fn release_temp_bytes(&mut self, bytes: usize) {
        self.active_temp_bytes = self.active_temp_bytes.saturating_sub(bytes);
    }

    /// Reserves a temporary file descriptor. Returns `ResourceLimitExceeded` if quota is exceeded.
    pub fn reserve_temp_file(&mut self) -> Result<()> {
        let new_total = self.active_temp_files.saturating_add(1);
        if new_total > self.limits.max_temp_files {
            return Err(Error::ResourceLimitExceeded {
                resource: "temp_files".to_string(),
                requested: 1,
                limit: self.limits.max_temp_files,
            });
        }
        self.active_temp_files = new_total;
        self.peak_temp_files = self.peak_temp_files.max(self.active_temp_files);
        Ok(())
    }

    /// Releases a temporary file descriptor reservation.
    pub fn release_temp_file(&mut self) {
        self.active_temp_files = self.active_temp_files.saturating_sub(1);
    }

    /// Returns currently active memory reservations in bytes.
    pub fn active_memory_reservations(&self) -> usize {
        self.active_memory_reservations
    }

    /// Returns currently active temporary disk files.
    pub fn active_temp_files(&self) -> usize {
        self.active_temp_files
    }

    /// Returns currently active temporary disk bytes.
    pub fn active_temp_bytes(&self) -> usize {
        self.active_temp_bytes
    }

    /// Returns peak memory reserved in bytes during this execution.
    pub fn peak_memory_bytes(&self) -> usize {
        self.peak_memory_bytes
    }

    /// Returns peak temporary bytes used during this execution.
    pub fn peak_temp_bytes(&self) -> usize {
        self.peak_temp_bytes
    }

    /// Returns peak temporary files open during this execution.
    pub fn peak_temp_files(&self) -> usize {
        self.peak_temp_files
    }

    /// Returns `true` if all active reservations have been completely released.
    pub fn is_clean(&self) -> bool {
        self.active_memory_reservations == 0
            && self.active_temp_files == 0
            && self.active_temp_bytes == 0
    }
}

/// Query execution context passed through the physical operator tree.
#[derive(Debug)]
pub struct ExecutionContext {
    /// Monotonically increasing execution identifier.
    pub execution_id: u64,
    /// Owning transaction identifier.
    pub txn_id: TxnId,
    /// Centralized resource manager for this execution.
    pub resource_manager: ExecutionResourceManager,
    /// Flag signaling query cancellation.
    pub cancellation_requested: bool,
    /// Final execution status classification.
    pub termination_status: ExecutionTerminationStatus,
    /// Backpressure row demand signaled by parent operator down to child.
    pub row_demand: Option<usize>,
    /// Optional feedback context.
    pub feedback_context: Option<FeedbackContext>,
    /// Diagnostic errors accumulated during cleanup (suppressed if a primary error exists).
    pub cleanup_errors: Vec<String>,
}

impl ExecutionContext {
    /// Creates a fresh `ExecutionContext` for a new query execution.
    pub fn new(execution_id: u64, txn_id: TxnId, limits: ExecutionResourceLimits) -> Self {
        Self {
            execution_id,
            txn_id,
            resource_manager: ExecutionResourceManager::new(limits),
            cancellation_requested: false,
            termination_status: ExecutionTerminationStatus::CompletedNormally,
            row_demand: None,
            feedback_context: None,
            cleanup_errors: Vec::new(),
        }
    }

    /// Creates an `ExecutionContext` for a given transaction with unlimited resource bounds.
    pub fn for_txn(txn_id: TxnId) -> Self {
        Self::new(txn_id.0, txn_id, ExecutionResourceLimits::unlimited())
    }

    /// Requests cancellation of this execution.
    pub fn request_cancellation(&mut self) {
        self.cancellation_requested = true;
    }

    /// Checks if cancellation has been requested, returning `Error::ExecutionCancelled` if so.
    pub fn check_cancellation(&self) -> Result<()> {
        if self.cancellation_requested {
            Err(Error::ExecutionCancelled)
        } else {
            Ok(())
        }
    }
}

/// Canonical pull-based execution operator lifecycle trait.
///
/// Guarantees:
/// 1. `open`: Called exactly once prior to iteration. Transitions operator from `Created` to `Opened`.
/// 2. `next`: Pulls the next qualifying row. Transitions to `Running` (on row) or `Exhausted` (on None).
/// 3. `close`: Releases all operator resources and closes child operators. Transitions to `Closed`.
pub trait ExecutionOperator: Send {
    /// Returns the logical schema of rows emitted by this operator.
    fn schema(&self) -> &Schema;

    /// Initializes this operator and its children.
    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()>;

    /// Pulls the next row from this operator. Returns `None` when exhausted.
    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>>;

    /// Deterministically releases all resources and closes child operators.
    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()>;

    /// Returns the current lifecycle state of this operator.
    fn state(&self) -> OperatorLifecycleState;

    /// Produces a snapshot of operator execution metrics and counters.
    fn metrics(&self) -> OperatorMetrics;
}

/// Deterministic cleanup helper ensuring child operators are closed on success, early termination, or error.
///
/// Guarantees that the primary execution error is preserved and not replaced by a secondary cleanup error.
pub fn safe_close<O: ExecutionOperator + ?Sized>(
    op: &mut O,
    ctx: &mut ExecutionContext,
    primary_err: Option<Error>,
) -> Result<()> {
    match op.close(ctx) {
        Ok(()) => match primary_err {
            Some(err) => Err(err),
            None => Ok(()),
        },
        Err(cleanup_err) => {
            if let Some(primary) = primary_err {
                ctx.cleanup_errors.push(cleanup_err.to_string());
                Err(primary)
            } else {
                Err(cleanup_err)
            }
        }
    }
}

// ============================================================================
// Concrete Execution Operators
// ============================================================================

/// Physical table scan execution operator (Primary/Secondary/Covering/Skip/MultiIndex/FullScan/CTE).
pub struct PhysicalScanOperator {
    table_name: String,
    schema: Schema,
    rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    row_goal: Option<usize>,
    early_stop_bound: Option<usize>,
    early_terminated: bool,
    memory_reserved: usize,
}

impl PhysicalScanOperator {
    /// Creates a new `PhysicalScanOperator`.
    pub fn new(
        table_name: String,
        schema: Schema,
        rows: Vec<Row>,
        metrics: OperatorMetrics,
        row_goal: Option<usize>,
        early_stop_bound: Option<usize>,
        early_terminated: bool,
    ) -> Self {
        Self {
            table_name,
            schema,
            rows,
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            row_goal,
            early_stop_bound,
            early_terminated,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for PhysicalScanOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition(
            &format!("Scan({})", self.table_name),
            self.state,
            &[OperatorLifecycleState::Created],
        )?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        // Account memory for buffered scan rows in resource manager
        let estimated_mem = self.rows.len() * (std::mem::size_of::<Row>() + 64);
        ctx.resource_manager.reserve_memory(estimated_mem)?;
        self.memory_reserved = estimated_mem;
        self.metrics.counters.peak_memory_bytes = estimated_mem;

        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            &format!("Scan({})", self.table_name),
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        // Respect row demand or early stop bound
        let effective_limit = self.early_stop_bound.or(self.row_goal).or(ctx.row_demand);
        if let Some(limit) = effective_limit {
            if self.cursor >= limit {
                self.early_terminated = true;
                self.state = OperatorLifecycleState::Exhausted;
                return Ok(None);
            }
        }

        if self.cursor < self.rows.len() {
            let row = self.rows[self.cursor].clone();
            self.cursor += 1;
            self.metrics.actual_rows += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.state = OperatorLifecycleState::Closed;
        Ok(())
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.actual_rows = self.cursor;
        m.counters.early_terminated = self.early_terminated;
        m
    }
}

/// Physical filter execution operator evaluating a 3-valued boolean predicate.
pub struct FilterExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    predicate: Expr,
    schema: Schema,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
}

impl FilterExecutionOperator {
    /// Creates a new `FilterExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        predicate: Expr,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let schema = child.schema().clone();
        let metrics = OperatorMetrics::new(OperatorKind::Filter, estimated_rows, cost);
        Self {
            child,
            predicate,
            schema,
            state: OperatorLifecycleState::Created,
            metrics,
        }
    }
}

impl ExecutionOperator for FilterExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("Filter", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "Filter",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        loop {
            ctx.check_cancellation()?;
            match self.child.next(ctx)? {
                Some(row) => {
                    self.metrics.input_rows += 1;
                    self.metrics.counters.rows_evaluated += 1;

                    let val = self.predicate.evaluate(&row, &self.schema)?;
                    if val == Value::Boolean(true) {
                        self.metrics.actual_rows += 1;
                        self.metrics.counters.rows_passed += 1;
                        self.state = OperatorLifecycleState::Running;
                        return Ok(Some(row));
                    } else {
                        self.metrics.counters.rows_rejected += 1;
                    }
                }
                None => {
                    self.state = OperatorLifecycleState::Exhausted;
                    return Ok(None);
                }
            }
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical projection execution operator.
pub struct ProjectExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    projections: Vec<(Expr, Option<String>)>,
    schema: Schema,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
}

impl ProjectExecutionOperator {
    /// Creates a new `ProjectExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        projections: Vec<(Expr, Option<String>)>,
        schema: Schema,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let metrics = OperatorMetrics::new(OperatorKind::Project, estimated_rows, cost);
        Self {
            child,
            projections,
            schema,
            state: OperatorLifecycleState::Created,
            metrics,
        }
    }
}

impl ExecutionOperator for ProjectExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("Project", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "Project",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        match self.child.next(ctx)? {
            Some(row) => {
                self.metrics.input_rows += 1;
                let mut values = Vec::with_capacity(self.projections.len());
                for (expr, _) in &self.projections {
                    values.push(expr.evaluate(&row, self.child.schema())?);
                }
                self.metrics.actual_rows += 1;
                self.state = OperatorLifecycleState::Running;
                Ok(Some(Row::new(values)))
            }
            None => {
                self.state = OperatorLifecycleState::Exhausted;
                Ok(None)
            }
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical offset skipping and limit pagination operator applying demand backpressure.
pub struct OffsetLimitExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    offset: Option<usize>,
    limit: Option<usize>,
    skipped: usize,
    emitted: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
}

impl OffsetLimitExecutionOperator {
    /// Creates a new `OffsetLimitExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        offset: Option<usize>,
        limit: Option<usize>,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let metrics = OperatorMetrics::new(OperatorKind::LimitOffset, estimated_rows, cost);
        Self {
            child,
            offset,
            limit,
            skipped: 0,
            emitted: 0,
            state: OperatorLifecycleState::Created,
            metrics,
        }
    }
}

impl ExecutionOperator for OffsetLimitExecutionOperator {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition(
            "LimitOffset",
            self.state,
            &[OperatorLifecycleState::Created],
        )?;
        self.state = OperatorLifecycleState::Opened;
        self.skipped = 0;
        self.emitted = 0;

        // Establish demand backpressure
        if let Some(lim) = self.limit {
            ctx.row_demand = Some(self.offset.unwrap_or(0).saturating_add(lim));
        }

        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "LimitOffset",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        // Skip offset rows
        let target_offset = self.offset.unwrap_or(0);
        while self.skipped < target_offset {
            match self.child.next(ctx)? {
                Some(_) => {
                    self.skipped += 1;
                    self.metrics.input_rows += 1;
                }
                None => {
                    self.state = OperatorLifecycleState::Exhausted;
                    return Ok(None);
                }
            }
        }

        // Check if limit has been reached
        if let Some(lim) = self.limit {
            if self.emitted >= lim {
                // Backpressure boundary: DO NOT pull from child anymore!
                self.state = OperatorLifecycleState::Exhausted;
                return Ok(None);
            }
        }

        match self.child.next(ctx)? {
            Some(row) => {
                self.metrics.input_rows += 1;
                self.emitted += 1;
                self.metrics.actual_rows += 1;
                self.state = OperatorLifecycleState::Running;
                Ok(Some(row))
            }
            None => {
                self.state = OperatorLifecycleState::Exhausted;
                Ok(None)
            }
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical in-memory / external sort operator.
pub struct SortExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    sort_keys: Vec<SortKey>,
    sort_config: SortConfig,
    sorted_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl SortExecutionOperator {
    /// Creates a new `SortExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        sort_keys: Vec<SortKey>,
        sort_config: SortConfig,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let metrics = OperatorMetrics::new(OperatorKind::Sort, estimated_rows, cost);
        Self {
            child,
            sort_keys,
            sort_config,
            sorted_rows: Vec::new(),
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for SortExecutionOperator {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("Sort", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        self.child.open(ctx)?;

        // Blocking operator: fully consume child rows
        let mut input_rows = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            input_rows.push(row);
        }
        self.child.close(ctx)?;

        self.metrics.input_rows = input_rows.len();

        let (sorted, sort_metrics) = SortOperator::apply_with_config(
            input_rows,
            self.schema(),
            &self.sort_keys,
            &self.sort_config,
        )?;

        // Resource accounting
        self.metrics.counters.runs_created = sort_metrics.runs_created;
        self.metrics.counters.bytes_spilled = sort_metrics.bytes_spilled;
        self.metrics.counters.merge_passes = sort_metrics.merge_passes;
        self.metrics.counters.peak_memory_bytes = sort_metrics.peak_memory_bytes;
        self.metrics.counters.sort_required = true;
        self.metrics.counters.sort_eliminated = false;

        if sort_metrics.peak_memory_bytes > 0 {
            ctx.resource_manager
                .reserve_memory(sort_metrics.peak_memory_bytes)?;
            self.memory_reserved = sort_metrics.peak_memory_bytes;
        }

        self.sorted_rows = sorted;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "Sort",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.sorted_rows.len() {
            let row = self.sorted_rows[self.cursor].clone();
            self.cursor += 1;
            self.metrics.actual_rows += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.sorted_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical bounded priority queue Top-N operator.
pub struct TopNExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    sort_keys: Vec<SortKey>,
    limit: Option<usize>,
    offset: Option<usize>,
    sort_config: SortConfig,
    sorted_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl TopNExecutionOperator {
    /// Creates a new `TopNExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        sort_keys: Vec<SortKey>,
        limit: Option<usize>,
        offset: Option<usize>,
        sort_config: SortConfig,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let metrics = OperatorMetrics::new(OperatorKind::TopN, estimated_rows, cost);
        Self {
            child,
            sort_keys,
            limit,
            offset,
            sort_config,
            sorted_rows: Vec::new(),
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for TopNExecutionOperator {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("TopN", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        self.child.open(ctx)?;

        let mut input_rows = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            input_rows.push(row);
        }
        self.child.close(ctx)?;

        self.metrics.input_rows = input_rows.len();

        let (top_n, top_n_metrics) = TopNOperator::apply_with_config(
            input_rows,
            self.schema(),
            &self.sort_keys,
            self.limit,
            self.offset,
            &self.sort_config,
        )?;

        self.metrics.counters.top_n_candidate_capacity = top_n_metrics.candidate_capacity;
        self.metrics.counters.top_n_rows_retained = top_n_metrics.rows_retained;
        self.metrics.counters.top_n_rows_discarded = top_n_metrics.rows_discarded;
        self.metrics.counters.top_n_early_terminated = top_n_metrics.early_terminated;
        self.metrics.counters.early_terminated = top_n_metrics.early_terminated;
        self.metrics.counters.peak_memory_bytes = top_n_metrics.peak_memory_bytes;
        self.metrics.counters.top_n_limit = top_n_metrics.limit.unwrap_or(0);
        self.metrics.counters.top_n_offset = top_n_metrics.offset.unwrap_or(0);
        self.metrics.counters.sort_required = true;
        self.metrics.counters.sort_eliminated = false;

        if top_n_metrics.peak_memory_bytes > 0 {
            ctx.resource_manager
                .reserve_memory(top_n_metrics.peak_memory_bytes)?;
            self.memory_reserved = top_n_metrics.peak_memory_bytes;
        }

        self.sorted_rows = top_n;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "TopN",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.sorted_rows.len() {
            let row = self.sorted_rows[self.cursor].clone();
            self.cursor += 1;
            self.metrics.actual_rows += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.sorted_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Generic container operator holding pre-executed subplan rows and metrics.
pub struct MaterializedResultOperator {
    schema: Schema,
    rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl MaterializedResultOperator {
    /// Creates a new `MaterializedResultOperator`.
    pub fn new(schema: Schema, rows: Vec<Row>, metrics: OperatorMetrics) -> Self {
        Self {
            schema,
            rows,
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for MaterializedResultOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition(
            "MaterializedResult",
            self.state,
            &[OperatorLifecycleState::Created],
        )?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        let estimated = self.rows.len() * (std::mem::size_of::<Row>() + 64);
        ctx.resource_manager.reserve_memory(estimated)?;
        self.memory_reserved = estimated;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "MaterializedResult",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.rows.len() {
            let row = self.rows[self.cursor].clone();
            self.cursor += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.state = OperatorLifecycleState::Closed;
        Ok(())
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.actual_rows = self.cursor;
        m
    }
}

/// Physical cost-based ordered streaming aggregation execution operator.
pub struct StreamingAggregateExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    group_by: Vec<Expr>,
    aggregates: Vec<AggregateExpr>,
    sort_required: bool,
    sort_config: Option<SortConfig>,
    schema: Schema,
    result_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl StreamingAggregateExecutionOperator {
    /// Creates a new `StreamingAggregateExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        group_by: Vec<Expr>,
        aggregates: Vec<AggregateExpr>,
        sort_required: bool,
        sort_config: Option<SortConfig>,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Result<Self> {
        let input_schema = child.schema();
        let mut out_cols = Vec::with_capacity(group_by.len() + aggregates.len());
        for (idx, grp_expr) in group_by.iter().enumerate() {
            let dt = grp_expr.infer_type(input_schema)?;
            let name = match grp_expr {
                Expr::Column(col_name) => col_name.clone(),
                _ => format!("_group_{idx}"),
            };
            out_cols.push(Column::new(name, dt, true));
        }
        for agg_expr in &aggregates {
            let dt = agg_expr.infer_type(input_schema)?;
            let name = agg_expr.display_name();
            out_cols.push(Column::new(name, dt, true));
        }
        let schema = Schema::new_projected(out_cols);
        let metrics = OperatorMetrics::new(OperatorKind::StreamingAggregate, estimated_rows, cost);

        Ok(Self {
            child,
            group_by,
            aggregates,
            sort_required,
            sort_config,
            schema,
            result_rows: Vec::new(),
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        })
    }
}

impl ExecutionOperator for StreamingAggregateExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition(
            "StreamingAggregate",
            self.state,
            &[OperatorLifecycleState::Created],
        )?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        self.child.open(ctx)?;

        let mut input_rows = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            input_rows.push(row);
        }
        self.child.close(ctx)?;

        self.metrics.input_rows = input_rows.len();

        let (schema, rows, stream_metrics) = StreamingAggregateOperator::new(
            self.child.schema(),
            &self.group_by,
            &self.aggregates,
            self.sort_required,
            self.sort_config.clone(),
        )
        .execute(input_rows)?;

        self.metrics.counters.streaming_aggregate_input_rows = stream_metrics.input_rows;
        self.metrics.counters.streaming_aggregate_output_groups = stream_metrics.output_groups;
        self.metrics.counters.streaming_aggregate_group_transitions =
            stream_metrics.group_transitions;
        self.metrics.counters.streaming_aggregate_max_group_size = stream_metrics.max_group_size;
        self.metrics.counters.streaming_aggregate_aggregate_updates =
            stream_metrics.aggregate_updates;
        self.metrics.counters.streaming_aggregate_sort_required = stream_metrics.sort_required;
        self.metrics.counters.streaming_aggregate_sort_spilled = stream_metrics.sort_spilled;
        self.metrics.counters.streaming_aggregate_memory_bytes = stream_metrics.memory_bytes;
        self.metrics.counters.rows_evaluated = stream_metrics.input_rows;

        if stream_metrics.memory_bytes > 0 {
            ctx.resource_manager
                .reserve_memory(stream_metrics.memory_bytes)?;
            self.memory_reserved = stream_metrics.memory_bytes;
        }

        self.schema = schema;
        self.result_rows = rows;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "StreamingAggregate",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.result_rows.len() {
            let row = self.result_rows[self.cursor].clone();
            self.cursor += 1;
            self.metrics.actual_rows += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.result_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical hash / external aggregation execution operator.
pub struct AggregateExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    group_by: Vec<Expr>,
    aggregates: Vec<AggregateExpr>,
    aggregate_config: AggregateConfig,
    schema: Schema,
    result_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl AggregateExecutionOperator {
    /// Creates a new `AggregateExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        group_by: Vec<Expr>,
        aggregates: Vec<AggregateExpr>,
        aggregate_config: AggregateConfig,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let metrics = OperatorMetrics::new(OperatorKind::Aggregate, estimated_rows, cost);
        Self {
            child,
            group_by,
            aggregates,
            aggregate_config,
            schema: Schema::new_projected(Vec::new()),
            result_rows: Vec::new(),
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for AggregateExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("Aggregate", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        self.child.open(ctx)?;

        let mut input_rows = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            input_rows.push(row);
        }
        self.child.close(ctx)?;

        self.metrics.input_rows = input_rows.len();

        let (schema, rows, agg_metrics) = AggregateOperator::apply_with_config(
            input_rows,
            self.child.schema(),
            &self.group_by,
            &self.aggregates,
            &self.aggregate_config,
        )?;

        self.metrics.counters.group_count = agg_metrics.groups_created;
        self.metrics.counters.spill_partitions = agg_metrics.spill_partitions;
        self.metrics.counters.runs_created = agg_metrics.runs_created;
        self.metrics.counters.bytes_spilled = agg_metrics.bytes_spilled;
        self.metrics.counters.merge_passes = agg_metrics.merge_passes;
        self.metrics.counters.peak_memory_bytes = agg_metrics.peak_memory_bytes;
        self.metrics.counters.rows_evaluated = self.metrics.input_rows;

        if agg_metrics.peak_memory_bytes > 0 {
            ctx.resource_manager
                .reserve_memory(agg_metrics.peak_memory_bytes)?;
            self.memory_reserved = agg_metrics.peak_memory_bytes;
        }

        self.schema = schema;
        self.result_rows = rows;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "Aggregate",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.result_rows.len() {
            let row = self.result_rows[self.cursor].clone();
            self.cursor += 1;
            self.metrics.actual_rows += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.result_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical window function execution operator.
pub struct WindowExecutionOperator {
    child: Box<dyn ExecutionOperator>,
    window_exprs: Vec<crate::query::expr::WindowExpr>,
    schema: Schema,
    result_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl WindowExecutionOperator {
    /// Creates a new `WindowExecutionOperator`.
    pub fn new(
        child: Box<dyn ExecutionOperator>,
        window_exprs: Vec<crate::query::expr::WindowExpr>,
        estimated_rows: Option<f64>,
        cost: Option<f64>,
    ) -> Self {
        let metrics = OperatorMetrics::new(OperatorKind::Window, estimated_rows, cost);
        Self {
            child,
            window_exprs,
            schema: Schema::new_projected(Vec::new()),
            result_rows: Vec::new(),
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for WindowExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("Window", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        self.child.open(ctx)?;

        let mut input_rows = Vec::new();
        while let Some(row) = self.child.next(ctx)? {
            input_rows.push(row);
        }
        self.child.close(ctx)?;

        self.metrics.input_rows = input_rows.len();

        let (schema, rows) =
            WindowOperator::apply(input_rows, self.child.schema(), &self.window_exprs)?;

        let estimated_mem = rows.len() * (std::mem::size_of::<Row>() + 64);
        if estimated_mem > 0 {
            ctx.resource_manager.reserve_memory(estimated_mem)?;
            self.memory_reserved = estimated_mem;
        }

        self.schema = schema;
        self.result_rows = rows;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "Window",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.result_rows.len() {
            let row = self.result_rows[self.cursor].clone();
            self.cursor += 1;
            self.metrics.actual_rows += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.result_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        self.child.close(ctx)
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.child.metrics()];
        m
    }
}

/// Physical join execution operator encapsulating left, right, joined rows, and execution metrics.
pub struct JoinExecutionOperator {
    left: Box<dyn ExecutionOperator>,
    right: Box<dyn ExecutionOperator>,
    schema: Schema,
    result_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl JoinExecutionOperator {
    /// Creates a new `JoinExecutionOperator`.
    pub fn new(
        left: Box<dyn ExecutionOperator>,
        right: Box<dyn ExecutionOperator>,
        schema: Schema,
        result_rows: Vec<Row>,
        metrics: OperatorMetrics,
    ) -> Self {
        Self {
            left,
            right,
            schema,
            result_rows,
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for JoinExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition("Join", self.state, &[OperatorLifecycleState::Created])?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        let estimated = self.result_rows.len() * (std::mem::size_of::<Row>() + 64);
        if estimated > 0 {
            ctx.resource_manager.reserve_memory(estimated)?;
            self.memory_reserved = estimated;
        }

        self.left.open(ctx)?;
        self.right.open(ctx)?;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "Join",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.result_rows.len() {
            let row = self.result_rows[self.cursor].clone();
            self.cursor += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.result_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        let _ = self.left.close(ctx);
        let _ = self.right.close(ctx);
        Ok(())
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.left.metrics(), self.right.metrics()];
        m
    }
}

/// Physical relational set operation execution operator.
pub struct SetOperationExecutionOperator {
    left: Box<dyn ExecutionOperator>,
    right: Box<dyn ExecutionOperator>,
    schema: Schema,
    result_rows: Vec<Row>,
    cursor: usize,
    state: OperatorLifecycleState,
    metrics: OperatorMetrics,
    memory_reserved: usize,
}

impl SetOperationExecutionOperator {
    /// Creates a new `SetOperationExecutionOperator`.
    pub fn new(
        left: Box<dyn ExecutionOperator>,
        right: Box<dyn ExecutionOperator>,
        schema: Schema,
        result_rows: Vec<Row>,
        metrics: OperatorMetrics,
    ) -> Self {
        Self {
            left,
            right,
            schema,
            result_rows,
            cursor: 0,
            state: OperatorLifecycleState::Created,
            metrics,
            memory_reserved: 0,
        }
    }
}

impl ExecutionOperator for SetOperationExecutionOperator {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        validate_transition(
            "SetOperation",
            self.state,
            &[OperatorLifecycleState::Created],
        )?;
        self.state = OperatorLifecycleState::Opened;
        self.cursor = 0;

        let estimated = self.result_rows.len() * (std::mem::size_of::<Row>() + 64);
        if estimated > 0 {
            ctx.resource_manager.reserve_memory(estimated)?;
            self.memory_reserved = estimated;
        }

        self.left.open(ctx)?;
        self.right.open(ctx)?;
        Ok(())
    }

    fn next(&mut self, ctx: &mut ExecutionContext) -> Result<Option<Row>> {
        validate_transition(
            "SetOperation",
            self.state,
            &[
                OperatorLifecycleState::Opened,
                OperatorLifecycleState::Running,
                OperatorLifecycleState::Exhausted,
            ],
        )?;

        if self.state == OperatorLifecycleState::Exhausted {
            return Ok(None);
        }

        ctx.check_cancellation()?;

        if self.cursor < self.result_rows.len() {
            let row = self.result_rows[self.cursor].clone();
            self.cursor += 1;
            self.state = OperatorLifecycleState::Running;
            Ok(Some(row))
        } else {
            self.state = OperatorLifecycleState::Exhausted;
            Ok(None)
        }
    }

    fn close(&mut self, ctx: &mut ExecutionContext) -> Result<()> {
        if self.state == OperatorLifecycleState::Closed {
            return Ok(());
        }
        if self.memory_reserved > 0 {
            ctx.resource_manager.release_memory(self.memory_reserved);
            self.memory_reserved = 0;
        }
        self.result_rows.clear();
        self.state = OperatorLifecycleState::Closed;
        let _ = self.left.close(ctx);
        let _ = self.right.close(ctx);
        Ok(())
    }

    fn state(&self) -> OperatorLifecycleState {
        self.state
    }

    fn metrics(&self) -> OperatorMetrics {
        let mut m = self.metrics.clone();
        m.children = vec![self.left.metrics(), self.right.metrics()];
        m
    }
}

/// Unified Physical Plan Executor orchestrating full query execution through the canonical lifecycle.
pub struct PhysicalPlanExecutor;

impl PhysicalPlanExecutor {
    /// Executes a physical execution operator tree through its canonical lifecycle:
    /// `open -> next* -> close -> metrics`.
    pub fn execute_operator_tree(
        mut root: Box<dyn ExecutionOperator>,
        ctx: &mut ExecutionContext,
    ) -> Result<(QueryResult, OperatorMetrics)> {
        let op_start = Instant::now();

        // 1. Open root operator
        if let Err(open_err) = root.open(ctx) {
            if matches!(
                open_err,
                Error::ResourceLimitExceeded { .. }
                    | Error::ExternalSortQuotaExceeded { .. }
                    | Error::ExternalAggregateQuotaExceeded { .. }
                    | Error::ExternalJoinQuotaExceeded { .. }
                    | Error::ExternalMultiIndexQuotaExceeded { .. }
            ) {
                ctx.termination_status = ExecutionTerminationStatus::FailedResourceLimit;
            } else {
                ctx.termination_status = ExecutionTerminationStatus::FailedExecutionError;
            }
            return safe_close(&mut *root, ctx, Some(open_err)).map(|_| unreachable!());
        }

        // 2. Pull rows from root operator
        let mut output_rows = Vec::new();
        let mut execution_error: Option<Error> = None;

        loop {
            if let Err(canc_err) = ctx.check_cancellation() {
                ctx.termination_status = ExecutionTerminationStatus::Cancelled;
                execution_error = Some(canc_err);
                break;
            }

            match root.next(ctx) {
                Ok(Some(row)) => {
                    output_rows.push(row);
                }
                Ok(None) => {
                    break;
                }
                Err(err) => {
                    if matches!(
                        err,
                        Error::ResourceLimitExceeded { .. }
                            | Error::ExternalSortQuotaExceeded { .. }
                            | Error::ExternalAggregateQuotaExceeded { .. }
                            | Error::ExternalJoinQuotaExceeded { .. }
                            | Error::ExternalMultiIndexQuotaExceeded { .. }
                    ) {
                        ctx.termination_status = ExecutionTerminationStatus::FailedResourceLimit;
                    } else {
                        ctx.termination_status = ExecutionTerminationStatus::FailedExecutionError;
                    }
                    execution_error = Some(err);
                    break;
                }
            }
        }

        // 3. Close root operator with error preservation
        let schema = root.schema().clone();
        let mut metrics = root.metrics();
        metrics.execution_time = op_start.elapsed();

        let close_res = safe_close(&mut *root, ctx, execution_error);
        close_res?;

        // 4. Assert clean resource release
        if !ctx.resource_manager.is_clean() {
            return Err(Error::InternalExecutionError(format!(
                "Resource leak detected after execution: memory={} bytes, temp_files={}, temp_bytes={}",
                ctx.resource_manager.active_memory_reservations(),
                ctx.resource_manager.active_temp_files(),
                ctx.resource_manager.active_temp_bytes(),
            )));
        }

        Ok((QueryResult::new(schema, output_rows), metrics))
    }
}
