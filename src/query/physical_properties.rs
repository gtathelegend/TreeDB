use std::collections::BTreeSet;
use std::fmt;

use crate::engine::types::Schema;
use crate::query::expr::{AggregateExpr, Expr};
use crate::query::operator::{SortDirection, SortKey};
use crate::query::optimizer::{AccessPath, PhysicalTableScan, SecondaryScanType, TableOptInfo};

/// Describes an ordering property of a single column in an execution stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OrderingColumn {
    /// Canonical or bare column name or alias.
    pub column: String,
    /// Sort direction (Ascending or Descending).
    pub direction: SortDirection,
    /// Whether NULL values appear first.
    pub nulls_first: bool,
}

impl fmt::Display for OrderingColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}{}",
            self.column,
            self.direction,
            if self.nulls_first {
                " NULLS FIRST"
            } else {
                " NULLS LAST"
            }
        )
    }
}

/// Physical requirement specifying the sequence and direction of required ordering keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiredOrdering {
    /// Ordered sequence of required columns.
    pub columns: Vec<OrderingColumn>,
}

impl RequiredOrdering {
    /// Creates a new `RequiredOrdering`.
    pub fn new(columns: Vec<OrderingColumn>) -> Self {
        Self { columns }
    }

    /// Creates a `RequiredOrdering` from query sort keys and optional table qualifier.
    pub fn from_sort_keys(sort_keys: &[SortKey], table_qualifier: Option<&str>) -> Self {
        let columns = sort_keys
            .iter()
            .map(|k| {
                let col_name = match &k.expression {
                    Expr::Column(name) => name.clone(),
                    other => other.to_string(),
                };
                let bare = col_name
                    .split('.')
                    .next_back()
                    .unwrap_or(&col_name)
                    .to_string();
                let full_col = if let Some(qual) = table_qualifier {
                    if !col_name.contains('.') {
                        format!("{qual}.{bare}")
                    } else {
                        col_name
                    }
                } else {
                    col_name
                };
                let nulls_first = k
                    .nulls_first
                    .unwrap_or(k.direction == SortDirection::Ascending);
                OrderingColumn {
                    column: full_col,
                    direction: k.direction,
                    nulls_first,
                }
            })
            .collect();
        Self { columns }
    }

    /// Returns `true` if there are no required ordering columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

impl fmt::Display for RequiredOrdering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cols: Vec<String> = self.columns.iter().map(|c| c.to_string()).collect();
        write!(f, "{}", cols.join(", "))
    }
}

/// Represents the physical ordering guarantees provided by an operator or access path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderingProperty {
    /// Ordered sequence of columns.
    pub columns: Vec<OrderingColumn>,
}

impl OrderingProperty {
    /// Creates a new `OrderingProperty`.
    pub fn new(columns: Vec<OrderingColumn>) -> Self {
        Self { columns }
    }

    /// Returns `true` if this property guarantees no ordering.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Constructs an `OrderingProperty` from Sort keys established by a Sort operator.
    pub fn from_sort_keys(sort_keys: &[SortKey]) -> Self {
        let columns = sort_keys
            .iter()
            .map(|k| {
                let col_name = match &k.expression {
                    Expr::Column(name) => name.clone(),
                    other => other.to_string(),
                };
                let nulls_first = k
                    .nulls_first
                    .unwrap_or(k.direction == SortDirection::Ascending);
                OrderingColumn {
                    column: col_name,
                    direction: k.direction,
                    nulls_first,
                }
            })
            .collect();
        Self { columns }
    }

    /// Checks if this ordering property satisfies the given `RequiredOrdering`.
    pub fn satisfies_required_ordering(&self, req: &RequiredOrdering) -> bool {
        if req.columns.is_empty() {
            return true;
        }
        if self.columns.len() < req.columns.len() {
            return false;
        }
        for (i, req_col) in req.columns.iter().enumerate() {
            let req_bare = req_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&req_col.column);
            let ord_col = &self.columns[i];
            let ord_bare = ord_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&ord_col.column);

