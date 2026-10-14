use std::fmt;

use crate::engine::types::Value;
use crate::query::expr::{AggregateExpr, Expr, SubqueryKind, WindowExpr};
use crate::query::external_multi_index::MultiIndexExecutionMode;
use crate::query::operator::SortDirection;
use crate::query::optimizer::{
    AccessPath, AggregateStrategy, MultiIndexStrategy, PhysicalJoin, PhysicalPlan, PhysicalSetOp,
    PhysicalTableScan, SecondaryScanType,
};
use crate::query::physical_properties::{OrderingProperty, RowGoal};
use crate::query::plan_cache::ResourcePlanningSignature;
use crate::query::SortKey;
use crate::sql::binder::{BoundCte, BoundCteKind};

/// Semantic version of the physical plan canonicalization and fingerprinting format.
pub const PHYSICAL_PLAN_FINGERPRINT_VERSION: u32 = 1;

/// Deterministic 64-bit FNV-1a hash of a byte sequence.
pub const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

/// Deterministic 128-bit FNV-1a hash of a byte sequence for high collision resistance.
pub const fn fnv1a_128(bytes: &[u8]) -> u128 {
    let mut hash: u128 = 0x6c62272e07bb014262b821756295c58d;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u128;
        hash = hash.wrapping_mul(0x0000000001000000000000000000013b);
        i += 1;
    }
    hash
}

/// Strongly typed, deterministic physical plan fingerprint representing execution strategy and semantics.
///
/// Encapsulates:
/// - Semantic version of the canonical plan encoder.
/// - 128-bit collision-resistant digest for fast comparisons and map keys.
/// - The complete canonical representation string for diagnostics, logging, and collision proof.
#[derive(Clone, Eq)]
pub struct PhysicalPlanFingerprint {
    version: u32,
    digest: u128,
    canonical: String,
}

impl PhysicalPlanFingerprint {
    /// Computes the canonical physical plan fingerprint from a physical plan.
    pub fn from_plan(plan: &PhysicalPlan) -> Self {
        Self::from_plan_with_resources(plan, None)
    }

    /// Computes the canonical physical plan fingerprint with optional resource configuration.
    pub fn from_plan_with_resources(
        plan: &PhysicalPlan,
        resources: Option<&ResourcePlanningSignature>,
    ) -> Self {
        let canonical = canonicalize_plan(plan, resources);
        let digest = fnv1a_128(canonical.as_bytes());
        Self {
            version: PHYSICAL_PLAN_FINGERPRINT_VERSION,
            digest,
            canonical,
        }
    }

    /// Constructs a fingerprint directly from a pre-computed canonical representation.
    pub fn from_canonical(canonical: String) -> Self {
        let digest = fnv1a_128(canonical.as_bytes());
        Self {
            version: PHYSICAL_PLAN_FINGERPRINT_VERSION,
            digest,
            canonical,
        }
    }

    /// Returns the semantic encoder version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Returns the 128-bit integer digest.
    pub fn digest(&self) -> u128 {
        self.digest
    }

    /// Returns a deterministic 32-character hexadecimal representation of this fingerprint's digest.
    pub fn to_hex(&self) -> String {
        format!("{:032x}", self.digest)
    }

    /// Returns the underlying canonical serialized representation string.
    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    /// Returns the underlying canonical serialized representation string.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }
}

impl PartialEq for PhysicalPlanFingerprint {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.digest == other.digest
            && self.canonical == other.canonical
    }
}

impl std::hash::Hash for PhysicalPlanFingerprint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.version.hash(state);
        self.digest.hash(state);
    }
}

impl PartialOrd for PhysicalPlanFingerprint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PhysicalPlanFingerprint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.version
            .cmp(&other.version)
            .then_with(|| self.digest.cmp(&other.digest))
            .then_with(|| self.canonical.cmp(&other.canonical))
    }
}

impl fmt::Display for PhysicalPlanFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.digest)
    }
}

impl fmt::Debug for PhysicalPlanFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "PhysicalPlanFingerprint(v={}, {:032x})",
            self.version, self.digest
        )
    }
}

