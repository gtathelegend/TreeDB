use std::time::Instant;

use crate::engine::types::{Column, Row, Schema, Value};
use crate::error::Result;
use crate::query::expr::{AggregateExpr, Expr};
use crate::query::external_aggregate::estimate_group_entry_memory;
use crate::query::external_sort::{ExternalSorter, SortConfig};
use crate::query::operator::{AggregateState, CanonicalValue, GroupKey, SortDirection, SortKey};

/// Telemetry metrics produced during physical execution of streaming aggregation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingAggregateMetrics {
    /// Total number of rows consumed by the streaming aggregation operator.
    pub input_rows: usize,
    /// Total number of distinct aggregate groups emitted.
    pub output_groups: usize,
    /// Number of distinct group transitions observed during iteration.
    pub group_transitions: usize,
    /// Maximum row count observed within a single grouping bucket.
    pub max_group_size: usize,
    /// Total number of incremental aggregate state updates evaluated.
    pub aggregate_updates: usize,
    /// Whether an explicit pre-aggregation sort was required by the physical plan.
    pub sort_required: bool,
    /// Whether the pre-aggregation sort spilled temporary runs to disk.
    pub sort_spilled: bool,
    /// Total bytes spilled to disk if pre-aggregation sort spilled.
    pub sort_spill_bytes: usize,
    /// Peak memory footprint estimated in bytes during streaming aggregation.
    pub memory_bytes: usize,
    /// Wall-clock execution time elapsed inside the operator in microseconds.
    pub execution_time_us: u64,
}

/// Physical operator performing cost-based ordered streaming aggregation.
pub struct StreamingAggregateOperator<'a> {
    input_schema: &'a Schema,
    group_by: &'a [Expr],
    aggregates: &'a [AggregateExpr],
    sort_required: bool,
    sort_config: Option<SortConfig>,
}

impl<'a> StreamingAggregateOperator<'a> {
    /// Creates a new `StreamingAggregateOperator`.
    pub fn new(
        input_schema: &'a Schema,
        group_by: &'a [Expr],
        aggregates: &'a [AggregateExpr],
        sort_required: bool,
        sort_config: Option<SortConfig>,
    ) -> Self {
        Self {
            input_schema,
            group_by,
            aggregates,
            sort_required,
            sort_config,
        }
    }