            if req_bare != ord_bare {
                return false;
            }
            if ord_col.direction != req_col.direction {
                return false;
            }
            if ord_col.nulls_first != req_col.nulls_first {
                return false;
            }
            if req_col.column.contains('.') && ord_col.column.contains('.') {
                let req_qual = req_col.column.split('.').next().unwrap();
                let ord_qual = ord_col.column.split('.').next().unwrap();
                if req_qual != ord_qual {
                    return false;
                }
            }
        }
        true
    }

    /// Checks if this ordering property satisfies another `OrderingProperty` in prefix order.
    pub fn satisfies(&self, other: &OrderingProperty) -> bool {
        if other.columns.is_empty() {
            return true;
        }
        if self.columns.len() < other.columns.len() {
            return false;
        }
        for (i, other_col) in other.columns.iter().enumerate() {
            let this_col = &self.columns[i];
            let other_bare = other_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&other_col.column);
            let this_bare = this_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&this_col.column);
            if !other_bare.eq_ignore_ascii_case(this_bare) {
                return false;
            }
            if this_col.direction != other_col.direction {
                return false;
            }
            if this_col.nulls_first != other_col.nulls_first {
                return false;
            }
        }
        true
    }

    /// Checks if this ordering property satisfies the required ORDER BY sort keys in prefix order.
    pub fn satisfies_order_by(
        &self,
        order_by: &[SortKey],
        table_qualifier: Option<&str>,
        projections: Option<&[(Expr, Option<String>)]>,
    ) -> bool {
        if order_by.is_empty() {
            return true;
        }
        if self.columns.len() < order_by.len() {
            return false;
        }
        for (i, sort_key) in order_by.iter().enumerate() {
            let req_col_name = match &sort_key.expression {
                Expr::Column(name) => name,
                _ => return false,
            };
            let mut resolved_col_name = req_col_name.clone();
            if let Some(projs) = projections {
                for (expr, alias) in projs {
                    if let Some(alias_name) = alias {
                        if alias_name.eq_ignore_ascii_case(req_col_name) {
                            if let Expr::Column(underlying) = expr {
                                resolved_col_name = underlying.clone();
                                break;
                            } else {
                                // Alias for a non-column expression (e.g. id + 1 AS x) cannot match bare column order
                                return false;
                            }
                        }
                    }
                }
            }

            let req_bare = resolved_col_name
                .split('.')
                .next_back()
                .unwrap_or(resolved_col_name.as_str());
            let ord_col = &self.columns[i];
            let ord_bare = ord_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&ord_col.column);

            let names_match = req_bare.eq_ignore_ascii_case(ord_bare)
                || resolved_col_name.eq_ignore_ascii_case(&ord_col.column)
                || req_col_name.eq_ignore_ascii_case(ord_bare)
                || req_col_name.eq_ignore_ascii_case(&ord_col.column);

            if !names_match {
                return false;
            }
            if ord_col.direction != sort_key.direction {
                return false;
            }
            let req_nulls_first = sort_key
                .nulls_first
                .unwrap_or(sort_key.direction == SortDirection::Ascending);
            if ord_col.nulls_first != req_nulls_first {
                return false;
            }
            if let (Some(qual), Some(req_qual)) =
                (table_qualifier, resolved_col_name.split('.').next())
            {
                if resolved_col_name.contains('.') && !req_qual.eq_ignore_ascii_case(qual) {
                    return false;
                }
            }
        }
        true
    }

    /// Checks if this ordering property satisfies the required GROUP BY keys in prefix order.
    pub fn satisfies_group_by(&self, group_by: &[Expr], table_qualifier: Option<&str>) -> bool {
        if group_by.is_empty() {
            return true;
        }
        if self.columns.len() < group_by.len() {
            return false;
        }
        for (i, req_expr) in group_by.iter().enumerate() {
            let req_col_name = match req_expr {
                Expr::Column(name) => name,
                _ => return false,
            };
            let req_bare = req_col_name.split('.').next_back().unwrap_or(req_col_name);
            let ord_col = &self.columns[i];
            let ord_bare = ord_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&ord_col.column);

            if req_bare != ord_bare {
                return false;
            }
            if ord_col.direction != SortDirection::Ascending {
                return false;
            }
            if let (Some(qual), Some(req_qual)) = (table_qualifier, req_col_name.split('.').next())
            {
                if req_col_name.contains('.') && req_qual != qual {
                    return false;
                }
            }
        }
        true
    }

    /// Checks if this ordering property satisfies the required equi-join keys in order.
    pub fn satisfies_join_keys(
        &self,
        required_keys: &[Expr],
        table_qualifier: Option<&str>,
    ) -> bool {
        if required_keys.is_empty() {
            return true;
        }
        if self.columns.len() < required_keys.len() {
            return false;
        }
        for (i, req_expr) in required_keys.iter().enumerate() {
            let req_col_name = match req_expr {
                Expr::Column(name) => name,
                _ => return false,
            };
            let req_bare = req_col_name.split('.').next_back().unwrap_or(req_col_name);
            let ord_col = &self.columns[i];
            let ord_bare = ord_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&ord_col.column);

            if req_bare != ord_bare {
                return false;
            }
            if ord_col.direction != SortDirection::Ascending {
                return false;
            }
            if let (Some(qual), Some(req_qual)) = (table_qualifier, req_col_name.split('.').next())
            {
                if req_col_name.contains('.') && req_qual != qual {
                    return false;
                }
            }
        }
        true
    }

    /// Projects this ordering property through a SELECT projection list.
    /// If an ordered column is projected directly, its ordering is preserved under its name/alias.
    /// If an ordered column is not projected or is wrapped in a computed expression, ordering on that column is lost.
    pub fn project(&self, projections: &[(Expr, Option<String>)]) -> Self {
        if projections.is_empty() {
            return self.clone();
        }
        let mut new_cols = Vec::new();
        for ord_col in &self.columns {
            let ord_bare = ord_col
                .column
                .split('.')
                .next_back()
                .unwrap_or(&ord_col.column);
            let mut found = false;
            for (expr, alias) in projections {
                if let Expr::Column(col_name) = expr {
                    let bare = col_name.split('.').next_back().unwrap_or(col_name);
                    if bare == ord_bare || col_name == &ord_col.column {
                        let out_name = alias.clone().unwrap_or_else(|| col_name.clone());
                        new_cols.push(OrderingColumn {
                            column: out_name,
                            direction: ord_col.direction,
                            nulls_first: ord_col.nulls_first,
                        });
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                // Prefix broken
                break;
            }
        }
        Self::new(new_cols)
    }
}

impl fmt::Display for OrderingProperty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.columns.is_empty() {
            write!(f, "NoOrdering")
        } else {
            let cols: Vec<String> = self.columns.iter().map(|c| c.to_string()).collect();
            write!(f, "{}", cols.join(", "))
        }
    }
}

