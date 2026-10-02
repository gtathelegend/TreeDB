use std::collections::HashMap;
use std::fmt;

use crate::query::plan_cache::{ParamPlanningSignature, ResourcePlanningSignature};

/// Default maximum number of distinct feedback entries retained in memory.
pub const DEFAULT_FEEDBACK_CAPACITY: usize = 512;

/// Default smoothing factor for exponentially weighted moving average (EWMA).
pub const DEFAULT_FEEDBACK_ALPHA: f64 = 0.3;

/// Default estimation error ratio threshold (e.g. 2.0x error) required to trigger re-optimization.
pub const DEFAULT_REPLAN_THRESHOLD_RATIO: f64 = 2.0;

/// Default minimum number of observations required before triggering a re-optimization.
pub const DEFAULT_REPLAN_MIN_OBSERVATIONS: usize = 2;

/// Upper multiplier clamp for a single observation relative to the estimate.
pub const OUTLIER_MAX_RATIO: f64 = 100.0;

/// Lower multiplier clamp for a single observation relative to the estimate.
pub const OUTLIER_MIN_RATIO: f64 = 0.01;

/// Deterministic classification of operator spill behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpillStatus {
    /// The operator has consistently fit in memory and never spilled.
    NeverSpilled,
    /// The operator has spilled to disk in observed executions.
    Spilled,
    /// No conclusive spill observation is available.
    Unknown,
}

impl fmt::Display for SpillStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeverSpilled => write!(f, "NeverSpilled"),
            Self::Spilled => write!(f, "Spilled"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

/// Observed runtime resource usage for an executed physical operator.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceObservation {
    /// Configured execution memory budget in bytes at observation time.
    pub memory_budget_bytes: usize,
    /// Optimizer-estimated peak memory in bytes.
    pub estimated_peak_memory_bytes: usize,
    /// Runtime actual peak memory in bytes observed.
    pub actual_peak_memory_bytes: usize,
    /// Optimizer-estimated spill bytes.
    pub estimated_spill_bytes: usize,
    /// Runtime actual bytes spilled to temporary disk files.
    pub actual_spill_bytes: usize,
    /// Number of temporary spill runs or partitions created.
    pub spill_runs: usize,
    /// Total temporary file bytes written.
    pub temporary_bytes: usize,
    /// Total number of temporary files used.
    pub temporary_files: usize,
    /// Observed wall-clock execution time in microseconds.
    pub execution_time_us: u64,
    /// Whether an external disk spill occurred.
    pub did_spill: bool,
    /// Whether the observed actual was clamped to prevent outlier destruction.
    pub is_clamped: bool,
}

impl ResourceObservation {
    /// Creates a default `ResourceObservation` with the specified memory budget.
    pub fn new(memory_budget_bytes: usize) -> Self {
        Self {
            memory_budget_bytes,
            estimated_peak_memory_bytes: 0,
            actual_peak_memory_bytes: 0,
            estimated_spill_bytes: 0,
            actual_spill_bytes: 0,
            spill_runs: 0,
            temporary_bytes: 0,
            temporary_files: 0,
            execution_time_us: 0,
            did_spill: false,
            is_clamped: false,
        }
    }

    /// Creates a `ResourceObservation` with complete metrics.
    #[allow(clippy::too_many_arguments)]
    pub fn with_metrics(
        memory_budget_bytes: usize,
        estimated_peak_memory_bytes: usize,
        actual_peak_memory_bytes: usize,
        estimated_spill_bytes: usize,
        actual_spill_bytes: usize,
        spill_runs: usize,
        temporary_bytes: usize,
        temporary_files: usize,
        execution_time_us: u64,
    ) -> Self {
        let did_spill = actual_spill_bytes > 0 || spill_runs > 0 || temporary_files > 0;
        Self {
            memory_budget_bytes,
            estimated_peak_memory_bytes,
            actual_peak_memory_bytes,
            estimated_spill_bytes,
            actual_spill_bytes,
            spill_runs,
            temporary_bytes,
            temporary_files,
            execution_time_us,
            did_spill,
            is_clamped: false,
        }
    }
}

