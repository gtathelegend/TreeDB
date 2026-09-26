use std::cmp::Ordering;

/// Canonical cost parameters for TreeDB's centralized cost model.
pub const PAGE_READ_COST: f64 = 1.0;
pub const ROW_CPU_COST: f64 = 0.01;
pub const FILTER_CPU_COST: f64 = 0.005;
pub const BPLUS_TREE_HEIGHT_COST: f64 = 2.0;
pub const SEC_INDEX_LOOKUP_COST: f64 = 2.0;
pub const SEC_INDEX_FETCH_COST: f64 = 0.02;
pub const PK_LOOKUP_COST: f64 = 1.0;
pub const TUPLE_FETCH_COST: f64 = 0.5;
pub const SORT_CPU_COST: f64 = 0.05;
pub const JOIN_CPU_COST: f64 = 0.02;
pub const AGG_CPU_COST: f64 = 0.01;
pub const WINDOW_CPU_COST: f64 = 0.02;

/// Centralized, deterministic cost model estimating execution effort for physical plans.
pub struct CostModel;

impl CostModel {
    /// Computes the cost of a full sequential table scan.
    pub fn table_scan_cost(table_rows: f64, num_filter_exprs: usize) -> f64 {
        PAGE_READ_COST
            + (ROW_CPU_COST * table_rows)
            + (FILTER_CPU_COST * table_rows * (num_filter_exprs as f64))
    }

    /// Computes the cost of a primary key point lookup.
    pub fn primary_point_lookup_cost() -> f64 {
        BPLUS_TREE_HEIGHT_COST + TUPLE_FETCH_COST
    }

    /// Computes the cost of a primary key range scan over estimated matched rows.
    pub fn primary_range_scan_cost(estimated_rows: f64) -> f64 {
        BPLUS_TREE_HEIGHT_COST + ((ROW_CPU_COST + TUPLE_FETCH_COST) * estimated_rows.max(1.0))
    }

    /// Computes the cost of a secondary index point lookup.
    pub fn secondary_point_lookup_cost(estimated_matches: f64) -> f64 {
        let matches = estimated_matches.max(1.0);
        SEC_INDEX_LOOKUP_COST
            + ((PK_LOOKUP_COST + TUPLE_FETCH_COST + SEC_INDEX_FETCH_COST) * matches)
    }

    /// Computes the cost of a secondary index range scan.
    pub fn secondary_range_scan_cost(estimated_matches: f64) -> f64 {
        let matches = estimated_matches.max(1.0);
        SEC_INDEX_LOOKUP_COST + ((TUPLE_FETCH_COST + SEC_INDEX_FETCH_COST) * matches)
    }

    /// Computes the cost of a nested-loop join.
    pub fn nested_loop_join_cost(
        left_cost: f64,
        left_rows: f64,
        right_cost: f64,
        right_rows: f64,
    ) -> f64 {
        let left_r = left_rows.max(1.0);
        let right_r = right_rows.max(1.0);
        left_cost + (left_r * right_cost) + (JOIN_CPU_COST * left_r * right_r)
    }

    /// Computes the cost of evaluating residual filter expressions.
    pub fn filter_cost(input_rows: f64) -> f64 {
        FILTER_CPU_COST * input_rows.max(0.0)
    }

    /// Computes the cost of in-memory sorting.
    pub fn sort_cost(input_rows: f64) -> f64 {
        let rows = input_rows.max(0.0);
        if rows <= 1.0 {
            0.0
        } else {
            SORT_CPU_COST * rows * rows.log2()
        }
    }

    /// Computes the cost of aggregation.
    pub fn aggregate_cost(input_rows: f64) -> f64 {
        AGG_CPU_COST * input_rows.max(0.0)
    }

    /// Computes the cost of window function evaluation.
    pub fn window_cost(input_rows: f64) -> f64 {
        WINDOW_CPU_COST * input_rows.max(0.0)
    }
}

/// Composite cost and row estimate tuple with deterministic comparison helpers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanCost {
    /// Estimated execution cost in abstract cost units.
    pub cost: f64,
    /// Estimated output cardinality (row count).
    pub rows: f64,
}

impl PlanCost {
    /// Creates a new `PlanCost`.
    pub const fn new(cost: f64, rows: f64) -> Self {
        Self { cost, rows }
    }

    /// Deterministic comparison for cost-based plan selection with epsilon.
    pub fn deterministic_cmp(&self, other: &Self) -> Ordering {
        const EPSILON: f64 = 1e-9;
        if (self.cost - other.cost).abs() > EPSILON {
            self.cost.total_cmp(&other.cost)
        } else if (self.rows - other.rows).abs() > EPSILON {
            self.rows.total_cmp(&other.rows)
        } else {
            Ordering::Equal
        }
    }
}