/// Global physical properties describing execution stream characteristics.
/// Physical row-goal / bounded-output requirement for an execution subtree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RowGoal {
    /// Number of candidate qualifying rows the child subtree must produce (e.g. `OFFSET + LIMIT`).
    pub candidate_rows: u64,
    /// Number of output rows the parent operator requires (e.g. `LIMIT`).
    pub required_output_rows: u64,
    /// Number of initial rows discarded by pagination (e.g. `OFFSET`).
    pub offset: u64,
}

impl RowGoal {
    /// Creates a new `RowGoal`.
    pub const fn new(candidate_rows: u64, required_output_rows: u64) -> Self {
        let offset = candidate_rows.saturating_sub(required_output_rows);
        Self {
            candidate_rows,
            required_output_rows,
            offset,
        }
    }

    /// Constructs a `RowGoal` from optional LIMIT and OFFSET parameters.
    pub fn from_limit_offset(limit: Option<usize>, offset: Option<usize>) -> Option<Self> {
        let l = limit?;
        let o = offset.unwrap_or(0);
        let cand = (o + l) as u64;
        Some(Self {
            candidate_rows: cand,
            required_output_rows: l as u64,
            offset: o as u64,
        })
    }

    /// Returns the candidate requirement $K = \text{OFFSET} + \text{LIMIT}$.
    pub const fn k(&self) -> u64 {
        self.candidate_rows
    }
}

