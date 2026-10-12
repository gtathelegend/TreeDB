use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::engine::types::{Row, Schema};
use crate::error::Result;
use crate::query::external_sort::{estimate_row_memory, SortConfig};
use crate::query::operator::{compare_rows_prepared, PreparedSortKey, SortKey};

/// Execution telemetry and statistics for a Top-N operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopNExecutionMetrics {
    /// Total number of rows consumed by the Top-N operator.
    pub rows_input: usize,
    /// Total number of rows emitted by the Top-N operator.
    pub rows_output: usize,
    /// Maximum capacity of candidates maintained in the heap (offset + limit).
    pub candidate_capacity: usize,
    /// Number of candidate rows retained in the heap.
    pub rows_retained: usize,
    /// Number of candidate rows discarded (worse than current heap top).
    pub rows_discarded: usize,
    /// Peak in-memory byte footprint estimated during Top-N execution.
    pub peak_memory_bytes: usize,
    /// Configured LIMIT bound.
    pub limit: Option<usize>,
    /// Configured OFFSET skip count.
    pub offset: Option<usize>,
    /// Whether early termination was reached.
    pub early_terminated: bool,
}

/// An item stored in the Top-N bounded Max-Heap.
///
/// Notice: In Rust's `BinaryHeap`, `peek()` yields the *maximum* item according to `Ord`.
/// In Top-N, we want to retain the smallest (highest-ranking in SQL order) $K$ candidates.
/// Thus, a row that is *worse* in SQL order (or arrived later in tie-break) is considered *greater* in `Ord`,
/// so that the worst candidate sits at the top of the heap and can be evicted.
struct TopNHeapItem<'s, 'k> {
    seq: u64,
    row: Row,
    schema: &'s Schema,
    prepared_keys: &'k [PreparedSortKey],
}

impl<'s, 'k> PartialEq for TopNHeapItem<'s, 'k> {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}

impl<'s, 'k> Eq for TopNHeapItem<'s, 'k> {}

impl<'s, 'k> Ord for TopNHeapItem<'s, 'k> {
    fn cmp(&self, other: &Self) -> Ordering {
        match compare_rows_prepared(&self.row, &other.row, self.schema, self.prepared_keys) {
            Ok(Ordering::Equal) => self.seq.cmp(&other.seq),
            Ok(ord) => ord,
            Err(_) => self.seq.cmp(&other.seq),
        }
    }
}

impl<'s, 'k> PartialOrd for TopNHeapItem<'s, 'k> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Physical bounded sort (Top-N) operator.
///
/// Evaluates `ORDER BY ... [OFFSET M] LIMIT N` over an input row stream using a bounded
/// priority queue of size $M + N$, avoiding the memory and CPU overhead of sorting the entire stream.
pub struct TopNOperator;