/// Smoothed runtime resource feedback aggregated over multiple executions.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceFeedback {
    /// Number of resource observations recorded.
    pub observation_count: usize,
    /// Smoothed effective peak memory estimate in bytes (EWMA).
    pub effective_peak_memory_bytes: f64,
    /// Smoothed effective spill bytes estimate (EWMA).
    pub effective_spill_bytes: f64,
    /// Smoothed effective spill runs estimate (EWMA).
    pub effective_spill_runs: f64,
    /// Aggregate spill status classification.
    pub spill_status: SpillStatus,
    /// Most recent resource observation.
    pub last_observation: Option<ResourceObservation>,
    /// Whether resource misestimates triggered replanning.
    pub replan_triggered: bool,
}

impl ResourceFeedback {
    /// Creates a new `ResourceFeedback` from an initial observation.
    pub fn new(obs: ResourceObservation) -> Self {
        let did_spill = obs.did_spill || obs.actual_spill_bytes > 0 || obs.spill_runs > 0;
        let spill_status = if did_spill {
            SpillStatus::Spilled
        } else {
            SpillStatus::NeverSpilled
        };
        Self {
            observation_count: 1,
            effective_peak_memory_bytes: obs.actual_peak_memory_bytes as f64,
            effective_spill_bytes: obs.actual_spill_bytes as f64,
            effective_spill_runs: obs.spill_runs as f64,
            spill_status,
            last_observation: Some(obs),
            replan_triggered: false,
        }
    }

    /// Returns the confidence level of this resource feedback based on observation count in `[0.0, 1.0]`.
    pub fn confidence(&self) -> f64 {
        match self.observation_count {
            0 => 0.0,
            1 => 0.5,
            2 => 0.75,
            3 => 0.9,
            _ => 1.0,
        }
    }
}

/// Normalized structural context categorizing the physical operator or predicate being observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FeedbackContext {
    /// Table scan with access path and optional residual filter expression.
    Scan {
        table_name: String,
        predicate_repr: String,
        access_path_repr: String,
    },
    /// In-memory or post-scan filter expression.
    Filter {
        table_name: String,
        expr_repr: String,
    },
    /// Relational join between two table relations.
    Join {
        join_type: String,
        left_table: String,
        right_table: String,
        on_clause: String,
    },
    /// Relational SEMI or ANTI join probe.
    SemiAnti {
        strategy: String,
        left_table: String,
        right_table: String,
        on_clause: String,
    },
    /// Multi-index intersection or union candidate generation.
    MultiIndex {
        table_name: String,
        strategy: String,
        indexes: Vec<String>,
    },
    /// Composite index skip-scan prefix enumeration.
    SkipScan {
        table_name: String,
        index_name: String,
        prefix_len: usize,
    },
    /// External spilled execution operator (sort, aggregate, hash join, multi-index).
    ExternalOp {
        op_kind: String,
        relation_name: String,
    },
    /// Sort operator with ordering keys representation.
    Sort { order_by: String },
    /// Aggregation operator with group-by and aggregate expressions.
    Aggregate {
        group_by: String,
        aggregates: String,
    },
}

impl fmt::Display for FeedbackContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan {
                table_name,
                predicate_repr,
                access_path_repr,
            } => write!(
                f,
                "Scan(table={table_name}, access={access_path_repr}, filter={predicate_repr})"
            ),
            Self::Filter {
                table_name,
                expr_repr,
            } => write!(f, "Filter(table={table_name}, expr={expr_repr})"),
            Self::Join {
                join_type,
                left_table,
                right_table,
                on_clause,
            } => write!(
                f,
                "Join(type={join_type}, left={left_table}, right={right_table}, on={on_clause})"
            ),
            Self::SemiAnti {
                strategy,
                left_table,
                right_table,
                on_clause,
            } => write!(
                f,
                "SemiAnti(strategy={strategy}, left={left_table}, right={right_table}, on={on_clause})"
            ),
            Self::MultiIndex {
                table_name,
                strategy,
                indexes,
            } => write!(
                f,
                "MultiIndex(table={table_name}, strategy={strategy}, indexes=[{}])",
                indexes.join(", ")
            ),
            Self::SkipScan {
                table_name,
                index_name,
                prefix_len,
            } => write!(
                f,
                "SkipScan(table={table_name}, index={index_name}, prefix_len={prefix_len})"
            ),
            Self::ExternalOp {
                op_kind,
                relation_name,
            } => write!(
                f,
                "ExternalOp(kind={op_kind}, relation={relation_name})"
            ),
            Self::Sort { order_by } => write!(f, "Sort(order_by=[{order_by}])"),
            Self::Aggregate {
                group_by,
                aggregates,
            } => write!(
                f,
                "Aggregate(group_by=[{group_by}], aggs=[{aggregates}])"
            ),
        }
    }
}