// ============================================================================
// Canonical Serializer Implementation
// ============================================================================

/// Produces the complete, deterministic canonical representation of a physical plan.
pub fn canonicalize_plan(
    plan: &PhysicalPlan,
    resources: Option<&ResourcePlanningSignature>,
) -> String {
    let mut out = String::with_capacity(512);

    // 1. Version header
    use std::fmt::Write;
    let _ = write!(out, "V={};", PHYSICAL_PLAN_FINGERPRINT_VERSION);

    // 2. Resource configuration if specified
    if let Some(res) = resources {
        canonicalize_resource_signature(res, &mut out);
        out.push(';');
    }

    // 3. CTEs in declaration order
    if !plan.ctes.is_empty() {
        out.push_str("CTES=[");
        for (i, cte) in plan.ctes.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_cte(cte, &mut out);
        }
        out.push_str("];");
    }

    // 4. Base table scan
    out.push_str("BASE=");
    canonicalize_table_scan(&plan.base_table, &mut out);

    // 5. Joins in execution sequence
    if !plan.joins.is_empty() {
        out.push_str(";JOINS=[");
        for (i, j) in plan.joins.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_join(j, &mut out);
        }
        out.push(']');
    }

    // 6. Post-join filter
    if let Some(pjf) = &plan.post_join_filter {
        out.push_str(";POST_JOIN_FILTER=");
        canonicalize_expr(pjf, &mut out);
    }

    // 7. Group By & Aggregation
    if !plan.group_by.is_empty() || !plan.aggregates.is_empty() || plan.aggregate_strategy.is_some()
    {
        out.push_str(";AGG(strat=");
        match plan.aggregate_strategy {
            Some(AggregateStrategy::Hash) => out.push_str("Hash"),
            Some(AggregateStrategy::Streaming { sort_required }) => {
                let _ = write!(out, "Streaming(sort={})", sort_required);
            }
            Some(AggregateStrategy::External) => out.push_str("External"),
            Some(AggregateStrategy::Index) => out.push_str("Index"),
            None => out.push_str("None"),
        }
        out.push_str(",grp=[");
        for (i, g) in plan.group_by.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_expr(g, &mut out);
        }
        out.push_str("],aggs=[");
        for (i, a) in plan.aggregates.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_aggregate_expr(a, &mut out);
        }
        out.push_str("])");
    }

    // 8. Having filter
    if let Some(h) = &plan.having {
        out.push_str(";HAVING=");
        canonicalize_expr(h, &mut out);
    }

    // 9. Window expressions
    if !plan.window_exprs.is_empty() {
        out.push_str(";WINDOWS=[");
        for (i, w) in plan.window_exprs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_window_expr(w, &mut out);
        }
        out.push(']');
    }

    // 10. Projections in strict output sequence
    if !plan.projections.is_empty() {
        out.push_str(";PROJ=[");
        for (i, (expr, alias)) in plan.projections.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_expr(expr, &mut out);
            if let Some(a) = alias {
                out.push_str(" AS ");
                out.push_str(a);
            }
        }
        out.push(']');
    }

    // 11. Order By & Sort
    if !plan.order_by.is_empty() {
        let _ = write!(
            out,
            ";ORDER_BY(sort={},top_n={},keys=[",
            plan.order_by_sort_required, plan.order_by_top_n
        );
        for (i, sk) in plan.order_by.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_sort_key(sk, &mut out);
        }
        out.push_str("])");
    }

    // 12. Guaranteed physical ordering
    if let Some(po) = &plan.provided_ordering {
        out.push_str(";PROVIDED_ORD=");
        canonicalize_ordering_property(po, &mut out);
    }

    // 13. Pagination & Row goals
    if let Some(off) = plan.offset {
        let _ = write!(out, ";OFFSET={}", off);
    }
    if let Some(lim) = plan.limit {
        let _ = write!(out, ";LIMIT={}", lim);
    }
    if let Some(rg) = &plan.row_goal {
        out.push_str(";ROW_GOAL=");
        canonicalize_row_goal(rg, &mut out);
    }
    if plan.row_goal_early_termination {
        out.push_str(";EARLY_TERM=true");
    }

    // 14. Relational Set Operation
    if let Some(so) = &plan.set_op {
        out.push_str(";SET_OP=");
        canonicalize_set_op(so, &mut out);
    }

    out
}

