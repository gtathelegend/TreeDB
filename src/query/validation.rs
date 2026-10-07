use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::engine::catalog::IndexMetadata;
use crate::engine::types::{DataType, Schema};
use crate::query::expr::{AggregateExpr, Expr};
use crate::query::external_hash_join::analyze_equi_join;
use crate::query::operator::SortDirection;
use crate::query::optimizer::{
    AccessPath, AggregateStrategy, JoinStrategy, PhysicalJoin, PhysicalPlan, PhysicalTableScan,
    SecondaryScanType, TableOptInfo, MAX_MULTI_INDEXES,
};
use crate::query::physical_properties::{
    ordering_property_for_table_scan, OrderingColumn, OrderingProperty, RequiredPhysicalProperties,
    RowGoal,
};
use crate::query::plan_cache::{
    extract_plan_references, ParamPlanningSignature, PlanValidity, ResourcePlanningSignature,
};
use crate::sql::ast::JoinType;
use crate::sql::binder::TableCatalog;

/// Structured error representing a physical plan contract or invariant violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanValidationError {
    /// A referenced column is missing from the available schema.
    MissingColumn {
        operator: String,
        column: String,
        available: Vec<String>,
        reason: String,
    },
    /// A column or expression has an invalid or incompatible data type.
    InvalidColumnType {
        operator: String,
        column: String,
        expected: String,
        actual: String,
        reason: String,
    },
    /// Child output schema does not match parent expected input schema.
    SchemaMismatch {
        operator: String,
        expected: String,
        actual: String,
        reason: String,
    },
    /// Declared or required ordering contract is violated.
    OrderingViolation {
        operator: String,
        expected: String,
        actual: String,
        reason: String,
    },
    /// Column demand requirement is not satisfied by the physical plan node.
    RequiredColumnsViolation {
        operator: String,
        missing_columns: Vec<String>,
        available_columns: Vec<String>,
        reason: String,
    },
    /// Row goal contract or LIMIT/OFFSET relationship is violated.
    RowGoalViolation {
        operator: String,
        expected: String,
        actual: String,
        reason: String,
    },
    /// An invalid or inconsistent physical access path was specified.
    InvalidAccessPath {
        table: String,
        access_path: String,
        reason: String,
    },
    /// An index reference is invalid or does not belong to the target table.
    InvalidIndexReference {
        table: String,
        index: String,
        reason: String,
    },
    /// A column is marked as covered but does not exist in the index payload.
    InvalidCoveringPayload {
        table: String,
        index: String,
        missing_columns: Vec<String>,
        covered_columns: Vec<String>,
        reason: String,
    },
    /// Late materialization dependencies or access paths are invalid.
    InvalidLateMaterialization { table: String, reason: String },
    /// A physical join operator violates its relational or schema contracts.
    InvalidJoin {
        operator: String,
        join_type: String,
        reason: String,
    },
    /// An aggregate operator violates its grouping or streaming contracts.
    InvalidAggregate {
        operator: String,
        strategy: Option<String>,
        reason: String,
    },
    /// A window function violates its partitioning, ordering, or frame contracts.
    InvalidWindow {
        operator: String,
        function: String,
        reason: String,
    },
    /// A set operation has mismatched column counts, types, or ordering claims.
    InvalidSetOperation { operator: String, reason: String },
    /// Statement-local CTE or common subplan materialization contract violation.
    InvalidMaterialization { cte_name: String, reason: String },
    /// A planned runtime filter violates safety invariants or outer join rules.
    InvalidRuntimeFilter { filter_type: String, reason: String },
    /// Resource configuration parameters are physically impossible.
    InvalidResourceConfiguration {
        subsystem: String,
        value: usize,
        limit: usize,
        reason: String,
    },
    /// A cached plan entry is stale or invalid against current catalog generations.
    InvalidPlanCacheEntry { reason: String },
    /// Generic operator contract or structural invariant violation.
    InvalidOperatorContract { operator: String, reason: String },
}

impl PlanValidationError {
    /// Returns the diagnostic category name.
    pub fn category(&self) -> &'static str {
        match self {
            Self::MissingColumn { .. } => "MissingColumn",
            Self::InvalidColumnType { .. } => "InvalidColumnType",
            Self::SchemaMismatch { .. } => "SchemaMismatch",
            Self::OrderingViolation { .. } => "OrderingViolation",
            Self::RequiredColumnsViolation { .. } => "RequiredColumnsViolation",
            Self::RowGoalViolation { .. } => "RowGoalViolation",
            Self::InvalidAccessPath { .. } => "InvalidAccessPath",
            Self::InvalidIndexReference { .. } => "InvalidIndexReference",
            Self::InvalidCoveringPayload { .. } => "InvalidCoveringPayload",
            Self::InvalidLateMaterialization { .. } => "InvalidLateMaterialization",
            Self::InvalidJoin { .. } => "InvalidJoin",
            Self::InvalidAggregate { .. } => "InvalidAggregate",
            Self::InvalidWindow { .. } => "InvalidWindow",
            Self::InvalidSetOperation { .. } => "InvalidSetOperation",
            Self::InvalidMaterialization { .. } => "InvalidMaterialization",
            Self::InvalidRuntimeFilter { .. } => "InvalidRuntimeFilter",
            Self::InvalidResourceConfiguration { .. } => "InvalidResourceConfiguration",
            Self::InvalidPlanCacheEntry { .. } => "InvalidPlanCacheEntry",
            Self::InvalidOperatorContract { .. } => "InvalidOperatorContract",
        }
    }

    /// Returns the operator name if available.
    pub fn operator(&self) -> Option<&str> {
        match self {
            Self::MissingColumn { operator, .. }
            | Self::InvalidColumnType { operator, .. }
            | Self::SchemaMismatch { operator, .. }
            | Self::OrderingViolation { operator, .. }
            | Self::RequiredColumnsViolation { operator, .. }
            | Self::RowGoalViolation { operator, .. }
            | Self::InvalidJoin { operator, .. }
            | Self::InvalidAggregate { operator, .. }
            | Self::InvalidWindow { operator, .. }
            | Self::InvalidSetOperation { operator, .. }
            | Self::InvalidOperatorContract { operator, .. } => Some(operator),
            _ => None,
        }
    }

    /// Returns the rejection explanation.
    pub fn reason(&self) -> &str {
        match self {
            Self::MissingColumn { reason, .. }
            | Self::InvalidColumnType { reason, .. }
            | Self::SchemaMismatch { reason, .. }
            | Self::OrderingViolation { reason, .. }
            | Self::RequiredColumnsViolation { reason, .. }
            | Self::RowGoalViolation { reason, .. }
            | Self::InvalidAccessPath { reason, .. }
            | Self::InvalidIndexReference { reason, .. }
            | Self::InvalidCoveringPayload { reason, .. }
            | Self::InvalidLateMaterialization { reason, .. }
            | Self::InvalidJoin { reason, .. }
            | Self::InvalidAggregate { reason, .. }
            | Self::InvalidWindow { reason, .. }
            | Self::InvalidSetOperation { reason, .. }
            | Self::InvalidMaterialization { reason, .. }
            | Self::InvalidRuntimeFilter { reason, .. }
            | Self::InvalidResourceConfiguration { reason, .. }
            | Self::InvalidPlanCacheEntry { reason }
            | Self::InvalidOperatorContract { reason, .. } => reason,
        }
    }
}