/// Strongly typed, deterministic lookup key for adaptive cardinality and resource feedback.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FeedbackKey {
    /// Structural operator / predicate context.
    pub context: FeedbackContext,
    /// Parameter planning signature if applicable (isolating parameter shapes).
    pub param_signature: Option<ParamPlanningSignature>,
    /// Database statistics generation at observation time.
    pub statistics_generation: u64,
    /// Resource planning signature (isolating memory-budget and spill configurations).
    pub resource_signature: Option<ResourcePlanningSignature>,
}

impl FeedbackKey {
    /// Creates a new `FeedbackKey`.
    pub fn new(
        context: FeedbackContext,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self {
            context,
            param_signature,
            statistics_generation,
            resource_signature: None,
        }
    }

    /// Sets the resource planning signature for this feedback key.
    pub fn with_resource_signature(
        mut self,
        resource_signature: Option<ResourcePlanningSignature>,
    ) -> Self {
        self.resource_signature = resource_signature;
        self
    }

    /// Helper to create a scan feedback key.
    pub fn scan(
        table_name: impl Into<String>,
        access_path_repr: impl fmt::Display,
        predicate_repr: impl fmt::Display,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::new(
            FeedbackContext::Scan {
                table_name: table_name.into(),
                predicate_repr: predicate_repr.to_string(),
                access_path_repr: access_path_repr.to_string(),
            },
            param_signature,
            statistics_generation,
        )
    }

    /// Helper to create a join feedback key.
    pub fn join(
        left_table: impl Into<String>,
        right_table: impl Into<String>,
        on_clause: impl fmt::Display,
        join_type: impl fmt::Display,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::new(
            FeedbackContext::Join {
                join_type: join_type.to_string(),
                left_table: left_table.into(),
                right_table: right_table.into(),
                on_clause: on_clause.to_string(),
            },
            param_signature,
            statistics_generation,
        )
    }

    /// Helper to create a multi-index feedback key.
    pub fn multi_index(
        table_name: impl Into<String>,
        strategy: impl Into<String>,
        indexes: Vec<String>,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::new(
            FeedbackContext::MultiIndex {
                table_name: table_name.into(),
                strategy: strategy.into(),
                indexes,
            },
            param_signature,
            statistics_generation,
        )
    }

    /// Helper to create a skip-scan feedback key.
    pub fn skip_scan(
        table_name: impl Into<String>,
        index_name: impl Into<String>,
        prefix_len: usize,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::new(
            FeedbackContext::SkipScan {
                table_name: table_name.into(),
                index_name: index_name.into(),
                prefix_len,
            },
            param_signature,
            statistics_generation,
        )
    }

    /// Helper to create an external operator feedback key.
    pub fn external_op(
        op_kind: impl Into<String>,
        relation_name: impl Into<String>,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::new(
            FeedbackContext::ExternalOp {
                op_kind: op_kind.into(),
                relation_name: relation_name.into(),
            },
            param_signature,
            statistics_generation,
        )
    }

    /// Helper to create a semi/anti join feedback key.
    pub fn semi_anti(
        left_table: impl Into<String>,
        right_table: impl Into<String>,
        on_clause: impl fmt::Display,
        join_type: crate::sql::ast::JoinType,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::new(
            FeedbackContext::SemiAnti {
                strategy: format!("{:?}", join_type),
                left_table: left_table.into(),
                right_table: right_table.into(),
                on_clause: on_clause.to_string(),
            },
            param_signature,
            statistics_generation,
        )
    }

    /// Helper to create an external operator feedback key.
    pub fn external_operator(
        relation_name: impl Into<String>,
        op_kind: impl Into<String>,
        param_signature: Option<ParamPlanningSignature>,
        statistics_generation: u64,
    ) -> Self {
        Self::external_op(
            op_kind,
            relation_name,
            param_signature,
            statistics_generation,
        )
    }

