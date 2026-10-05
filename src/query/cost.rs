use std::cmp::Ordering;

use crate::query::feedback::SpillStatus;

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
pub const STREAMING_AGG_CPU_COST: f64 = 0.005;
pub const WINDOW_CPU_COST: f64 = 0.02;
pub const HASH_BUILD_CPU_COST: f64 = 0.01;
pub const HASH_PROBE_CPU_COST: f64 = 0.005;

pub const SKIP_SCAN_PROBE_COST: f64 = 0.05;
pub const RECORD_ID_SET_OP_CPU_COST: f64 = 0.001;
pub const SPILL_RECORD_ID_IO_COST: f64 = 0.005;

/// Centralized, deterministic cost model estimating execution effort for physical plans.
pub struct CostModel;

impl CostModel {
    /// Computes the cost of a multi-index intersection scan combining candidate RecordIds.
    pub fn multi_index_intersection_cost(
        child_scan_costs: &[f64],
        child_estimated_rows: &[f64],
        surviving_candidates: f64,
        num_residual_filters: usize,
        is_external: bool,
    ) -> f64 {
        let scan_cost_sum: f64 = child_scan_costs.iter().sum();
        let total_candidates: f64 = child_estimated_rows.iter().sum();
        let set_op_cost = if is_external {
            total_candidates * (RECORD_ID_SET_OP_CPU_COST * 2.0 + SPILL_RECORD_ID_IO_COST)
        } else {
            total_candidates * RECORD_ID_SET_OP_CPU_COST
        };
        let fetch_cost = surviving_candidates.max(0.0) * TUPLE_FETCH_COST;
        let residual_cost =
            FILTER_CPU_COST * surviving_candidates.max(0.0) * (num_residual_filters as f64);
        scan_cost_sum + set_op_cost + fetch_cost + residual_cost
    }

    /// Computes the cost of a multi-index union scan combining candidate RecordIds with deduplication.
    pub fn multi_index_union_cost(
        child_scan_costs: &[f64],
        child_estimated_rows: &[f64],
        surviving_candidates: f64,
        num_residual_filters: usize,
        is_external: bool,
    ) -> f64 {
        let scan_cost_sum: f64 = child_scan_costs.iter().sum();
        let total_candidates: f64 = child_estimated_rows.iter().sum();
        let set_op_cost = if is_external {
            total_candidates * (RECORD_ID_SET_OP_CPU_COST * 3.0 + SPILL_RECORD_ID_IO_COST)
        } else {
            total_candidates * RECORD_ID_SET_OP_CPU_COST * 1.5
        };
        let fetch_cost = surviving_candidates.max(0.0) * TUPLE_FETCH_COST;
        let residual_cost =
            FILTER_CPU_COST * surviving_candidates.max(0.0) * (num_residual_filters as f64);
        scan_cost_sum + set_op_cost + fetch_cost + residual_cost
    }

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

