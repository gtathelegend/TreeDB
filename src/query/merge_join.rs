use std::cmp::Ordering;

use crate::engine::types::{Row, Schema, Value};
use crate::error::Result;
use crate::query::external_hash_join::{
    create_joined_schema, EquiJoinPlan, JoinConfig, JoinExecutionMetrics, JoinKey,
};
use crate::query::external_sort::{ExternalSorter, SortConfig};
use crate::query::operator::SortKey;
use crate::sql::ast::JoinType;

pub use crate::query::physical_properties::{
    ordering_property_for_table_scan, OrderingColumn, OrderingProperty, PhysicalProperties,
    RequiredOrdering,
};

/// Cost-based equality Merge Join operator supporting Inner and Outer joins.
pub struct MergeJoinOperator<'a> {
    left_schema: &'a Schema,
    right_schema: &'a Schema,
    join_type: JoinType,
    equi_plan: EquiJoinPlan,
    left_sorted: bool,
    right_sorted: bool,
    config: JoinConfig,
    row_goal: Option<usize>,
}

impl<'a> MergeJoinOperator<'a> {
    /// Creates a new `MergeJoinOperator`.
    pub fn new(
        left_schema: &'a Schema,
        right_schema: &'a Schema,
        join_type: JoinType,
        equi_plan: EquiJoinPlan,
        left_sorted: bool,
        right_sorted: bool,
        config: JoinConfig,
    ) -> Self {
        Self {
            left_schema,
            right_schema,
            join_type,
            equi_plan,
            left_sorted,
            right_sorted,
            config,
            row_goal: None,
        }
    }

    /// Sets the row goal for early termination during merge scan.
    pub fn with_row_goal(mut self, goal: Option<usize>) -> Self {
        self.row_goal = goal;
        self
    }

    /// Executes the merge join on input left and right rows.
    pub fn execute(
        &self,
        mut left_rows: Vec<Row>,
        mut right_rows: Vec<Row>,
    ) -> Result<(Schema, Vec<Row>, JoinExecutionMetrics)> {
        let mut metrics = JoinExecutionMetrics {
            rows_left: left_rows.len(),
            rows_right: right_rows.len(),
            merge_join_left_rows: left_rows.len(),
            merge_join_right_rows: right_rows.len(),
            merge_join_sort_left: !self.left_sorted,
            merge_join_sort_right: !self.right_sorted,
            ..Default::default()
        };

        let sort_config = SortConfig {
            memory_budget_bytes: self.config.memory_budget_bytes,
            max_temp_files: self.config.max_temp_files,
            max_temp_bytes: self.config.max_temp_bytes,
            max_merge_passes: self.config.max_partition_passes,
            temp_dir: self.config.temp_dir.clone(),
        };

        // 1. Sort left input if not already ordered
        if !self.left_sorted && !left_rows.is_empty() {
            let sort_keys: Vec<SortKey> = self
                .equi_plan
                .left_keys
                .iter()
                .map(|k| SortKey::asc(k.clone()))
                .collect();
            let sorter = ExternalSorter::new(self.left_schema, &sort_keys, sort_config.clone());
            let (sorted_left, sort_metrics) = sorter.sort(left_rows)?;
            left_rows = sorted_left;
            metrics.runs_created += sort_metrics.runs_created;
            metrics.bytes_spilled += sort_metrics.bytes_spilled;
            metrics.merge_passes += sort_metrics.merge_passes;
            metrics.peak_memory_bytes = metrics
                .peak_memory_bytes
                .max(sort_metrics.peak_memory_bytes);
        }

        // 2. Sort right input if not already ordered
        if !self.right_sorted && !right_rows.is_empty() {
            let sort_keys: Vec<SortKey> = self
                .equi_plan
                .right_keys
                .iter()
                .map(|k| SortKey::asc(k.clone()))
                .collect();
            let sorter = ExternalSorter::new(self.right_schema, &sort_keys, sort_config);
            let (sorted_right, sort_metrics) = sorter.sort(right_rows)?;
            right_rows = sorted_right;
            metrics.runs_created += sort_metrics.runs_created;
            metrics.bytes_spilled += sort_metrics.bytes_spilled;
            metrics.merge_passes += sort_metrics.merge_passes;
            metrics.peak_memory_bytes = metrics
                .peak_memory_bytes
                .max(sort_metrics.peak_memory_bytes);
        }

        let joined_schema =
            create_joined_schema(self.left_schema, self.right_schema, self.join_type);
        let mut out_rows = Vec::new();

        let left_len = left_rows.len();
        let right_len = right_rows.len();
        let mut left_idx = 0;
        let mut right_idx = 0;

        // 3. Merge iteration over sorted streams
        while left_idx < left_len && right_idx < right_len {
            let left_key = JoinKey::from_row(
                &left_rows[left_idx],
                self.left_schema,
                &self.equi_plan.left_keys,
            )?;
            let right_key = JoinKey::from_row(
                &right_rows[right_idx],
                self.right_schema,
                &self.equi_plan.right_keys,
            )?;

            if left_key.has_null() {
                metrics.merge_join_null_left_keys += 1;
                if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                    metrics.merge_join_unmatched_left_rows += 1;
                    out_rows.push(emit_null_padded_right(
                        &left_rows[left_idx],
                        self.right_schema,
                    ));
                }
                left_idx += 1;
                continue;
            }

            if right_key.has_null() {
                metrics.merge_join_null_right_keys += 1;
                if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
                    metrics.merge_join_unmatched_right_rows += 1;
                    out_rows.push(emit_null_padded_left(
                        &right_rows[right_idx],
                        self.left_schema,
                    ));
                }
                right_idx += 1;
                continue;
            }