    /// Returns the primary table name associated with this feedback key if available.
    pub fn table_name(&self) -> Option<&str> {
        match &self.context {
            FeedbackContext::Scan { table_name, .. }
            | FeedbackContext::Filter { table_name, .. }
            | FeedbackContext::MultiIndex { table_name, .. }
            | FeedbackContext::SkipScan { table_name, .. } => Some(table_name.as_str()),
            FeedbackContext::Join { left_table, .. }
            | FeedbackContext::SemiAnti { left_table, .. } => Some(left_table.as_str()),
            FeedbackContext::ExternalOp { relation_name, .. } => Some(relation_name.as_str()),
            FeedbackContext::Sort { .. } | FeedbackContext::Aggregate { .. } => None,
        }
    }
}

/// A single runtime execution observation for an operator or predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackObservation {
    /// Optimizer-estimated output rows.
    pub estimated_rows: f64,
    /// Runtime actual output rows observed.
    pub actual_rows: f64,
    /// Estimated operator cost if available.
    pub estimated_cost: Option<f64>,
    /// Observed wall-clock time in microseconds if available.
    pub actual_time_us: Option<u64>,
    /// Number of input rows evaluated if applicable.
    pub input_rows: Option<usize>,
    /// Number of output rows produced if applicable.
    pub output_rows: Option<usize>,
    /// Actual distinct prefix values for skip scans.
    pub actual_distinct_prefixes: Option<usize>,
    /// Actual candidate rows for multi-index scans.
    pub actual_candidates: Option<usize>,
    /// Left input rows for joins.
    pub left_rows: Option<usize>,
    /// Right input rows for joins.
    pub right_rows: Option<usize>,
    /// Whether an external spill occurred.
    pub spill_occurred: Option<bool>,
    /// Detailed resource metrics observation if available.
    pub resource: Option<ResourceObservation>,
    /// Whether the observed actual was clamped to prevent outlier destruction.
    pub is_clamped: bool,
}

impl FeedbackObservation {
    /// Creates a new `FeedbackObservation` with basic row counts.
    pub fn new(estimated_rows: f64, actual_rows: f64) -> Self {
        Self {
            estimated_rows: estimated_rows.max(0.0),
            actual_rows: actual_rows.max(0.0),
            estimated_cost: None,
            actual_time_us: None,
            input_rows: None,
            output_rows: Some(actual_rows as usize),
            actual_distinct_prefixes: None,
            actual_candidates: None,
            left_rows: None,
            right_rows: None,
            spill_occurred: None,
            resource: None,
            is_clamped: false,
        }
    }

    /// Creates a new `FeedbackObservation` with full execution metrics.
    pub fn with_metrics(
        estimated_rows: f64,
        actual_rows: f64,
        estimated_cost: Option<f64>,
        actual_time_us: Option<u64>,
        input_rows: Option<usize>,
        output_rows: Option<usize>,
    ) -> Self {
        Self {
            estimated_rows: estimated_rows.max(0.0),
            actual_rows: actual_rows.max(0.0),
            estimated_cost,
            actual_time_us,
            input_rows,
            output_rows,
            actual_distinct_prefixes: None,
            actual_candidates: None,
            left_rows: None,
            right_rows: None,
            spill_occurred: None,
            resource: None,
            is_clamped: false,
        }
    }

    /// Attaches resource observation metrics.
    pub fn with_resource(mut self, resource: ResourceObservation) -> Self {
        self.resource = Some(resource);
        self
    }

    /// Computes the estimation error ratio (`actual / estimated` or `estimated / actual`).
    pub fn error_ratio(&self) -> f64 {
        compute_estimation_error(self.estimated_rows, self.actual_rows)
    }

    /// Computes the absolute estimation error.
    pub fn absolute_error(&self) -> f64 {
        (self.actual_rows - self.estimated_rows).abs()
    }
}

/// Action taken during feedback evaluation and recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackAction {
    /// Initial observation recorded for this key.
    Recorded,
    /// Existing observation updated using EWMA smoothing.
    Updated,
    /// Observation was clamped due to outlier boundaries.
    Clamped,
    /// Severe misestimate crossed the threshold and triggered re-optimization.
    ReplanTriggered,
    /// Feedback entry invalidated due to catalog or statistics changes.
    Invalidated,
}

