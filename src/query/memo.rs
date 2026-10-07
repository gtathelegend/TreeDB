//! Property-Aware Optimizer Memo & Global Physical Alternative Management.
//!
//! Provides a bounded, deterministic memo equivalence framework for query planning.
//! Separates logical identity (relations, predicates, projections, joins, grouping, windows, CTEs)
//! from physical implementations (indexes, scan algorithms, join strategies, sort/top-n,
//! aggregations, and materializations), recording the best physical implementation for each
//! combination of (Logical Group, Required Physical Properties).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::error::{Error, Result};
use crate::query::cost::CostModel;
use crate::query::optimizer::{AccessPath, PhysicalPlan};
use crate::query::physical_properties::{
    OrderingProperty, PhysicalProperties, RequiredColumns, RowGoal,
};
use crate::query::plan_cache::ResourcePlanningSignature;
use crate::sql::ast::JoinType;

/// Unique logical equivalence group identifier within the optimizer memo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(pub u32);

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Group({})", self.0)
    }
}

/// Bounded limits and thresholds governing memo exploration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoConfig {
    /// Maximum allowed number of logical memo groups.
    pub max_groups: usize,
    /// Maximum physical alternatives allowed per group.
    pub max_alternatives_per_group: usize,
    /// Maximum property requirement variants cached per group.
    pub max_property_variants: usize,
    /// Maximum recursive optimization steps before fallback.
    pub max_optimization_iterations: usize,
}

impl Default for MemoConfig {
    fn default() -> Self {
        Self {
            max_groups: 4096,
            max_alternatives_per_group: 64,
            max_property_variants: 32,
            max_optimization_iterations: 10000,
        }
    }
}

/// Runtime planning diagnostics and statistics recorded during memo optimization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoStats {
    /// Total logical equivalence groups created.
    pub groups: usize,
    /// Total physical alternatives considered across all groups.
    pub alternatives: usize,
    /// Total distinct property variants optimized across all groups.
    pub property_variants: usize,
    /// Total memo best-plan cache hits.
    pub cache_hits: usize,
    /// Total memo best-plan cache misses.
    pub cache_misses: usize,
    /// Total recursive optimization iterations executed.
    pub iterations: usize,
    /// Total physical alternatives pruned due to cost dominance or incompatibility.
    pub pruned_alternatives: usize,
    /// Whether any configuration limit was reached, triggering safe fallback.
    pub limits_reached: bool,
}

impl fmt::Display for MemoStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Memo:")?;
        writeln!(f, "  Groups: {}", self.groups)?;
        writeln!(
            f,
            "  Physical alternatives considered: {}",
            self.alternatives
        )?;
        writeln!(f, "  Property variants: {}", self.property_variants)?;
        writeln!(f, "  Memo hits: {}", self.cache_hits)?;
        writeln!(f, "  Memo misses: {}", self.cache_misses)?;
        writeln!(f, "  Iterations: {}", self.iterations)?;
        writeln!(f, "  Pruned alternatives: {}", self.pruned_alternatives)?;
        write!(f, "  Exploration limit reached: {}", self.limits_reached)
    }
}

/// Canonical logical relational operator representing a query subexpression.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogicalOp {
    /// Base relation table scan with optional qualifier and predicate.
    Scan {
        table_name: String,
        table_alias: Option<String>,
        filter: Option<String>,
    },
    /// Relational binary join over two logical input groups.
    Join {
        left: GroupId,
        right: GroupId,
        join_type: JoinType,
        on: String,
    },
    /// Grouping and aggregation over an input logical group.
    Aggregate {
        input: GroupId,
        group_by: Vec<String>,
        aggregates: Vec<String>,
        having: Option<String>,
    },
    /// Window function computation over an input logical group.
    Window {
        input: GroupId,
        window_exprs: Vec<String>,
    },
    /// Sorting, Top-N, or pagination over an input logical group.
    Sort {
        input: GroupId,
        order_by: Vec<String>,
        limit: Option<usize>,
        offset: Option<usize>,
    },
    /// Projection over an input logical group.
    Project {
        input: GroupId,
        projections: Vec<String>,
    },
    /// Relational set operation over two logical input groups.
    SetOp {
        op: crate::sql::SetOperator,
        left: GroupId,
        right: GroupId,
    },
    /// Common subplan / CTE reference identified by canonical logical fingerprint.
    Subplan { fingerprint: String },
}