pub fn canonicalize_table_scan(scan: &PhysicalTableScan, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(out, "Scan(table={}", scan.table_name);
    if let Some(alias) = &scan.table_alias {
        let _ = write!(out, ",alias={}", alias);
    }
    out.push_str(",access=");
    canonicalize_access_path(&scan.access_path, out);

    // Deterministically sorted column sets (set semantics)
    if !scan.required_columns.is_empty() {
        let mut sorted_req: Vec<String> = scan
            .required_columns
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        sorted_req.sort();
        sorted_req.dedup();
        out.push_str(",req_cols=[");
        out.push_str(&sorted_req.join(","));
        out.push(']');
    }
    if !scan.covered_columns.is_empty() {
        let mut sorted_cov: Vec<String> = scan
            .covered_columns
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        sorted_cov.sort();
        sorted_cov.dedup();
        out.push_str(",cov_cols=[");
        out.push_str(&sorted_cov.join(","));
        out.push(']');
    }
    if !scan.pruned_columns.is_empty() {
        let mut sorted_pruned: Vec<String> = scan
            .pruned_columns
            .iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        sorted_pruned.sort();
        sorted_pruned.dedup();
        out.push_str(",pruned_cols=[");
        out.push_str(&sorted_pruned.join(","));
        out.push(']');
    }

    if scan.late_materialization {
        out.push_str(",late_mat=true");
    }
    if let Some(icf) = &scan.index_covered_filter {
        out.push_str(",idx_cov_filter=");
        canonicalize_expr(icf, out);
    }
    if let Some(brf) = &scan.base_record_filter {
        out.push_str(",base_rec_filter=");
        canonicalize_expr(brf, out);
    }
    if let Some(res) = &scan.residual_filter {
        out.push_str(",residual=");
        canonicalize_expr(res, out);
    }
    if let Some(rg) = &scan.row_goal {
        out.push_str(",row_goal=");
        canonicalize_row_goal(rg, out);
    }
    if scan.early_termination_possible {
        out.push_str(",early_term=true");
    }
    if scan.scan_direction == crate::query::operator::SortDirection::Descending {
        out.push_str(",direction=desc");
    }

    out.push(')');
}

pub fn canonicalize_access_path(path: &AccessPath, out: &mut String) {
    use std::fmt::Write;
    match path {
        AccessPath::FullTableScan => out.push_str("FullTableScan"),
        AccessPath::PrimaryPointLookup { key } => {
            out.push_str("PrimaryPointLookup(key=");
            canonicalize_value(key, out);
            out.push(')');
        }
        AccessPath::PrimaryRangeScan { start, end } => {
            out.push_str("PrimaryRangeScan(start=");
            match start {
                Some(v) => canonicalize_value(v, out),
                None => out.push_str("None"),
            }
            out.push_str(",end=");
            match end {
                Some(v) => canonicalize_value(v, out),
                None => out.push_str("None"),
            }
            out.push(')');
        }
        AccessPath::SecondaryIndexScan {
            index_name,
            index_id,
            scan_type,
        } => {
            let _ = write!(
                out,
                "SecondaryIndex(idx={},id={},scan=",
                index_name, index_id.0
            );
            canonicalize_secondary_scan_type(scan_type, out);
            out.push(')');
        }
        AccessPath::IndexOnlyScan {
            index_name,
            index_id,
            scan_type,
            covered_columns,
        } => {
            let mut sorted_cov: Vec<String> = covered_columns
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            sorted_cov.sort();
            sorted_cov.dedup();
            let _ = write!(
                out,
                "IndexOnly(idx={},id={},covered=[{}],scan=",
                index_name,
                index_id.0,
                sorted_cov.join(",")
            );
            canonicalize_secondary_scan_type(scan_type, out);
            out.push(')');
        }
        AccessPath::MultiIndexScan {
            strategy,
            child_scans,
            execution_mode,
        } => {
            let strat_str = match strategy {
                MultiIndexStrategy::Intersection => "Intersection",
                MultiIndexStrategy::Union => "Union",
            };
            let mode_str = match execution_mode {
                MultiIndexExecutionMode::InMemory => "InMemory",
                MultiIndexExecutionMode::External => "External",
            };
            let _ = write!(
                out,
                "MultiIndex(strat={},mode={},children=[",
                strat_str, mode_str
            );
            // Canonicalize and sort child scans deterministically for set intersection/union commutativity
            let mut child_reprs: Vec<String> = child_scans
                .iter()
                .map(|cs| {
                    let mut s = String::new();
                    let _ = write!(s, "Child(idx={},id={},scan=", cs.index_name, cs.index_id.0);
                    canonicalize_secondary_scan_type(&cs.scan_type, &mut s);
                    s.push(')');
                    s
                })
                .collect();
            child_reprs.sort();
            out.push_str(&child_reprs.join(","));
            out.push_str("])");
        }
        AccessPath::EmptyResult => out.push_str("EmptyResult"),
    }
}

