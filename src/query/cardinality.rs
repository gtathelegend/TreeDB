use crate::engine::catalog::IndexMetadata;
use crate::engine::types::{Schema, Value};
use crate::query::expr::{BinaryOp, Expr, UnaryOp};
use crate::query::stats::TableStatistics;

/// Fallback defaults when statistics are unavailable.
pub const DEFAULT_TABLE_ROWS: f64 = 1000.0;
pub const DEFAULT_EQ_SELECTIVITY: f64 = 0.1;
pub const DEFAULT_RANGE_SELECTIVITY: f64 = 0.3333333333333333;
pub const DEFAULT_NULL_FRACTION: f64 = 0.05;

type RangeBound = Option<(Value, bool)>;
type ColumnRangeSpec = (RangeBound, RangeBound, Vec<usize>);

/// Deterministic cardinality and selectivity estimator.
pub struct CardinalityEstimator;

impl CardinalityEstimator {
    /// Returns the estimated base row count for a table relation.
    pub fn estimate_table_rows(stats: Option<&TableStatistics>) -> f64 {
        stats
            .map(|s| s.row_count as f64)
            .unwrap_or(DEFAULT_TABLE_ROWS)
    }

    /// Estimates output cardinality for an intersection of multiple index candidate sets.
    pub fn estimate_intersection_cardinality(table_rows: f64, child_selectivities: &[f64]) -> f64 {
        let base = table_rows.max(0.0);
        if child_selectivities.is_empty() {
            return base;
        }
        let mut combined_sel = 1.0;
        for &s in child_selectivities {
            combined_sel *= s.clamp(0.0, 1.0);
        }
        (base * combined_sel).clamp(0.0, base)
    }

    /// Estimates output cardinality for a union of multiple index candidate sets.
    pub fn estimate_union_cardinality(table_rows: f64, child_selectivities: &[f64]) -> f64 {
        let base = table_rows.max(0.0);
        if child_selectivities.is_empty() {
            return 0.0;
        }
        let mut complement_product = 1.0;
        for &s in child_selectivities {
            complement_product *= 1.0 - s.clamp(0.0, 1.0);
        }
        let union_sel = (1.0 - complement_product).clamp(0.0, 1.0);
        (base * union_sel).clamp(0.0, base)
    }

    /// Estimates the selectivity of an arbitrary boolean expression in `[0.0, 1.0]`.
    pub fn estimate_expr_selectivity(
        expr: &Expr,
        stats: Option<&TableStatistics>,
        schema: &Schema,
    ) -> f64 {
        match expr {
            Expr::Binary {
                op: BinaryOp::And, ..
            } => Self::estimate_conjunction_selectivity(expr, stats, schema),
            _ => Self::estimate_single_expr_selectivity(expr, stats, schema),
        }
    }

