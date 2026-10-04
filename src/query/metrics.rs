use std::fmt;
use std::time::Duration;

/// The functional classification of a physical execution operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorKind {
    /// Sequential or indexed table scan.
    TableScan {
        table_name: String,
        access_path: String,
    },
    /// In-memory or streaming tuple filtering.
    Filter,
    /// Output column projection.
    Project,
    /// In-memory sort.
    Sort,
    /// Offset skipping and limit truncation.
    LimitOffset,
    /// Nested-loop join.
    NestedLoopJoin { join_type: String },
    /// In-memory hash join.
    HashJoin { join_type: String },
    /// External spilled hash join.
    ExternalHashJoin { join_type: String },
    /// Secondary or primary index existence probe for SEMI join.
    IndexSemiProbe { index_name: String },
    /// Secondary or primary index existence probe for ANTI join.
    IndexAntiProbe { index_name: String },
    /// In-memory or streaming hash SEMI join.
    HashSemiJoin,
    /// In-memory or streaming hash ANTI join.
    HashAntiJoin,
    /// Nested-loop SEMI join with early match exit.
    NestedLoopSemiJoin,
    /// Nested-loop ANTI join with early match exit.
    NestedLoopAntiJoin,
    /// Group-by and aggregate computation.
    Aggregate,
    /// Cost-based ordered streaming aggregation.
    StreamingAggregate,
    /// Window function evaluation.
    Window,
    /// Subquery execution.
    Subquery { kind: String },
    /// Non-recursive Common Table Expression.
    Cte { name: String },
    /// Recursive Common Table Expression.
    RecursiveCte { name: String, iterations: usize },
    /// Relational set operation (UNION, INTERSECT, EXCEPT).
    SetOperation { op: String },
    /// Cost-based equality Merge Join.
    MergeJoin { join_type: String },
}

impl fmt::Display for OperatorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableScan {
                table_name,
                access_path,
            } => write!(f, "TableScan (table: {table_name}, access: {access_path})"),
            Self::Filter => write!(f, "Filter"),
            Self::Project => write!(f, "Project"),
            Self::Sort => write!(f, "Sort"),
            Self::LimitOffset => write!(f, "LimitOffset"),
            Self::NestedLoopJoin { join_type } => write!(f, "NestedLoopJoin ({join_type})"),
            Self::HashJoin { join_type } => write!(f, "HashJoin ({join_type})"),
            Self::ExternalHashJoin { join_type } => write!(f, "ExternalHashJoin ({join_type})"),
            Self::MergeJoin { join_type } => write!(f, "MergeJoin ({join_type})"),
            Self::IndexSemiProbe { index_name } => {
                write!(f, "IndexSemiProbe (index: {index_name})")
            }
            Self::IndexAntiProbe { index_name } => {
                write!(f, "IndexAntiProbe (index: {index_name})")
            }
            Self::HashSemiJoin => write!(f, "HashSemiJoin"),
            Self::HashAntiJoin => write!(f, "HashAntiJoin"),
            Self::NestedLoopSemiJoin => write!(f, "NestedLoopSemiJoin"),
            Self::NestedLoopAntiJoin => write!(f, "NestedLoopAntiJoin"),
            Self::Aggregate => write!(f, "Aggregate"),
            Self::StreamingAggregate => write!(f, "StreamingAggregate"),
            Self::Window => write!(f, "Window"),
            Self::Subquery { kind } => write!(f, "Subquery ({kind})"),
            Self::Cte { name } => write!(f, "CTE {name}"),
            Self::RecursiveCte { name, iterations } => {
                write!(f, "Recursive CTE {name} (iterations: {iterations})")
            }
            Self::SetOperation { op } => write!(f, "SetOperation ({op})"),
        }
    }
}

