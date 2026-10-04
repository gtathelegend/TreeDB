use std::fmt;

use crate::query::expr::Expr;
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhysicalProperties {
    /// Active physical ordering property, if guaranteed.
    pub ordering: Option<OrderingProperty>,
}

impl PhysicalProperties {
    /// Creates a new `PhysicalProperties` with optional ordering.
    pub fn new(ordering: Option<OrderingProperty>) -> Self {
        Self { ordering }
    }

    /// Creates a `PhysicalProperties` instance from guaranteed ordering.
    pub fn with_ordering(ordering: OrderingProperty) -> Self {
        Self {
            ordering: if ordering.is_empty() {
                None
            } else {
                Some(ordering)
            },
        }
    }

    /// Creates a `PhysicalProperties` instance with no guaranteed physical properties.
    pub fn none() -> Self {
        Self { ordering: None }
    }

    /// Returns `true` if no physical properties are guaranteed.
    pub fn is_empty(&self) -> bool {
        self.ordering.as_ref().is_none_or(|o| o.is_empty())
    }
}

impl fmt::Display for PhysicalProperties {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.ordering {
            Some(ord) if !ord.is_empty() => write!(f, "Ordering: {ord}"),
            _ => write!(f, "NoProperties"),
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
    match &scan.access_path {
        AccessPath::PrimaryRangeScan { .. } | AccessPath::FullTableScan => {
            let pk_col = table.schema.primary_key_column();
            OrderingProperty::new(vec![OrderingColumn {
                column: pk_col.name.clone(),
                direction: SortDirection::Ascending,
                nulls_first: true,
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
                                direction: SortDirection::Ascending,
                                nulls_first: true,
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
                                direction: SortDirection::Ascending,
                                nulls_first: true,
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
