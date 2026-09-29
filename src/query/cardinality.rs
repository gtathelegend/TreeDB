use crate::engine::catalog::IndexMetadata;
use crate::engine::types::{Schema, Value};
use crate::query::expr::{BinaryOp, Expr, UnaryOp};
use crate::query::stats::TableStatistics;

/// Fallback defaults when statistics are unavailable.
pub const DEFAULT_TABLE_ROWS: f64 = 1000.0;
pub const DEFAULT_EQ_SELECTIVITY: f64 = 0.1;
pub const DEFAULT_RANGE_SELECTIVITY: f64 = 0.3333333333333333;
pub const DEFAULT_NULL_FRACTION: f64 = 0.05;

/// Deterministic cardinality and selectivity estimator.
pub struct CardinalityEstimator;

impl CardinalityEstimator {
    /// Returns the estimated base row count for a table relation.
    pub fn estimate_table_rows(stats: Option<&TableStatistics>) -> f64 {
        stats
            .map(|s| s.row_count as f64)
            .unwrap_or(DEFAULT_TABLE_ROWS)
    }

    /// Estimates the selectivity of an arbitrary boolean expression in `[0.0, 1.0]`.
    pub fn estimate_expr_selectivity(
        expr: &Expr,
        stats: Option<&TableStatistics>,
        _schema: &Schema,
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
                    Self::estimate_expr_selectivity(&Expr::IsNull(inner.clone()), stats, _schema);
                1.0 - null_sel
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr: inner,
            } => {
                let inner_sel = Self::estimate_expr_selectivity(inner, stats, _schema);
                1.0 - inner_sel
            }
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => {
                    let s_left = Self::estimate_expr_selectivity(left, stats, _schema);
                    let s_right = Self::estimate_expr_selectivity(right, stats, _schema);
                    s_left * s_right
                }
                BinaryOp::Or => {
                    let s_left = Self::estimate_expr_selectivity(left, stats, _schema);
                    let s_right = Self::estimate_expr_selectivity(right, stats, _schema);
                    s_left + s_right - (s_left * s_right)
                }
                BinaryOp::Eq => {
                    let (col_opt, _val_opt) = extract_col_and_val(left, right);
                    if let Some(col_name) = col_opt {
                        let col_raw = col_name.split('.').next_back().unwrap_or(&col_name);
                        if let Some(st) = stats {
                            if let Some(col_st) = st.column(col_raw) {
                                if col_st.distinct_count > 0 {
                                    1.0 / (col_st.distinct_count as f64)
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
                        _schema,
                    );
                    1.0 - eq_sel
                }
                BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                    let (col_opt, val_opt) = extract_col_and_val(left, right);
                    if let (Some(col_name), Some(val)) = (col_opt, val_opt) {
                        let col_raw = col_name.split('.').next_back().unwrap_or(&col_name);
                        if let Some(st) = stats {
                            if let Some(col_st) = st.column(col_raw) {
                                if let (Some(min_v), Some(max_v)) = (&col_st.min, &col_st.max) {
                                    if let Some(frac) =
                                        estimate_range_fraction(min_v, max_v, &val, *op)
                                    {
                                        frac
                                    } else {
                                        DEFAULT_RANGE_SELECTIVITY
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
                let single_sel = Self::estimate_expr_selectivity(
                    &Expr::Binary {
                        left: inner.clone(),
                        op: BinaryOp::Eq,
                        right: Box::new(
                            list.first().cloned().unwrap_or(Expr::Literal(Value::Null)),
                        ),
                    },
                    stats,
                    _schema,
                );
                let list_sel = (single_sel * (list.len() as f64)).clamp(0.0, 1.0);
                if *negated {
                    1.0 - list_sel
                } else {
                    list_sel
                }
            }
            _ => 0.5,
        };

        sel.clamp(0.0, 1.0)
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
            return (base_rows * DEFAULT_RANGE_SELECTIVITY).max(1.0);
        }
        (DEFAULT_TABLE_ROWS * 0.01).max(1.0)
    }

    /// Estimates the number of distinct prefix combinations for skipped prefix columns.
    pub fn estimate_distinct_prefixes(
        stats: Option<&TableStatistics>,
        schema: &Schema,
        skipped_col_indices: &[usize],
    ) -> f64 {
        if let Some(st) = stats {
            let mut total_distinct = 1.0;
            for &idx in skipped_col_indices {
                let col_name = &schema.columns[idx].name;
                if let Some(col_st) = st.column(col_name) {
                    if col_st.distinct_count > 0 {
                        total_distinct *= col_st.distinct_count as f64;
                    } else {
                        total_distinct *= 10.0;
                    }
                } else {
                    total_distinct *= 10.0;
                }
            }
            total_distinct.min(st.row_count as f64).max(1.0)
        } else {
            10.0
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
                    (left_rows * p).clamp(if p > 0.0 { 1.0 } else { 0.0 }, left_rows)
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
                    (left_rows * non_match_p)
                        .clamp(if non_match_p > 0.0 { 1.0 } else { 0.0 }, left_rows)
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