/// Detailed counters recorded for specific physical operators.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OperatorCounters {
    /// Number of storage pages read / accessed during execution.
    pub pages_read: usize,
    /// Number of secondary or primary B+ Tree index lookups performed.
    pub index_lookups: usize,
    /// Number of raw data tuples fetched from record storage.
    pub tuples_fetched: usize,
    /// Total rows evaluated by a filter or scan predicate.
    pub rows_evaluated: usize,
    /// Total rows passing filter predicate evaluation.
    pub rows_passed: usize,
    /// Total rows rejected by filter predicate evaluation.
    pub rows_rejected: usize,
    /// Total rows consumed from the left relation during join.
    pub rows_left: usize,
    /// Total rows consumed from the right relation during join.
    pub rows_right: usize,
    /// Candidate cross-product pairs examined during join evaluation.
    pub candidate_pairs: usize,
    /// Join pairs that satisfied the join condition.
    pub matched_rows: usize,
    /// Outer rows that found at least one match during SEMI join.
    pub matched_outer_rows: usize,
    /// Outer rows that had no qualifying match during ANTI join.
    pub unmatched_outer_rows: usize,
    /// Number of index existence probes performed during SEMI/ANTI join.
    pub index_probes: usize,
    /// Number of index existence probes that found a matching entry.
    pub successful_probes: usize,
    /// Number of index existence probes that found no matching entry.
    pub failed_probes: usize,
    /// Number of inner rows inserted into the hash table.
    pub hash_build_rows: usize,
    /// Number of outer rows probed against the hash table.
    pub hash_probe_rows: usize,
    /// Total rows output by this operator.
    pub rows_output: usize,
    /// Distinct group keys produced during aggregation.
    pub group_count: usize,
    /// Number of distinct spill partitions used during external aggregation or hash join.
    pub spill_partitions: usize,
    /// Number of temporary runs created during external sort, aggregation, or join spilling.
    pub runs_created: usize,
    /// Total bytes written/spilled to temporary files during external execution.
    pub bytes_spilled: usize,
    /// Number of merge passes executed during external sorting or aggregation.
    pub merge_passes: usize,
    /// Number of partition passes executed during external hash join.
    pub partition_passes: usize,
    /// Peak in-memory bytes used during sort, aggregation, or join operations.
    pub peak_memory_bytes: usize,
    /// Number of index entries examined during index scan or lookup.
    pub index_entries_examined: usize,
    /// Number of base table records fetched from RecordStore.
    pub base_records_fetched: usize,
    /// Number of base table record fetches avoided via covering index.
    pub base_records_avoided: usize,
    /// Number of covered rows output directly from index without base table access.
    pub covered_rows_output: usize,
    /// Number of distinct leading-prefix values discovered from the index during skip scan.
    pub distinct_prefix_values: usize,
    /// Number of point lookups / prefix range scans executed across distinct prefix values during skip scan.
    pub skip_scan_probes: usize,
    /// Number of participating secondary indexes in multi-index scan.
    pub indexes_used: usize,
    /// Candidate record count before set intersection or union.
    pub candidate_rows_before_set_operation: usize,
    /// Candidate record count surviving set intersection or union.
    pub candidate_rows_after_set_operation: usize,
    /// Number of candidate record IDs evaluated during intersection.
    pub record_ids_intersected: usize,
    /// Number of candidate record IDs combined during union.
    pub record_ids_unioned: usize,
    /// Number of duplicate record IDs eliminated during union deduplication.
    pub duplicate_record_ids_removed: usize,
    /// Whether a runtime join filter was actively constructed and evaluated.
    pub runtime_filter_enabled: bool,
    /// Semantic type of the active runtime filter (ExactSet or Bloom).
    pub runtime_filter_type: Option<String>,
    /// Number of build-side rows inserted into the runtime filter.
    pub runtime_filter_build_rows: usize,
    /// Number of distinct keys stored in the runtime filter.
    pub runtime_filter_distinct_keys: usize,
    /// In-memory byte footprint of the runtime filter.
    pub runtime_filter_memory_bytes: usize,
    /// Number of probe-side rows evaluated against the runtime filter.
    pub runtime_filter_probe_rows: usize,
    /// Number of probe-side rows rejected by the runtime filter before hash lookup.
    pub runtime_filter_rejected_rows: usize,
    /// Number of probe-side rows passing the runtime filter into hash lookup.
    pub runtime_filter_passed_rows: usize,
    /// Number of probe-side rows with NULL keys skipped before runtime filter evaluation.
    pub runtime_filter_null_probe_rows: usize,
    /// Number of false positives observed (passed filter but produced zero join hash matches).
    pub runtime_filter_false_positive_rows: usize,
    /// Merge join specific: Left rows consumed
    pub merge_join_left_rows: usize,
    /// Merge join specific: Right rows consumed
    pub merge_join_right_rows: usize,
    /// Merge join specific: Total key comparisons performed
    pub merge_join_comparisons: usize,
    /// Merge join specific: Total key matches
    pub merge_join_key_matches: usize,
    /// Merge join specific: Duplicate left rows encountered
    pub merge_join_duplicate_left_rows: usize,
    /// Merge join specific: Duplicate right rows encountered
    pub merge_join_duplicate_right_rows: usize,
    /// Merge join specific: Total rows emitted
    pub merge_join_output_rows: usize,
    /// Merge join specific: Unmatched left rows emitted
    pub merge_join_unmatched_left_rows: usize,
    /// Merge join specific: Unmatched right rows emitted
    pub merge_join_unmatched_right_rows: usize,
    /// Merge join specific: Left keys containing NULL
    pub merge_join_null_left_keys: usize,
    /// Merge join specific: Right keys containing NULL
    pub merge_join_null_right_keys: usize,
    /// Merge join specific: Residual predicate evaluations
    pub merge_join_residual_evaluations: usize,
    /// Merge join specific: Residual predicate rejections
    pub merge_join_residual_rejections: usize,
    /// Merge join specific: Whether left input required sort
    pub merge_join_sort_left: bool,
    /// Merge join specific: Whether right input required sort
    pub merge_join_sort_right: bool,
    /// Streaming aggregate specific: Input rows
    pub streaming_aggregate_input_rows: usize,
    /// Streaming aggregate specific: Output groups
    pub streaming_aggregate_output_groups: usize,
    /// Streaming aggregate specific: Group transitions
    pub streaming_aggregate_group_transitions: usize,
    /// Streaming aggregate specific: Maximum group size
    pub streaming_aggregate_max_group_size: usize,
    /// Streaming aggregate specific: Aggregate state updates
    pub streaming_aggregate_aggregate_updates: usize,
    /// Streaming aggregate specific: Sort required flag
    pub streaming_aggregate_sort_required: bool,
    /// Streaming aggregate specific: Sort spilled flag
    pub streaming_aggregate_sort_spilled: bool,
    /// Streaming aggregate specific: Estimated memory footprint
    pub streaming_aggregate_memory_bytes: usize,
}