impl fmt::Display for RowGoal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RowGoal(candidate={}, output={}, offset={})",
            self.candidate_rows, self.required_output_rows, self.offset
        )
    }
}

/// Canonical, deterministic representation of column demand / required columns across queries and operators.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequiredColumns {
    /// Deterministic sorted set of column references.
    /// Can contain qualified names like "users.age", unqualified names like "age", or wildcard "*".
    pub columns: BTreeSet<String>,
}

impl RequiredColumns {
    /// Creates an empty `RequiredColumns` set.
    pub fn empty() -> Self {
        Self {
            columns: BTreeSet::new(),
        }
    }

    /// Creates a new `RequiredColumns` set.
    pub fn new() -> Self {
        Self::empty()
    }

    /// Creates a `RequiredColumns` representing all columns (wildcard `*`).
    pub fn all() -> Self {
        let mut columns = BTreeSet::new();
        columns.insert("*".to_string());
        Self { columns }
    }

    /// Returns `true` if there are no required columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Returns `true` if this demand requires all columns.
    pub fn is_all(&self) -> bool {
        self.columns.contains("*")
    }

    /// Number of column specifications in this set.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Inserts a column specification into the required columns set.
    pub fn insert(&mut self, col: impl Into<String>) {
        let s = col.into();
        if !s.is_empty() {
            self.columns.insert(s);
        }
    }

    /// Inserts multiple column specifications.
    pub fn insert_all(&mut self, cols: impl IntoIterator<Item = String>) {
        for c in cols {
            self.insert(c);
        }
    }

    /// Constructs a `RequiredColumns` from an iterator of column names.
    pub fn from_columns(cols: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut set = Self::empty();
        for c in cols {
            set.insert(c);
        }
        set
    }

    /// Constructs a `RequiredColumns` from columns referenced by an expression.
    pub fn from_expr(expr: &Expr) -> Self {
        let mut cols = BTreeSet::new();
        expr.collect_referenced_columns(&mut cols);
        Self { columns: cols }
    }