    /// Executes ordered streaming aggregation over input rows.
    pub fn execute(self, rows: Vec<Row>) -> Result<(Schema, Vec<Row>, StreamingAggregateMetrics)> {
        let start_time = Instant::now();
        let input_rows_count = rows.len();

        let mut metrics = StreamingAggregateMetrics {
            input_rows: input_rows_count,
            sort_required: self.sort_required,
            ..Default::default()
        };

        // Construct intermediate output schema: grouping columns first, then aggregate columns.
        let mut out_cols = Vec::with_capacity(self.group_by.len() + self.aggregates.len());
        for (idx, grp_expr) in self.group_by.iter().enumerate() {
            let dt = grp_expr.infer_type(self.input_schema)?;
            let name = match grp_expr {
                Expr::Column(col_name) => col_name.clone(),
                _ => format!("_group_{idx}"),
            };
            out_cols.push(Column::new(name, dt, true));
        }
        for agg_expr in self.aggregates {
            let dt = agg_expr.infer_type(self.input_schema)?;
            let name = agg_expr.display_name();
            out_cols.push(Column::new(name, dt, true));
        }
        let out_schema = Schema::new_projected(out_cols);

        // Perform explicit sorting if required
        let (sorted_rows, sort_memory_bytes) =
            if self.sort_required && !self.group_by.is_empty() && !rows.is_empty() {
                let sort_keys: Vec<SortKey> = self
                    .group_by
                    .iter()
                    .map(|expr| SortKey {
                        expression: expr.clone(),
                        direction: SortDirection::Ascending,
                        nulls_first: None,
                    })
                    .collect();

                let config = self.sort_config.unwrap_or_default();
                let sorter = ExternalSorter::new(self.input_schema, &sort_keys, config);
                let (sorted, sort_metrics) = sorter.sort(rows)?;
                metrics.sort_spilled =
                    sort_metrics.runs_created > 0 || sort_metrics.bytes_spilled > 0;
                metrics.sort_spill_bytes = sort_metrics.bytes_spilled;
                (sorted, sort_metrics.peak_memory_bytes)
            } else {
                (rows, 0)
            };

        // Case 1: Global aggregation (no GROUP BY)
        if self.group_by.is_empty() {
            let mut accumulators = Vec::with_capacity(self.aggregates.len());
            for agg in self.aggregates {
                accumulators.push(AggregateState::new(agg, self.input_schema)?);
            }

            for row in &sorted_rows {
                for acc in &mut accumulators {
                    acc.accumulate(row, self.input_schema)?;
                    metrics.aggregate_updates += 1;
                }
            }

            let agg_values: Vec<Value> = accumulators.into_iter().map(|acc| acc.finish()).collect();
            let output_row = Row::new(agg_values);

            metrics.output_groups = 1;
            metrics.max_group_size = input_rows_count;
            metrics.memory_bytes = sort_memory_bytes.max(256);
            metrics.execution_time_us = start_time.elapsed().as_micros() as u64;

            return Ok((out_schema, vec![output_row], metrics));
        }

        // Case 2: Grouped aggregation
        if sorted_rows.is_empty() {
            metrics.output_groups = 0;
            metrics.max_group_size = 0;
            metrics.group_transitions = 0;
            metrics.memory_bytes = sort_memory_bytes;
            metrics.execution_time_us = start_time.elapsed().as_micros() as u64;
            return Ok((out_schema, Vec::new(), metrics));
        }

        let mut output_rows = Vec::new();
        let mut current_group_key: Option<GroupKey> = None;
        let mut current_group_values: Vec<Value> = Vec::new();
        let mut current_accumulators: Vec<AggregateState> = Vec::new();
        let mut current_group_size: usize = 0;
        let mut max_group_size: usize = 0;
        let mut group_transitions: usize = 0;
        let mut peak_group_memory: usize = 0;

        for row in &sorted_rows {
            let mut group_vals = Vec::with_capacity(self.group_by.len());
            let mut canon_vals = Vec::with_capacity(self.group_by.len());
            for expr in self.group_by {
                let v = expr.evaluate(row, self.input_schema)?;
                canon_vals.push(CanonicalValue::from_value(&v));
                group_vals.push(v);
            }
            let key = GroupKey(canon_vals);

            if let Some(ref cur_key) = current_group_key {
                if cur_key == &key {
                    // Continuing current group
                    current_group_size += 1;
                    for acc in &mut current_accumulators {
                        acc.accumulate(row, self.input_schema)?;
                        metrics.aggregate_updates += 1;
                    }
                } else {
                    // Transition to new group
                    group_transitions += 1;
                    max_group_size = max_group_size.max(current_group_size);

                    let group_mem = estimate_group_entry_memory(
                        cur_key,
                        &current_group_values,
                        &current_accumulators,
                    );
                    peak_group_memory = peak_group_memory.max(group_mem);

                    // Finalize and emit previous group
                    let mut out_values = current_group_values;
                    for acc in current_accumulators {
                        out_values.push(acc.finish());
                    }
                    output_rows.push(Row::new(out_values));

                    // Initialize new group
                    current_group_key = Some(key);
                    current_group_values = group_vals;
                    current_group_size = 1;

                    let mut new_accs = Vec::with_capacity(self.aggregates.len());
                    for agg in self.aggregates {
                        let mut acc = AggregateState::new(agg, self.input_schema)?;
                        acc.accumulate(row, self.input_schema)?;
                        metrics.aggregate_updates += 1;
                        new_accs.push(acc);
                    }
                    current_accumulators = new_accs;
                }
            } else {
                // First row
                current_group_key = Some(key);
                current_group_values = group_vals;
                current_group_size = 1;

                let mut new_accs = Vec::with_capacity(self.aggregates.len());
                for agg in self.aggregates {
                    let mut acc = AggregateState::new(agg, self.input_schema)?;
                    acc.accumulate(row, self.input_schema)?;
                    metrics.aggregate_updates += 1;
                    new_accs.push(acc);
                }
                current_accumulators = new_accs;
            }
        }

        // Finalize last group
        if let Some(ref cur_key) = current_group_key {
            max_group_size = max_group_size.max(current_group_size);
            let group_mem =
                estimate_group_entry_memory(cur_key, &current_group_values, &current_accumulators);
            peak_group_memory = peak_group_memory.max(group_mem);

            let mut out_values = current_group_values;
            for acc in current_accumulators {
                out_values.push(acc.finish());
            }
            output_rows.push(Row::new(out_values));
        }

        metrics.output_groups = output_rows.len();
        metrics.group_transitions = group_transitions;
        metrics.max_group_size = max_group_size;
        metrics.memory_bytes = sort_memory_bytes.max(peak_group_memory).max(256);
        metrics.execution_time_us = start_time.elapsed().as_micros() as u64;

        Ok((out_schema, output_rows, metrics))
    }
}