pub fn canonicalize_secondary_scan_type(st: &SecondaryScanType, out: &mut String) {
    use std::fmt::Write;
    match st {
        SecondaryScanType::PointLookup(val) => {
            out.push_str("PointLookup(");
            canonicalize_value(val, out);
            out.push(')');
        }
        SecondaryScanType::CompositePointLookup(vals) => {
            out.push_str("CompositePointLookup([");
            for (i, v) in vals.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonicalize_value(v, out);
            }
            out.push_str("])");
        }
        SecondaryScanType::PrefixScan(vals) => {
            out.push_str("PrefixScan([");
            for (i, v) in vals.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonicalize_value(v, out);
            }
            out.push_str("])");
        }
        SecondaryScanType::RangeScan { start, end } => {
            out.push_str("RangeScan(start=");
            match start {
                Some(v) => canonicalize_value(v, out),
                None => out.push_str("None"),
            }
            out.push_str(",end=");
            match end {
                Some(v) => canonicalize_value(v, out),
                None => out.push_str("None"),
            }
            out.push(')');
        }
        SecondaryScanType::SkipScan {
            skipped_prefix_columns,
            constrained_columns,
        } => {
            let _ = write!(
                out,
                "SkipScan(skipped=[{}],constrained=[",
                skipped_prefix_columns.join(",")
            );
            // Sort constrained columns deterministically
            let mut constr_reprs: Vec<String> = constrained_columns
                .iter()
                .map(|(col, val)| {
                    let mut s = format!("{}=", col);
                    canonicalize_value(val, &mut s);
                    s
                })
                .collect();
            constr_reprs.sort();
            out.push_str(&constr_reprs.join(","));
            out.push_str("])");
        }
    }
}

pub fn canonicalize_join(join: &PhysicalJoin, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "Join(type={:?},strat={:?}",
        join.join_type, join.strategy
    );
    if let Some(idx) = &join.index_name {
        let _ = write!(out, ",idx={}", idx);
    }
    out.push_str(",on=");
    canonicalize_expr(&join.on, out);

    out.push_str(",right=");
    canonicalize_table_scan(&join.right, out);

    let _ = write!(out, ",lsort={},rsort={}", join.left_sort, join.right_sort);

    if let Some(rg) = &join.row_goal {
        out.push_str(",row_goal=");
        canonicalize_row_goal(rg, out);
    }
    if join.early_termination_possible {
        out.push_str(",early_term=true");
    }
    if let Some(phase) = &join.early_stop_phase {
        let _ = write!(out, ",phase={}", phase);
    }
    if let Some(rf) = &join.runtime_filter {
        let _ = write!(
            out,
            ",rf=RF(type={},scope={},keys=[",
            rf.filter_type, rf.scope
        );
        for (i, k) in rf.key_exprs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_expr(k, out);
        }
        out.push_str("])");
    }
    out.push(')');
}