    /// Computes the cost of a full hash join (in-memory or external spilled).
    pub fn hash_join_cost(
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

    /// Computes the cost of a merge join over ordered or sorted inputs.
    #[allow(clippy::too_many_arguments)]
    pub fn merge_join_cost(
        left_cost: f64,
        left_rows: f64,
        left_needs_sort: bool,
        right_cost: f64,
        right_rows: f64,
        right_needs_sort: bool,
        has_residual_filter: bool,
        memory_budget_bytes: usize,
    ) -> f64 {
        let left_r = left_rows.max(0.0);
        let right_r = right_rows.max(0.0);
        let sort_left_cost = if left_needs_sort {
            Self::sort_cost_with_budget(left_r, 64.0, memory_budget_bytes, SpillStatus::Unknown)
        } else {
            0.0
        };
        let sort_right_cost = if right_needs_sort {
            Self::sort_cost_with_budget(right_r, 64.0, memory_budget_bytes, SpillStatus::Unknown)
        } else {
            0.0
        };
        let merge_cpu_cost = (left_r + right_r) * (ROW_CPU_COST * 0.5 + JOIN_CPU_COST * 0.25);
        let residual_cost = if has_residual_filter {
            FILTER_CPU_COST * (left_r.min(right_r) + 1.0)
        } else {
            0.0
        };
        left_cost + right_cost + sort_left_cost + sort_right_cost + merge_cpu_cost + residual_cost
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

    /// Computes the cost of external disk sorting with run creation and multi-pass merging.
    pub fn external_sort_cost(
        input_rows: f64,
        estimated_row_bytes: f64,
        memory_budget_bytes: usize,
    ) -> f64 {
        let rows = input_rows.max(0.0);
        if rows <= 1.0 {
            return 0.0;
        }
        let in_mem_cost = SORT_CPU_COST * rows * rows.log2();
        let total_data_bytes = rows * estimated_row_bytes.max(16.0);
        let budget = (memory_budget_bytes as f64).max(4096.0);
        let runs = (total_data_bytes / budget).ceil().max(2.0);
        let fan_in = 16.0f64;
        let merge_passes = (runs.log2() / fan_in.log2()).ceil().max(1.0);
        let page_size = 4096.0;
        let io_cost = 2.0 * PAGE_READ_COST * (total_data_bytes / page_size) * merge_passes;
        in_mem_cost + io_cost + (runs * ROW_CPU_COST)
    }

    /// Computes sort cost taking memory budget and historical spill feedback into account.
    pub fn sort_cost_with_budget(
        input_rows: f64,
        estimated_row_bytes: f64,
        memory_budget_bytes: usize,
        spill_status: SpillStatus,
    ) -> f64 {
        let rows = input_rows.max(0.0);
        if rows <= 1.0 {
            return 0.0;
        }
        let total_data_bytes = rows * estimated_row_bytes.max(16.0);
        let will_spill = match spill_status {
            SpillStatus::Spilled => true,
            SpillStatus::NeverSpilled => false,
            SpillStatus::Unknown => total_data_bytes > (memory_budget_bytes as f64),
        };
        if will_spill {
            Self::external_sort_cost(rows, estimated_row_bytes, memory_budget_bytes)
        } else {
            Self::sort_cost(rows)
        }
    }

    /// Computes the CPU and memory cost of a Top-N bounded sort operator.
    pub fn top_n_cost(input_rows: f64, candidate_bound: usize, _estimated_row_bytes: f64) -> f64 {
        let rows = input_rows.max(0.0);
        if rows <= 1.0 || candidate_bound == 0 {
            return 0.0;
        }
        let k = (candidate_bound as f64).min(rows).max(1.0);
        let heap_cpu_cost = SORT_CPU_COST * rows * k.log2().max(1.0);
        let final_sort_cost = SORT_CPU_COST * k * k.log2();
        heap_cpu_cost + final_sort_cost
    }

    /// Computes the cost of in-memory aggregation.
    pub fn aggregate_cost(input_rows: f64) -> f64 {
        AGG_CPU_COST * input_rows.max(0.0)
    }

    /// Computes the cost of external disk aggregation with partitioning and spilling.
    pub fn external_aggregate_cost(
        input_rows: f64,
        estimated_group_bytes: f64,
        memory_budget_bytes: usize,
    ) -> f64 {
        let rows = input_rows.max(0.0);
        let base_agg = AGG_CPU_COST * rows;
        let total_bytes = rows * estimated_group_bytes.max(32.0);
        let budget = (memory_budget_bytes as f64).max(4096.0);
        let partitions = (total_bytes / budget).ceil().max(2.0);
        let page_size = 4096.0;
        let io_cost = 2.0 * PAGE_READ_COST * (total_bytes / page_size);
        base_agg + io_cost + (partitions * ROW_CPU_COST)
    }

    /// Computes aggregate cost taking memory budget and historical spill feedback into account.
    pub fn aggregate_cost_with_budget(
        input_rows: f64,
        estimated_group_bytes: f64,
        memory_budget_bytes: usize,
        spill_status: SpillStatus,
    ) -> f64 {
        let rows = input_rows.max(0.0);
        let total_bytes = rows * estimated_group_bytes.max(32.0);
        let will_spill = match spill_status {
            SpillStatus::Spilled => true,
            SpillStatus::NeverSpilled => false,
            SpillStatus::Unknown => total_bytes > (memory_budget_bytes as f64),
        };
        if will_spill {
            Self::external_aggregate_cost(rows, estimated_group_bytes, memory_budget_bytes)
        } else {
            Self::aggregate_cost(rows)
        }
    }

    /// Computes the cost of streaming aggregation over an ordered input stream.
    pub fn streaming_aggregate_cost(
        input_rows: f64,
        num_group_cols: usize,
        num_aggs: usize,
    ) -> f64 {
        let rows = input_rows.max(0.0);
        let group_key_cost = 0.001 * (num_group_cols as f64);
        let agg_update_cost = 0.002 * (num_aggs as f64);
        rows * (STREAMING_AGG_CPU_COST + group_key_cost + agg_update_cost)
    }

    /// Computes the cost of window function evaluation.
    pub fn window_cost(input_rows: f64) -> f64 {
        WINDOW_CPU_COST * input_rows.max(0.0)
    }

    /// Computes the build cost for an exact runtime join filter.
    pub fn runtime_filter_exact_build_cost(build_rows: f64) -> f64 {
        build_rows.max(0.0) * HASH_BUILD_CPU_COST
    }

    /// Computes the probe evaluation cost for an exact runtime join filter.
    pub fn runtime_filter_exact_probe_cost(probe_rows: f64) -> f64 {
        probe_rows.max(0.0) * HASH_PROBE_CPU_COST
    }

    /// Computes the build cost for a probabilistic Bloom runtime join filter.
    pub fn runtime_filter_bloom_build_cost(build_rows: f64) -> f64 {
        build_rows.max(0.0) * (HASH_BUILD_CPU_COST * 0.5)
    }

    /// Computes the probe evaluation cost for a probabilistic Bloom runtime join filter.
    pub fn runtime_filter_bloom_probe_cost(probe_rows: f64) -> f64 {
        probe_rows.max(0.0) * (HASH_PROBE_CPU_COST * 0.3)
    }

    /// Computes expected cost savings from rejected probe rows avoiding downstream hash lookup and tuple fetching.
    pub fn runtime_filter_expected_savings(probe_rows: f64, rejection_ratio: f64) -> f64 {
        probe_rows.max(0.0)
            * rejection_ratio.clamp(0.0, 1.0)
            * (TUPLE_FETCH_COST + HASH_PROBE_CPU_COST * 2.0)
    }

    /// Estimates the expected number of entries / candidate rows that must be examined
    /// to satisfy a bounded row-goal requirement $K$, given predicate selectivity $S$
    /// and total population $N$.
    pub fn expected_entries_for_row_goal(
        row_goal_k: f64,
        selectivity: f64,
        total_population: f64,
    ) -> f64 {
        let pop = total_population.max(1.0);
        let k = row_goal_k.max(1.0);
        if selectivity >= 1.0 {
            k.min(pop)
        } else if selectivity <= 0.0 {
            pop
        } else {
            let needed = (k / selectivity).ceil();
            needed.min(pop).max(1.0)
        }
    }

    /// Computes the execution cost of an ordered scan under a bounded row-goal requirement.
    pub fn ordered_scan_row_goal_cost(
        entries_examined: f64,
        is_covering: bool,
        is_primary_key: bool,
        num_residual_filters: usize,
    ) -> f64 {
        let examined = entries_examined.max(1.0);
        let filter_eval_cost = FILTER_CPU_COST * examined * (num_residual_filters as f64);
        if is_primary_key {
            BPLUS_TREE_HEIGHT_COST
                + ((ROW_CPU_COST + TUPLE_FETCH_COST) * examined)
                + filter_eval_cost
        } else if is_covering {
            SEC_INDEX_LOOKUP_COST
                + ((ROW_CPU_COST + SEC_INDEX_FETCH_COST) * examined)
                + filter_eval_cost
        } else {
            SEC_INDEX_LOOKUP_COST
                + ((TUPLE_FETCH_COST + SEC_INDEX_FETCH_COST) * examined)
                + filter_eval_cost
        }
    }

    /// Computes the residual filter evaluation cost for a row goal.
    pub fn row_goal_filter_cost(entries_examined: f64, num_residual_filters: usize) -> f64 {
        FILTER_CPU_COST * entries_examined.max(0.0) * (num_residual_filters as f64)
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