/// Runtime execution metrics collected for an individual physical operator node.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorMetrics {
    /// Semantic kind and identity of this operator.
    pub kind: OperatorKind,
    /// Optimizer-estimated output row cardinality, if available.
    pub estimated_rows: Option<f64>,
    /// Optimizer-estimated cost, if available.
    pub cost: Option<f64>,
    /// Actual number of output rows emitted by this operator.
    pub actual_rows: usize,
    /// Actual number of input rows consumed by this operator.
    pub input_rows: usize,
    /// Number of times this operator was invoked or iterated.
    pub execution_count: usize,
    /// Wall-clock execution time elapsed inside this operator.
    pub execution_time: Duration,
    /// Detailed domain-specific execution counters.
    pub counters: OperatorCounters,
    /// Metrics for child operators in the physical execution tree.
    pub children: Vec<OperatorMetrics>,
}

impl OperatorMetrics {
    /// Creates a new `OperatorMetrics` node with default counters and duration.
    pub fn new(kind: OperatorKind, estimated_rows: Option<f64>, cost: Option<f64>) -> Self {
        Self {
            kind,
            estimated_rows,
            cost,
            actual_rows: 0,
            input_rows: 0,
            execution_count: 1,
            execution_time: Duration::ZERO,
            counters: OperatorCounters::default(),
            children: Vec::new(),
        }
    }

    /// Computes the ratio of actual output rows to estimated rows: `actual / estimated`.
    ///
    /// Returns `None` if estimated rows is missing or non-positive.
    pub fn estimate_error_ratio(&self) -> Option<f64> {
        estimate_error_ratio(self.actual_rows, self.estimated_rows)
    }
}

/// Computes the ratio of actual output rows to estimated rows: `actual / estimated`.
///
/// Returns `None` if estimated rows is missing or non-positive.
pub fn estimate_error_ratio(actual_rows: usize, estimated_rows: Option<f64>) -> Option<f64> {
    match estimated_rows {
        Some(est) if est > 0.0 => Some((actual_rows as f64) / est),
        _ => None,
    }
}

impl OperatorMetrics {
    /// Adds a child operator metric node.
    pub fn add_child(&mut self, child: OperatorMetrics) {
        self.children.push(child);
    }

    /// Finds the first operator metrics node matching the predicate in depth-first search order.
    pub fn find_first<F: Fn(&OperatorMetrics) -> bool>(
        &self,
        predicate: &F,
    ) -> Option<&OperatorMetrics> {
        if predicate(self) {
            return Some(self);
        }
        for child in &self.children {
            if let Some(found) = child.find_first(predicate) {
                return Some(found);
            }
        }
        None
    }
}

/// Aggregate metrics for a complete query execution.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryExecutionMetrics {
    /// Total wall-clock time required to execute the complete query.
    pub total_time: Duration,
    /// Total output rows returned to the client.
    pub total_rows: usize,
    /// Root operator metrics tree.
    pub root_operator: Option<OperatorMetrics>,
    /// Aggregate count of rows scanned across all relations.
    pub total_rows_scanned: usize,
    /// Aggregate count of index lookups performed.
    pub total_index_lookups: usize,
    /// Aggregate count of tuples fetched from record storage.
    pub total_tuples_fetched: usize,
    /// Whether the physical plan was retrieved from the plan cache.
    pub plan_cache_hit: bool,
}

impl QueryExecutionMetrics {
    /// Creates a new `QueryExecutionMetrics` container.
    pub fn new(
        total_time: Duration,
        total_rows: usize,
        root_operator: Option<OperatorMetrics>,
    ) -> Self {
        let mut total_rows_scanned = 0;
        let mut total_index_lookups = 0;
        let mut total_tuples_fetched = 0;

        if let Some(root) = &root_operator {
            Self::accumulate_totals(
                root,
                &mut total_rows_scanned,
                &mut total_index_lookups,
                &mut total_tuples_fetched,
            );
        }

        Self {
            total_time,
            total_rows,
            root_operator,
            total_rows_scanned,
            total_index_lookups,
            total_tuples_fetched,
            plan_cache_hit: false,
        }
    }

    /// Sets whether this query execution was a plan cache hit.
    pub fn with_plan_cache_hit(mut self, hit: bool) -> Self {
        self.plan_cache_hit = hit;
        self
    }

    fn accumulate_totals(
        node: &OperatorMetrics,
        scanned: &mut usize,
        lookups: &mut usize,
        fetched: &mut usize,
    ) {
        *scanned += node.counters.rows_evaluated;
        *lookups += node.counters.index_lookups;
        *fetched += node.counters.tuples_fetched;

        for child in &node.children {
            Self::accumulate_totals(child, scanned, lookups, fetched);
        }
    }