pub fn canonicalize_expr(expr: &Expr, out: &mut String) {
    use std::fmt::Write;
    match expr {
        Expr::Literal(val) => {
            out.push_str("LIT(");
            canonicalize_value(val, out);
            out.push(')');
        }
        Expr::Column(name) => {
            let _ = write!(out, "COL({})", name);
        }
        Expr::OuterColumn {
            name,
            data_type,
            scope_depth,
        } => {
            let _ = write!(
                out,
                "OUTER_COL(name={},type={:?},depth={})",
                name, data_type, scope_depth
            );
        }
        Expr::Parameter { index, .. } => {
            // Parameter position is preserved; runtime parameter value is strictly excluded
            let _ = write!(out, "PARAM(${})", index);
        }
        Expr::Binary { left, op, right } => {
            out.push_str("BIN(");
            canonicalize_expr(left, out);
            let _ = write!(out, ",{:?},", op);
            canonicalize_expr(right, out);
            out.push(')');
        }
        Expr::Unary { op, expr } => {
            let _ = write!(out, "UN({:?},", op);
            canonicalize_expr(expr, out);
            out.push(')');
        }
        Expr::IsNull(e) => {
            out.push_str("IS_NULL(");
            canonicalize_expr(e, out);
            out.push(')');
        }
        Expr::IsNotNull(e) => {
            out.push_str("IS_NOT_NULL(");
            canonicalize_expr(e, out);
            out.push(')');
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let _ = write!(out, "IN_LIST(neg={},expr=", negated);
            canonicalize_expr(expr, out);
            out.push_str(",items=[");
            for (i, item) in list.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonicalize_expr(item, out);
            }
            out.push_str("])");
        }
        Expr::Subquery(sub) => {
            out.push_str("SUBQUERY(kind=");
            match &sub.kind {
                SubqueryKind::Scalar { data_type } => {
                    let _ = write!(out, "Scalar({:?})", data_type);
                }
                SubqueryKind::In { expr, negated } => {
                    let _ = write!(out, "In(neg={},expr=", negated);
                    canonicalize_expr(expr, out);
                    out.push(')');
                }
                SubqueryKind::Exists { negated } => {
                    let _ = write!(out, "Exists(neg={})", negated);
                }
            }
            let _ = write!(out, ",corr={}", sub.is_correlated);
            out.push_str(",plan=");
            out.push_str(&canonicalize_plan(&sub.plan, None));
            out.push(')');
        }
    }
}

pub fn canonicalize_value(val: &Value, out: &mut String) {
    use std::fmt::Write;
    match val {
        Value::Null => out.push_str("null"),
        Value::Int64(i) => {
            let _ = write!(out, "i64({})", i);
        }
        Value::UInt64(u) => {
            let _ = write!(out, "u64({})", u);
        }
        Value::Float64(f) => {
            canonicalize_float(*f, out);
        }
        Value::String(s) => {
            canonicalize_string(s, out);
        }
        Value::Bytes(b) => {
            out.push_str("bytes(");
            for byte in b {
                let _ = write!(out, "{:02x}", byte);
            }
            out.push(')');
        }
        Value::Boolean(b) => {
            let _ = write!(out, "bool({})", b);
        }
    }
}

pub fn canonicalize_float(f: f64, out: &mut String) {
    use std::fmt::Write;
    if f.is_nan() {
        out.push_str("f64(NaN)");
    } else if f.is_infinite() {
        if f.is_sign_negative() {
            out.push_str("f64(-Inf)");
        } else {
            out.push_str("f64(+Inf)");
        }
    } else if f == 0.0 {
        if f.is_sign_negative() {
            out.push_str("f64(-0.0)");
        } else {
            out.push_str("f64(+0.0)");
        }
    } else {
        // Output stable 16-hex bit pattern for deterministic portable comparison
        let _ = write!(out, "f64({:016x})", f.to_bits());
    }
}

pub fn canonicalize_string(s: &str, out: &mut String) {
    out.push_str("str(\"");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push_str("\")");
}