    /// Constructs a `RequiredColumns` from multiple expressions.
    pub fn from_exprs<'a>(exprs: impl IntoIterator<Item = &'a Expr>) -> Self {
        let mut cols = BTreeSet::new();
        for e in exprs {
            e.collect_referenced_columns(&mut cols);
        }
        Self { columns: cols }
    }

    /// Constructs a `RequiredColumns` from a slice of `SortKey`s.
    pub fn from_sort_keys(sort_keys: &[SortKey]) -> Self {
        let mut cols = BTreeSet::new();
        for sk in sort_keys {
            sk.expression.collect_referenced_columns(&mut cols);
        }
        Self { columns: cols }
    }

    /// Constructs a `RequiredColumns` from aggregate expressions.
    pub fn from_aggregates(aggregates: &[AggregateExpr]) -> Self {
        let mut cols = BTreeSet::new();
        for agg in aggregates {
            match agg {
                AggregateExpr::CountAll => {}
                AggregateExpr::Function { expr, .. } => {
                    expr.collect_referenced_columns(&mut cols);
                }
            }
        }
        Self { columns: cols }
    }

    /// Constructs a `RequiredColumns` from window expressions.
    pub fn from_window_exprs(window_exprs: &[crate::query::expr::WindowExpr]) -> Self {
        let mut cols = BTreeSet::new();
        for win in window_exprs {
            for arg in &win.args {
                arg.collect_referenced_columns(&mut cols);
            }
            if let Some(def) = &win.default {
                def.collect_referenced_columns(&mut cols);
            }
            for pb in &win.window_spec.partition_by {
                pb.collect_referenced_columns(&mut cols);
            }
            for ob in &win.window_spec.order_by {
                ob.expression.collect_referenced_columns(&mut cols);
            }
        }
        Self { columns: cols }
    }

    /// Returns the union of two `RequiredColumns` sets.
    pub fn union(&self, other: &Self) -> Self {
        let mut cols = self.columns.clone();
        cols.extend(other.columns.clone());
        Self { columns: cols }
    }

    /// Unions another `RequiredColumns` into `self`.
    pub fn union_with(&mut self, other: &Self) {
        self.columns.extend(other.columns.clone());
    }

    /// Returns the intersection of two `RequiredColumns` sets.
    pub fn intersect(&self, other: &Self) -> Self {
        if self.is_all() {
            return other.clone();
        }
        if other.is_all() {
            return self.clone();
        }
        let cols: BTreeSet<String> = self.columns.intersection(&other.columns).cloned().collect();
        Self { columns: cols }
    }

    /// Returns `true` if `self` contains all column demands in `other`.
    pub fn contains(&self, other: &Self) -> bool {
        if self.is_all() {
            return true;
        }
        other.columns.is_subset(&self.columns)
    }

    /// Checks whether a specific column is demanded for a given table qualifier and table name.
    pub fn is_required_for_table(&self, qualifier: &str, table_name: &str, col_name: &str) -> bool {
        if self.is_all() {
            return true;
        }
        if self.columns.contains(col_name) {
            return true;
        }
        if !qualifier.is_empty() {
            let qual_col = format!("{qualifier}.{col_name}");
            if self.columns.contains(&qual_col) || self.columns.contains(&format!("{qualifier}.*"))
            {
                return true;
            }
        }
        if !table_name.is_empty() && table_name != qualifier {
            let tbl_col = format!("{table_name}.{col_name}");
            if self.columns.contains(&tbl_col) || self.columns.contains(&format!("{table_name}.*"))
            {
                return true;
            }
        }
        // Case-insensitive fallback
        let col_lower = col_name.to_ascii_lowercase();
        for c in &self.columns {
            let c_lower = c.to_ascii_lowercase();
            if c_lower == col_lower {
                return true;
            }
            if let Some((q, rest)) = c_lower.split_once('.') {
                if (q == qualifier.to_ascii_lowercase() || q == table_name.to_ascii_lowercase())
                    && (rest == "*" || rest == col_lower)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Resolves the required column indices in `schema` for a table.
    pub fn table_column_indices(
        &self,
        qualifier: &str,
        table_name: &str,
        schema: &Schema,
    ) -> BTreeSet<usize> {
        let mut indices = BTreeSet::new();
        for (i, col) in schema.columns.iter().enumerate() {
            if self.is_required_for_table(qualifier, table_name, &col.name) {
                indices.insert(i);
            }
        }
        indices
    }

    /// Resolves the required column names in `schema` for a table.
    pub fn table_column_names(
        &self,
        qualifier: &str,
        table_name: &str,
        schema: &Schema,
    ) -> Vec<String> {
        schema
            .columns
            .iter()
            .filter(|col| self.is_required_for_table(qualifier, table_name, &col.name))
            .map(|col| col.name.clone())
            .collect()
    }

    /// Resolves the pruned (unneeded) column names in `schema` for a table.
    pub fn pruned_column_names(
        &self,
        qualifier: &str,
        table_name: &str,
        schema: &Schema,
    ) -> Vec<String> {
        schema
            .columns
            .iter()
            .filter(|col| !self.is_required_for_table(qualifier, table_name, &col.name))
            .map(|col| col.name.clone())
            .collect()
    }

    /// Returns a sorted `Vec` of all column demand strings.
    pub fn to_vec(&self) -> Vec<String> {
        self.columns.iter().cloned().collect()
    }
}

impl fmt::Display for RequiredColumns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cols: Vec<&str> = self.columns.iter().map(|s| s.as_str()).collect();
        write!(f, "[{}]", cols.join(", "))
    }
}