    /// Returns the estimate error ratio (`actual / estimated`) of the root operator, if available.
    pub fn root_estimate_error_ratio(&self) -> Option<f64> {
        self.root_operator
            .as_ref()
            .and_then(|r| r.estimate_error_ratio())
    }
}

/// Formats an execution plan with its corresponding runtime metrics.
pub fn format_plan_with_metrics(
    plan: &crate::query::optimizer::PhysicalPlan,
    metrics: &QueryExecutionMetrics,
) -> String {
    let mut out = String::new();
    let write_indent = |out: &mut String, lvl: usize| {
        for _ in 0..lvl {
            out.push_str("  ");
        }
    };

    let mut indent = 0;

    // 1. Format CTEs if present
    if !plan.ctes.is_empty() {
        for cte in &plan.ctes {
            write_indent(&mut out, indent);
            match &cte.kind {
                crate::sql::binder::BoundCteKind::NonRecursive { plan: cte_plan } => {
                    out.push_str(&format!("CTE {}\n", cte.name));
                    if !cte.dependencies.is_empty() {
                        write_indent(&mut out, indent + 1);
                        out.push_str(&format!("depends on: [{}]\n", cte.dependencies.join(", ")));
                    }
                    let plan_str = format!("{}", cte_plan);
                    for line in plan_str.lines() {
                        write_indent(&mut out, indent + 1);
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                crate::sql::binder::BoundCteKind::Recursive {
                    anchor_plan,
                    recursive_plan,
                    max_iterations,
                    max_rows,
                } => {
                    out.push_str(&format!("Recursive CTE {}\n", cte.name));
                    if !cte.dependencies.is_empty() {
                        write_indent(&mut out, indent + 1);
                        out.push_str(&format!("depends on: [{}]\n", cte.dependencies.join(", ")));
                    }
                    write_indent(&mut out, indent + 1);
                    out.push_str("Anchor\n");
                    let anchor_str = format!("{}", anchor_plan);
                    for line in anchor_str.lines() {
                        write_indent(&mut out, indent + 2);
                        out.push_str(line);
                        out.push('\n');
                    }
                    write_indent(&mut out, indent + 1);
                    out.push_str("Recursive Member\n");
                    let rec_str = format!("{}", recursive_plan);
                    for line in rec_str.lines() {
                        write_indent(&mut out, indent + 2);
                        out.push_str(line);
                        out.push('\n');
                    }
                    write_indent(&mut out, indent + 1);
                    out.push_str(&format!("Max Iterations: {max_iterations}\n"));
                    write_indent(&mut out, indent + 1);
                    out.push_str(&format!("Max Rows: {max_rows}\n"));
                }
            }
        }
        write_indent(&mut out, indent);
        out.push_str("Main Query\n");
        indent += 1;
    }

    // 2. Limit / Offset
    if plan.limit.is_some() || plan.offset.is_some() {
        write_indent(&mut out, indent);
        out.push_str(&format!(
            "LimitOffset (limit: {:?}, offset: {:?})\n",
            plan.limit, plan.offset
        ));
        indent += 1;
    }

    // 3. Projection
    if !plan.projections.is_empty() {
        write_indent(&mut out, indent);
        let projs: Vec<String> = plan
            .projections
            .iter()
            .map(|(e, a)| match a {
                Some(alias) => format!("{e} AS {alias}"),
                None => format!("{e}"),
            })
            .collect();
        out.push_str(&format!("Projection [{}]\n", projs.join(", ")));
        indent += 1;
    }

    // 4. Sort
    if !plan.order_by.is_empty() {
        write_indent(&mut out, indent);
        let keys: Vec<String> = plan
            .order_by
            .iter()
            .map(|k| format!("{} {:?}", k.expression, k.direction))
            .collect();
        out.push_str(&format!("Sort [{}]\n", keys.join(", ")));
        if let Some(root) = &metrics.root_operator {
            if let Some(sort_op) = find_operator_metrics(root, &|k| matches!(k, OperatorKind::Sort))
            {
                write_indent(&mut out, indent + 1);
                if sort_op.counters.runs_created > 0 {
                    out.push_str(&format!(
                        "actual_rows: {}, runs_created: {}, bytes_spilled: {}, merge_passes: {}\n",
                        sort_op.actual_rows,
                        sort_op.counters.runs_created,
                        sort_op.counters.bytes_spilled,
                        sort_op.counters.merge_passes
                    ));
                } else {
                    out.push_str(&format!("actual_rows: {}\n", sort_op.actual_rows));
                }
            }
        }
        indent += 1;
    }

    // 5. Set Operation
    if let Some(set_op) = &plan.set_op {
        write_indent(&mut out, indent);
        out.push_str(&format!("Set Operation: {}\n", set_op.op));
        write_indent(&mut out, indent + 1);
        out.push_str("Left\n");
        let left_str = format!("{}", set_op.left);
        for line in left_str.lines() {
            write_indent(&mut out, indent + 2);
            out.push_str(line);
            out.push('\n');
        }
        write_indent(&mut out, indent + 1);
        out.push_str("Right\n");
        let right_str = format!("{}", set_op.right);
        for line in right_str.lines() {
            write_indent(&mut out, indent + 2);
            out.push_str(line);
            out.push('\n');
        }
        return out;
    }

    // 6. Window
    if !plan.window_exprs.is_empty() {
        write_indent(&mut out, indent);
        out.push_str("Window\n");
        for we in &plan.window_exprs {
            write_indent(&mut out, indent + 1);
            out.push_str("Functions:\n");
            write_indent(&mut out, indent + 2);
            let args_str = if we.args.is_empty() {
                String::new()
            } else {
                let a: Vec<String> = we.args.iter().map(|e| e.to_string()).collect();
                a.join(", ")
            };
            out.push_str(&format!("{}({})\n", we.function, args_str));

            if !we.window_spec.partition_by.is_empty() {
                write_indent(&mut out, indent + 1);
                out.push_str("Partition By:\n");
                for p in &we.window_spec.partition_by {
                    write_indent(&mut out, indent + 2);
                    out.push_str(&format!("{p}\n"));
                }
            }

            if !we.window_spec.order_by.is_empty() {
                write_indent(&mut out, indent + 1);
                out.push_str("Order By:\n");
                for o in &we.window_spec.order_by {
                    write_indent(&mut out, indent + 2);
                    out.push_str(&format!("{} {:?}\n", o.expression, o.direction));
                }
            }

            write_indent(&mut out, indent + 1);
            out.push_str("Frame:\n");
            write_indent(&mut out, indent + 2);
            out.push_str(&format!(
                "{} -> {}\n",
                we.window_spec.frame.start, we.window_spec.frame.end
            ));
        }
        indent += 1;
    }

    // 7. Having
    if let Some(having) = &plan.having {
        write_indent(&mut out, indent);
        out.push_str(&format!("Having [{having}]\n"));
        indent += 1;
    }

    // 8. Aggregates
    if !plan.group_by.is_empty() || !plan.aggregates.is_empty() {
        write_indent(&mut out, indent);
        let groups: Vec<String> = plan.group_by.iter().map(|g| g.to_string()).collect();
        let aggs: Vec<String> = plan.aggregates.iter().map(|a| a.to_string()).collect();
        match plan.aggregate_strategy {
            Some(crate::query::optimizer::AggregateStrategy::Streaming { sort_required }) => {
                out.push_str(&format!(
                    "StreamingAggregate (group_by: [{}], aggs: [{proper_aggs}], sort_required: {})\n",
                    groups.join(", "),
                    if sort_required { "Yes" } else { "No" },
                    proper_aggs = aggs.join(", ")
                ));
            }
            Some(crate::query::optimizer::AggregateStrategy::External) => {
                out.push_str(&format!(
                    "ExternalAggregate (group_by: [{}], aggs: [{proper_aggs}])\n",
                    groups.join(", "),
                    proper_aggs = aggs.join(", ")
                ));
            }
            _ => {
                out.push_str(&format!(
                    "Aggregate (group_by: [{}], aggs: [{proper_aggs}])\n",
                    groups.join(", "),
                    proper_aggs = aggs.join(", ")
                ));
            }
        }
        if let Some(root) = &metrics.root_operator {
            if let Some(agg_op) = find_operator_metrics(root, &|k| {
                matches!(
                    k,
                    OperatorKind::Aggregate | OperatorKind::StreamingAggregate
                )
            }) {
                write_indent(&mut out, indent + 1);
                if matches!(agg_op.kind, OperatorKind::StreamingAggregate) {
                    out.push_str(&format!(
                        "actual_rows: {}, group_count: {}, input_rows: {}, group_transitions: {}, max_group_size: {}, aggregate_updates: {}, sort_required: {}, sort_spilled: {}, memory_bytes: {}\n",
                        agg_op.actual_rows,
                        agg_op.counters.group_count,
                        agg_op.counters.streaming_aggregate_input_rows,
                        agg_op.counters.streaming_aggregate_group_transitions,
                        agg_op.counters.streaming_aggregate_max_group_size,
                        agg_op.counters.streaming_aggregate_aggregate_updates,
                        if agg_op.counters.streaming_aggregate_sort_required { "Yes" } else { "No" },
                        if agg_op.counters.streaming_aggregate_sort_spilled { "Yes" } else { "No" },
                        agg_op.counters.streaming_aggregate_memory_bytes
                    ));
                } else if agg_op.counters.runs_created > 0 || agg_op.counters.bytes_spilled > 0 {
                    out.push_str(&format!(
                        "actual_rows: {}, group_count: {}, spill_partitions: {}, runs_created: {}, bytes_spilled: {}, merge_passes: {}, peak_memory_bytes: {}\n",
                        agg_op.actual_rows,
                        agg_op.counters.group_count,
                        agg_op.counters.spill_partitions,
                        agg_op.counters.runs_created,
                        agg_op.counters.bytes_spilled,
                        agg_op.counters.merge_passes,
                        agg_op.counters.peak_memory_bytes
                    ));
                } else {
                    out.push_str(&format!(
                        "actual_rows: {}, group_count: {}\n",
                        agg_op.actual_rows, agg_op.counters.group_count
                    ));
                }
            }
        }
        indent += 1;
    }

    // 9. Post-Join Filter
    if let Some(post_filter) = &plan.post_join_filter {
        write_indent(&mut out, indent);
        out.push_str(&format!("Filter [{post_filter}]\n"));
        indent += 1;
    }

    // 10. Table Scans and Joins with Metrics
    if plan.joins.is_empty() {
        write_indent(&mut out, indent);
        out.push_str(&format!(
            "TableScan (table: {}{}, access: {}{})\n",
            plan.base_table.table_name,
            plan.base_table
                .table_alias
                .as_ref()
                .map(|a| format!(" AS {a}"))
                .unwrap_or_default(),
            plan.base_table.access_path,
            plan.base_table
                .residual_filter
                .as_ref()
                .map(|rf| format!(", filter: [{rf}]"))
                .unwrap_or_default()
        ));
        write_indent(&mut out, indent + 1);
        let actual_rows_str = metrics.total_rows;
        if let Some(root) = &metrics.root_operator {
            if let Some(scan_op) =
                find_operator_metrics(root, &|k| matches!(k, OperatorKind::TableScan { .. }))
            {
                if scan_op.counters.indexes_used > 0 {
                    if scan_op.counters.runs_created > 0 {
                        out.push_str(&format!(
                            "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, indexes_used: {}, index_entries_examined: {}, candidate_rows_before_set_operation: {}, candidate_rows_after_set_operation: {}, record_ids_intersected: {}, record_ids_unioned: {}, duplicate_record_ids_removed: {}, base_records_fetched: {}, rows_output: {}, spill_runs_created: {}, spill_bytes_written: {}, candidate_rows_spilled: {}, peak_candidate_memory: {}\n",
                            plan.base_table.cost,
                            plan.base_table.estimated_rows,
                            actual_rows_str,
                            scan_op.counters.indexes_used,
                            scan_op.counters.index_entries_examined,
                            scan_op.counters.candidate_rows_before_set_operation,
                            scan_op.counters.candidate_rows_after_set_operation,
                            scan_op.counters.record_ids_intersected,
                            scan_op.counters.record_ids_unioned,
                            scan_op.counters.duplicate_record_ids_removed,
                            scan_op.counters.base_records_fetched,
                            actual_rows_str,
                            scan_op.counters.runs_created,
                            scan_op.counters.bytes_spilled,
                            scan_op.counters.candidate_rows_before_set_operation,
                            scan_op.counters.peak_memory_bytes,
                        ));
                    } else {
                        out.push_str(&format!(
                            "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, indexes_used: {}, index_entries_examined: {}, candidate_rows_before_set_operation: {}, candidate_rows_after_set_operation: {}, record_ids_intersected: {}, record_ids_unioned: {}, duplicate_record_ids_removed: {}, base_records_fetched: {}, rows_output: {}\n",
                            plan.base_table.cost,
                            plan.base_table.estimated_rows,
                            actual_rows_str,
                            scan_op.counters.indexes_used,
                            scan_op.counters.index_entries_examined,
                            scan_op.counters.candidate_rows_before_set_operation,
                            scan_op.counters.candidate_rows_after_set_operation,
                            scan_op.counters.record_ids_intersected,
                            scan_op.counters.record_ids_unioned,
                            scan_op.counters.duplicate_record_ids_removed,
                            scan_op.counters.base_records_fetched,
                            actual_rows_str
                        ));
                    }
                } else if scan_op.counters.skip_scan_probes > 0 {
                    out.push_str(&format!(
                        "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, distinct_prefix_values: {}, skip_scan_prefixes_discovered: {}, skip_scan_prefixes_probed: {}, skip_scan_index_entries_examined: {}, skip_scan_rows_output: {}, skip_scan_base_records_fetched: {}, skip_scan_base_records_avoided: {}, actual_prefix_values: {}, skip_scan_probes: {}, probes: {}, index_entries: {}, index_entries_examined: {}, rows: {}, rows_matched: {}, base_fetched: {}, base_records_fetched: {}, base_avoided: {}, base_records_avoided: {}, covered_rows_output: {}\n",
                        plan.base_table.cost,
                        plan.base_table.estimated_rows,
                        actual_rows_str,
                        scan_op.counters.distinct_prefix_values,
                        scan_op.counters.distinct_prefix_values,
                        scan_op.counters.skip_scan_probes,
                        scan_op.counters.index_entries_examined,
                        actual_rows_str,
                        scan_op.counters.base_records_fetched,
                        scan_op.counters.base_records_avoided,
                        scan_op.counters.distinct_prefix_values,
                        scan_op.counters.skip_scan_probes,
                        scan_op.counters.skip_scan_probes,
                        scan_op.counters.index_entries_examined,
                        scan_op.counters.index_entries_examined,
                        actual_rows_str,
                        actual_rows_str,
                        scan_op.counters.base_records_fetched,
                        scan_op.counters.base_records_fetched,
                        scan_op.counters.base_records_avoided,
                        scan_op.counters.base_records_avoided,
                        scan_op.counters.covered_rows_output
                    ));
                } else if scan_op.counters.base_records_avoided > 0
                    || scan_op.counters.index_entries_examined > 0
                {
                    out.push_str(&format!(
                        "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, index_entries_examined: {}, base_records_fetched: {}, base_records_avoided: {}, covered_rows_output: {}\n",
                        plan.base_table.cost,
                        plan.base_table.estimated_rows,
                        actual_rows_str,
                        scan_op.counters.index_entries_examined,
                        scan_op.counters.base_records_fetched,
                        scan_op.counters.base_records_avoided,
                        scan_op.counters.covered_rows_output
                    ));
                } else {
                    out.push_str(&format!(
                        "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}\n",
                        plan.base_table.cost, plan.base_table.estimated_rows, actual_rows_str
                    ));
                }
            } else {
                out.push_str(&format!(
                    "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}\n",
                    plan.base_table.cost, plan.base_table.estimated_rows, actual_rows_str
                ));
            }
        } else {
            out.push_str(&format!(
                "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}\n",
                plan.base_table.cost, plan.base_table.estimated_rows, actual_rows_str
            ));
        }
    } else {
        for (idx, join) in plan.joins.iter().enumerate() {
            write_indent(&mut out, indent);
            let is_semi_anti = join.join_type == crate::sql::ast::JoinType::Semi
                || join.join_type == crate::sql::ast::JoinType::Anti;
            if is_semi_anti {
                let base_title = match (join.join_type, join.strategy) {
                    (
                        crate::sql::ast::JoinType::Semi,
                        crate::query::optimizer::JoinStrategy::IndexProbe,
                    ) => "SemiJoin [IndexSemiProbe]",
                    (
                        crate::sql::ast::JoinType::Semi,
                        crate::query::optimizer::JoinStrategy::Hash,
                    ) => "SemiJoin [HashSemiJoin]",
                    (
                        crate::sql::ast::JoinType::Semi,
                        crate::query::optimizer::JoinStrategy::NestedLoop,
                    ) => "SemiJoin [NestedLoopSemiJoin]",
                    (
                        crate::sql::ast::JoinType::Anti,
                        crate::query::optimizer::JoinStrategy::IndexProbe,
                    ) => "AntiJoin [IndexAntiProbe]",
                    (
                        crate::sql::ast::JoinType::Anti,
                        crate::query::optimizer::JoinStrategy::Hash,
                    ) => "AntiJoin [HashAntiJoin]",
                    (
                        crate::sql::ast::JoinType::Anti,
                        crate::query::optimizer::JoinStrategy::NestedLoop,
                    ) => "AntiJoin [NestedLoopAntiJoin]",
                    _ => "Join",
                };
                out.push_str(&format!(
                    "{base_title} (on: [{proper_on}])\n",
                    proper_on = join.on
                ));
                write_indent(&mut out, indent + 1);
                out.push_str(&format!("strategy={:?}\n", join.strategy));
                if let Some(idx_name) = &join.index_name {
                    write_indent(&mut out, indent + 1);
                    out.push_str(&format!("index={idx_name}\n"));
                }
            } else if join.strategy == crate::query::optimizer::JoinStrategy::Merge {
                out.push_str(&format!(
                    "MergeJoin ({:?}, on: [{proper_on}])\n",
                    join.join_type,
                    proper_on = join.on
                ));
            } else {
                out.push_str(&format!(
                    "NestedLoopJoin ({:?}, on: [{proper_on}])\n",
                    join.join_type,
                    proper_on = join.on
                ));
            }
            write_indent(&mut out, indent + 1);
            if let Some(root) = &metrics.root_operator {
                if let Some(jm) = find_operator_metrics(root, &|k| {
                    matches!(
                        k,
                        OperatorKind::NestedLoopJoin { .. }
                            | OperatorKind::HashJoin { .. }
                            | OperatorKind::ExternalHashJoin { .. }
                            | OperatorKind::MergeJoin { .. }
                            | OperatorKind::IndexSemiProbe { .. }
                            | OperatorKind::IndexAntiProbe { .. }
                            | OperatorKind::HashSemiJoin
                            | OperatorKind::HashAntiJoin
                            | OperatorKind::NestedLoopSemiJoin
                            | OperatorKind::NestedLoopAntiJoin
                    )
                }) {
                    if is_semi_anti {
                        out.push_str(&format!(
                            "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, outer_rows: {}, inner_rows: {}, matched_outer_rows: {}, unmatched_outer_rows: {}, candidate_pairs: {}, rows_output: {}, index_probes: {}, successful_probes: {}, failed_probes: {}, hash_build_rows: {}, hash_probe_rows: {}, bytes_spilled: {}, runs_created: {}\n",
                            join.cost,
                            join.estimated_rows,
                            metrics.total_rows,
                            jm.counters.rows_left,
                            jm.counters.rows_right,
                            jm.counters.matched_outer_rows,
                            jm.counters.unmatched_outer_rows,
                            jm.counters.candidate_pairs,
                            jm.actual_rows,
                            jm.counters.index_probes,
                            jm.counters.successful_probes,
                            jm.counters.failed_probes,
                            jm.counters.hash_build_rows,
                            jm.counters.hash_probe_rows,
                            jm.counters.bytes_spilled,
                            jm.counters.runs_created
                        ));
                    } else if matches!(jm.kind, OperatorKind::MergeJoin { .. }) {
                        out.push_str(&format!(
                            "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, left_rows: {}, right_rows: {}, comparisons: {}, key_matches: {}, duplicate_left: {}, duplicate_right: {}, unmatched_left: {}, unmatched_right: {}, null_left_keys: {}, null_right_keys: {}, residual_evals: {}, residual_rejections: {}, sort_left: {}, sort_right: {}\n",
                            join.cost,
                            join.estimated_rows,
                            metrics.total_rows,
                            jm.counters.merge_join_left_rows,
                            jm.counters.merge_join_right_rows,
                            jm.counters.merge_join_comparisons,
                            jm.counters.merge_join_key_matches,
                            jm.counters.merge_join_duplicate_left_rows,
                            jm.counters.merge_join_duplicate_right_rows,
                            jm.counters.merge_join_unmatched_left_rows,
                            jm.counters.merge_join_unmatched_right_rows,
                            jm.counters.merge_join_null_left_keys,
                            jm.counters.merge_join_null_right_keys,
                            jm.counters.merge_join_residual_evaluations,
                            jm.counters.merge_join_residual_rejections,
                            jm.counters.merge_join_sort_left,
                            jm.counters.merge_join_sort_right,
                        ));
                    } else if jm.counters.runs_created > 0 || jm.counters.bytes_spilled > 0 {
                        out.push_str(&format!(
                            "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}, runs_created: {}, bytes_spilled: {}, partition_passes: {}, peak_memory_bytes: {}\n",
                            join.cost,
                            join.estimated_rows,
                            metrics.total_rows,
                            jm.counters.runs_created,
                            jm.counters.bytes_spilled,
                            jm.counters.partition_passes,
                            jm.counters.peak_memory_bytes
                        ));
                    } else {
                        out.push_str(&format!(
                            "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}\n",
                            join.cost, join.estimated_rows, metrics.total_rows
                        ));
                    }
                    if jm.counters.runtime_filter_enabled {
                        write_indent(&mut out, indent + 1);
                        let rf_type = jm
                            .counters
                            .runtime_filter_type
                            .as_deref()
                            .unwrap_or("ExactSet");
                        out.push_str(&format!(
                            "Runtime Filter: type: {}, build_rows: {}, distinct_keys: {}, memory_bytes: {}, probe_rows: {}, rejected_rows: {}, passed_rows: {}, null_probe_rows: {}, false_positive_rows: {}\n",
                            rf_type,
                            jm.counters.runtime_filter_build_rows,
                            jm.counters.runtime_filter_distinct_keys,
                            jm.counters.runtime_filter_memory_bytes,
                            jm.counters.runtime_filter_probe_rows,
                            jm.counters.runtime_filter_rejected_rows,
                            jm.counters.runtime_filter_passed_rows,
                            jm.counters.runtime_filter_null_probe_rows,
                            jm.counters.runtime_filter_false_positive_rows
                        ));
                    }
                } else {
                    out.push_str(&format!(
                        "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}\n",
                        join.cost, join.estimated_rows, metrics.total_rows
                    ));
                }
            } else {
                out.push_str(&format!(
                    "cost: {:.2}, estimated_rows: {:.0}, actual_rows: {}\n",
                    join.cost, join.estimated_rows, metrics.total_rows
                ));
            }
            indent += 1;

            if idx == 0 {
                write_indent(&mut out, indent);
                out.push_str(&format!(
                    "Left: TableScan (table: {}{}, access: {}{})\n",
                    plan.base_table.table_name,
                    plan.base_table
                        .table_alias
                        .as_ref()
                        .map(|a| format!(" AS {a}"))
                        .unwrap_or_default(),
                    plan.base_table.access_path,
                    plan.base_table
                        .residual_filter
                        .as_ref()
                        .map(|rf| format!(", filter: [{rf}]"))
                        .unwrap_or_default()
                ));
                write_indent(&mut out, indent + 1);
                out.push_str(&format!(
                    "cost: {:.2}, estimated_rows: {:.0}\n",
                    plan.base_table.cost, plan.base_table.estimated_rows
                ));
            }

            write_indent(&mut out, indent);
            out.push_str(&format!(
                "Right: TableScan (table: {}{}, access: {}{})\n",
                join.right.table_name,
                join.right
                    .table_alias
                    .as_ref()
                    .map(|a| format!(" AS {a}"))
                    .unwrap_or_default(),
                join.right.access_path,
                join.right
                    .residual_filter
                    .as_ref()
                    .map(|rf| format!(", filter: [{rf}]"))
                    .unwrap_or_default()
            ));
            write_indent(&mut out, indent + 1);
            out.push_str(&format!(
                "cost: {:.2}, estimated_rows: {:.0}\n",
                join.right.cost, join.right.estimated_rows
            ));
        }
    }

    out
}

/// Recursively searches an operator metrics tree for the first node matching `predicate`.
pub fn find_operator_metrics<'a, F>(
    node: &'a OperatorMetrics,
    predicate: &F,
) -> Option<&'a OperatorMetrics>
where
    F: Fn(&OperatorKind) -> bool,
{
    if predicate(&node.kind) {
        return Some(node);
    }
    for child in &node.children {
        if let Some(m) = find_operator_metrics(child, predicate) {
            return Some(m);
        }
    }
    None
}
