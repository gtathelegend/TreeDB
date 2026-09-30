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
pub const HASH_BUILD_CPU_COST: f64 = 0.01;
pub const HASH_PROBE_CPU_COST: f64 = 0.005;

pub const SKIP_SCAN_PROBE_COST: f64 = 0.05;

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

    /// Computes the cost of a covering secondary index point lookup (avoids base-record fetches).
    pub fn index_only_point_lookup_cost(estimated_matches: f64) -> f64 {
        let matches = estimated_matches.max(1.0);
        SEC_INDEX_LOOKUP_COST + ((ROW_CPU_COST + SEC_INDEX_FETCH_COST) * matches)
    }

    /// Computes the cost of a covering secondary index range scan (avoids base-record fetches).
    pub fn index_only_range_scan_cost(estimated_matches: f64) -> f64 {
        let matches = estimated_matches.max(1.0);
        SEC_INDEX_LOOKUP_COST + ((ROW_CPU_COST + SEC_INDEX_FETCH_COST) * matches)
    }

    /// Computes the cost of enumerating distinct prefix values across secondary index leaf pages.
    pub fn index_enumeration_cost(total_index_entries: f64, estimated_leaf_pages: f64) -> f64 {
        let pages = estimated_leaf_pages.max(1.0);
        let entries = total_index_entries.max(1.0);
        (BPLUS_TREE_HEIGHT_COST * 0.1)
            + (pages * PAGE_READ_COST * 0.05)
            + (entries * ROW_CPU_COST * 0.2)
    }

    /// Computes the cost of a composite index skip scan.
    pub fn skip_scan_cost(
        enumeration_cost: f64,
        distinct_prefixes: f64,
        estimated_matches: f64,
        num_filter_exprs: usize,
    ) -> f64 {
        let prefixes = distinct_prefixes.max(1.0);
        let matches = estimated_matches.max(1.0);
        enumeration_cost
            + (prefixes * SKIP_SCAN_PROBE_COST)
            + ((TUPLE_FETCH_COST + SEC_INDEX_FETCH_COST) * matches)
            + (FILTER_CPU_COST * matches * (num_filter_exprs as f64))
    }

    /// Computes the cost of a covering composite index skip scan (avoids base-record fetches).
    pub fn index_only_skip_scan_cost(
        enumeration_cost: f64,
        distinct_prefixes: f64,
        estimated_matches: f64,
        num_filter_exprs: usize,
    ) -> f64 {
        let prefixes = distinct_prefixes.max(1.0);
        let matches = estimated_matches.max(1.0);
        enumeration_cost
            + (prefixes * SKIP_SCAN_PROBE_COST)
            + ((ROW_CPU_COST + SEC_INDEX_FETCH_COST) * matches)
            + (FILTER_CPU_COST * matches * (num_filter_exprs as f64))
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

    /// Computes the cost of a nested-loop SEMI join with early exit on first match.
    pub fn nested_loop_semi_cost(
        left_cost: f64,
        left_rows: f64,
        right_cost: f64,
        right_rows: f64,
        match_prob: f64,
    ) -> f64 {
        let left_r = left_rows.max(1.0);
        let right_r = right_rows.max(1.0);
        let p = match_prob.clamp(0.0, 1.0);
        let avg_inner_scanned = (p * (right_r * 0.5)) + ((1.0 - p) * right_r);
        left_cost
            + (left_r * (avg_inner_scanned * (ROW_CPU_COST + JOIN_CPU_COST)))
            + (right_cost * 0.1)
    }

    /// Computes the cost of a nested-loop ANTI join with early exit on first match.
    pub fn nested_loop_anti_cost(
        left_cost: f64,
        left_rows: f64,
        right_cost: f64,
        right_rows: f64,
        match_prob: f64,
    ) -> f64 {
        Self::nested_loop_semi_cost(left_cost, left_rows, right_cost, right_rows, match_prob)
    }

    /// Computes the cost of an index existence probe for SEMI/ANTI joins.
    pub fn index_probe_join_cost(
        left_cost: f64,
        left_rows: f64,
        null_frac: f64,
        is_primary_key: bool,
        has_residual_filter: bool,
    ) -> f64 {
        let left_r = left_rows.max(1.0);
        let non_null_fraction = (1.0 - null_frac).clamp(0.0, 1.0);
        let probe_rows = left_r * non_null_fraction;

        let single_probe_cost = if is_primary_key {
            (ROW_CPU_COST * 2.0)
                + (if has_residual_filter {
                    TUPLE_FETCH_COST
                } else {
                    0.0
                })
        } else {
            SEC_INDEX_FETCH_COST
                + (if has_residual_filter {
                    TUPLE_FETCH_COST
                } else {
                    0.0
                })
        };

        left_cost + (probe_rows * single_probe_cost) + (left_r * ROW_CPU_COST)
    }

    /// Computes the cost of a hash SEMI join (in-memory or external partitioned spill).
    pub fn hash_semi_cost(
        left_cost: f64,
        left_rows: f64,
        right_cost: f64,
        right_rows: f64,
        memory_budget_bytes: usize,
    ) -> f64 {
        let left_r = left_rows.max(1.0);
        let right_r = right_rows.max(1.0);
        let estimated_row_bytes = 64.0;
        let estimated_build_bytes = right_r * estimated_row_bytes;

        let build_cost = right_cost + (right_r * (ROW_CPU_COST + HASH_BUILD_CPU_COST));
        let probe_cost = left_r * (ROW_CPU_COST + HASH_PROBE_CPU_COST);

        let spill_cost = if estimated_build_bytes > (memory_budget_bytes as f64) {
            let page_size = 4096.0;
            let left_pages = (left_r * estimated_row_bytes / page_size).ceil();
            let right_pages = (estimated_build_bytes / page_size).ceil();
            2.0 * PAGE_READ_COST * (left_pages + right_pages)
        } else {
            0.0
        };

        left_cost + build_cost + probe_cost + spill_cost
    }

    /// Computes the cost of a hash ANTI join.
    pub fn hash_anti_cost(
        left_cost: f64,
        left_rows: f64,
        right_cost: f64,
        right_rows: f64,
        memory_budget_bytes: usize,
    ) -> f64 {
        Self::hash_semi_cost(
            left_cost,
            left_rows,
            right_cost,
            right_rows,
            memory_budget_bytes,
        )
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