impl LogicalOp {
    /// Generates a deterministic, canonical string fingerprint for this logical operator.
    pub fn fingerprint(&self) -> String {
        match self {
            Self::Scan {
                table_name,
                table_alias,
                filter,
            } => {
                format!(
                    "SCAN[table={},alias={:?},filter={:?}]",
                    table_name, table_alias, filter
                )
            }
            Self::Join {
                left,
                right,
                join_type,
                on,
            } => {
                format!(
                    "JOIN[type={:?},left={},right={},on={}]",
                    join_type, left.0, right.0, on
                )
            }
            Self::Aggregate {
                input,
                group_by,
                aggregates,
                having,
            } => {
                format!(
                    "AGG[input={},grp=[{}],agg=[{}],having={:?}]",
                    input.0,
                    group_by.join(","),
                    aggregates.join(","),
                    having
                )
            }
            Self::Window {
                input,
                window_exprs,
            } => {
                format!("WIN[input={},exprs=[{}]]", input.0, window_exprs.join(","))
            }
            Self::Sort {
                input,
                order_by,
                limit,
                offset,
            } => {
                format!(
                    "SORT[input={},ord=[{}],lim={:?},off={:?}]",
                    input.0,
                    order_by.join(","),
                    limit,
                    offset
                )
            }
            Self::Project { input, projections } => {
                format!("PROJ[input={},projs=[{}]]", input.0, projections.join(","))
            }
            Self::SetOp { op, left, right } => {
                format!("SETOP[op={:?},left={},right={}]", op, left.0, right.0)
            }
            Self::Subplan { fingerprint } => {
                format!("SUBPLAN[{fingerprint}]")
            }
        }
    }
}

/// Canonical property requirement key specifying physical optimization constraints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalPropertyKey {
    /// Canonical representation of required ordering.
    pub ordering: String,
    /// Canonical representation of required columns.
    pub required_columns: String,
    /// Canonical representation of row goal limit/offset.
    pub row_goal: String,
    /// Canonical representation of resource constraints.
    pub resource_signature: String,
}

impl PhysicalPropertyKey {
    /// Constructs a physical property key from components.
    pub fn new(
        ordering: &OrderingProperty,
        req_cols: &RequiredColumns,
        row_goal: Option<&RowGoal>,
        resource_sig: Option<&ResourcePlanningSignature>,
    ) -> Self {
        let ordering_str = format!("{ordering}");
        let req_cols_str = format!("{req_cols}");
        let row_goal_str = match row_goal {
            Some(rg) => format!("RG(k={},offset={:?})", rg.k(), rg.offset),
            None => "None".to_string(),
        };
        let res_sig_str = match resource_sig {
            Some(sig) => format!(
                "RES(sort={},join={},agg={},multi={})",
                sig.sort_budget, sig.join_budget, sig.agg_budget, sig.multi_index_budget
            ),
            None => "None".to_string(),
        };

        Self {
            ordering: ordering_str,
            required_columns: req_cols_str,
            row_goal: row_goal_str,
            resource_signature: res_sig_str,
        }
    }

    /// Generates a canonical fingerprint string for this property key.
    pub fn fingerprint(&self) -> String {
        format!(
            "ord:{};cols:{};rg:{};res:{}",
            self.ordering, self.required_columns, self.row_goal, self.resource_signature
        )
    }

    /// Creates an empty unconstrained physical property key.
    pub fn unconstrained() -> Self {
        Self {
            ordering: "Unordered".to_string(),
            required_columns: "*".to_string(),
            row_goal: "None".to_string(),
            resource_signature: "None".to_string(),
        }
    }
}

impl fmt::Display for PhysicalPropertyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[ord={},cols={},rg={},res={}]",
            self.ordering, self.required_columns, self.row_goal, self.resource_signature
        )
    }
}

/// A candidate physical implementation alternative for a logical memo group.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalAlternative {
    /// Unique alternative index within the group.
    pub id: usize,
    /// Human-readable description of this physical choice.
    pub description: String,
    /// The physical plan implementing this choice.
    pub plan: PhysicalPlan,
    /// Estimated execution cost.
    pub cost: f64,
    /// Estimated output row cardinality.
    pub cardinality: f64,
    /// Physical properties guaranteed by this implementation.
    pub properties: PhysicalProperties,
    /// Canonical fingerprint of this physical plan.
    pub fingerprint: String,
    /// Strongly typed canonical physical plan fingerprint.
    pub plan_fingerprint: crate::query::fingerprint::PhysicalPlanFingerprint,
}