impl TopNOperator {
    /// Applies Top-N bounded sorting using the default sort configuration.
    pub fn apply(
        rows: Vec<Row>,
        schema: &Schema,
        sort_keys: &[SortKey],
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> Result<Vec<Row>> {
        Self::apply_with_config(
            rows,
            schema,
            sort_keys,
            limit,
            offset,
            &SortConfig::default(),
        )
        .map(|(r, _)| r)
    }

    /// Applies Top-N bounded sorting using the specified sort configuration,
    /// returning the ordered, paginated rows and execution telemetry.
    pub fn apply_with_config(
        rows: Vec<Row>,
        schema: &Schema,
        sort_keys: &[SortKey],
        limit: Option<usize>,
        offset: Option<usize>,
        config: &SortConfig,
    ) -> Result<(Vec<Row>, TopNExecutionMetrics)> {
        let rows_input = rows.len();
        let limit_val = limit.unwrap_or(usize::MAX);
        let offset_val = offset.unwrap_or(0);

        if limit_val == 0 {
            return Ok((
                Vec::new(),
                TopNExecutionMetrics {
                    rows_input,
                    rows_output: 0,
                    candidate_capacity: 0,
                    rows_retained: 0,
                    rows_discarded: rows_input,
                    peak_memory_bytes: 0,
                    limit,
                    offset,
                    early_terminated: false,
                },
            ));
        }

        let capacity = offset_val.saturating_add(limit_val);

        // If no limit or capacity unbounded, fallback to full sort
        if capacity == usize::MAX {
            let (sorted, sort_metrics) =
                crate::query::external_sort::ExternalSorter::new(schema, sort_keys, config.clone())
                    .sort(rows)?;
            let paginated: Vec<Row> = sorted.into_iter().skip(offset_val).collect();
            return Ok((
                paginated.clone(),
                TopNExecutionMetrics {
                    rows_input,
                    rows_output: paginated.len(),
                    candidate_capacity: capacity,
                    rows_retained: rows_input,
                    rows_discarded: 0,
                    peak_memory_bytes: sort_metrics.peak_memory_bytes,
                    limit,
                    offset,
                    early_terminated: false,
                },
            ));
        }

        let prepared_keys = PreparedSortKey::prepare(schema, sort_keys)?;

        let fallback_rows = rows.clone();
        let mut heap: BinaryHeap<TopNHeapItem> =
            BinaryHeap::with_capacity(capacity.min(rows_input));
        let mut current_memory: usize = 0;
        let mut peak_memory: usize = 0;
        let mut rows_discarded: usize = 0;
        let mut budget_exceeded = false;

        for (seq_idx, row) in rows.into_iter().enumerate() {
            let row_mem = estimate_row_memory(&row);

            if heap.len() < capacity {
                current_memory += row_mem;
                peak_memory = peak_memory.max(current_memory);

                if current_memory > config.memory_budget_bytes {
                    budget_exceeded = true;
                    break;
                }

                heap.push(TopNHeapItem {
                    seq: seq_idx as u64,
                    row,
                    schema,
                    prepared_keys: &prepared_keys,
                });
            } else {
                let candidate = TopNHeapItem {
                    seq: seq_idx as u64,
                    row,
                    schema,
                    prepared_keys: &prepared_keys,
                };

                let should_replace = if let Some(worst) = heap.peek() {
                    match compare_rows_prepared(&candidate.row, &worst.row, schema, &prepared_keys)?
                    {
                        Ordering::Less => true,
                        Ordering::Equal => candidate.seq < worst.seq,
                        Ordering::Greater => false,
                    }
                } else {
                    true
                };

                if should_replace {
                    let evicted = heap.pop().unwrap();
                    let evicted_mem = estimate_row_memory(&evicted.row);
                    current_memory = current_memory.saturating_sub(evicted_mem) + row_mem;
                    peak_memory = peak_memory.max(current_memory);

                    if current_memory > config.memory_budget_bytes {
                        budget_exceeded = true;
                        break;
                    }

                    heap.push(candidate);
                    rows_discarded += 1;
                } else {
                    rows_discarded += 1;
                }
            }
        }

        if budget_exceeded {
            let (sorted, sort_metrics) =
                crate::query::external_sort::ExternalSorter::new(schema, sort_keys, config.clone())
                    .sort(fallback_rows)?;
            let paginated: Vec<Row> = sorted
                .into_iter()
                .skip(offset_val)
                .take(limit_val)
                .collect();
            return Ok((
                paginated.clone(),
                TopNExecutionMetrics {
                    rows_input,
                    rows_output: paginated.len(),
                    candidate_capacity: capacity,
                    rows_retained: rows_input,
                    rows_discarded: 0,
                    peak_memory_bytes: sort_metrics.peak_memory_bytes,
                    limit,
                    offset,
                    early_terminated: false,
                },
            ));
        }

        let rows_retained = heap.len();
        let mut items: Vec<TopNHeapItem> = heap.into_vec();

        // Sort items into final ascending order
        let mut sort_err = Ok(());
        items.sort_by(|a, b| {
            if sort_err.is_err() {
                return Ordering::Equal;
            }
            match compare_rows_prepared(&a.row, &b.row, schema, &prepared_keys) {
                Ok(Ordering::Equal) => a.seq.cmp(&b.seq),
                Ok(ord) => ord,
                Err(e) => {
                    sort_err = Err(e);
                    Ordering::Equal
                }
            }
        });
        sort_err?;

        let result_rows: Vec<Row> = items
            .into_iter()
            .skip(offset_val)
            .take(limit_val)
            .map(|item| item.row)
            .collect();

        let rows_output = result_rows.len();

        Ok((
            result_rows,
            TopNExecutionMetrics {
                rows_input,
                rows_output,
                candidate_capacity: capacity,
                rows_retained,
                rows_discarded,
                peak_memory_bytes: peak_memory,
                limit,
                offset,
                early_terminated: false,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::{Column, DataType, Value};
    use crate::query::expr::Expr;
    use crate::query::operator::SortDirection;

    #[test]
    fn test_top_n_unit_desc() {
        let schema = Schema::new(
            vec![
                Column::new("id", DataType::UInt64, false),
                Column::new("name", DataType::String, false),
                Column::new("age", DataType::Int64, true),
                Column::new("balance", DataType::Float64, true),
            ],
            0,
        )
        .unwrap();

        let rows = vec![
            Row::new(vec![
                Value::UInt64(1),
                Value::String("Alice".into()),
                Value::Int64(30),
                Value::Float64(100.0),
            ]),
            Row::new(vec![
                Value::UInt64(2),
                Value::String("Bob".into()),
                Value::Int64(20),
                Value::Float64(50.0),
            ]),
            Row::new(vec![
                Value::UInt64(4),
                Value::String("David".into()),
                Value::Null,
                Value::Float64(200.0),
            ]),
        ];

        let sort_keys = vec![SortKey {
            expression: Expr::Column("balance".into()),
            direction: SortDirection::Descending,
            nulls_first: None,
        }];

        let (res, metrics) = TopNOperator::apply_with_config(
            rows,
            &schema,
            &sort_keys,
            Some(2),
            None,
            &SortConfig::default(),
        )
        .unwrap();

        eprintln!("Result: {:?}", res);
        eprintln!("Metrics: {:?}", metrics);
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].values[0], Value::UInt64(4)); // David (200.0)
        assert_eq!(res[1].values[0], Value::UInt64(1)); // Alice (100.0)
    }
}