pub fn canonicalize_aggregate_expr(agg: &AggregateExpr, out: &mut String) {
    match agg {
        AggregateExpr::CountAll => out.push_str("COUNT(*)"),
        AggregateExpr::Function { func, expr } => {
            use std::fmt::Write;
            let _ = write!(out, "{:?}(", func);
            canonicalize_expr(expr, out);
            out.push(')');
        }
    }
}

pub fn canonicalize_window_expr(win: &WindowExpr, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(out, "WIN(func={:?}", win.function);
    if !win.args.is_empty() {
        out.push_str(",args=[");
        for (i, a) in win.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            canonicalize_expr(a, out);
        }
        out.push(']');
    }
    out.push_str(",part=[");
    for (i, p) in win.window_spec.partition_by.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        canonicalize_expr(p, out);
    }
    out.push_str("],order=[");
    for (i, o) in win.window_spec.order_by.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        canonicalize_sort_key(o, out);
    }
    out.push_str("],frame=");
    let _ = write!(
        out,
        "ROWS_BETWEEN_{}_{}",
        win.window_spec.frame.start, win.window_spec.frame.end
    );
    if let Some(off) = win.offset {
        let _ = write!(out, ",offset={}", off);
    }
    if let Some(def) = &win.default {
        out.push_str(",default=");
        canonicalize_expr(def, out);
    }
    out.push(')');
}

pub fn canonicalize_sort_key(sk: &SortKey, out: &mut String) {
    use std::fmt::Write;
    canonicalize_expr(&sk.expression, out);
    let nulls_first = sk
        .nulls_first
        .unwrap_or(sk.direction == SortDirection::Ascending);
    let _ = write!(
        out,
        " {:?} NULLS_{}",
        sk.direction,
        if nulls_first { "FIRST" } else { "LAST" }
    );
}

pub fn canonicalize_ordering_property(ord: &OrderingProperty, out: &mut String) {
    use std::fmt::Write;
    out.push('[');
    for (i, c) in ord.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{} {:?} NULLS_{}",
            c.column,
            c.direction,
            if c.nulls_first { "FIRST" } else { "LAST" }
        );
    }
    out.push(']');
}

pub fn canonicalize_row_goal(rg: &RowGoal, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "RowGoal(cand={},req={},off={})",
        rg.candidate_rows, rg.required_output_rows, rg.offset
    );
}

pub fn canonicalize_set_op(so: &PhysicalSetOp, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(out, "SetOp(op={:?},schema=[", so.op);
    for (i, col) in so.schema.columns.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{:?}", col.data_type);
    }
    out.push_str("],left=");
    out.push_str(&canonicalize_plan(&so.left, None));
    out.push_str(",right=");
    out.push_str(&canonicalize_plan(&so.right, None));
    out.push(')');
}

pub fn canonicalize_cte(cte: &BoundCte, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "CTE(name={},deps=[{}],kind=",
        cte.name,
        cte.dependencies.join(",")
    );
    match &cte.kind {
        BoundCteKind::NonRecursive {
            plan,
            decision,
            materialize_id,
            ..
        } => {
            let _ = write!(
                out,
                "NonRecursive(decision={:?},mat_id={:?},plan=",
                decision, materialize_id
            );
            out.push_str(&canonicalize_plan(plan, None));
            out.push(')');
        }
        BoundCteKind::Recursive {
            anchor_plan,
            recursive_plan,
            max_iterations,
            max_rows,
        } => {
            let _ = write!(
                out,
                "Recursive(max_iters={},max_rows={},anchor=",
                max_iterations, max_rows
            );
            out.push_str(&canonicalize_plan(anchor_plan, None));
            out.push_str(",recursive=");
            out.push_str(&canonicalize_plan(recursive_plan, None));
            out.push(')');
        }
    }
    out.push(')');
}

pub fn canonicalize_resource_signature(res: &ResourcePlanningSignature, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(
        out,
        "RES_SIG(mem={},temp_files={},temp_bytes={},passes={},sort={},agg={},join={},multi={})",
        res.memory_budget_bytes,
        res.max_temp_files,
        res.max_temp_bytes,
        res.max_merge_passes,
        res.sort_budget,
        res.agg_budget,
        res.join_budget,
        res.multi_index_budget,
    );
}