/// Physical requirements demanded by a parent execution node (ordering, row-goal bounds, and column demand).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequiredPhysicalProperties {
    /// Required physical ordering.
    pub ordering: RequiredOrdering,
    /// Bounded row-goal requirement if output is bounded.
    pub row_goal: Option<RowGoal>,
    /// Column demand required by the consumer subtree.
    pub required_columns: RequiredColumns,
}

impl RequiredPhysicalProperties {
    /// Creates a new `RequiredPhysicalProperties` with ordering and row-goal.
    pub fn new(ordering: RequiredOrdering, row_goal: Option<RowGoal>) -> Self {
        Self {
            ordering,
            row_goal,
            required_columns: RequiredColumns::empty(),
        }
    }

    /// Creates a `RequiredPhysicalProperties` with all properties.
    pub fn with_all(
        ordering: RequiredOrdering,
        row_goal: Option<RowGoal>,
        required_columns: RequiredColumns,
    ) -> Self {
        Self {
            ordering,
            row_goal,
            required_columns,
        }
    }

    /// Creates a requirement with only ordering.
    pub fn with_ordering(ordering: RequiredOrdering) -> Self {
        Self {
            ordering,
            row_goal: None,
            required_columns: RequiredColumns::empty(),
        }
    }

    /// Creates a requirement with only row goal.
    pub fn with_row_goal(row_goal: RowGoal) -> Self {
        Self {
            ordering: RequiredOrdering {
                columns: Vec::new(),
            },
            row_goal: Some(row_goal),
            required_columns: RequiredColumns::empty(),
        }
    }

    /// Creates a requirement with only required columns.
    pub fn with_required_columns(required_columns: RequiredColumns) -> Self {
        Self {
            ordering: RequiredOrdering::default(),
            row_goal: None,
            required_columns,
        }
    }

    /// Returns `true` if no physical properties, row goals, or column demands are required.
    pub fn is_empty(&self) -> bool {
        self.ordering.is_empty() && self.row_goal.is_none() && self.required_columns.is_empty()
    }
}

impl fmt::Display for RequiredPhysicalProperties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if !self.ordering.is_empty() {
            parts.push(format!("Ordering: {}", self.ordering));
        }
        if let Some(rg) = &self.row_goal {
            parts.push(format!("{rg}"));
        }
        if !self.required_columns.is_empty() {
            parts.push(format!("RequiredColumns: {}", self.required_columns));
        }
        if parts.is_empty() {
            write!(f, "NoRequirements")
        } else {
            write!(f, "{}", parts.join(", "))
        }
    }
}

/// Global physical properties describing execution stream characteristics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhysicalProperties {
    /// Active physical ordering property, if guaranteed.
    pub ordering: Option<OrderingProperty>,
    /// Active physical row-goal property, if guaranteed/bounded.
    pub row_goal: Option<RowGoal>,
    /// Output columns provided / materialized in the execution stream.
    pub required_columns: Option<RequiredColumns>,
}

impl PhysicalProperties {
    /// Creates a new `PhysicalProperties` with optional ordering.
    pub fn new(ordering: Option<OrderingProperty>) -> Self {
        Self {
            ordering,
            row_goal: None,
            required_columns: None,
        }
    }

    /// Creates a `PhysicalProperties` instance from guaranteed ordering.
    pub fn with_ordering(ordering: OrderingProperty) -> Self {
        Self {
            ordering: if ordering.is_empty() {
                None
            } else {
                Some(ordering)
            },
            row_goal: None,
            required_columns: None,
        }
    }

    /// Creates a `PhysicalProperties` with both ordering and row-goal.
    pub fn with_ordering_and_row_goal(
        ordering: Option<OrderingProperty>,
        row_goal: Option<RowGoal>,
    ) -> Self {
        Self {
            ordering,
            row_goal,
            required_columns: None,
        }
    }