/// The winning best-plan result for a specific physical property requirement key.
#[derive(Debug, Clone, PartialEq)]
pub struct BestPlan {
    /// The optimal physical plan.
    pub physical_plan: PhysicalPlan,
    /// Total estimated execution cost.
    pub cost: f64,
    /// Total estimated output row count.
    pub cardinality: f64,
    /// Guaranteed physical properties at plan root.
    pub properties: PhysicalProperties,
    /// Identifier of the winning physical alternative.
    pub alternative_id: usize,
    /// Human-readable explanation of why this plan was chosen.
    pub description: String,
}

/// A memo group containing logical equivalent expressions, physical alternatives, and cached best plans.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoGroup {
    /// Unique identifier of this group.
    pub id: GroupId,
    /// Canonical logical operator defining this equivalence class.
    pub logical_op: LogicalOp,
    /// Physical implementation alternatives registered in this group.
    pub alternatives: Vec<PhysicalAlternative>,
    /// Best plans cached by physical property requirement key.
    pub best_plans: BTreeMap<PhysicalPropertyKey, BestPlan>,
    /// Set of physical fingerprints registered to prevent duplicate alternatives.
    pub alternative_fingerprints: BTreeSet<String>,
}

impl MemoGroup {
    /// Creates a new memo group with the specified ID and logical operator.
    pub fn new(id: GroupId, logical_op: LogicalOp) -> Self {
        Self {
            id,
            logical_op,
            alternatives: Vec::new(),
            best_plans: BTreeMap::new(),
            alternative_fingerprints: BTreeSet::new(),
        }
    }

    /// Converts physical alternatives in this group into a bounded candidate set for adaptive selection.
    pub fn to_candidate_set(
        &self,
        max_candidates: usize,
    ) -> crate::query::lifecycle::PlanCandidateSet {
        crate::query::lifecycle::PlanCandidateSet::from_alternatives(
            &self.alternatives,
            max_candidates,
        )
    }
}

/// Bounded property-aware optimizer memo managing physical alternatives and optimal subplans.
#[derive(Debug, Clone, PartialEq)]
pub struct OptimizerMemo {
    /// Equivalence memo groups indexed by GroupId.
    pub groups: Vec<MemoGroup>,
    /// Map of canonical logical fingerprints to GroupIds for fast deduplication.
    pub group_index: BTreeMap<String, GroupId>,
    /// Configuration limits for exploration bounding.
    pub config: MemoConfig,
    /// Planning statistics and exploration metrics.
    pub stats: MemoStats,
}

impl Default for OptimizerMemo {
    fn default() -> Self {
        Self::new(MemoConfig::default())
    }
}

impl OptimizerMemo {
    /// Constructs a new `OptimizerMemo` with the given configuration limits.
    pub fn new(config: MemoConfig) -> Self {
        Self {
            groups: Vec::new(),
            group_index: BTreeMap::new(),
            config,
            stats: MemoStats::default(),
        }
    }

    /// Returns a slice over all memo groups.
    pub fn groups(&self) -> &[MemoGroup] {
        &self.groups
    }

    /// Retrieves a reference to a memo group by GroupId.
    pub fn group(&self, id: GroupId) -> Option<&MemoGroup> {
        self.groups.get(id.0 as usize)
    }

    /// Returns reference to planning statistics and exploration metrics.
    pub fn stats(&self) -> &MemoStats {
        &self.stats
    }

    /// Retrieves an existing group or creates a new one for the given logical operator.
    pub fn get_or_create_group(&mut self, op: LogicalOp) -> Result<GroupId> {
        let fp = op.fingerprint();
        if let Some(&gid) = self.group_index.get(&fp) {
            return Ok(gid);
        }

        if self.groups.len() >= self.config.max_groups {
            self.stats.limits_reached = true;
            return Err(Error::QueryExecutionError(format!(
                "Optimizer memo group limit ({}) reached",
                self.config.max_groups
            )));
        }

        let gid = GroupId(self.groups.len() as u32);
        let group = MemoGroup::new(gid, op);
        self.groups.push(group);
        self.group_index.insert(fp, gid);
        self.stats.groups = self.groups.len();
        Ok(gid)
    }