            metrics.merge_join_comparisons += 1;
            match left_key.cmp(&right_key) {
                Ordering::Less => {
                    if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                        metrics.merge_join_unmatched_left_rows += 1;
                        out_rows.push(emit_null_padded_right(
                            &left_rows[left_idx],
                            self.right_schema,
                        ));
                    }
                    left_idx += 1;
                }
                Ordering::Greater => {
                    if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
                        metrics.merge_join_unmatched_right_rows += 1;
                        out_rows.push(emit_null_padded_left(
                            &right_rows[right_idx],
                            self.left_schema,
                        ));
                    }
                    right_idx += 1;
                }
                Ordering::Equal => {
                    metrics.merge_join_key_matches += 1;
                    metrics.duplicate_runs_processed += 1;

                    // Find end of duplicate run on left
                    let mut left_end = left_idx + 1;
                    while left_end < left_len {
                        let next_k = JoinKey::from_row(
                            &left_rows[left_end],
                            self.left_schema,
                            &self.equi_plan.left_keys,
                        )?;
                        if next_k == left_key {
                            left_end += 1;
                        } else {
                            break;
                        }
                    }
                    let left_run_count = left_end - left_idx;
                    if left_run_count > 1 {
                        metrics.merge_join_duplicate_left_rows += left_run_count - 1;
                    }

                    // Find end of duplicate run on right
                    let mut right_end = right_idx + 1;
                    while right_end < right_len {
                        let next_k = JoinKey::from_row(
                            &right_rows[right_end],
                            self.right_schema,
                            &self.equi_plan.right_keys,
                        )?;
                        if next_k == right_key {
                            right_end += 1;
                        } else {
                            break;
                        }
                    }
                    let right_run_count = right_end - right_idx;
                    if right_run_count > 1 {
                        metrics.merge_join_duplicate_right_rows += right_run_count - 1;
                    }

                    // Cross product of duplicate runs
                    let mut left_matched_any = vec![false; left_run_count];
                    let mut right_matched_any = vec![false; right_run_count];

                    for (l_offset, l_row) in left_rows[left_idx..left_end].iter().enumerate() {
                        for (r_offset, r_row) in right_rows[right_idx..right_end].iter().enumerate()
                        {
                            metrics.candidate_pairs += 1;
                            let mut combined_values =
                                Vec::with_capacity(l_row.values.len() + r_row.values.len());
                            combined_values.extend_from_slice(&l_row.values);
                            combined_values.extend_from_slice(&r_row.values);
                            let candidate_row = Row::new(combined_values);

                            let matches_residual = if let Some(residual) =
                                &self.equi_plan.residual_predicate
                            {
                                metrics.merge_join_residual_evaluations += 1;
                                let res_val = residual.evaluate(&candidate_row, &joined_schema)?;
                                if res_val == Value::Boolean(true) {
                                    true
                                } else {
                                    metrics.merge_join_residual_rejections += 1;
                                    false
                                }
                            } else {
                                true
                            };

                            if matches_residual {
                                metrics.matched_rows += 1;
                                out_rows.push(candidate_row);
                                left_matched_any[l_offset] = true;
                                right_matched_any[r_offset] = true;
                                if let Some(k) = self.row_goal {
                                    if self.join_type == JoinType::Inner && out_rows.len() >= k {
                                        metrics.early_terminated = true;
                                        metrics.row_goal_satisfied = true;
                                        metrics.row_goal = Some(k);
                                        metrics.rows_output = out_rows.len();
                                        return Ok((joined_schema, out_rows, metrics));
                                    }
                                }
                            }
                        }
                    }

                    // For LEFT/FULL outer join, emit unmatched rows from left run
                    if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                        for (l_offset, &matched) in left_matched_any.iter().enumerate() {
                            if !matched {
                                metrics.merge_join_unmatched_left_rows += 1;
                                out_rows.push(emit_null_padded_right(
                                    &left_rows[left_idx + l_offset],
                                    self.right_schema,
                                ));
                            }
                        }
                    }

                    // For RIGHT/FULL outer join, emit unmatched rows from right run
                    if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
                        for (r_offset, &matched) in right_matched_any.iter().enumerate() {
                            if !matched {
                                metrics.merge_join_unmatched_right_rows += 1;
                                out_rows.push(emit_null_padded_left(
                                    &right_rows[right_idx + r_offset],
                                    self.left_schema,
                                ));
                            }
                        }
                    }

                    left_idx = left_end;
                    right_idx = right_end;
                }
            }
        }

        // Drain remaining left rows
        while left_idx < left_len {
            if self.join_type == JoinType::Left || self.join_type == JoinType::Full {
                metrics.merge_join_unmatched_left_rows += 1;
                out_rows.push(emit_null_padded_right(
                    &left_rows[left_idx],
                    self.right_schema,
                ));
            }
            left_idx += 1;
        }

        // Drain remaining right rows
        while right_idx < right_len {
            if self.join_type == JoinType::Right || self.join_type == JoinType::Full {
                metrics.merge_join_unmatched_right_rows += 1;
                out_rows.push(emit_null_padded_left(
                    &right_rows[right_idx],
                    self.left_schema,
                ));
            }
            right_idx += 1;
        }

        metrics.rows_output = out_rows.len();
        metrics.merge_join_output_rows = out_rows.len();
        Ok((joined_schema, out_rows, metrics))
    }
}

fn emit_null_padded_right(left_row: &Row, right_schema: &Schema) -> Row {
    let mut values = left_row.values.clone();
    values.resize(
        left_row.values.len() + right_schema.columns.len(),
        Value::Null,
    );
    Row::new(values)
}

fn emit_null_padded_left(right_row: &Row, left_schema: &Schema) -> Row {
    let mut values = vec![Value::Null; left_schema.columns.len()];
    values.extend_from_slice(&right_row.values);
    Row::new(values)
}