    /// Creates a `PhysicalProperties` with ordering, row-goal, and column demand.
    pub fn with_all(
        ordering: Option<OrderingProperty>,
        row_goal: Option<RowGoal>,
        required_columns: Option<RequiredColumns>,
    ) -> Self {
        Self {
            ordering,
            row_goal,
            required_columns,
        }
    }

    /// Creates a `PhysicalProperties` instance with no guaranteed physical properties.
    pub fn none() -> Self {
        Self {
            ordering: None,
            row_goal: None,
            required_columns: None,
        }
    }

    /// Returns `true` if no physical properties are guaranteed.
    pub fn is_empty(&self) -> bool {
        self.ordering.as_ref().is_none_or(|o| o.is_empty())
            && self.row_goal.is_none()
            && self.required_columns.as_ref().is_none_or(|c| c.is_empty())
    }
}

impl fmt::Display for PhysicalProperties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(ord) = &self.ordering {
            if !ord.is_empty() {
                parts.push(format!("Ordering: {ord}"));
            }
        }
        if let Some(rg) = &self.row_goal {
            parts.push(format!("{rg}"));
        }
        if let Some(cols) = &self.required_columns {
            if !cols.is_empty() {
                parts.push(format!("ProvidedColumns: {cols}"));
            }
        }
        if parts.is_empty() {
            write!(f, "NoProperties")
        } else {
            write!(f, "{}", parts.join(", "))
        }
    }
}

/// Infers the output `OrderingProperty` produced by a table scan access path.
pub fn ordering_property_for_table_scan(
    table: &TableOptInfo,
    scan: &PhysicalTableScan,
) -> OrderingProperty {
    if table.is_cte {
        return OrderingProperty::default();
    }
    let (direction, nulls_first) = match scan.scan_direction {
        SortDirection::Ascending => (SortDirection::Ascending, true),
        SortDirection::Descending => (SortDirection::Descending, false),
    };
    match &scan.access_path {
        AccessPath::PrimaryRangeScan { .. } | AccessPath::FullTableScan => {
            let pk_col = table.schema.primary_key_column();
            OrderingProperty::new(vec![OrderingColumn {
                column: pk_col.name.clone(),
                direction,
                nulls_first,
            }])
        }
        AccessPath::SecondaryIndexScan {
            index_name,
            scan_type,
            ..
        }
        | AccessPath::IndexOnlyScan {
            index_name,
            scan_type,
            ..
        } => {
            if let Some(idx_meta) = table.indexes.iter().find(|idx| &idx.name == index_name) {
                let key_col_names: Vec<String> = idx_meta
                    .column_indices
                    .iter()
                    .filter_map(|&i| table.schema.columns.get(i).map(|c| c.name.clone()))
                    .collect();

                match scan_type {
                    SecondaryScanType::RangeScan { .. } => {
                        let cols = key_col_names
                            .into_iter()
                            .map(|name| OrderingColumn {
                                column: name,
                                direction,
                                nulls_first,
                            })
                            .collect();
                        OrderingProperty::new(cols)
                    }
                    SecondaryScanType::PrefixScan(prefix_vals) => {
                        let skip = prefix_vals.len();
                        let cols = key_col_names
                            .into_iter()
                            .skip(skip)
                            .map(|name| OrderingColumn {
                                column: name,
                                direction,
                                nulls_first,
                            })
                            .collect();
                        OrderingProperty::new(cols)
                    }
                    SecondaryScanType::PointLookup(_)
                    | SecondaryScanType::CompositePointLookup(_) => {
                        // All key columns are fixed; ordering can trivially satisfy empty/prefix
                        OrderingProperty::default()
                    }
                    SecondaryScanType::SkipScan { .. } => {
                        // Skip scan does not guarantee global ordering
                        OrderingProperty::default()
                    }
                }
            } else {
                OrderingProperty::default()
            }
        }
        AccessPath::PrimaryPointLookup { .. } => OrderingProperty::default(),
        AccessPath::MultiIndexScan { .. } | AccessPath::EmptyResult => OrderingProperty::default(),
    }
}