impl fmt::Display for FeedbackAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recorded => write!(f, "Recorded"),
            Self::Updated => write!(f, "Updated"),
            Self::Clamped => write!(f, "Clamped"),
            Self::ReplanTriggered => write!(f, "ReplanTriggered"),
            Self::Invalidated => write!(f, "Invalidated"),
        }
    }
}

/// An entry in the runtime cardinality and resource feedback registry.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackEntry {
    /// Feedback identity key.
    pub key: FeedbackKey,
    /// Total number of executions observed for this key.
    pub observation_count: usize,
    /// Effective smoothed cardinality estimate computed via EWMA.
    pub effective_cardinality: f64,
    /// Most recent estimated row count.
    pub last_estimated_rows: f64,
    /// Most recent actual row count.
    pub last_actual_rows: f64,
    /// Most recent estimation error ratio.
    pub last_estimation_error: f64,
    /// Smoothed runtime resource feedback if available.
    pub resource_feedback: Option<ResourceFeedback>,
    /// Whether the latest observation triggered plan cache invalidation / replanning.
    pub replan_triggered: bool,
    /// Last action performed on this feedback entry.
    pub last_action: FeedbackAction,
    /// Logical access timestamp for deterministic LRU eviction.
    pub last_accessed: u64,
}

impl FeedbackEntry {
    /// Creates a new `FeedbackEntry` from an initial observation.
    pub fn new(key: FeedbackKey, obs: FeedbackObservation, access_time: u64) -> Self {
        let err = obs.error_ratio();
        let action =
            if obs.is_clamped || obs.resource.as_ref().map(|r| r.is_clamped).unwrap_or(false) {
                FeedbackAction::Clamped
            } else {
                FeedbackAction::Recorded
            };
        let rf = obs.resource.map(ResourceFeedback::new);
        Self {
            key,
            observation_count: 1,
            effective_cardinality: obs.actual_rows,
            last_estimated_rows: obs.estimated_rows,
            last_actual_rows: obs.actual_rows,
            last_estimation_error: err,
            resource_feedback: rf,
            replan_triggered: false,
            last_action: action,
            last_accessed: access_time,
        }
    }

    /// Returns the confidence level of this feedback entry based on observation count in `[0.0, 1.0]`.
    pub fn confidence(&self) -> f64 {
        match self.observation_count {
            0 => 0.0,
            1 => 0.5,
            2 => 0.75,
            3 => 0.9,
            _ => 1.0,
        }
    }

    /// Returns the confidence level of the resource feedback.
    pub fn resource_confidence(&self) -> f64 {
        self.resource_feedback
            .as_ref()
            .map(|r| r.confidence())
            .unwrap_or(0.0)
    }

    /// Returns feedback-adjusted peak memory estimate in bytes.
    pub fn adjusted_peak_memory(&self, base_estimate_bytes: f64) -> f64 {
        if let Some(res) = &self.resource_feedback {
            let conf = res.confidence();
            (conf * res.effective_peak_memory_bytes) + ((1.0 - conf) * base_estimate_bytes)
        } else {
            base_estimate_bytes
        }
    }

    /// Returns feedback-adjusted spill bytes estimate.
    pub fn adjusted_spill_bytes(&self, base_estimate_bytes: f64) -> f64 {
        if let Some(res) = &self.resource_feedback {
            let conf = res.confidence();
            (conf * res.effective_spill_bytes) + ((1.0 - conf) * base_estimate_bytes)
        } else {
            base_estimate_bytes
        }
    }

    /// Returns the observed spill classification.
    pub fn spill_status(&self) -> SpillStatus {
        self.resource_feedback
            .as_ref()
            .map(|r| r.spill_status)
            .unwrap_or(SpillStatus::Unknown)
    }

    /// Returns whether this entry indicates a replan is triggered.
    pub fn should_replan(&self) -> bool {
        self.replan_triggered
    }
}

/// In-memory, bounded, invalidation-safe registry of runtime cardinality and resource feedback.
#[derive(Debug, Clone)]
pub struct FeedbackRegistry {
    capacity: usize,
    entries: HashMap<FeedbackKey, FeedbackEntry>,
    access_counter: u64,
    alpha: f64,
    replan_threshold_ratio: f64,
    replan_min_observations: usize,
}