    fn estimate_single_expr_selectivity(
        expr: &Expr,
        stats: Option<&TableStatistics>,
        schema: &Schema,
    ) -> f64 {
        let sel = match expr {
            Expr::Literal(Value::Boolean(b))
            | Expr::Parameter {
                value: Value::Boolean(b),
                ..
            } => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Expr::IsNull(inner) => {
                if let Expr::Column(col_name) = inner.as_ref() {
                    let col_raw = col_name.split('.').next_back().unwrap_or(col_name);
                    if let Some(st) = stats {
                        if let Some(col_st) = st.column(col_raw) {
                            return col_st.null_fraction().clamp(0.0, 1.0);
                        }
                    }
                }
                DEFAULT_NULL_FRACTION
            }
            Expr::IsNotNull(inner) => {
                let null_sel =
                    Self::estimate_expr_selectivity(&Expr::IsNull(inner.clone()), stats, schema);
                1.0 - null_sel
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr: inner,
            } => {
                let null_frac = if let Expr::Column(col_name) = inner.as_ref() {
                    let col_raw = col_name.split('.').next_back().unwrap_or(col_name);
                    stats
                        .and_then(|st| st.column(col_raw))
                        .map(|c| c.null_fraction())
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                let inner_sel = Self::estimate_expr_selectivity(inner, stats, schema);
                ((1.0 - null_frac) - inner_sel).clamp(0.0, 1.0)
            }
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => Self::estimate_conjunction_selectivity(expr, stats, schema),
                BinaryOp::Or => {
                    let s_left = Self::estimate_expr_selectivity(left, stats, schema);
                    let s_right = Self::estimate_expr_selectivity(right, stats, schema);
                    s_left + s_right - (s_left * s_right)
                }
                BinaryOp::Eq => {
                    let (col_opt, val_opt) = extract_col_and_val(left, right);
                    if let (Some(col_name), Some(val)) = (col_opt, val_opt) {
                        let col_raw = col_name.split('.').next_back().unwrap_or(&col_name);
                        if let Some(st) = stats {
                            if let Some(col_st) = st.column(col_raw) {
                                // Try histogram estimation if available
                                if let Some(hist) = &col_st.histogram {
                                    if !hist.buckets.is_empty() && col_st.row_count > 0 {
                                        let mut total_eq_rows = 0.0;
                                        let mut match_found = false;
                                        for bucket in &hist.buckets {
                                            if is_val_in_bucket(
                                                &val,
                                                &bucket.lower_bound,
                                                &bucket.upper_bound,
                                            ) {
                                                match_found = true;
                                                if bucket.distinct_count > 0 {
                                                    total_eq_rows += (bucket.row_count as f64)
                                                        / (bucket.distinct_count as f64);
                                                }
                                            }
                                        }
                                        if match_found {
                                            return (total_eq_rows / (col_st.row_count as f64))
                                                .clamp(0.0, 1.0 - col_st.null_fraction());
                                        }
                                    }
                                }
                                if col_st.distinct_count > 0 {
                                    (1.0 / (col_st.distinct_count as f64))
                                        .clamp(0.0, 1.0 - col_st.null_fraction())
                                } else {
                                    DEFAULT_EQ_SELECTIVITY
                                }
                            } else {
                                DEFAULT_EQ_SELECTIVITY
                            }
                        } else {
                            DEFAULT_EQ_SELECTIVITY
                        }
                    } else {
                        DEFAULT_EQ_SELECTIVITY
                    }
                }
                BinaryOp::NotEq => {
                    let eq_sel = Self::estimate_expr_selectivity(
                        &Expr::Binary {
                            left: left.clone(),
                            op: BinaryOp::Eq,
                            right: right.clone(),
                        },
                        stats,
                        schema,
                    );
                    let null_frac = if let Expr::Column(col_name) = left.as_ref() {
                        let col_raw = col_name.split('.').next_back().unwrap_or(col_name);
                        stats
                            .and_then(|st| st.column(col_raw))
                            .map(|c| c.null_fraction())
                            .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    ((1.0 - null_frac) - eq_sel).clamp(0.0, 1.0)
                }
                BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                    let (col_opt, val_opt) = extract_col_and_val(left, right);
                    if let (Some(col_name), Some(val)) = (col_opt, val_opt) {
                        let col_raw = col_name.split('.').next_back().unwrap_or(&col_name);
                        if let Some(st) = stats {
                            if let Some(col_st) = st.column(col_raw) {
                                // Try histogram range estimation
                                if let Some(hist) = &col_st.histogram {
                                    if !hist.buckets.is_empty() && col_st.row_count > 0 {
                                        let (min_b, max_b) = range_op_to_bounds(*op, &val);
                                        let sel = estimate_histogram_range(
                                            hist,
                                            col_st.row_count,
                                            min_b,
                                            max_b,
                                        );
                                        return sel;
                                    }
                                }
                                if let (Some(min_v), Some(max_v)) = (&col_st.min, &col_st.max) {
                                    if let Some(frac) =
                                        estimate_range_fraction(min_v, max_v, &val, *op)
                                    {
                                        return frac * (1.0 - col_st.null_fraction());
                                    } else {
                                        return DEFAULT_RANGE_SELECTIVITY
                                            * (1.0 - col_st.null_fraction());
                                    }
                                } else {
                                    return DEFAULT_RANGE_SELECTIVITY
                                        * (1.0 - col_st.null_fraction());
                                }
                            } else {
                                DEFAULT_RANGE_SELECTIVITY
                            }
                        } else {
                            DEFAULT_RANGE_SELECTIVITY
                        }
                    } else {
                        DEFAULT_RANGE_SELECTIVITY
                    }
                }
                _ => DEFAULT_EQ_SELECTIVITY,
            },
            Expr::InList {
                expr: inner,
                list,
                negated,
            } => {
                // Deduplicate literals using CanonicalValue
                let mut unique_vals = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for l in list {
                    if let Expr::Literal(v) = l {
                        let c = crate::query::CanonicalValue::from_value(v);
                        if seen.insert(c) {
                            unique_vals.push(v.clone());
                        }
                    }
                }
                if unique_vals.is_empty() {
                    return if *negated { 1.0 } else { 0.0 };
                }

                let mut total_in_sel = 0.0;
                for v in &unique_vals {
                    let single_sel = Self::estimate_expr_selectivity(
                        &Expr::Binary {
                            left: inner.clone(),
                            op: BinaryOp::Eq,
                            right: Box::new(Expr::Literal(v.clone())),
                        },
                        stats,
                        schema,
                    );
                    total_in_sel += single_sel;
                }
                let null_frac = if let Expr::Column(col_name) = inner.as_ref() {
                    let col_raw = col_name.split('.').next_back().unwrap_or(col_name);
                    stats
                        .and_then(|st| st.column(col_raw))
                        .map(|c| c.null_fraction())
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                let list_sel = total_in_sel.clamp(0.0, (1.0 - null_frac).max(0.0));
                if *negated {
                    ((1.0 - null_frac) - list_sel).clamp(0.0, 1.0)
                } else {
                    list_sel
                }
            }
            _ => 0.5,
        };

        sel.clamp(0.0, 1.0)
    }

    /// Helper to evaluate conjunction selectivity using column-group correlation statistics where available.
    fn estimate_conjunction_selectivity(
        expr: &Expr,
        stats: Option<&TableStatistics>,
        schema: &Schema,
    ) -> f64 {
        let mut conjuncts = Vec::new();
        flatten_conjunction(expr, &mut conjuncts);

        if conjuncts.is_empty() {
            return 1.0;
        }
        if conjuncts.len() == 1 {
            return Self::estimate_single_expr_selectivity(conjuncts[0], stats, schema);
        }

        let mut handled = vec![false; conjuncts.len()];
        let mut combined_sel = 1.0;

        if let Some(st) = stats {
            if st.row_count > 0 && !st.column_groups.is_empty() {
                // Extract column equality and IsNull constraints
                // Maps col_idx -> (CanonicalValue, conjunct_index)
                let mut col_constraints: std::collections::HashMap<
                    usize,
                    (crate::query::CanonicalValue, usize),
                > = std::collections::HashMap::new();

                for (idx, &conj) in conjuncts.iter().enumerate() {
                    match conj {
                        Expr::Binary {
                            left,
                            op: BinaryOp::Eq,
                            right,
                        } => {
                            let (col_opt, val_opt) = extract_col_and_val(left, right);
                            if let (Some(col_name), Some(val)) = (col_opt, val_opt) {
                                let col_raw = col_name.split('.').next_back().unwrap_or(&col_name);
                                if let Some(col_idx) =
                                    schema.columns.iter().position(|c| c.name == col_raw)
                                {
                                    col_constraints.insert(
                                        col_idx,
                                        (crate::query::CanonicalValue::from_value(&val), idx),
                                    );
                                }
                            }
                        }
                        Expr::IsNull(inner) => {
                            if let Expr::Column(col_name) = inner.as_ref() {
                                let col_raw = col_name.split('.').next_back().unwrap_or(col_name);
                                if let Some(col_idx) =
                                    schema.columns.iter().position(|c| c.name == col_raw)
                                {
                                    col_constraints
                                        .insert(col_idx, (crate::query::CanonicalValue::Null, idx));
                                }
                            }
                        }
                        _ => {}
                    }
                }

                // Check available column groups for matching multi-column constraints
                for (group_key, group_stats) in &st.column_groups {
                    let cols = &group_key.column_indices;
                    if cols.iter().all(|c_idx| col_constraints.contains_key(c_idx))
                        && cols.iter().all(|c_idx| {
                            let (_, c_conj_idx) = col_constraints[c_idx];
                            !handled[c_conj_idx]
                        })
                    {
                        // All columns in this group are constrained by unhandled conjuncts!
                        let target_vals: Vec<crate::query::CanonicalValue> = cols
                            .iter()
                            .map(|c_idx| col_constraints[c_idx].0.clone())
                            .collect();

                        // Try finding target_vals in Top-K
                        let mut joint_sel_opt: Option<f64> = None;
                        for entry in &group_stats.top_k {
                            if entry.values == target_vals {
                                joint_sel_opt =
                                    Some((entry.frequency as f64) / (st.row_count as f64));
                                break;
                            }
                        }

                        let joint_sel = match joint_sel_opt {
                            Some(s) => s,
                            None => {
                                // Not in top-k
                                let is_all_null = target_vals
                                    .iter()
                                    .all(|v| matches!(v, crate::query::CanonicalValue::Null));
                                if is_all_null && group_stats.all_null_count > 0 {
                                    (group_stats.all_null_count as f64) / (st.row_count as f64)
                                } else {
                                    let top_k_sum_freq: usize =
                                        group_stats.top_k.iter().map(|e| e.frequency).sum();
                                    let rem_freq = st.row_count.saturating_sub(top_k_sum_freq);
                                    let rem_distinct = group_stats
                                        .distinct_count
                                        .saturating_sub(group_stats.top_k.len());

                                    if rem_distinct > 0 && rem_freq > 0 {
                                        let est_freq = (rem_freq as f64) / (rem_distinct as f64);
                                        (est_freq / (st.row_count as f64)).clamp(0.0, 1.0)
                                    } else if group_stats.distinct_count > 0 {
                                        1.0 / (group_stats.distinct_count as f64)
                                    } else {
                                        // Fallback to product of single-column selectivities
                                        let mut fallback_prod = 1.0;
                                        for c_idx in cols {
                                            let (_, c_conj_idx) = col_constraints[c_idx];
                                            fallback_prod *= Self::estimate_single_expr_selectivity(
                                                conjuncts[c_conj_idx],
                                                stats,
                                                schema,
                                            );
                                        }
                                        fallback_prod
                                    }
                                }
                            }
                        };

                        combined_sel *= joint_sel.clamp(0.0, 1.0);
                        for c_idx in cols {
                            let (_, c_conj_idx) = col_constraints[c_idx];
                            handled[c_conj_idx] = true;
                        }
                    }
                }
            }

            // Group same-column range constraints for single-column range intersection
            let mut col_range_map: std::collections::HashMap<String, ColumnRangeSpec> =
                std::collections::HashMap::new();

            for (idx, &conj) in conjuncts.iter().enumerate() {
                if handled[idx] {
                    continue;
                }
                if let Expr::Binary { left, op, right } = conj {
                    if matches!(
                        op,
                        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
                    ) {
                        let (col_opt, val_opt) = extract_col_and_val(left, right);
                        if let (Some(col_name), Some(val)) = (col_opt, val_opt) {
                            let col_raw = col_name
                                .split('.')
                                .next_back()
                                .unwrap_or(&col_name)
                                .to_string();
                            let (new_min, new_max) = range_op_to_bounds(*op, &val);
                            let entry =
                                col_range_map
                                    .entry(col_raw)
                                    .or_insert((None, None, Vec::new()));
                            entry.2.push(idx);

                            if let Some(min_b) = new_min {
                                entry.0 = match &entry.0 {
                                    None => Some(min_b),
                                    Some((cur_v, cur_inc)) => {
                                        let ord = crate::query::compare_values_for_sort(
                                            &min_b.0,
                                            cur_v,
                                            crate::query::SortDirection::Ascending,
                                            None,
                                        )
                                        .unwrap_or(std::cmp::Ordering::Equal);
                                        match ord {
                                            std::cmp::Ordering::Greater => Some(min_b),
                                            std::cmp::Ordering::Equal => {
                                                Some((min_b.0, *cur_inc && min_b.1))
                                            }
                                            _ => Some((cur_v.clone(), *cur_inc)),
                                        }
                                    }
                                };
                            }

                            if let Some(max_b) = new_max {
                                entry.1 = match &entry.1 {
                                    None => Some(max_b),
                                    Some((cur_v, cur_inc)) => {
                                        let ord = crate::query::compare_values_for_sort(
                                            &max_b.0,
                                            cur_v,
                                            crate::query::SortDirection::Ascending,
                                            None,
                                        )
                                        .unwrap_or(std::cmp::Ordering::Equal);
                                        match ord {
                                            std::cmp::Ordering::Less => Some(max_b),
                                            std::cmp::Ordering::Equal => {
                                                Some((max_b.0, *cur_inc && max_b.1))
                                            }
                                            _ => Some((cur_v.clone(), *cur_inc)),
                                        }
                                    }
                                };
                            }
                        }
                    }
                }
            }

            for (col_raw, (min_b, max_b, conj_indices)) in col_range_map {
                if let Some(col_st) = st.column(&col_raw) {
                    if let Some(hist) = &col_st.histogram {
                        if !hist.buckets.is_empty() && col_st.row_count > 0 {
                            let range_sel =
                                estimate_histogram_range(hist, col_st.row_count, min_b, max_b);
                            combined_sel *= range_sel;
                            for conj_idx in conj_indices {
                                handled[conj_idx] = true;
                            }
                        }
                    }
                }
            }
        }

        // Multiply remaining unhandled conjunct selectivities
        for (idx, &conj) in conjuncts.iter().enumerate() {
            if !handled[idx] {
                let s = Self::estimate_single_expr_selectivity(conj, stats, schema);
                combined_sel *= s;
            }
        }

        combined_sel.clamp(0.0, 1.0)
    }

    /// Estimates the output cardinality for a table scan with optional residual filter.
    pub fn estimate_scan_cardinality(
        stats: Option<&TableStatistics>,
        filter: Option<&Expr>,
        schema: &Schema,
    ) -> f64 {
        let base_rows = Self::estimate_table_rows(stats);
        if base_rows <= 0.0 {
            return 0.0;
        }
        let sel = match filter {
            Some(f) => Self::estimate_expr_selectivity(f, stats, schema),
            None => 1.0,
        };
        (base_rows * sel).max(1.0)
    }

    /// Estimates output cardinality for a primary key point lookup.
    pub fn estimate_pk_point_rows() -> f64 {
        1.0
    }

    /// Estimates output cardinality for a primary key range scan.
    pub fn estimate_pk_range_rows(
        stats: Option<&TableStatistics>,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> f64 {
        if let Some(st) = stats {
            let base_rows = st.row_count as f64;
            if base_rows <= 0.0 {
                return 0.0;
            }
            if start.is_none() && end.is_none() {
                return base_rows;
            }
            for col_st in st.columns.values() {
                if let Some(hist) = &col_st.histogram {
                    let min_b = start.cloned().map(|v| (v, true));
                    let max_b = end.cloned().map(|v| (v, true));
                    let sel = estimate_histogram_range(hist, st.row_count, min_b, max_b);
                    return (base_rows * sel).max(1.0);
                }
            }
            return (base_rows * DEFAULT_RANGE_SELECTIVITY).max(1.0);
        }
        (DEFAULT_TABLE_ROWS * 0.01).max(1.0)
    }

    /// Estimates output cardinality for a secondary index point lookup.
    pub fn estimate_sec_point_rows(
        stats: Option<&TableStatistics>,
        index_meta: &IndexMetadata,
    ) -> f64 {
        if index_meta.unique {
            return 1.0;
        }
        if let Some(st) = stats {
            if let Some(idx_st) = st.index(&index_meta.name) {
                if idx_st.distinct_key_count > 0 {
                    return ((idx_st.indexed_row_count as f64)
                        / (idx_st.distinct_key_count as f64))
                        .max(1.0);
                }
            }
            let base_rows = st.row_count as f64;
            return (base_rows * DEFAULT_EQ_SELECTIVITY).max(1.0);
        }
        1.0
    }

    /// Estimates output cardinality for a composite secondary index prefix scan.
    pub fn estimate_sec_prefix_rows(
        stats: Option<&TableStatistics>,
        _index_meta: &IndexMetadata,
        prefix_len: usize,
    ) -> f64 {
        let p_len = prefix_len.max(1) as i32;
        if let Some(st) = stats {
            let base_rows = st.row_count as f64;
            if base_rows <= 0.0 {
                return 0.0;
            }
            let sel = DEFAULT_RANGE_SELECTIVITY / (p_len as f64);
            return (base_rows * sel).max(1.0);
        }
        (DEFAULT_TABLE_ROWS * 0.01 / (p_len as f64)).max(1.0)
    }

    /// Estimates output cardinality for a secondary index range scan.
    pub fn estimate_sec_range_rows(
        stats: Option<&TableStatistics>,
        _index_meta: &IndexMetadata,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> f64 {
        if let Some(st) = stats {
            let base_rows = st.row_count as f64;
            if base_rows <= 0.0 {
                return 0.0;
            }
            if start.is_none() && end.is_none() {
                return base_rows;
            }
            for col_st in st.columns.values() {
                if let Some(hist) = &col_st.histogram {
                    let min_b = start.cloned().map(|v| (v, true));
                    let max_b = end.cloned().map(|v| (v, true));
                    let sel = estimate_histogram_range(hist, st.row_count, min_b, max_b);
                    return (base_rows * sel).max(1.0);
                }
            }
            return (base_rows * DEFAULT_RANGE_SELECTIVITY).max(1.0);
        }
        (DEFAULT_TABLE_ROWS * 0.01).max(1.0)
    }

    /// Estimates the number of distinct prefix combinations for skipped prefix columns.
    ///
    /// Uses a bounded heuristic combining individual column NDVs while guarding against
    /// unsafe multiplication on correlated columns by capping the estimate to table
    /// cardinality and index entries.
    pub fn estimate_distinct_prefixes(
        stats: Option<&TableStatistics>,
        schema: &Schema,
        skipped_col_indices: &[usize],
    ) -> f64 {
        if let Some(st) = stats {
            let table_rows = (st.row_count as f64).max(1.0);
            let mut total_distinct = 1.0f64;
            for &idx in skipped_col_indices {
                if idx >= schema.columns.len() {
                    continue;
                }
                let col_name = &schema.columns[idx].name;
                let col_ndv = if let Some(col_st) = st.column(col_name) {
                    if col_st.distinct_count > 0 {
                        col_st.distinct_count as f64
                    } else {
                        10.0f64.min(table_rows)
                    }
                } else {
                    10.0f64.min(table_rows)
                };
                total_distinct = (total_distinct * col_ndv).min(table_rows);
            }
            total_distinct.clamp(1.0, table_rows)
        } else {
            10.0f64.clamp(1.0, DEFAULT_TABLE_ROWS)
        }
    }

    /// Estimates output cardinality for a composite secondary index skip scan.
    pub fn estimate_skip_scan_rows(
        stats: Option<&TableStatistics>,
        schema: &Schema,
        constrained_col_indices: &[usize],
    ) -> f64 {
        if let Some(st) = stats {
            let base_rows = st.row_count as f64;
            let mut sel = 1.0;
            for &idx in constrained_col_indices {
                let col_name = &schema.columns[idx].name;
                if let Some(col_st) = st.column(col_name) {
                    if col_st.distinct_count > 0 {
                        sel *= 1.0 / (col_st.distinct_count as f64);
                    } else {
                        sel *= DEFAULT_EQ_SELECTIVITY;
                    }
                } else {
                    sel *= DEFAULT_EQ_SELECTIVITY;
                }
            }
            (base_rows * sel).max(1.0)
        } else {
            1.0
        }
    }

    /// Estimates the output cardinality for an inner join between left and right relations.
    pub fn estimate_inner_join_cardinality(
        left_rows: f64,
        left_stats: Option<&TableStatistics>,
        right_rows: f64,
        right_stats: Option<&TableStatistics>,
        on_expr: &Expr,
        schema: &Schema,
    ) -> f64 {
        if left_rows <= 0.0 || right_rows <= 0.0 {
            return 0.0;
        }

        let conjuncts = crate::query::optimizer::split_conjunction(on_expr);
        if conjuncts.is_empty() {
            return (left_rows * right_rows).max(1.0);
        }

        let mut total_selectivity = 1.0;

        for conj in &conjuncts {
            if let Expr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
            } = conj
            {
                if let (Expr::Column(col_a), Expr::Column(col_b)) = (left.as_ref(), right.as_ref())
                {
                    let name_a = col_a.split('.').next_back().unwrap_or(col_a);
                    let name_b = col_b.split('.').next_back().unwrap_or(col_b);

                    let ndv_a = left_stats
                        .and_then(|s| s.column(name_a))
                        .map(|c| c.distinct_count as f64)
                        .unwrap_or(left_rows);

                    let ndv_b = right_stats
                        .and_then(|s| s.column(name_b))
                        .map(|c| c.distinct_count as f64)
                        .unwrap_or(right_rows);

                    let max_ndv = ndv_a.max(ndv_b).max(1.0);
                    total_selectivity *= 1.0 / max_ndv;
                    continue;
                }
            }

            let sel = Self::estimate_expr_selectivity(conj, None, schema);
            total_selectivity *= sel;
        }

        let rows = left_rows * right_rows * total_selectivity;
        rows.max(1.0)
    }

    /// Estimates the probability that an individual outer row has at least one matching row in the inner relation.
    pub fn estimate_semi_match_probability(
        left_rows: f64,
        left_stats: Option<&TableStatistics>,
        right_rows: f64,
        right_stats: Option<&TableStatistics>,
        on_expr: &Expr,
        schema: &Schema,
    ) -> f64 {
        if left_rows <= 0.0 || right_rows <= 0.0 {
            return 0.0;
        }

        let conjuncts = crate::query::optimizer::split_conjunction(on_expr);
        if conjuncts.is_empty() {
            return 1.0;
        }

        let mut equi_found = false;
        let mut prob = 1.0;

        for conj in &conjuncts {
            if let Expr::Binary {
                left,
                op: BinaryOp::Eq,
                right,
            } = conj
            {
                if let (Expr::Column(col_a), Expr::Column(col_b)) = (left.as_ref(), right.as_ref())
                {
                    equi_found = true;
                    let name_a = col_a.split('.').next_back().unwrap_or(col_a);
                    let name_b = col_b.split('.').next_back().unwrap_or(col_b);

                    let null_frac_a = left_stats
                        .and_then(|s| s.column(name_a))
                        .map(|c| c.null_fraction())
                        .unwrap_or(0.0);

                    let ndv_a = left_stats
                        .and_then(|s| s.column(name_a))
                        .map(|c| c.distinct_count as f64)
                        .unwrap_or(left_rows);

                    let ndv_b = right_stats
                        .and_then(|s| s.column(name_b))
                        .map(|c| c.distinct_count as f64)
                        .unwrap_or(right_rows);

                    let max_ndv = ndv_a.max(ndv_b).max(1.0);
                    let equi_p = (ndv_b / max_ndv).clamp(0.0, 1.0);
                    prob *= (1.0 - null_frac_a) * equi_p;
                    continue;
                }
            }

            let sel = Self::estimate_expr_selectivity(conj, None, schema);
            prob *= sel;
        }

        if !equi_found {
            let sel = Self::estimate_expr_selectivity(on_expr, None, schema);
            prob = 1.0 - (1.0 - sel).powf(right_rows.max(1.0));
        }

        prob.clamp(0.0, 1.0)
    }

    /// Estimates the output cardinality for a join operation taking `JoinType` into account.
    pub fn estimate_join_cardinality(
        join_type: crate::sql::ast::JoinType,
        left_rows: f64,
        left_stats: Option<&TableStatistics>,
        right_rows: f64,
        right_stats: Option<&TableStatistics>,
        on_expr: &Expr,
        schema: &Schema,
    ) -> f64 {
        match join_type {
            crate::sql::ast::JoinType::Inner => Self::estimate_inner_join_cardinality(
                left_rows,
                left_stats,
                right_rows,
                right_stats,
                on_expr,
                schema,
            ),
            crate::sql::ast::JoinType::Left => Self::estimate_inner_join_cardinality(
                left_rows,
                left_stats,
                right_rows,
                right_stats,
                on_expr,
                schema,
            )
            .max(left_rows),
            crate::sql::ast::JoinType::Right => Self::estimate_inner_join_cardinality(
                left_rows,
                left_stats,
                right_rows,
                right_stats,
                on_expr,
                schema,
            )
            .max(right_rows),
            crate::sql::ast::JoinType::Full => Self::estimate_inner_join_cardinality(
                left_rows,
                left_stats,
                right_rows,
                right_stats,
                on_expr,
                schema,
            )
            .max(left_rows)
            .max(right_rows),
            crate::sql::ast::JoinType::Semi => {
                if left_rows <= 0.0 || right_rows <= 0.0 {
                    0.0
                } else {
                    let p = Self::estimate_semi_match_probability(
                        left_rows,
                        left_stats,
                        right_rows,
                        right_stats,
                        on_expr,
                        schema,
                    );
                    let min_bound = if p > 0.0 { 1.0f64.min(left_rows) } else { 0.0 };
                    (left_rows * p).clamp(min_bound, left_rows)
                }
            }
            crate::sql::ast::JoinType::Anti => {
                if left_rows <= 0.0 {
                    0.0
                } else if right_rows <= 0.0 {
                    left_rows
                } else {
                    let p = Self::estimate_semi_match_probability(
                        left_rows,
                        left_stats,
                        right_rows,
                        right_stats,
                        on_expr,
                        schema,
                    );
                    let non_match_p = 1.0 - p;
                    let min_bound = if non_match_p > 0.0 {
                        1.0f64.min(left_rows)
                    } else {
                        0.0
                    };
                    (left_rows * non_match_p).clamp(min_bound, left_rows)
                }
            }
        }
    }
}

fn extract_col_and_val(left: &Expr, right: &Expr) -> (Option<String>, Option<Value>) {
    match (left, right) {
        (Expr::Column(col), Expr::Literal(val))
        | (Expr::Column(col), Expr::Parameter { value: val, .. }) => {
            (Some(col.clone()), Some(val.clone()))
        }
        (Expr::Literal(val), Expr::Column(col))
        | (Expr::Parameter { value: val, .. }, Expr::Column(col)) => {
            (Some(col.clone()), Some(val.clone()))
        }
        _ => (None, None),
    }
}

fn is_val_in_bucket(val: &Value, lower: &Value, upper: &Value) -> bool {
    let ge_lower = crate::query::compare_values_for_sort(
        val,
        lower,
        crate::query::SortDirection::Ascending,
        None,
    )
    .map(|o| o != std::cmp::Ordering::Less)
    .unwrap_or(false);

    let le_upper = crate::query::compare_values_for_sort(
        val,
        upper,
        crate::query::SortDirection::Ascending,
        None,
    )
    .map(|o| o != std::cmp::Ordering::Greater)
    .unwrap_or(false);

    ge_lower && le_upper
}

fn range_op_to_bounds(op: BinaryOp, val: &Value) -> (RangeBound, RangeBound) {
    match op {
        BinaryOp::Lt => (None, Some((val.clone(), false))),
        BinaryOp::LtEq => (None, Some((val.clone(), true))),
        BinaryOp::Gt => (Some((val.clone(), false)), None),
        BinaryOp::GtEq => (Some((val.clone(), true)), None),
        _ => (None, None),
    }
}

fn calculate_numeric_overlap_fraction(
    b_low: &Value,
    b_high: &Value,
    q_start: &Value,
    q_end: &Value,
) -> f64 {
    match (b_low, b_high, q_start, q_end) {
        (Value::Int64(bl), Value::Int64(bh), Value::Int64(qs), Value::Int64(qe)) => {
            let b_range = (*bh as i128) - (*bl as i128);
            if b_range <= 0 {
                return 1.0;
            }
            let q_range = (*qe as i128) - (*qs as i128);
            if q_range <= 0 {
                return 0.0;
            }
            ((q_range as f64) / (b_range as f64)).clamp(0.0, 1.0)
        }
        (Value::UInt64(bl), Value::UInt64(bh), Value::UInt64(qs), Value::UInt64(qe)) => {
            let b_range = (*bh as u128) - (*bl as u128);
            if b_range == 0 {
                return 1.0;
            }
            let q_range = (*qe as u128) - (*qs as u128);
            ((q_range as f64) / (b_range as f64)).clamp(0.0, 1.0)
        }
        (Value::Float64(bl), Value::Float64(bh), Value::Float64(qs), Value::Float64(qe)) => {
            if bl.is_nan() || bh.is_nan() || bh <= bl {
                return 0.5;
            }
            let b_range = *bh - *bl;
            let q_range = *qe - *qs;
            if b_range <= 0.0 {
                return 1.0;
            }
            (q_range / b_range).clamp(0.0, 1.0)
        }
        _ => 0.5,
    }
}

fn estimate_histogram_range(
    hist: &crate::query::stats::Histogram,
    total_table_rows: usize,
    min_bound: Option<(Value, bool)>,
    max_bound: Option<(Value, bool)>,
) -> f64 {
    if total_table_rows == 0 || hist.buckets.is_empty() {
        return 0.0;
    }

    if let (Some((ref min_v, ref min_inc)), Some((ref max_v, ref max_inc))) =
        (&min_bound, &max_bound)
    {
        if let Ok(ord) = crate::query::compare_values_for_sort(
            min_v,
            max_v,
            crate::query::SortDirection::Ascending,
            None,
        ) {
            match ord {
                std::cmp::Ordering::Greater => return 0.0,
                std::cmp::Ordering::Equal if !min_inc || !max_inc => return 0.0,
                _ => {}
            }
        }
    }

    let mut total_matching_rows = 0.0;

    for bucket in &hist.buckets {
        let b_low = &bucket.lower_bound;
        let b_high = &bucket.upper_bound;

        if let Some((ref min_v, ref min_inc)) = min_bound {
            if let Ok(ord) = crate::query::compare_values_for_sort(
                min_v,
                b_high,
                crate::query::SortDirection::Ascending,
                None,
            ) {
                if ord == std::cmp::Ordering::Greater
                    || (ord == std::cmp::Ordering::Equal && !min_inc)
                {
                    continue;
                }
            }
        }

        if let Some((ref max_v, ref max_inc)) = max_bound {
            if let Ok(ord) = crate::query::compare_values_for_sort(
                max_v,
                b_low,
                crate::query::SortDirection::Ascending,
                None,
            ) {
                if ord == std::cmp::Ordering::Less || (ord == std::cmp::Ordering::Equal && !max_inc)
                {
                    continue;
                }
            }
        }

        let min_covers = match min_bound {
            None => true,
            Some((ref min_v, ref min_inc)) => {
                match crate::query::compare_values_for_sort(
                    min_v,
                    b_low,
                    crate::query::SortDirection::Ascending,
                    None,
                ) {
                    Ok(std::cmp::Ordering::Less) => true,
                    Ok(std::cmp::Ordering::Equal) => *min_inc,
                    _ => false,
                }
            }
        };

        let max_covers = match max_bound {
            None => true,
            Some((ref max_v, ref max_inc)) => {
                match crate::query::compare_values_for_sort(
                    max_v,
                    b_high,
                    crate::query::SortDirection::Ascending,
                    None,
                ) {
                    Ok(std::cmp::Ordering::Greater) => true,
                    Ok(std::cmp::Ordering::Equal) => *max_inc,
                    _ => false,
                }
            }
        };

        if min_covers && max_covers {
            total_matching_rows += bucket.row_count as f64;
        } else {
            let q_start = match min_bound {
                None => b_low,
                Some((ref min_v, _)) => {
                    if let Ok(std::cmp::Ordering::Greater) = crate::query::compare_values_for_sort(
                        min_v,
                        b_low,
                        crate::query::SortDirection::Ascending,
                        None,
                    ) {
                        min_v
                    } else {
                        b_low
                    }
                }
            };

            let q_end = match max_bound {
                None => b_high,
                Some((ref max_v, _)) => {
                    if let Ok(std::cmp::Ordering::Less) = crate::query::compare_values_for_sort(
                        max_v,
                        b_high,
                        crate::query::SortDirection::Ascending,
                        None,
                    ) {
                        max_v
                    } else {
                        b_high
                    }
                }
            };

            let frac = calculate_numeric_overlap_fraction(b_low, b_high, q_start, q_end);
            total_matching_rows += (bucket.row_count as f64) * frac;
        }
    }

    let non_null_count = (total_table_rows.saturating_sub(hist.null_count)) as f64;
    let clamped_matching = total_matching_rows.clamp(0.0, non_null_count);
    (clamped_matching / (total_table_rows as f64)).clamp(0.0, 1.0)
}

fn estimate_range_fraction(min_v: &Value, max_v: &Value, val: &Value, op: BinaryOp) -> Option<f64> {
    match (min_v, max_v, val) {
        (Value::Int64(min_i), Value::Int64(max_i), Value::Int64(v)) => {
            if max_i <= min_i {
                return Some(DEFAULT_RANGE_SELECTIVITY);
            }
            let total_range = (*max_i - *min_i) as f64;
            let val_offset = (*v - *min_i) as f64;
            let frac = match op {
                BinaryOp::Lt | BinaryOp::LtEq => val_offset / total_range,
                BinaryOp::Gt | BinaryOp::GtEq => (total_range - val_offset) / total_range,
                _ => DEFAULT_RANGE_SELECTIVITY,
            };
            Some(frac.clamp(0.001, 1.0))
        }
        (Value::UInt64(min_u), Value::UInt64(max_u), Value::UInt64(v)) => {
            if max_u <= min_u {
                return Some(DEFAULT_RANGE_SELECTIVITY);
            }
            let total_range = (*max_u - *min_u) as f64;
            let val_offset = v.saturating_sub(*min_u) as f64;
            let frac = match op {
                BinaryOp::Lt | BinaryOp::LtEq => val_offset / total_range,
                BinaryOp::Gt | BinaryOp::GtEq => (total_range - val_offset) / total_range,
                _ => DEFAULT_RANGE_SELECTIVITY,
            };
            Some(frac.clamp(0.001, 1.0))
        }
        (Value::Float64(min_f), Value::Float64(max_f), Value::Float64(v)) => {
            if max_f <= min_f {
                return Some(DEFAULT_RANGE_SELECTIVITY);
            }
            let total_range = *max_f - *min_f;
            let val_offset = *v - *min_f;
            let frac = match op {
                BinaryOp::Lt | BinaryOp::LtEq => val_offset / total_range,
                BinaryOp::Gt | BinaryOp::GtEq => (total_range - val_offset) / total_range,
                _ => DEFAULT_RANGE_SELECTIVITY,
            };
            Some(frac.clamp(0.001, 1.0))
        }
        _ => None,
    }
}

fn flatten_conjunction<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary {
        left,
        op: BinaryOp::And,
        right,
    } = expr
    {
        flatten_conjunction(left, out);
        flatten_conjunction(right, out);
    } else {
        out.push(expr);
    }
}