    /// Adds a physical implementation alternative to the specified group if not already present.
    ///
    /// Returns `Ok(true)` if the alternative was successfully added, or `Ok(false)` if deduplicated.
    pub fn add_alternative(
        &mut self,
        group_id: GroupId,
        plan: PhysicalPlan,
        description: impl Into<String>,
    ) -> Result<bool> {
        if (group_id.0 as usize) >= self.groups.len() {
            return Err(Error::QueryExecutionError(format!(
                "Invalid memo group ID: {}",
                group_id.0
            )));
        }

        let desc = description.into();
        let fp = self.compute_physical_fingerprint(&plan, &desc);

        let group = &mut self.groups[group_id.0 as usize];
        if group.alternative_fingerprints.contains(&fp) {
            return Ok(false);
        }

        if group.alternatives.len() >= self.config.max_alternatives_per_group {
            self.stats.limits_reached = true;
            return Ok(false);
        }

        let alt_id = group.alternatives.len();
        let cost = plan.total_cost;
        let cardinality = plan.estimated_rows;

        // Derive physical properties provided by this plan
        let properties = PhysicalProperties {
            ordering: plan.provided_ordering.clone(),
            required_columns: Some(RequiredColumns::all()),
            row_goal: plan.row_goal,
        };

        let plan_fp = plan.fingerprint();
        let alt = PhysicalAlternative {
            id: alt_id,
            description: desc,
            plan,
            cost,
            cardinality,
            properties,
            fingerprint: fp.clone(),
            plan_fingerprint: plan_fp,
        };

        group.alternative_fingerprints.insert(fp);
        group.alternatives.push(alt);
        self.stats.alternatives += 1;
        Ok(true)
    }

    /// Computes a canonical, deterministic fingerprint for a physical plan alternative.
    pub fn compute_physical_fingerprint(&self, plan: &PhysicalPlan, desc: &str) -> String {
        format!("desc={};{}", desc, plan.fingerprint().canonical())
    }

    /// Computes the strongly typed physical plan fingerprint for a plan.
    pub fn plan_fingerprint(
        &self,
        plan: &PhysicalPlan,
    ) -> crate::query::fingerprint::PhysicalPlanFingerprint {
        plan.fingerprint()
    }