impl Default for FeedbackRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedbackRegistry {
    /// Creates a new `FeedbackRegistry` with default capacity and parameters.
    pub fn new() -> Self {
        Self::with_config(
            DEFAULT_FEEDBACK_CAPACITY,
            DEFAULT_FEEDBACK_ALPHA,
            DEFAULT_REPLAN_THRESHOLD_RATIO,
            DEFAULT_REPLAN_MIN_OBSERVATIONS,
        )
    }

    /// Creates a new `FeedbackRegistry` with a given capacity and default hyperparameters.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_config(
            capacity,
            DEFAULT_FEEDBACK_ALPHA,
            DEFAULT_REPLAN_THRESHOLD_RATIO,
            DEFAULT_REPLAN_MIN_OBSERVATIONS,
        )
    }

    /// Creates a new `FeedbackRegistry` with customized capacity and threshold settings.
    pub fn with_config(
        capacity: usize,
        alpha: f64,
        replan_threshold_ratio: f64,
        replan_min_observations: usize,
    ) -> Self {
        let capacity = capacity.max(1);
        let alpha = alpha.clamp(0.01, 1.0);
        let replan_threshold_ratio = replan_threshold_ratio.max(1.01);
        let replan_min_observations = replan_min_observations.max(1);

        Self {
            capacity,
            entries: HashMap::with_capacity(capacity),
            access_counter: 0,
            alpha,
            replan_threshold_ratio,
            replan_min_observations,
        }
    }

    /// Records an execution observation for the given key, updating EWMA and checking replan triggers.
    pub fn record_observation(
        &mut self,
        key: FeedbackKey,
        mut obs: FeedbackObservation,
    ) -> &FeedbackEntry {
        self.access_counter += 1;
        let counter = self.access_counter;

        // Apply cardinality outlier clamping if necessary
        let est_safe = obs.estimated_rows.max(0.001);
        let max_allowed = est_safe * OUTLIER_MAX_RATIO;
        let min_allowed = est_safe * OUTLIER_MIN_RATIO;

        if obs.actual_rows > max_allowed {
            obs.actual_rows = max_allowed;
            obs.is_clamped = true;
        } else if obs.actual_rows < min_allowed && obs.actual_rows > 0.0 {
            obs.actual_rows = min_allowed;
            obs.is_clamped = true;
        }

        // Apply resource outlier clamping if present
        let mut resource_error_triggered = false;
        if let Some(res) = &mut obs.resource {
            let est_mem = (res.estimated_peak_memory_bytes.max(1024)) as f64;
            let max_mem = est_mem * OUTLIER_MAX_RATIO;
            let min_mem = est_mem * OUTLIER_MIN_RATIO;
            if res.actual_peak_memory_bytes as f64 > max_mem {
                res.actual_peak_memory_bytes = max_mem as usize;
                res.is_clamped = true;
            } else if (res.actual_peak_memory_bytes as f64) < min_mem
                && res.actual_peak_memory_bytes > 0
            {
                res.actual_peak_memory_bytes = min_mem as usize;
                res.is_clamped = true;
            }

            if res.estimated_spill_bytes > 0 {
                let est_spill = (res.estimated_spill_bytes.max(1024)) as f64;
                let max_spill = est_spill * OUTLIER_MAX_RATIO;
                let min_spill = est_spill * OUTLIER_MIN_RATIO;
                if res.actual_spill_bytes as f64 > max_spill {
                    res.actual_spill_bytes = max_spill as usize;
                    res.is_clamped = true;
                } else if (res.actual_spill_bytes as f64) < min_spill && res.actual_spill_bytes > 0
                {
                    res.actual_spill_bytes = min_spill as usize;
                    res.is_clamped = true;
                }
            }

            // Check resource trigger conditions
            let unexpected_spill = res.estimated_spill_bytes == 0
                && (res.actual_spill_bytes > 0 || res.did_spill || res.spill_runs > 0);
            let mem_err = compute_estimation_error(
                res.estimated_peak_memory_bytes as f64,
                res.actual_peak_memory_bytes as f64,
            );
            let spill_err = if res.estimated_spill_bytes > 0 && res.actual_spill_bytes > 0 {
                compute_estimation_error(
                    res.estimated_spill_bytes as f64,
                    res.actual_spill_bytes as f64,
                )
            } else {
                1.0
            };

            resource_error_triggered = unexpected_spill
                || (res.estimated_peak_memory_bytes > 0 && mem_err >= self.replan_threshold_ratio)
                || spill_err >= self.replan_threshold_ratio;
        }

        let err_ratio = compute_estimation_error(obs.estimated_rows, obs.actual_rows);
        let cardinality_crosses_threshold = err_ratio >= self.replan_threshold_ratio
            || (obs.estimated_rows == 0.0 && obs.actual_rows > 0.0);

        let crosses_threshold = cardinality_crosses_threshold || resource_error_triggered;

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.observation_count += 1;
            entry.last_estimated_rows = obs.estimated_rows;
            entry.last_actual_rows = obs.actual_rows;
            entry.last_estimation_error = err_ratio;
            entry.last_accessed = counter;

            // EWMA cardinality: new_estimate = alpha * actual + (1 - alpha) * previous_feedback
            entry.effective_cardinality =
                (self.alpha * obs.actual_rows) + ((1.0 - self.alpha) * entry.effective_cardinality);

            // EWMA resource feedback if present
            if let Some(ref res_obs) = obs.resource {
                let did_spill =
                    res_obs.did_spill || res_obs.actual_spill_bytes > 0 || res_obs.spill_runs > 0;
                if let Some(rf) = &mut entry.resource_feedback {
                    rf.observation_count += 1;
                    rf.effective_peak_memory_bytes = (self.alpha
                        * res_obs.actual_peak_memory_bytes as f64)
                        + ((1.0 - self.alpha) * rf.effective_peak_memory_bytes);
                    rf.effective_spill_bytes = (self.alpha * res_obs.actual_spill_bytes as f64)
                        + ((1.0 - self.alpha) * rf.effective_spill_bytes);
                    rf.effective_spill_runs = (self.alpha * res_obs.spill_runs as f64)
                        + ((1.0 - self.alpha) * rf.effective_spill_runs);
                    if did_spill {
                        rf.spill_status = SpillStatus::Spilled;
                    } else if rf.spill_status != SpillStatus::Spilled {
                        rf.spill_status = SpillStatus::NeverSpilled;
                    }
                    rf.last_observation = Some(res_obs.clone());
                } else {
                    entry.resource_feedback = Some(ResourceFeedback::new(res_obs.clone()));
                }
            }

            let should_replan =
                entry.observation_count >= self.replan_min_observations && crosses_threshold;

            entry.replan_triggered = should_replan;
            if let Some(rf) = &mut entry.resource_feedback {
                rf.replan_triggered = should_replan;
            }

            entry.last_action = if should_replan {
                FeedbackAction::ReplanTriggered
            } else if obs.is_clamped || obs.resource.as_ref().map(|r| r.is_clamped).unwrap_or(false)
            {
                FeedbackAction::Clamped
            } else {
                FeedbackAction::Updated
            };

            self.entries.get(&key).unwrap()
        } else {
            // Evict LRU if capacity exceeded
            if self.entries.len() >= self.capacity {
                self.evict_lru();
            }

            let mut entry = FeedbackEntry::new(key.clone(), obs.clone(), counter);
            if self.replan_min_observations <= 1 && crosses_threshold {
                entry.replan_triggered = true;
                if let Some(rf) = &mut entry.resource_feedback {
                    rf.replan_triggered = true;
                }
                entry.last_action = FeedbackAction::ReplanTriggered;
            }

            self.entries.insert(key.clone(), entry);
            self.entries.get(&key).unwrap()
        }
    }

    /// Looks up feedback for a specific key, returning `None` if absent or generation mismatch.
    pub fn get_feedback(&self, key: &FeedbackKey) -> Option<&FeedbackEntry> {
        self.entries.get(key)
    }

    /// Returns feedback-adjusted cardinality for an estimate if feedback exists and matches the statistics generation.
    pub fn get_adjusted_cardinality(&self, key: &FeedbackKey, base_estimate: f64) -> f64 {
        if let Some(entry) = self.entries.get(key) {
            if entry.key.statistics_generation == key.statistics_generation {
                let conf = entry.confidence();
                return (conf * entry.effective_cardinality) + ((1.0 - conf) * base_estimate);
            }
        }
        base_estimate
    }

    /// Returns feedback-adjusted peak memory estimate in bytes.
    pub fn get_adjusted_peak_memory(&self, key: &FeedbackKey, base_estimate_bytes: f64) -> f64 {
        if let Some(entry) = self.entries.get(key) {
            if entry.key.statistics_generation == key.statistics_generation {
                return entry.adjusted_peak_memory(base_estimate_bytes);
            }
        }
        base_estimate_bytes
    }

    /// Returns feedback-adjusted spill bytes estimate.
    pub fn get_adjusted_spill_bytes(&self, key: &FeedbackKey, base_estimate_bytes: f64) -> f64 {
        if let Some(entry) = self.entries.get(key) {
            if entry.key.statistics_generation == key.statistics_generation {
                return entry.adjusted_spill_bytes(base_estimate_bytes);
            }
        }
        base_estimate_bytes
    }

    /// Returns the spill classification for a feedback key.
    pub fn get_spill_status(&self, key: &FeedbackKey) -> SpillStatus {
        if let Some(entry) = self.entries.get(key) {
            if entry.key.statistics_generation == key.statistics_generation {
                return entry.spill_status();
            }
        }
        SpillStatus::Unknown
    }

    /// Returns `true` if feedback for the given key suggests triggering a query re-optimization.
    pub fn should_replan(&self, key: &FeedbackKey) -> bool {
        self.entries
            .get(key)
            .map(|e| e.replan_triggered)
            .unwrap_or(false)
    }

    /// Records an execution observation (alias for `record_observation`).
    pub fn record(&mut self, key: FeedbackKey, obs: FeedbackObservation) -> &FeedbackEntry {
        self.record_observation(key, obs)
    }

    /// Looks up feedback for a specific key (alias for `get_feedback`).
    pub fn get(&self, key: &FeedbackKey) -> Option<&FeedbackEntry> {
        self.get_feedback(key)
    }

    /// Clears all recorded runtime feedback entries.
    pub fn clear_all(&mut self) {
        self.entries.clear();
    }

    /// Clears all recorded runtime feedback entries (alias for `clear_all`).
    pub fn clear(&mut self) {
        self.clear_all();
    }

    /// Clears feedback entries associated with a specific table.
    pub fn clear_table_feedback(&mut self, table_name: &str) {
        self.entries
            .retain(|k, _| k.table_name() != Some(table_name));
    }

    /// Clears feedback entries associated with a specific table (alias for `clear_table_feedback`).
    pub fn clear_table(&mut self, table_name: &str) {
        self.clear_table_feedback(table_name);
    }

    /// Invalidates all feedback entries whose statistics generation does not match the active generation.
    pub fn invalidate_generation(&mut self, table_name: &str, current_stats_gen: u64) {
        self.entries.retain(|k, _| {
            if k.table_name() == Some(table_name) {
                k.statistics_generation == current_stats_gen
            } else {
                true
            }
        });
    }

    /// Returns the number of active feedback entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the feedback registry is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns a vector containing clones of all active feedback entries.
    pub fn all_entries(&self) -> Vec<FeedbackEntry> {
        self.entries.values().cloned().collect()
    }

    /// Returns an iterator over references to active feedback entries.
    pub fn entries(&self) -> impl Iterator<Item = &FeedbackEntry> {
        self.entries.values()
    }

    fn evict_lru(&mut self) {
        let mut oldest_key: Option<FeedbackKey> = None;
        let mut oldest_time = u64::MAX;

        for (k, entry) in &self.entries {
            if entry.last_accessed < oldest_time {
                oldest_time = entry.last_accessed;
                oldest_key = Some(k.clone());
            }
        }

        if let Some(k) = oldest_key {
            self.entries.remove(&k);
        }
    }
}

/// Safely calculates the estimation error ratio between estimated and actual row counts.
/// Handles zero estimates, division-by-zero, and prevents `NaN`/`Infinity`.
pub fn compute_estimation_error(estimated: f64, actual: f64) -> f64 {
    let est = estimated.max(0.001);
    let act = actual.max(0.001);

    let ratio = act / est;
    if ratio >= 1.0 {
        ratio
    } else {
        1.0 / ratio
    }
}