impl fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Plan validation failed:")?;
        match self {
            Self::MissingColumn {
                operator,
                column,
                available,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "column: {column}")?;
                writeln!(f, "available: [{}]", available.join(", "))?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidColumnType {
                operator,
                column,
                expected,
                actual,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "column: {column}")?;
                writeln!(f, "expected: {expected}")?;
                writeln!(f, "actual: {actual}")?;
                write!(f, "reason: {reason}")
            }
            Self::SchemaMismatch {
                operator,
                expected,
                actual,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "expected: {expected}")?;
                writeln!(f, "actual: {actual}")?;
                write!(f, "reason: {reason}")
            }
            Self::OrderingViolation {
                operator,
                expected,
                actual,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "property: Ordering")?;
                writeln!(f, "expected: {expected}")?;
                writeln!(f, "actual: {actual}")?;
                write!(f, "reason: {reason}")
            }
            Self::RequiredColumnsViolation {
                operator,
                missing_columns,
                available_columns,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "missing: [{}]", missing_columns.join(", "))?;
                writeln!(f, "available: [{}]", available_columns.join(", "))?;
                write!(f, "reason: {reason}")
            }
            Self::RowGoalViolation {
                operator,
                expected,
                actual,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "property: RowGoal")?;
                writeln!(f, "expected: {expected}")?;
                writeln!(f, "actual: {actual}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidAccessPath {
                table,
                access_path,
                reason,
            } => {
                writeln!(f, "table: {table}")?;
                writeln!(f, "access_path: {access_path}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidIndexReference {
                table,
                index,
                reason,
            } => {
                writeln!(f, "table: {table}")?;
                writeln!(f, "index: {index}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidCoveringPayload {
                table,
                index,
                missing_columns,
                covered_columns,
                reason,
            } => {
                writeln!(f, "table: {table}")?;
                writeln!(f, "index: {index}")?;
                writeln!(f, "missing: [{}]", missing_columns.join(", "))?;
                writeln!(f, "covered: [{}]", covered_columns.join(", "))?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidLateMaterialization { table, reason } => {
                writeln!(f, "table: {table}")?;
                writeln!(f, "property: LateMaterialization")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidJoin {
                operator,
                join_type,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "join_type: {join_type}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidAggregate {
                operator,
                strategy,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                if let Some(strat) = strategy {
                    writeln!(f, "strategy: {strat}")?;
                }
                write!(f, "reason: {reason}")
            }
            Self::InvalidWindow {
                operator,
                function,
                reason,
            } => {
                writeln!(f, "operator: {operator}")?;
                writeln!(f, "function: {function}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidSetOperation { operator, reason } => {
                writeln!(f, "operator: {operator}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidMaterialization { cte_name, reason } => {
                writeln!(f, "cte: {cte_name}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidRuntimeFilter {
                filter_type,
                reason,
            } => {
                writeln!(f, "filter_type: {filter_type}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidResourceConfiguration {
                subsystem,
                value,
                limit,
                reason,
            } => {
                writeln!(f, "subsystem: {subsystem}")?;
                writeln!(f, "value: {value}")?;
                writeln!(f, "limit: {limit}")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidPlanCacheEntry { reason } => {
                writeln!(f, "cache: PlanCache")?;
                write!(f, "reason: {reason}")
            }
            Self::InvalidOperatorContract { operator, reason } => {
                writeln!(f, "operator: {operator}")?;
                write!(f, "reason: {reason}")
            }
        }
    }
}

impl std::error::Error for PlanValidationError {}

/// Explicit type-level proof that a `PhysicalPlan` has passed contract and safety validation.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedPhysicalPlan {
    plan: PhysicalPlan,
    output_schema: Schema,
}

impl ValidatedPhysicalPlan {
    /// Creates a validated plan wrapper. Internal to validator.
    pub fn new(plan: PhysicalPlan, output_schema: Schema) -> Self {
        Self {
            plan,
            output_schema,
        }
    }

    /// Borrows the validated physical plan.
    pub fn plan(&self) -> &PhysicalPlan {
        &self.plan
    }

    /// Consumes this wrapper and returns the underlying physical plan.
    pub fn into_inner(self) -> PhysicalPlan {
        self.plan
    }

    /// Output schema guaranteed by this validated plan.
    pub fn output_schema(&self) -> &Schema {
        &self.output_schema
    }
}

impl std::ops::Deref for ValidatedPhysicalPlan {
    type Target = PhysicalPlan;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

/// Metadata describing a table for validation purposes.
#[derive(Debug, Clone, PartialEq)]
pub struct TableValidationInfo {
    /// Canonical table name.
    pub name: String,
    /// Optional table alias.
    pub alias: Option<String>,
    /// Schema of the table.
    pub schema: Schema,
    /// Attached secondary indexes.
    pub indexes: Vec<IndexMetadata>,
    /// Whether this table represents an in-memory CTE or derived table.
    pub is_cte: bool,
}

impl TableValidationInfo {
    /// Creates a new `TableValidationInfo`.
    pub fn new(
        name: impl Into<String>,
        alias: Option<impl Into<String>>,
        schema: Schema,
        indexes: Vec<IndexMetadata>,
    ) -> Self {
        Self {
            name: name.into(),
            alias: alias.map(|a| a.into()),
            schema,
            indexes,
            is_cte: false,
        }
    }

    /// Marks this table as a CTE.
    pub fn with_cte(mut self, is_cte: bool) -> Self {
        self.is_cte = is_cte;
        self
    }
}

/// Context containing catalog, schema, index, and resource metadata required to validate a physical plan.
#[derive(Debug, Clone, Default)]
pub struct PlanValidationContext {
    /// Participating tables keyed by canonical name and alias.
    pub tables: BTreeMap<String, TableValidationInfo>,
    /// Optional resource planning signature.
    pub resource_signature: Option<ResourcePlanningSignature>,
    /// Active catalog generation counter.
    pub catalog_generation: Option<u64>,
    /// Active index generation counter.
    pub index_generation: Option<u64>,
    /// Active statistics generation counter.
    pub statistics_generation: Option<u64>,
    /// Parameter planning signature if validating parameterized plans.
    pub param_signature: Option<ParamPlanningSignature>,
    /// Expected physical properties if validating root demand.
    pub expected_properties: Option<RequiredPhysicalProperties>,
    /// Cached plan validity descriptor if validating a cached plan.
    pub plan_validity: Option<PlanValidity>,
    /// Whether this validation context contains the full authoritative catalog.
    pub is_catalog_authoritative: bool,
}

impl PlanValidationContext {
    /// Creates an empty validation context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets whether this context represents the authoritative system catalog.
    pub fn with_catalog_authoritative(mut self, auth: bool) -> Self {
        self.is_catalog_authoritative = auth;
        self
    }

    /// Constructs a validation context from optimizer `TableOptInfo` descriptors.
    pub fn from_tables(tables: &[TableOptInfo]) -> Self {
        let mut ctx = Self::new();
        for t in tables {
            let info = TableValidationInfo {
                name: t.name.clone(),
                alias: t.alias.clone(),
                schema: t.schema.clone(),
                indexes: t.indexes.clone(),
                is_cte: t.is_cte,
            };
            ctx.tables.insert(t.name.clone(), info.clone());
            if let Some(alias) = &t.alias {
                ctx.tables.insert(alias.clone(), info);
            }
        }
        ctx
    }

    /// Constructs a validation context by querying a `TableCatalog`.
    pub fn from_catalog(
        catalog: &dyn TableCatalog,
        table_names: &[&str],
    ) -> std::result::Result<Self, PlanValidationError> {
        let mut ctx = Self::new().with_catalog_authoritative(true);
        for &name in table_names {
            let schema = catalog.get_table_schema(name).map_err(|e| {
                PlanValidationError::InvalidAccessPath {
                    table: name.to_string(),
                    access_path: "Unknown".to_string(),
                    reason: format!("Catalog lookup failed for table '{name}': {e}"),
                }
            })?;
            let indexes = catalog.get_table_indexes(name).unwrap_or_default();
            let info = TableValidationInfo::new(name, None::<String>, schema, indexes);
            ctx.tables.insert(name.to_string(), info);
        }
        Ok(ctx)
    }

    /// Adds a table to this validation context.
    pub fn with_table(mut self, info: TableValidationInfo) -> Self {
        self.tables.insert(info.name.clone(), info.clone());
        if let Some(alias) = &info.alias {
            self.tables.insert(alias.clone(), info);
        }
        self
    }

    /// Sets the resource planning signature.
    pub fn with_resource_signature(mut self, sig: Option<ResourcePlanningSignature>) -> Self {
        self.resource_signature = sig;
        self
    }

    /// Sets catalog, index, and statistics generation numbers.
    pub fn with_generations(mut self, catalog_gen: u64, index_gen: u64, stats_gen: u64) -> Self {
        self.catalog_generation = Some(catalog_gen);
        self.index_generation = Some(index_gen);
        self.statistics_generation = Some(stats_gen);
        self
    }

    /// Sets parameter planning signature.
    pub fn with_param_signature(mut self, sig: Option<ParamPlanningSignature>) -> Self {
        self.param_signature = sig;
        self
    }

    /// Sets expected root physical properties.
    pub fn with_expected_properties(mut self, req: Option<RequiredPhysicalProperties>) -> Self {
        self.expected_properties = req;
        self
    }

    /// Sets plan cache validity descriptor.
    pub fn with_plan_validity(mut self, validity: Option<PlanValidity>) -> Self {
        self.plan_validity = validity;
        self
    }

    /// Looks up table metadata by canonical name or alias.
    pub fn get_table(&self, name_or_alias: &str) -> Option<&TableValidationInfo> {
        self.tables.get(name_or_alias)
    }
}

/// Standalone, reusable physical plan validator.
///
/// Serves as a formal safety layer between optimization and execution.
/// Does not rewrite, mutate, optimize, repair, or execute query plans.
pub struct PhysicalPlanValidator;

impl PhysicalPlanValidator {
    /// Validates that a physical execution plan satisfies all declared schema, ordering,
    /// column demand, row-goal, access-path, join, and resource contracts.
    pub fn validate(
        plan: &PhysicalPlan,
        context: &PlanValidationContext,
    ) -> std::result::Result<ValidatedPhysicalPlan, PlanValidationError> {
        // 1. Resource signature validation
        Self::validate_resources(context)?;

        // 2. Plan cache entry / staleness validation
        Self::validate_plan_cache_entry(plan, context)?;

        // 3. CTE / Materialization validation
        let cte_schemas = Self::validate_ctes(&plan.ctes, context)?;

        // Build augmented context including CTEs
        let mut aug_context = context.clone();
        for (cte_name, cte_schema) in &cte_schemas {
            aug_context.tables.insert(
                cte_name.clone(),
                TableValidationInfo {
                    name: cte_name.clone(),
                    alias: None,
                    schema: cte_schema.clone(),
                    indexes: Vec::new(),
                    is_cte: true,
                },
            );
        }

        // 4. Validate physical plan pipeline (or set operation)
        let (output_schema, provided_ordering) = if let Some(set_op) = &plan.set_op {
            Self::validate_set_operation(set_op, &aug_context, plan)?
        } else {
            Self::validate_select_pipeline(plan, &aug_context)?
        };

        // 5. Root provided ordering check
        if let Some(prov) = &plan.provided_ordering {
            if !provided_ordering.satisfies(prov) {
                return Err(PlanValidationError::OrderingViolation {
                    operator: "Root".to_string(),
                    expected: format!("{prov}"),
                    actual: format!("{provided_ordering}"),
                    reason: "Physical plan root declares provided_ordering that is not guaranteed by the operator tree".to_string(),
                });
            }
        }

        // 6. Expected properties check if context declared expectations
        if let Some(expected) = &context.expected_properties {
            if !expected.ordering.is_empty()
                && !provided_ordering.satisfies_required_ordering(&expected.ordering)
            {
                return Err(PlanValidationError::OrderingViolation {
                    operator: "Root".to_string(),
                    expected: format!("{}", expected.ordering),
                    actual: format!("{provided_ordering}"),
                    reason: "Physical plan output ordering does not satisfy required physical properties".to_string(),
                });
            }

            if !expected.required_columns.is_all() && !expected.required_columns.is_empty() {
                for req_col in &expected.required_columns.columns {
                    if req_col == "*" {
                        continue;
                    }
                    let bare = req_col.split('.').next_back().unwrap_or(req_col);
                    let found = output_schema.columns.iter().any(|c| {
                        c.name.eq_ignore_ascii_case(req_col) || c.name.eq_ignore_ascii_case(bare)
                    });
                    if !found {
                        return Err(PlanValidationError::RequiredColumnsViolation {
                            operator: "Root".to_string(),
                            missing_columns: vec![req_col.clone()],
                            available_columns: output_schema
                                .columns
                                .iter()
                                .map(|c| c.name.clone())
                                .collect(),
                            reason: format!(
                                "Physical plan output does not provide required column '{req_col}'"
                            ),
                        });
                    }
                }
            }

            if let Some(rg) = &expected.row_goal {
                if let Some(plan_rg) = &plan.row_goal {
                    if plan_rg.candidate_rows > rg.candidate_rows {
                        return Err(PlanValidationError::RowGoalViolation {
                            operator: "Root".to_string(),
                            expected: format!("{rg}"),
                            actual: format!("{plan_rg}"),
                            reason: "Plan root row goal candidate bound exceeds required row goal"
                                .to_string(),
                        });
                    }
                }
            }
        }

        // 7. Validate plan root row goal sanity
        if let Some(rg) = &plan.row_goal {
            Self::validate_row_goal_struct(rg, "Root")?;
        }
        if let (Some(l), Some(o)) = (plan.limit, plan.offset) {
            if let Some(rg) = &plan.row_goal {
                let expected_cand = (l + o) as u64;
                if rg.candidate_rows != expected_cand {
                    return Err(PlanValidationError::RowGoalViolation {
                        operator: "LimitOffset".to_string(),
                        expected: format!(
                            "candidate_rows == {expected_cand} (LIMIT {l} + OFFSET {o})"
                        ),
                        actual: format!("candidate_rows == {}", rg.candidate_rows),
                        reason: "Plan root row goal candidate count does not match LIMIT + OFFSET"
                            .to_string(),
                    });
                }
            }
        }

        Ok(ValidatedPhysicalPlan::new(plan.clone(), output_schema))
    }

    /// Formats deterministic validation diagnostics for a plan.
    pub fn format_validation_diagnostics(
        plan: &PhysicalPlan,
        context: &PlanValidationContext,
    ) -> String {
        match Self::validate(plan, context) {
            Ok(_) => format!("{plan}\nValidation: OK"),
            Err(err) => format!("{err}"),
        }
    }

    // ------------------------------------------------------------------------
    // Internal validation methods
    // ------------------------------------------------------------------------

    fn validate_resources(
        context: &PlanValidationContext,
    ) -> std::result::Result<(), PlanValidationError> {
        let sig = match &context.resource_signature {
            Some(s) => s,
            None => return Ok(()),
        };

        if sig.memory_budget_bytes == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "memory_budget_bytes".to_string(),
                value: 0,
                limit: 1,
                reason: "Execution memory budget must be greater than zero".to_string(),
            });
        }
        if sig.max_temp_files == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "max_temp_files".to_string(),
                value: 0,
                limit: 1,
                reason: "Maximum temporary files limit must be greater than zero".to_string(),
            });
        }
        if sig.max_temp_bytes == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "max_temp_bytes".to_string(),
                value: 0,
                limit: 1,
                reason: "Maximum temporary storage bytes must be greater than zero".to_string(),
            });
        }
        if sig.max_merge_passes == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "max_merge_passes".to_string(),
                value: 0,
                limit: 1,
                reason: "Maximum merge passes must be greater than zero".to_string(),
            });
        }
        if sig.sort_budget == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "sort_budget".to_string(),
                value: 0,
                limit: 1,
                reason: "Sort memory budget must be greater than zero".to_string(),
            });
        }
        if sig.agg_budget == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "agg_budget".to_string(),
                value: 0,
                limit: 1,
                reason: "Aggregation memory budget must be greater than zero".to_string(),
            });
        }
        if sig.join_budget == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "join_budget".to_string(),
                value: 0,
                limit: 1,
                reason: "Join memory budget must be greater than zero".to_string(),
            });
        }
        if sig.multi_index_budget == 0 {
            return Err(PlanValidationError::InvalidResourceConfiguration {
                subsystem: "multi_index_budget".to_string(),
                value: 0,
                limit: 1,
                reason: "Multi-index candidate memory budget must be greater than zero".to_string(),
            });
        }
        Ok(())
    }

    fn validate_plan_cache_entry(
        plan: &PhysicalPlan,
        context: &PlanValidationContext,
    ) -> std::result::Result<(), PlanValidationError> {
        // If a PlanValidity was attached to context, verify generation numbers
        if let Some(validity) = &context.plan_validity {
            if let Some(cur_cat) = context.catalog_generation {
                if validity.catalog_generation != cur_cat {
                    return Err(PlanValidationError::InvalidPlanCacheEntry {
                        reason: format!(
                            "Cached plan catalog generation ({}) does not match current catalog generation ({cur_cat})",
                            validity.catalog_generation
                        ),
                    });
                }
            }
            if let Some(cur_idx) = context.index_generation {
                if validity.index_generation != cur_idx {
                    return Err(PlanValidationError::InvalidPlanCacheEntry {
                        reason: format!(
                            "Cached plan index generation ({}) does not match current index generation ({cur_idx})",
                            validity.index_generation
                        ),
                    });
                }
            }
            if let Some(cur_stats) = context.statistics_generation {
                if validity.statistics_generation != cur_stats {
                    return Err(PlanValidationError::InvalidPlanCacheEntry {
                        reason: format!(
                            "Cached plan statistics generation ({}) does not match current statistics generation ({cur_stats})",
                            validity.statistics_generation
                        ),
                    });
                }
            }
            if let (Some(expected_sig), Some(actual_sig)) =
                (&validity.resource_signature, &context.resource_signature)
            {
                if expected_sig != actual_sig {
                    return Err(PlanValidationError::InvalidPlanCacheEntry {
                        reason: "Cached plan resource planning signature does not match active resource configuration".to_string(),
                    });
                }
            }
        }

        // Verify that all referenced tables and secondary indexes exist in the catalog
        if context.is_catalog_authoritative && !context.tables.is_empty() {
            let (ref_tables, ref_indexes) = extract_plan_references(plan);
            for tbl in ref_tables {
                if !context.tables.contains_key(&tbl) && !plan.ctes.iter().any(|c| c.name == tbl) {
                    return Err(PlanValidationError::InvalidPlanCacheEntry {
                        reason: format!("Referenced table '{tbl}' does not exist in the active database catalog"),
                    });
                }
            }
            for idx in ref_indexes {
                let index_exists = context
                    .tables
                    .values()
                    .any(|t| t.indexes.iter().any(|i| i.name == idx));
                if !index_exists {
                    return Err(PlanValidationError::InvalidPlanCacheEntry {
                        reason: format!("Referenced index '{idx}' does not exist or was dropped"),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_ctes(
        ctes: &[crate::sql::binder::BoundCte],
        context: &PlanValidationContext,
    ) -> std::result::Result<BTreeMap<String, Schema>, PlanValidationError> {
        let mut cte_schemas = BTreeMap::new();
        let mut cur_ctx = context.clone();

        for cte in ctes {
            if cte.name.is_empty() {
                return Err(PlanValidationError::InvalidMaterialization {
                    cte_name: String::new(),
                    reason: "CTE name cannot be empty".to_string(),
                });
            }
            match &cte.kind {
                crate::sql::binder::BoundCteKind::NonRecursive { plan, .. } => {
                    if cur_ctx.is_catalog_authoritative {
                        let validated_inner = Self::validate(plan, &cur_ctx)?;
                        if validated_inner.output_schema().columns.len()
                            != cte.output_schema.columns.len()
                        {
                            return Err(PlanValidationError::InvalidMaterialization {
                                cte_name: cte.name.clone(),
                                reason: format!(
                                    "CTE inner plan column count ({}) does not match declared CTE schema column count ({})",
                                    validated_inner.output_schema().columns.len(),
                                    cte.output_schema.columns.len()
                                ),
                            });
                        }
                    }
                    cte_schemas.insert(cte.name.clone(), cte.output_schema.clone());
                    cur_ctx.tables.insert(
                        cte.name.clone(),
                        TableValidationInfo {
                            name: cte.name.clone(),
                            alias: None,
                            schema: cte.output_schema.clone(),
                            indexes: Vec::new(),
                            is_cte: true,
                        },
                    );
                }
                crate::sql::binder::BoundCteKind::Recursive {
                    anchor_plan,
                    recursive_plan,
                    max_iterations,
                    max_rows,
                } => {
                    if *max_iterations == 0 {
                        return Err(PlanValidationError::InvalidMaterialization {
                            cte_name: cte.name.clone(),
                            reason: "Recursive CTE max_iterations must be greater than zero"
                                .to_string(),
                        });
                    }
                    if *max_rows == 0 {
                        return Err(PlanValidationError::InvalidMaterialization {
                            cte_name: cte.name.clone(),
                            reason: "Recursive CTE max_rows must be greater than zero".to_string(),
                        });
                    }
                    if cur_ctx.is_catalog_authoritative {
                        let validated_anchor = Self::validate(anchor_plan, &cur_ctx)?;
                        if validated_anchor.output_schema().columns.len()
                            != cte.output_schema.columns.len()
                        {
                            return Err(PlanValidationError::InvalidMaterialization {
                                cte_name: cte.name.clone(),
                                reason: "Recursive CTE anchor plan output schema does not match declared schema".to_string(),
                            });
                        }
                    }
                    cte_schemas.insert(cte.name.clone(), cte.output_schema.clone());
                    cur_ctx.tables.insert(
                        cte.name.clone(),
                        TableValidationInfo {
                            name: cte.name.clone(),
                            alias: None,
                            schema: cte.output_schema.clone(),
                            indexes: Vec::new(),
                            is_cte: true,
                        },
                    );
                    if cur_ctx.is_catalog_authoritative {
                        let validated_rec = Self::validate(recursive_plan, &cur_ctx)?;
                        if validated_rec.output_schema().columns.len()
                            != cte.output_schema.columns.len()
                        {
                            return Err(PlanValidationError::InvalidMaterialization {
                                cte_name: cte.name.clone(),
                                reason: "Recursive CTE recursive member output schema does not match declared schema".to_string(),
                            });
                        }
                    }
                }
            }
        }
        Ok(cte_schemas)
    }

    fn validate_set_operation(
        set_op: &crate::query::optimizer::PhysicalSetOp,
        context: &PlanValidationContext,
        parent_plan: &PhysicalPlan,
    ) -> std::result::Result<(Schema, OrderingProperty), PlanValidationError> {
        let left_val = Self::validate(&set_op.left, context)?;
        let right_val = Self::validate(&set_op.right, context)?;

        let left_schema = left_val.output_schema();
        let right_schema = right_val.output_schema();

        if left_schema.columns.len() != right_schema.columns.len() {
            return Err(PlanValidationError::InvalidSetOperation {
                operator: format!("{:?}", set_op.op),
                reason: format!(
                    "Left side has {} columns but right side has {}",
                    left_schema.columns.len(),
                    right_schema.columns.len()
                ),
            });
        }

        for (i, (lc, rc)) in left_schema
            .columns
            .iter()
            .zip(right_schema.columns.iter())
            .enumerate()
        {
            if crate::sql::binder::unify_set_types(lc.data_type, rc.data_type).is_none() {
                return Err(PlanValidationError::InvalidSetOperation {
                    operator: format!("{:?}", set_op.op),
                    reason: format!(
                        "Column {} type mismatch: left is {}, right is {}",
                        i, lc.data_type, rc.data_type
                    ),
                });
            }
        }

        // Set operations conservatively produce NoOrdering unless explicitly sorted
        let provided_ordering = if parent_plan.order_by_sort_required || parent_plan.order_by_top_n
        {
            OrderingProperty::from_sort_keys(&parent_plan.order_by)
        } else {
            OrderingProperty::default()
        };

        if !parent_plan.order_by.is_empty()
            && !parent_plan.order_by_sort_required
            && !parent_plan.order_by_top_n
        {
            return Err(PlanValidationError::OrderingViolation {
                operator: format!("{:?}", set_op.op),
                expected: "Explicit Sort or TopN for set operation ORDER BY".to_string(),
                actual: "sort_required=false, top_n=false".to_string(),
                reason: "Relational set operations cannot advertise ordering without explicit sort"
                    .to_string(),
            });
        }

        Ok((set_op.schema.clone(), provided_ordering))
    }

    fn validate_select_pipeline(
        plan: &PhysicalPlan,
        context: &PlanValidationContext,
    ) -> std::result::Result<(Schema, OrderingProperty), PlanValidationError> {
        // 1. Validate Base Table Scan
        let (mut current_schema, mut current_ordering) =
            Self::validate_table_scan(&plan.base_table, context, true)?;

        // If there are joins, qualify the base table schema
        if !plan.joins.is_empty() {
            let q = plan.base_table.qualifier();
            let qual_cols: Vec<crate::engine::types::Column> = current_schema
                .columns
                .iter()
                .map(|c| {
                    let col_name = if c.name.contains('.') {
                        c.name.clone()
                    } else {
                        format!("{q}.{}", c.name)
                    };
                    crate::engine::types::Column::new(col_name, c.data_type, c.nullable)
                })
                .collect();
            current_schema = Schema::new_projected(qual_cols);
        }

        // 2. Validate Physical Joins
        for join in &plan.joins {
            let (next_schema, next_ordering) =
                Self::validate_join(join, &current_schema, &current_ordering, context)?;
            current_schema = next_schema;
            current_ordering = next_ordering;
        }

        // 3. Validate Post-Join Filter
        if let Some(post_filter) = &plan.post_join_filter {
            Self::validate_boolean_expr(post_filter, &current_schema, "PostJoinFilter")?;
            Self::validate_subqueries_in_expr(post_filter, context)?;
        }

        // 4. Validate Aggregation
        if !plan.group_by.is_empty() || !plan.aggregates.is_empty() {
            let (post_agg_schema, post_agg_ordering) =
                Self::validate_aggregation(plan, &current_schema, &current_ordering, context)?;
            current_schema = post_agg_schema;
            current_ordering = post_agg_ordering;
        }

        // 5. Validate Having Filter
        if let Some(having) = &plan.having {
            Self::validate_boolean_expr(having, &current_schema, "Having")?;
            Self::validate_subqueries_in_expr(having, context)?;
        }

        // 6. Validate Window Functions
        if !plan.window_exprs.is_empty() {
            current_schema =
                Self::validate_window_functions(&plan.window_exprs, &current_schema, context)?;
            if let Some(w) = plan.window_exprs.first() {
                let mut win_cols = Vec::new();
                for p in &w.window_spec.partition_by {
                    if let Expr::Column(col) = p {
                        win_cols.push(crate::query::physical_properties::OrderingColumn {
                            column: col.clone(),
                            direction: crate::query::operator::SortDirection::Ascending,
                            nulls_first: true,
                        });
                    }
                }
                for o in &w.window_spec.order_by {
                    if let Expr::Column(col) = &o.expression {
                        win_cols.push(crate::query::physical_properties::OrderingColumn {
                            column: col.clone(),
                            direction: o.direction,
                            nulls_first: o.nulls_first.unwrap_or(
                                o.direction == crate::query::operator::SortDirection::Ascending,
                            ),
                        });
                    }
                }
                if !win_cols.is_empty() {
                    current_ordering =
                        crate::query::physical_properties::OrderingProperty::new(win_cols);
                }
            }
        }

        // 7. Validate Sort / TopN (ORDER BY evaluated against working schema)
        if !plan.order_by.is_empty() {
            current_ordering =
                Self::validate_sort_and_top_n(plan, &current_schema, &current_ordering)?;
        }

        let stream_ordering = current_ordering.clone();

        // 8. Validate Projections
        let projected_ordering = if !plan.projections.is_empty() {
            let projected_schema =
                Self::validate_projections(&plan.projections, &current_schema, context)?;
            let proj_ord = current_ordering.project(&plan.projections);
            current_schema = projected_schema;
            proj_ord
        } else {
            current_ordering
        };

        // If plan declares provided_ordering, check against either stream or projected ordering
        let final_provided = if let Some(prov) = &plan.provided_ordering {
            if stream_ordering.satisfies(prov) {
                prov.clone()
            } else if projected_ordering.satisfies(prov) {
                projected_ordering
            } else {
                return Err(PlanValidationError::OrderingViolation {
                    operator: "Root".to_string(),
                    expected: format!("{prov}"),
                    actual: format!("{stream_ordering}"),
                    reason: "Physical plan root declares provided_ordering that is not guaranteed by the operator tree".to_string(),
                });
            }
        } else {
            projected_ordering
        };

        Ok((current_schema, final_provided))
    }

    fn validate_table_scan(
        scan: &PhysicalTableScan,
        context: &PlanValidationContext,
        is_base: bool,
    ) -> std::result::Result<(Schema, OrderingProperty), PlanValidationError> {
        let op_name = if is_base {
            "BaseTableScan"
        } else {
            "JoinRightScan"
        };

        if scan.table_name.is_empty() {
            return Err(PlanValidationError::InvalidAccessPath {
                table: String::new(),
                access_path: format!("{:?}", scan.access_path),
                reason: format!("{op_name} table name is empty"),
            });
        }

        let tbl_info = match context
            .get_table(&scan.table_name)
            .or_else(|| context.get_table(scan.qualifier()))
        {
            Some(info) => info,
            None => {
                if context.is_catalog_authoritative {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: scan.table_name.clone(),
                        access_path: format!("{:?}", scan.access_path),
                        reason: format!(
                            "Table '{}' not found in catalog or active CTEs",
                            scan.table_name
                        ),
                    });
                } else {
                    return Ok((
                        Schema::new_projected(Vec::new()),
                        OrderingProperty::default(),
                    ));
                }
            }
        };

        // Access path validation
        Self::validate_access_path(scan, tbl_info)?;

        // Column demand & required columns validation
        Self::validate_column_demand(scan, tbl_info)?;

        // Row-goal validation on scan
        if let Some(rg) = &scan.row_goal {
            Self::validate_row_goal_struct(rg, op_name)?;
        }
        if scan.early_termination_possible {
            if scan.row_goal.is_none() {
                return Err(PlanValidationError::RowGoalViolation {
                    operator: op_name.to_string(),
                    expected: "Active row goal for early termination".to_string(),
                    actual: "row_goal=None".to_string(),
                    reason:
                        "Table scan claims early termination possible but has no active row goal"
                            .to_string(),
                });
            }
            if matches!(
                scan.access_path,
                AccessPath::MultiIndexScan { .. }
                    | AccessPath::SecondaryIndexScan {
                        scan_type: SecondaryScanType::SkipScan { .. },
                        ..
                    }
            ) {
                return Err(PlanValidationError::RowGoalViolation {
                    operator: op_name.to_string(),
                    expected: "Ordered single index or table scan for early termination"
                        .to_string(),
                    actual: format!("{:?}", scan.access_path),
                    reason: "Multi-index scans and skip scans do not support early termination"
                        .to_string(),
                });
            }
        }

        // Residual filter validation
        if let Some(rf) = &scan.residual_filter {
            Self::validate_boolean_expr(rf, &tbl_info.schema, op_name)?;
            Self::validate_subqueries_in_expr(rf, context)?;
        }

        // Output ordering for scan
        let opt_info = TableOptInfo::new(
            &tbl_info.name,
            tbl_info.alias.as_deref(),
            tbl_info.schema.clone(),
            tbl_info.indexes.clone(),
        )
        .with_cte(tbl_info.is_cte);
        let scan_ordering = ordering_property_for_table_scan(&opt_info, scan);

        Ok((tbl_info.schema.clone(), scan_ordering))
    }

    fn validate_access_path(
        scan: &PhysicalTableScan,
        table: &TableValidationInfo,
    ) -> std::result::Result<(), PlanValidationError> {
        match &scan.access_path {
            AccessPath::PrimaryPointLookup { key } => {
                let pk_col = table.schema.primary_key_column();
                if key.data_type() != DataType::Null && key.data_type() != pk_col.data_type {
                    return Err(PlanValidationError::InvalidColumnType {
                        operator: "PrimaryPointLookup".to_string(),
                        column: pk_col.name.clone(),
                        expected: format!("{:?}", pk_col.data_type),
                        actual: format!("{:?}", key.data_type()),
                        reason: "Primary key point lookup key type does not match table primary key column type".to_string(),
                    });
                }
            }
            AccessPath::PrimaryRangeScan { start, end } => {
                let pk_col = table.schema.primary_key_column();
                if let Some(s) = start {
                    if s.data_type() != DataType::Null && s.data_type() != pk_col.data_type {
                        return Err(PlanValidationError::InvalidColumnType {
                            operator: "PrimaryRangeScan".to_string(),
                            column: pk_col.name.clone(),
                            expected: format!("{:?}", pk_col.data_type),
                            actual: format!("{:?}", s.data_type()),
                            reason: "Primary range start key type does not match primary key column type".to_string(),
                        });
                    }
                }
                if let Some(e) = end {
                    if e.data_type() != DataType::Null && e.data_type() != pk_col.data_type {
                        return Err(PlanValidationError::InvalidColumnType {
                            operator: "PrimaryRangeScan".to_string(),
                            column: pk_col.name.clone(),
                            expected: format!("{:?}", pk_col.data_type),
                            actual: format!("{:?}", e.data_type()),
                            reason:
                                "Primary range end key type does not match primary key column type"
                                    .to_string(),
                        });
                    }
                }
            }
            AccessPath::SecondaryIndexScan {
                index_name,
                scan_type,
                ..
            }
            | AccessPath::IndexOnlyScan {
                index_name,
                scan_type,
                ..
            } => {
                let idx_meta = table
                    .indexes
                    .iter()
                    .find(|idx| &idx.name == index_name)
                    .ok_or_else(|| PlanValidationError::InvalidIndexReference {
                        table: table.name.clone(),
                        index: index_name.clone(),
                        reason: format!(
                            "Secondary index '{index_name}' does not belong to table '{}'",
                            table.name
                        ),
                    })?;

                Self::validate_secondary_scan_type(scan_type, idx_meta, table)?;
            }
            AccessPath::MultiIndexScan { child_scans, .. } => {
                if child_scans.is_empty() {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: table.name.clone(),
                        access_path: "MultiIndexScan".to_string(),
                        reason: "MultiIndexScan must contain at least one child index scan"
                            .to_string(),
                    });
                }
                if child_scans.len() > MAX_MULTI_INDEXES {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: table.name.clone(),
                        access_path: "MultiIndexScan".to_string(),
                        reason: format!(
                            "MultiIndexScan child scans ({}) exceed MAX_MULTI_INDEXES ({})",
                            child_scans.len(),
                            MAX_MULTI_INDEXES
                        ),
                    });
                }
                for child in child_scans {
                    let idx_meta = table
                        .indexes
                        .iter()
                        .find(|idx| idx.name == child.index_name)
                        .ok_or_else(|| PlanValidationError::InvalidIndexReference {
                            table: table.name.clone(),
                            index: child.index_name.clone(),
                            reason: format!(
                                "MultiIndexScan child index '{}' does not belong to table '{}'",
                                child.index_name, table.name
                            ),
                        })?;
                    Self::validate_secondary_scan_type(&child.scan_type, idx_meta, table)?;
                }
            }
            AccessPath::FullTableScan | AccessPath::EmptyResult => {}
        }
        Ok(())
    }

    fn validate_secondary_scan_type(
        scan_type: &SecondaryScanType,
        idx_meta: &IndexMetadata,
        table: &TableValidationInfo,
    ) -> std::result::Result<(), PlanValidationError> {
        match scan_type {
            SecondaryScanType::PointLookup(val) => {
                if idx_meta.column_indices.is_empty() {
                    return Err(PlanValidationError::InvalidIndexReference {
                        table: table.name.clone(),
                        index: idx_meta.name.clone(),
                        reason: "Index has no key columns".to_string(),
                    });
                }
                let first_col_idx = idx_meta.column_indices[0];
                if let Some(col) = table.schema.columns.get(first_col_idx) {
                    if val.data_type() != DataType::Null && val.data_type() != col.data_type {
                        return Err(PlanValidationError::InvalidColumnType {
                            operator: "PointLookup".to_string(),
                            column: col.name.clone(),
                            expected: format!("{:?}", col.data_type),
                            actual: format!("{:?}", val.data_type()),
                            reason: "Point lookup value type does not match index key column type"
                                .to_string(),
                        });
                    }
                }
            }
            SecondaryScanType::CompositePointLookup(vals) => {
                if vals.len() != idx_meta.column_indices.len() {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: table.name.clone(),
                        access_path: format!("CompositePointLookup(vals={})", vals.len()),
                        reason: format!(
                            "Composite point lookup value count ({}) does not match index key column count ({})",
                            vals.len(),
                            idx_meta.column_indices.len()
                        ),
                    });
                }
            }
            SecondaryScanType::PrefixScan(vals) => {
                if vals.len() > idx_meta.column_indices.len() {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: table.name.clone(),
                        access_path: format!("PrefixScan(vals={})", vals.len()),
                        reason: format!(
                            "Prefix scan value count ({}) exceeds index key column count ({})",
                            vals.len(),
                            idx_meta.column_indices.len()
                        ),
                    });
                }
            }
            SecondaryScanType::RangeScan { start, end } => {
                if idx_meta.column_indices.is_empty() {
                    return Err(PlanValidationError::InvalidIndexReference {
                        table: table.name.clone(),
                        index: idx_meta.name.clone(),
                        reason: "Index has no key columns".to_string(),
                    });
                }
                let first_col_idx = idx_meta.column_indices[0];
                if let Some(col) = table.schema.columns.get(first_col_idx) {
                    if let Some(s) = start {
                        if s.data_type() != DataType::Null && s.data_type() != col.data_type {
                            return Err(PlanValidationError::InvalidColumnType {
                                operator: "RangeScan".to_string(),
                                column: col.name.clone(),
                                expected: format!("{:?}", col.data_type),
                                actual: format!("{:?}", s.data_type()),
                                reason:
                                    "Range scan start key type does not match index key column type"
                                        .to_string(),
                            });
                        }
                    }
                    if let Some(e) = end {
                        if e.data_type() != DataType::Null && e.data_type() != col.data_type {
                            return Err(PlanValidationError::InvalidColumnType {
                                operator: "RangeScan".to_string(),
                                column: col.name.clone(),
                                expected: format!("{:?}", col.data_type),
                                actual: format!("{:?}", e.data_type()),
                                reason:
                                    "Range scan end key type does not match index key column type"
                                        .to_string(),
                            });
                        }
                    }
                }
            }
            SecondaryScanType::SkipScan {
                skipped_prefix_columns,
                constrained_columns,
            } => {
                if skipped_prefix_columns.is_empty() || constrained_columns.is_empty() {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: table.name.clone(),
                        access_path: "SkipScan".to_string(),
                        reason: "Skip scan must have non-empty skipped prefix and constrained suffix columns".to_string(),
                    });
                }
                let total_cols = skipped_prefix_columns.len() + constrained_columns.len();
                if total_cols > idx_meta.column_indices.len() {
                    return Err(PlanValidationError::InvalidAccessPath {
                        table: table.name.clone(),
                        access_path: "SkipScan".to_string(),
                        reason: format!(
                            "Skip scan total columns ({total_cols}) exceed composite index key columns ({})",
                            idx_meta.column_indices.len()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_column_demand(
        scan: &PhysicalTableScan,
        table: &TableValidationInfo,
    ) -> std::result::Result<(), PlanValidationError> {
        let table_col_names: BTreeSet<String> = table
            .schema
            .columns
            .iter()
            .map(|c| c.name.to_ascii_lowercase())
            .collect();

        // 1. Required columns must exist in table schema
        for col in &scan.required_columns {
            let bare = col
                .split('.')
                .next_back()
                .unwrap_or(col)
                .to_ascii_lowercase();
            if !table_col_names.contains(&bare) {
                return Err(PlanValidationError::RequiredColumnsViolation {
                    operator: "TableScan".to_string(),
                    missing_columns: vec![col.clone()],
                    available_columns: table
                        .schema
                        .columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect(),
                    reason: format!(
                        "Required column '{col}' does not exist in table '{}' schema",
                        table.name
                    ),
                });
            }
        }

        // 2. Pruned columns must exist in table schema
        for col in &scan.pruned_columns {
            let bare = col
                .split('.')
                .next_back()
                .unwrap_or(col)
                .to_ascii_lowercase();
            if !table_col_names.contains(&bare) {
                return Err(PlanValidationError::MissingColumn {
                    operator: "TableScan".to_string(),
                    column: col.clone(),
                    available: table
                        .schema
                        .columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect(),
                    reason: format!(
                        "Pruned column '{col}' does not exist in table '{}' schema",
                        table.name
                    ),
                });
            }
        }

        // 3. Required columns and pruned columns must be disjoint
        let req_set: BTreeSet<String> = scan
            .required_columns
            .iter()
            .map(|c| c.split('.').next_back().unwrap_or(c).to_ascii_lowercase())
            .collect();
        for col in &scan.pruned_columns {
            let bare = col
                .split('.')
                .next_back()
                .unwrap_or(col)
                .to_ascii_lowercase();
            if req_set.contains(&bare) {
                return Err(PlanValidationError::RequiredColumnsViolation {
                    operator: "TableScan".to_string(),
                    missing_columns: vec![col.clone()],
                    available_columns: scan.required_columns.clone(),
                    reason: format!(
                        "Column '{col}' is contradictory: marked as both required and pruned"
                    ),
                });
            }
        }

        // 4. Covering Index & IndexOnlyScan payload validation
        let index_for_covering = match &scan.access_path {
            AccessPath::IndexOnlyScan { index_name, .. }
            | AccessPath::SecondaryIndexScan { index_name, .. } => Some(index_name.clone()),
            _ => None,
        };

        if let Some(index_name) = index_for_covering {
            let idx_meta = table
                .indexes
                .iter()
                .find(|idx| idx.name == index_name)
                .ok_or_else(|| PlanValidationError::InvalidIndexReference {
                    table: table.name.clone(),
                    index: index_name.clone(),
                    reason: format!("Index '{index_name}' not found on table '{}'", table.name),
                })?;

            let mut actual_covered_indices = BTreeSet::new();
            for &k in &idx_meta.column_indices {
                actual_covered_indices.insert(k);
            }
            for &inc in &idx_meta.include_column_indices {
                actual_covered_indices.insert(inc);
            }

            let actual_covered_names: BTreeSet<String> = actual_covered_indices
                .iter()
                .filter_map(|&i| {
                    table
                        .schema
                        .columns
                        .get(i)
                        .map(|c| c.name.to_ascii_lowercase())
                })
                .collect();

            // Check scan.covered_columns
            for cov in &scan.covered_columns {
                let bare = cov
                    .split('.')
                    .next_back()
                    .unwrap_or(cov)
                    .to_ascii_lowercase();
                if !actual_covered_names.contains(&bare) {
                    return Err(PlanValidationError::InvalidCoveringPayload {
                        table: table.name.clone(),
                        index: index_name.clone(),
                        missing_columns: vec![cov.clone()],
                        covered_columns: actual_covered_names.into_iter().collect(),
                        reason: format!(
                            "Column '{cov}' is marked as covered but does not exist in index '{index_name}' key or included payload"
                        ),
                    });
                }
            }

            // If IndexOnlyScan, also check covered_columns in access_path and required_columns
            if let AccessPath::IndexOnlyScan {
                covered_columns, ..
            } = &scan.access_path
            {
                for cov in covered_columns {
                    let bare = cov
                        .split('.')
                        .next_back()
                        .unwrap_or(cov)
                        .to_ascii_lowercase();
                    if !actual_covered_names.contains(&bare) {
                        return Err(PlanValidationError::InvalidCoveringPayload {
                            table: table.name.clone(),
                            index: index_name.clone(),
                            missing_columns: vec![cov.clone()],
                            covered_columns: actual_covered_names.into_iter().collect(),
                            reason: format!(
                                "Column '{cov}' is marked as covered but does not exist in index '{index_name}' key or included payload"
                            ),
                        });
                    }
                }

                for req in &scan.required_columns {
                    let bare = req
                        .split('.')
                        .next_back()
                        .unwrap_or(req)
                        .to_ascii_lowercase();
                    if !actual_covered_names.contains(&bare) {
                        return Err(PlanValidationError::RequiredColumnsViolation {
                            operator: "IndexOnlyScan".to_string(),
                            missing_columns: vec![req.clone()],
                            available_columns: actual_covered_names.into_iter().collect(),
                            reason: format!(
                                "IndexOnlyScan requires column '{req}' but it is not available in index '{index_name}' covering payload"
                            ),
                        });
                    }
                }
            }
        } else if !scan.covered_columns.is_empty() {
            return Err(PlanValidationError::InvalidCoveringPayload {
                table: table.name.clone(),
                index: String::new(),
                missing_columns: scan.covered_columns.clone(),
                covered_columns: Vec::new(),
                reason: "Table scan specifies covered columns without a secondary or index-only access path".to_string(),
            });
        }

        // 5. Late materialization verification
        if scan.late_materialization {
            match &scan.access_path {
                AccessPath::SecondaryIndexScan { index_name, .. }
                | AccessPath::IndexOnlyScan { index_name, .. } => {
                    let idx_meta = table.indexes.iter().find(|idx| &idx.name == index_name);
                    if let Some(idx) = idx_meta {
                        let mut covered_idx_set = BTreeSet::new();
                        for &k in &idx.column_indices {
                            covered_idx_set.insert(k);
                        }
                        for &inc in &idx.include_column_indices {
                            covered_idx_set.insert(inc);
                        }
                        let actual_cov_names: BTreeSet<String> = covered_idx_set
                            .iter()
                            .filter_map(|&i| {
                                table
                                    .schema
                                    .columns
                                    .get(i)
                                    .map(|c| c.name.to_ascii_lowercase())
                            })
                            .collect();

                        // Any index_covered_filter columns must be strictly in the index payload
                        if let Some(icf) = &scan.index_covered_filter {
                            let mut ref_cols = BTreeSet::new();
                            icf.collect_referenced_columns(&mut ref_cols);
                            for col in ref_cols {
                                let bare = col
                                    .split('.')
                                    .next_back()
                                    .unwrap_or(&col)
                                    .to_ascii_lowercase();
                                if !actual_cov_names.contains(&bare) {
                                    return Err(PlanValidationError::InvalidLateMaterialization {
                                        table: table.name.clone(),
                                        reason: format!(
                                            "Index-covered pre-filter references column '{col}' which is absent from index '{index_name}' payload"
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                other => {
                    return Err(PlanValidationError::InvalidLateMaterialization {
                        table: table.name.clone(),
                        reason: format!(
                            "Late materialization requires a secondary index scan, but found '{other}'"
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    fn validate_join(
        join: &PhysicalJoin,
        current_schema: &Schema,
        current_ordering: &OrderingProperty,
        context: &PlanValidationContext,
    ) -> std::result::Result<(Schema, OrderingProperty), PlanValidationError> {
        let (right_schema, right_ordering) =
            Self::validate_table_scan(&join.right, context, false)?;

        let q = join.right.qualifier();
        let qual_right_cols: Vec<crate::engine::types::Column> = right_schema
            .columns
            .iter()
            .map(|c| {
                let name = if c.name.contains('.') {
                    c.name.clone()
                } else {
                    format!("{q}.{}", c.name)
                };
                crate::engine::types::Column::new(name, c.data_type, c.nullable)
            })
            .collect();
        let qualified_right_schema = Schema::new_projected(qual_right_cols);

        // Combined schema for evaluating join on predicate
        let mut combined_cols = current_schema.columns.clone();
        combined_cols.extend(qualified_right_schema.columns.clone());
        let combined_schema = Schema::new_projected(combined_cols);

        // Validate join on condition
        if let Err(err) = Self::validate_boolean_expr(&join.on, &combined_schema, "JoinOn") {
            let conjuncts = crate::query::optimizer::split_conjunction(&join.on);
            for conj in &conjuncts {
                let can_eval =
                    crate::query::external_hash_join::expr_can_evaluate_on(conj, current_schema)
                        || crate::query::external_hash_join::expr_can_evaluate_on(
                            conj,
                            &qualified_right_schema,
                        )
                        || crate::query::external_hash_join::expr_can_evaluate_on(
                            conj,
                            &right_schema,
                        );
                if !can_eval {
                    if let Expr::Binary { left, right, .. } = conj {
                        let l_ok = crate::query::external_hash_join::expr_can_evaluate_on(
                            left,
                            current_schema,
                        ) || crate::query::external_hash_join::expr_can_evaluate_on(
                            left,
                            &qualified_right_schema,
                        ) || crate::query::external_hash_join::expr_can_evaluate_on(
                            left,
                            &right_schema,
                        );
                        let r_ok = crate::query::external_hash_join::expr_can_evaluate_on(
                            right,
                            current_schema,
                        ) || crate::query::external_hash_join::expr_can_evaluate_on(
                            right,
                            &qualified_right_schema,
                        ) || crate::query::external_hash_join::expr_can_evaluate_on(
                            right,
                            &right_schema,
                        );
                        if !l_ok || !r_ok {
                            return Err(err);
                        }
                    } else {
                        return Err(err);
                    }
                }
            }
        }
        Self::validate_subqueries_in_expr(&join.on, context)?;

        // Strategy-specific validation
        match join.strategy {
            JoinStrategy::IndexProbe => {
                let index_name =
                    join.index_name
                        .as_ref()
                        .ok_or_else(|| PlanValidationError::InvalidJoin {
                            operator: "IndexProbe".to_string(),
                            join_type: format!("{:?}", join.join_type),
                            reason: "IndexProbe join strategy requires an index_name".to_string(),
                        })?;
                let tbl_info = context
                    .get_table(&join.right.table_name)
                    .or_else(|| context.get_table(join.right.qualifier()))
                    .ok_or_else(|| PlanValidationError::InvalidAccessPath {
                        table: join.right.table_name.clone(),
                        access_path: "IndexProbe".to_string(),
                        reason: format!(
                            "Join right table '{}' not found in catalog",
                            join.right.table_name
                        ),
                    })?;
                let idx_found = if index_name == "__primary__" {
                    true
                } else {
                    tbl_info.indexes.iter().any(|idx| &idx.name == index_name)
                };
                if !idx_found {
                    return Err(PlanValidationError::InvalidIndexReference {
                        table: join.right.table_name.clone(),
                        index: index_name.clone(),
                        reason: format!(
                            "IndexProbe index '{index_name}' does not belong to right table '{}'",
                            join.right.table_name
                        ),
                    });
                }
            }
            JoinStrategy::Merge => {
                let equi_opt = analyze_equi_join(&join.on, current_schema, &qualified_right_schema);
                let equi_plan = match equi_opt {
                    Some(ep) => ep,
                    None => {
                        return Err(PlanValidationError::OrderingViolation {
                            operator: "MergeJoin".to_string(),
                            expected: "At least one equi-join condition".to_string(),
                            actual: format!("{}", join.on),
                            reason: "Merge join requires an equi-join condition".to_string(),
                        });
                    }
                };

                if !join.left_sort {
                    let req_left = OrderingProperty::new(
                        equi_plan
                            .left_keys
                            .iter()
                            .map(|k| OrderingColumn {
                                column: match k {
                                    Expr::Column(n) => n.clone(),
                                    other => other.to_string(),
                                },
                                direction: SortDirection::Ascending,
                                nulls_first: true,
                            })
                            .collect(),
                    );
                    if !current_ordering.satisfies(&req_left) {
                        return Err(PlanValidationError::OrderingViolation {
                            operator: "MergeJoin".to_string(),
                            expected: format!("Left input ordered by [{}]", req_left),
                            actual: format!("{current_ordering}"),
                            reason: "Merge join left input does not satisfy required equi-key ordering and left_sort is false".to_string(),
                        });
                    }
                }

                if !join.right_sort {
                    let req_right = OrderingProperty::new(
                        equi_plan
                            .right_keys
                            .iter()
                            .map(|k| OrderingColumn {
                                column: match k {
                                    Expr::Column(n) => n.clone(),
                                    other => other.to_string(),
                                },
                                direction: SortDirection::Ascending,
                                nulls_first: true,
                            })
                            .collect(),
                    );
                    if !right_ordering.satisfies(&req_right) {
                        return Err(PlanValidationError::OrderingViolation {
                            operator: "MergeJoin".to_string(),
                            expected: format!("Right input ordered by [{}]", req_right),
                            actual: format!("{right_ordering}"),
                            reason: "Merge join right input does not satisfy required equi-key ordering and right_sort is false".to_string(),
                        });
                    }
                }
            }
            JoinStrategy::Hash => {
                let equi_opt = analyze_equi_join(&join.on, current_schema, &qualified_right_schema);
                if equi_opt.is_none() {
                    return Err(PlanValidationError::InvalidJoin {
                        operator: "HashJoin".to_string(),
                        join_type: format!("{:?}", join.join_type),
                        reason: "Hash join requires at least one equi-join predicate".to_string(),
                    });
                }
            }
            JoinStrategy::NestedLoop => {}
        }

        // Row-goal early termination check for joins
        if join.early_termination_possible {
            if join.row_goal.is_none() {
                return Err(PlanValidationError::RowGoalViolation {
                    operator: format!("{:?}", join.strategy),
                    expected: "Active row goal for early termination".to_string(),
                    actual: "row_goal=None".to_string(),
                    reason: "Join claims early termination possible but has no active row goal"
                        .to_string(),
                });
            }
            if matches!(
                join.join_type,
                JoinType::Left | JoinType::Right | JoinType::Full
            ) {
                return Err(PlanValidationError::RowGoalViolation {
                    operator: format!("{:?}", join.strategy),
                    expected: "Inner or semi join for early termination".to_string(),
                    actual: format!("{:?}", join.join_type),
                    reason: "Outer joins cannot safely early terminate without incorrectly omitting unmatched outer rows".to_string(),
                });
            }
        }
        if let Some(rg) = &join.row_goal {
            Self::validate_row_goal_struct(rg, &format!("{:?}", join.strategy))?;
        }

        // Runtime filter validation
        if let Some(rf) = &join.runtime_filter {
            if join.join_type != JoinType::Inner && join.join_type != JoinType::Semi {
                return Err(PlanValidationError::InvalidRuntimeFilter {
                    filter_type: format!("{}", rf.filter_type),
                    reason: format!(
                        "Runtime filter planned for unsupported join type {:?}",
                        join.join_type
                    ),
                });
            }
            if rf.key_exprs.is_empty() {
                return Err(PlanValidationError::InvalidRuntimeFilter {
                    filter_type: format!("{}", rf.filter_type),
                    reason: "Runtime filter key expressions cannot be empty".to_string(),
                });
            }
            for key_expr in &rf.key_exprs {
                let mut cols = BTreeSet::new();
                key_expr.collect_referenced_columns(&mut cols);
                for c in cols {
                    let bare = c.split('.').next_back().unwrap_or(&c);
                    let found = qualified_right_schema.columns.iter().any(|rc| {
                        rc.name.eq_ignore_ascii_case(&c) || rc.name.eq_ignore_ascii_case(bare)
                    }) || current_schema.columns.iter().any(|lc| {
                        lc.name.eq_ignore_ascii_case(&c) || lc.name.eq_ignore_ascii_case(bare)
                    });
                    if !found {
                        return Err(PlanValidationError::InvalidRuntimeFilter {
                            filter_type: format!("{}", rf.filter_type),
                            reason: format!(
                                "Runtime filter build key references column '{c}' not found in join schemas"
                            ),
                        });
                    }
                }
            }
        }

        // Join output schema computation
        let next_schema = match join.join_type {
            JoinType::Inner => combined_schema,
            JoinType::Left => {
                let mut cols = current_schema.columns.clone();
                for c in &qualified_right_schema.columns {
                    cols.push(crate::engine::types::Column::new(
                        c.name.clone(),
                        c.data_type,
                        true,
                    ));
                }
                Schema::new_projected(cols)
            }
            JoinType::Right => {
                let mut cols = Vec::new();
                for c in &current_schema.columns {
                    cols.push(crate::engine::types::Column::new(
                        c.name.clone(),
                        c.data_type,
                        true,
                    ));
                }
                cols.extend(qualified_right_schema.columns.clone());
                Schema::new_projected(cols)
            }
            JoinType::Full => {
                let mut cols = Vec::new();
                for c in &current_schema.columns {
                    cols.push(crate::engine::types::Column::new(
                        c.name.clone(),
                        c.data_type,
                        true,
                    ));
                }
                for c in &qualified_right_schema.columns {
                    cols.push(crate::engine::types::Column::new(
                        c.name.clone(),
                        c.data_type,
                        true,
                    ));
                }
                Schema::new_projected(cols)
            }
            JoinType::Semi | JoinType::Anti => {
                // SEMI / ANTI join: output MUST contain only outer/left columns!
                current_schema.clone()
            }
        };

        // Output ordering after join
        let next_ordering = match join.strategy {
            JoinStrategy::Merge => {
                if let Some(equi) =
                    analyze_equi_join(&join.on, current_schema, &qualified_right_schema)
                {
                    let mut cols = Vec::new();
                    for k in &equi.left_keys {
                        if let Expr::Column(col) = k {
                            cols.push(OrderingColumn {
                                column: col.clone(),
                                direction: SortDirection::Ascending,
                                nulls_first: true,
                            });
                        }
                    }
                    for k in &equi.right_keys {
                        if let Expr::Column(col) = k {
                            cols.push(OrderingColumn {
                                column: col.clone(),
                                direction: SortDirection::Ascending,
                                nulls_first: true,
                            });
                        }
                    }
                    OrderingProperty::new(cols)
                } else {
                    current_ordering.clone()
                }
            }
            JoinStrategy::NestedLoop => match join.join_type {
                JoinType::Inner | JoinType::Left | JoinType::Semi | JoinType::Anti => {
                    current_ordering.clone()
                }
                _ => OrderingProperty::default(),
            },
            _ => OrderingProperty::default(),
        };

        Ok((next_schema, next_ordering))
    }

    fn validate_aggregation(
        plan: &PhysicalPlan,
        current_schema: &Schema,
        current_ordering: &OrderingProperty,
        context: &PlanValidationContext,
    ) -> std::result::Result<(Schema, OrderingProperty), PlanValidationError> {
        let mut out_cols = Vec::new();

        // Validate GROUP BY expressions
        for grp in &plan.group_by {
            let dt =
                grp.infer_type(current_schema)
                    .map_err(|e| PlanValidationError::MissingColumn {
                        operator: "GroupBy".to_string(),
                        column: grp.to_string(),
                        available: current_schema
                            .columns
                            .iter()
                            .map(|c| c.name.clone())
                            .collect(),
                        reason: format!("GROUP BY expression references unavailable column: {e}"),
                    })?;
            Self::validate_subqueries_in_expr(grp, context)?;
            out_cols.push(crate::engine::types::Column::new(grp.to_string(), dt, true));
        }

        // Validate aggregate expressions
        for agg in &plan.aggregates {
            match agg {
                AggregateExpr::CountAll => {
                    out_cols.push(crate::engine::types::Column::new(
                        "COUNT(*)",
                        DataType::UInt64,
                        false,
                    ));
                }
                AggregateExpr::Function { expr, func } => {
                    let dt = agg.infer_type(current_schema).map_err(|e| {
                        PlanValidationError::MissingColumn {
                            operator: format!("{:?}", func),
                            column: expr.to_string(),
                            available: current_schema
                                .columns
                                .iter()
                                .map(|c| c.name.clone())
                                .collect(),
                            reason: format!(
                                "Aggregate function argument references unavailable column: {e}"
                            ),
                        }
                    })?;
                    Self::validate_subqueries_in_expr(expr, context)?;
                    out_cols.push(crate::engine::types::Column::new(
                        format!("{}({})", func, expr),
                        dt,
                        true,
                    ));
                }
            }
        }

        // Validate streaming aggregate ordering requirement
        if let Some(AggregateStrategy::Streaming { sort_required }) = plan.aggregate_strategy {
            if !sort_required
                && !plan.group_by.is_empty()
                && !current_ordering.satisfies_group_by(&plan.group_by, None)
            {
                return Err(PlanValidationError::OrderingViolation {
                    operator: "StreamingAggregate".to_string(),
                    expected: format!(
                        "Input stream ordered by GROUP BY keys [{}]",
                        plan.group_by
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    actual: format!("{current_ordering}"),
                    reason: "Streaming aggregate requires input stream ordered by GROUP BY keys when sort_required is false".to_string(),
                });
            }
        }

        let post_agg_schema = Schema::new_projected(out_cols);

        let post_agg_ordering = match plan.aggregate_strategy {
            Some(AggregateStrategy::Streaming { .. }) => {
                let cols = plan
                    .group_by
                    .iter()
                    .map(|e| OrderingColumn {
                        column: e.to_string(),
                        direction: SortDirection::Ascending,
                        nulls_first: true,
                    })
                    .collect();
                OrderingProperty::new(cols)
            }
            _ => OrderingProperty::default(),
        };

        Ok((post_agg_schema, post_agg_ordering))
    }

    fn validate_window_functions(
        window_exprs: &[crate::query::expr::WindowExpr],
        current_schema: &Schema,
        context: &PlanValidationContext,
    ) -> std::result::Result<Schema, PlanValidationError> {
        let mut cols = current_schema.columns.clone();

        for win in window_exprs {
            // Partition by
            for p in &win.window_spec.partition_by {
                p.infer_type(current_schema)
                    .map_err(|e| PlanValidationError::MissingColumn {
                        operator: "WindowPartitionBy".to_string(),
                        column: p.to_string(),
                        available: current_schema
                            .columns
                            .iter()
                            .map(|c| c.name.clone())
                            .collect(),
                        reason: format!(
                            "Window partition expression references unavailable column: {e}"
                        ),
                    })?;
                Self::validate_subqueries_in_expr(p, context)?;
            }

            // Order by
            for o in &win.window_spec.order_by {
                o.expression.infer_type(current_schema).map_err(|e| {
                    PlanValidationError::MissingColumn {
                        operator: "WindowOrderBy".to_string(),
                        column: o.expression.to_string(),
                        available: current_schema
                            .columns
                            .iter()
                            .map(|c| c.name.clone())
                            .collect(),
                        reason: format!(
                            "Window order expression references unavailable column: {e}"
                        ),
                    }
                })?;
                Self::validate_subqueries_in_expr(&o.expression, context)?;
            }

            // Function args
            for arg in &win.args {
                arg.infer_type(current_schema)
                    .map_err(|e| PlanValidationError::MissingColumn {
                        operator: format!("Window({})", win.function),
                        column: arg.to_string(),
                        available: current_schema
                            .columns
                            .iter()
                            .map(|c| c.name.clone())
                            .collect(),
                        reason: format!(
                            "Window function argument references unavailable column: {e}"
                        ),
                    })?;
                Self::validate_subqueries_in_expr(arg, context)?;
            }

            let win_col_name = win.display_name.clone();
            cols.push(crate::engine::types::Column::new(
                win_col_name,
                win.output_type,
                true,
            ));
        }

        Ok(Schema::new_projected(cols))
    }

    fn validate_projections(
        projections: &[(Expr, Option<String>)],
        current_schema: &Schema,
        context: &PlanValidationContext,
    ) -> std::result::Result<Schema, PlanValidationError> {
        let mut cols = Vec::with_capacity(projections.len());

        for (expr, alias) in projections {
            let dt = expr.infer_type(current_schema).map_err(|e| {
                PlanValidationError::MissingColumn {
                    operator: "Projection".to_string(),
                    column: expr.to_string(),
                    available: current_schema
                        .columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect(),
                    reason: format!("Projection references column not in child output schema: {e}"),
                }
            })?;
            Self::validate_subqueries_in_expr(expr, context)?;
            let col_name = alias.clone().unwrap_or_else(|| expr.to_string());
            cols.push(crate::engine::types::Column::new(col_name, dt, true));
        }

        Ok(Schema::new_projected(cols))
    }

    fn validate_sort_and_top_n(
        plan: &PhysicalPlan,
        current_schema: &Schema,
        current_ordering: &OrderingProperty,
    ) -> std::result::Result<OrderingProperty, PlanValidationError> {
        for sk in &plan.order_by {
            sk.expression.infer_type(current_schema).map_err(|e| {
                PlanValidationError::MissingColumn {
                    operator: "OrderBy".to_string(),
                    column: sk.expression.to_string(),
                    available: current_schema
                        .columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect(),
                    reason: format!("ORDER BY key references unavailable column: {e}"),
                }
            })?;
        }

        if plan.order_by_top_n {
            if plan.limit.is_none() {
                return Err(PlanValidationError::RowGoalViolation {
                    operator: "TopN".to_string(),
                    expected: "LIMIT specified for TopN".to_string(),
                    actual: "LIMIT None".to_string(),
                    reason: "TopN operator requires bounded LIMIT".to_string(),
                });
            }
            return Ok(OrderingProperty::from_sort_keys(&plan.order_by));
        }

        if plan.order_by_sort_required {
            return Ok(OrderingProperty::from_sort_keys(&plan.order_by));
        }

        // When sort_required is false and top_n is false, stream must already satisfy order_by
        if !current_ordering.satisfies_order_by(&plan.order_by, None, Some(&plan.projections)) {
            return Err(PlanValidationError::OrderingViolation {
                operator: "Sort".to_string(),
                expected: format!(
                    "Stream satisfying ORDER BY [{}]",
                    plan.order_by
                        .iter()
                        .map(|k| format!("{} {:?}", k.expression, k.direction))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                actual: format!("{current_ordering}"),
                reason: "Plan declares sort_required=false, but input stream ordering does not satisfy ORDER BY keys".to_string(),
            });
        }

        Ok(current_ordering.clone())
    }

    fn validate_boolean_expr(
        expr: &Expr,
        schema: &Schema,
        op_name: &str,
    ) -> std::result::Result<(), PlanValidationError> {
        let dt = expr
            .infer_type(schema)
            .map_err(|e| PlanValidationError::MissingColumn {
                operator: op_name.to_string(),
                column: expr.to_string(),
                available: schema.columns.iter().map(|c| c.name.clone()).collect(),
                reason: format!("Filter predicate references column missing from schema: {e}"),
            })?;
        if dt != DataType::Boolean && dt != DataType::Null {
            return Err(PlanValidationError::InvalidColumnType {
                operator: op_name.to_string(),
                column: expr.to_string(),
                expected: "DataType::Boolean".to_string(),
                actual: format!("{dt:?}"),
                reason: "Filter predicate expression must evaluate to Boolean".to_string(),
            });
        }
        Ok(())
    }

    fn validate_subqueries_in_expr(
        expr: &Expr,
        context: &PlanValidationContext,
    ) -> std::result::Result<(), PlanValidationError> {
        match expr {
            Expr::Subquery(sub) => {
                if context.is_catalog_authoritative {
                    Self::validate(&sub.plan, context)?;
                }
            }
            Expr::Unary { expr, .. } | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
                Self::validate_subqueries_in_expr(expr, context)?;
            }
            Expr::Binary { left, right, .. } => {
                Self::validate_subqueries_in_expr(left, context)?;
                Self::validate_subqueries_in_expr(right, context)?;
            }
            Expr::InList { expr, list, .. } => {
                Self::validate_subqueries_in_expr(expr, context)?;
                for item in list {
                    Self::validate_subqueries_in_expr(item, context)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_row_goal_struct(
        rg: &RowGoal,
        op_name: &str,
    ) -> std::result::Result<(), PlanValidationError> {
        if rg.candidate_rows < rg.required_output_rows {
            return Err(PlanValidationError::RowGoalViolation {
                operator: op_name.to_string(),
                expected: format!(
                    "candidate_rows >= required_output_rows ({})",
                    rg.required_output_rows
                ),
                actual: format!("candidate_rows = {}", rg.candidate_rows),
                reason: "Row goal candidate bound cannot be less than required output rows"
                    .to_string(),
            });
        }
        if rg.candidate_rows != rg.offset + rg.required_output_rows {
            return Err(PlanValidationError::RowGoalViolation {
                operator: op_name.to_string(),
                expected: format!(
                    "candidate_rows == offset + required_output_rows ({} + {} = {})",
                    rg.offset,
                    rg.required_output_rows,
                    rg.offset + rg.required_output_rows
                ),
                actual: format!("candidate_rows = {}", rg.candidate_rows),
                reason:
                    "Row goal invariant candidate_rows = offset + required_output_rows violated"
                        .to_string(),
            });
        }
        Ok(())
    }
}