    /// Optimizes a logical memo group under the required physical properties, selecting the best physical plan.
    pub fn optimize_group(
        &mut self,
        group_id: GroupId,
        req_key: &PhysicalPropertyKey,
        req_ordering: &OrderingProperty,
        req_columns: &RequiredColumns,
        req_row_goal: Option<&RowGoal>,
    ) -> Result<BestPlan> {
        if (group_id.0 as usize) >= self.groups.len() {
            return Err(Error::QueryExecutionError(format!(
                "Invalid memo group ID: {}",
                group_id.0
            )));
        }

        self.stats.iterations += 1;
        if self.stats.iterations >= self.config.max_optimization_iterations {
            self.stats.limits_reached = true;
            return Err(Error::QueryExecutionError(format!(
                "Optimizer memo iteration limit ({}) reached",
                self.config.max_optimization_iterations
            )));
        }

        // Check if optimal plan is already cached
        if let Some(best) = self.groups[group_id.0 as usize].best_plans.get(req_key) {
            self.stats.cache_hits += 1;
            return Ok(best.clone());
        }

        self.stats.cache_misses += 1;

        let group = &self.groups[group_id.0 as usize];
        if group.alternatives.is_empty() {
            return Err(Error::QueryExecutionError(format!(
                "Memo group {} has no registered physical alternatives",
                group_id.0
            )));
        }

        let mut candidate_plans: Vec<(f64, f64, usize, PhysicalPlan, String)> = Vec::new();

        for alt in &group.alternatives {
            let mut plan = alt.plan.clone();
            let mut cost = alt.cost;
            let mut card = alt.cardinality;

            // 1. Check Required Columns
            if !req_columns.is_all() {
                let plan_cols = &plan.base_table.required_columns;
                if !plan_cols.is_empty()
                    && !req_columns.columns.iter().all(|c| {
                        plan_cols.contains(c)
                            || plan_cols.contains(&format!("{}.{}", plan.base_table.qualifier(), c))
                    })
                {
                    // If this alternative does not provide the required columns and cannot fetch them, prune it
                    if matches!(
                        plan.base_table.access_path,
                        AccessPath::IndexOnlyScan { .. }
                    ) {
                        self.stats.pruned_alternatives += 1;
                        continue;
                    }
                }
            }

            // 2. Check Ordering Property Satisfaction / Enforcement
            let provides_ordering = plan
                .provided_ordering
                .as_ref()
                .map(|ord| ord.satisfies(req_ordering))
                .unwrap_or_else(|| req_ordering.is_empty());

            if !provides_ordering {
                // If ordering is not naturally satisfied by this alternative, check if Sort is required
                if !req_ordering.is_empty() {
                    let sort_cost = CostModel::sort_cost(card);
                    cost += sort_cost;
                    plan.order_by_sort_required = true;
                    plan.provided_ordering = Some(req_ordering.clone());
                }
            }

            // 3. Check Row Goal Satisfaction / Top-N
            if let Some(rg) = req_row_goal {
                if !provides_ordering && !req_ordering.is_empty() {
                    // Unordered stream under row goal + order by -> Top-N
                    let top_n_cost = CostModel::top_n_cost(card, rg.k() as usize, 64.0);
                    cost = alt.cost + top_n_cost;
                    plan.order_by_top_n = true;
                    plan.order_by_sort_required = false;
                    plan.row_goal = Some(*rg);
                    plan.row_goal_early_termination = true;
                    card = (card).min(rg.k() as f64);
                } else if provides_ordering {
                    // Ordered scan under row goal -> Early stop without sort
                    plan.order_by_top_n = false;
                    plan.order_by_sort_required = false;
                    plan.row_goal = Some(*rg);
                    plan.row_goal_early_termination = true;
                    card = (card).min(rg.k() as f64);
                }
            }

            plan.total_cost = cost;
            plan.estimated_rows = card;
            candidate_plans.push((cost, card, alt.id, plan, alt.description.clone()));
        }

        if candidate_plans.is_empty() {
            return Err(Error::QueryExecutionError(format!(
                "No valid physical alternative satisfied properties {req_key} in group {}",
                group_id.0
            )));
        }

        // Deterministic Pareto & Tie-breaking selection:
        // 1. Lower estimated cost (with epsilon 1e-6)
        // 2. Lower estimated output cardinality
        // 3. Lower examined candidate rows
        // 4. Index-only / covering preference
        // 5. Lexicographical alternative description
        candidate_plans.sort_by(|a, b| {
            let cost_diff = a.0 - b.0;
            if cost_diff.abs() > 1e-6 {
                a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                let card_diff = a.1 - b.1;
                if card_diff.abs() > 1e-6 {
                    a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                } else {
                    let a_cov =
                        matches!(a.3.base_table.access_path, AccessPath::IndexOnlyScan { .. });
                    let b_cov =
                        matches!(b.3.base_table.access_path, AccessPath::IndexOnlyScan { .. });
                    match (a_cov, b_cov) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a.4.cmp(&b.4),
                    }
                }
            }
        });

        let (best_cost, best_card, best_alt_id, best_physical, best_desc) =
            candidate_plans.remove(0);

        let best_plan = BestPlan {
            physical_plan: best_physical.clone(),
            cost: best_cost,
            cardinality: best_card,
            properties: PhysicalProperties {
                ordering: best_physical.provided_ordering.clone(),
                required_columns: Some(req_columns.clone()),
                row_goal: req_row_goal.cloned(),
            },
            alternative_id: best_alt_id,
            description: best_desc,
        };

        // Cache best plan in memo group if within variant limit
        let group_mut = &mut self.groups[group_id.0 as usize];
        if group_mut.best_plans.len() < self.config.max_property_variants {
            group_mut
                .best_plans
                .insert(req_key.clone(), best_plan.clone());
            self.stats.property_variants += 1;
        } else {
            self.stats.limits_reached = true;
        }

        Ok(best_plan)
    }

    /// Returns a reference to a cached best plan if already computed.
    pub fn find_best(&self, group_id: GroupId, req: &PhysicalPropertyKey) -> Option<&BestPlan> {
        self.groups
            .get(group_id.0 as usize)
            .and_then(|g| g.best_plans.get(req))
    }

    /// Formats concise memo diagnostics for EXPLAIN output.
    pub fn explain_diagnostics(&self) -> String {
        format!("{}", self.stats)
    }

    /// Extracts a bounded candidate set from the specified memo group for adaptive selection.
    pub fn to_candidate_set(
        &self,
        group_id: GroupId,
        max_candidates: usize,
    ) -> Option<crate::query::lifecycle::PlanCandidateSet> {
        self.groups
            .get(group_id.0 as usize)
            .map(|g| g.to_candidate_set(max_candidates))
    }
}
