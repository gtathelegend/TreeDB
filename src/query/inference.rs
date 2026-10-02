use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::engine::types::Value;
use crate::query::expr::{BinaryOp, Expr, UnaryOp};
use crate::query::operator::CanonicalValue;

/// Helper function to compare two scalar Values via CanonicalValue.
pub fn compare_values(v1: &Value, v2: &Value) -> Option<std::cmp::Ordering> {
    let c1 = CanonicalValue::from_value(v1);
    let c2 = CanonicalValue::from_value(v2);
    c1.partial_cmp(&c2)
}

/// Maximum bounded propagation rounds for transitive inference to avoid unbounded cycles.
pub const MAX_PROPAGATION_ROUNDS: usize = 8;

/// Safety classification of an optimization / constraint inference rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSafety {
    /// Formally proven to preserve exact SQL semantics including 3VL and NULL handling.
    Safe,
    /// Unsafe to apply in general relational contexts.
    Unsafe,
    /// Not currently supported or implemented.
    NotImplemented,
}

/// Source or reasoning behind an inferred predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceSource {
    /// Inferred via equi-join condition across an INNER JOIN (e.g. `u.id = o.user_id`).
    InnerJoinEquality,
    /// Inferred via transitive equality classes (e.g. `A = B`, `B = C` => `A = C`).
    TransitiveEquality,
    /// Inferred via intersection and consolidation of same-column range boundaries.
    RangeIntersection,
    /// Equality constraint dominates and subsumes enclosing range constraints.
    EqualityRangeAbsorption,
    /// Common constraint extracted across all disjoint branches of an OR expression.
    CommonOrExtraction,
}

impl fmt::Display for InferenceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InnerJoinEquality => write!(f, "INNER JOIN equality"),
            Self::TransitiveEquality => write!(f, "Transitive equality"),
            Self::RangeIntersection => write!(f, "Range intersection"),
            Self::EqualityRangeAbsorption => write!(f, "Equality range absorption"),
            Self::CommonOrExtraction => write!(f, "Common OR extraction"),
        }
    }
}

/// Record of an inferred predicate for query optimization and EXPLAIN diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredPredicate {
    /// Original user-specified or structural predicate.
    pub original: Expr,
    /// Inferred logical predicate.
    pub inferred: Expr,
    /// Originating transformation source.
    pub source: InferenceSource,
    /// Formal safety classification.
    pub safety: RuleSafety,
}

/// Bounded metrics for predicate inference and constraint propagation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceMetrics {
    /// Number of new predicates inferred and propagated.
    pub predicates_inferred: usize,
    /// Number of individual column constraints extracted.
    pub constraints_extracted: usize,
    /// Number of constraint merges performed.
    pub constraints_merged: usize,
    /// Number of logical contradictions / unsatisfiable states detected.
    pub contradictions_detected: usize,
    /// Number of equi-join constraint propagations applied.
    pub join_propagations: usize,
    /// Number of range boundary intersections computed.
    pub range_intersections: usize,
    /// Number of propagation iteration rounds executed.
    pub inference_rounds: usize,
}

/// Range boundary definition (inclusive or exclusive).
#[derive(Debug, Clone, PartialEq)]
pub enum RangeBound {
    /// Inclusive boundary: `>= val` or `<= val`.
    Inclusive(Value),
    /// Exclusive boundary: `> val` or `< val`.
    Exclusive(Value),
}

impl RangeBound {
    /// Returns a reference to the enclosed scalar value.
    pub fn value(&self) -> &Value {
        match self {
            Self::Inclusive(v) | Self::Exclusive(v) => v,
        }
    }

    /// Returns `true` if this boundary is inclusive.
    pub fn is_inclusive(&self) -> bool {
        matches!(self, Self::Inclusive(_))
    }
}

/// A contiguous 1D range constraint for an ordered column `(lower, upper)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRange {
    /// Lower bound (e.g. `> 18` or `>= 18`).
    pub lower: Option<RangeBound>,
    /// Upper bound (e.g. `< 30` or `<= 30`).
    pub upper: Option<RangeBound>,
}

impl ColumnRange {
    /// Creates an unbounded range `(-inf, +inf)`.
    pub fn unbounded() -> Self {
        Self {
            lower: None,
            upper: None,
        }
    }

    /// Intersects `self` with another `ColumnRange`.
    /// Returns `Ok(intersection)` or `Err(reason)` if the intersection is empty (contradiction).
    pub fn intersect(&self, other: &ColumnRange) -> std::result::Result<Self, String> {
        let new_lower = match (&self.lower, &other.lower) {
            (None, None) => None,
            (Some(l), None) | (None, Some(l)) => Some(l.clone()),
            (Some(l1), Some(l2)) => {
                let v1 = l1.value();
                let v2 = l2.value();
                let cmp = compare_values(v1, v2).unwrap_or(std::cmp::Ordering::Equal);
                if cmp == std::cmp::Ordering::Equal {
                    if !l1.is_inclusive() || !l2.is_inclusive() {
                        Some(RangeBound::Exclusive(v1.clone()))
                    } else {
                        Some(l1.clone())
                    }
                } else if cmp == std::cmp::Ordering::Greater {
                    Some(l1.clone())
                } else {
                    Some(l2.clone())
                }
            }
        };

        let new_upper = match (&self.upper, &other.upper) {
            (None, None) => None,
            (Some(u), None) | (None, Some(u)) => Some(u.clone()),
            (Some(u1), Some(u2)) => {
                let v1 = u1.value();
                let v2 = u2.value();
                let cmp = compare_values(v1, v2).unwrap_or(std::cmp::Ordering::Equal);
                if cmp == std::cmp::Ordering::Equal {
                    if !u1.is_inclusive() || !u2.is_inclusive() {
                        Some(RangeBound::Exclusive(v1.clone()))
                    } else {
                        Some(u1.clone())
                    }
                } else if cmp == std::cmp::Ordering::Less {
                    Some(u1.clone())
                } else {
                    Some(u2.clone())
                }
            }
        };

        // Check if lower bound exceeds upper bound
        if let (Some(l), Some(u)) = (&new_lower, &new_upper) {
            let lv = l.value();
            let uv = u.value();
            let cmp = compare_values(lv, uv).unwrap_or(std::cmp::Ordering::Equal);
            if cmp == std::cmp::Ordering::Greater {
                return Err(format!(
                    "range contradiction: lower bound {lv} > upper bound {uv}"
                ));
            }
            if cmp == std::cmp::Ordering::Equal && (!l.is_inclusive() || !u.is_inclusive()) {
                return Err(format!(
                    "range contradiction: lower bound {lv} == upper bound {uv} with exclusive boundary"
                ));
            }
        }

        Ok(Self {
            lower: new_lower,
            upper: new_upper,
        })
    }

    /// Checks if a scalar value falls within this range.
    pub fn contains(&self, val: &Value) -> bool {
        if val.is_null() {
            return false;
        }
        if let Some(l) = &self.lower {
            let lv = l.value();
            let cmp = compare_values(val, lv).unwrap_or(std::cmp::Ordering::Equal);
            if l.is_inclusive() {
                if cmp == std::cmp::Ordering::Less {
                    return false;
                }
            } else if cmp == std::cmp::Ordering::Less || cmp == std::cmp::Ordering::Equal {
                return false;
            }
        }
        if let Some(u) = &self.upper {
            let uv = u.value();
            let cmp = compare_values(val, uv).unwrap_or(std::cmp::Ordering::Equal);
            if u.is_inclusive() {
                if cmp == std::cmp::Ordering::Greater {
                    return false;
                }
            } else if cmp == std::cmp::Ordering::Greater || cmp == std::cmp::Ordering::Equal {
                return false;
            }
        }
        true
    }
}

/// Inferred constraint on an individual column.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    /// Exact scalar equality: `col = value`.
    Exact(Value),
    /// Range restriction over scalar values: `[lower, upper)`.
    Range(ColumnRange),
    /// Explicit NULL: `col IS NULL`.
    IsNull,
    /// Explicit non-null: `col IS NOT NULL`.
    IsNotNull,
    /// Proven contradiction on this column.
    Contradiction(String),
}

impl ColumnConstraint {
    /// Merges two constraints for the same column.
    pub fn merge(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Contradiction(r), _) | (_, Self::Contradiction(r)) => {
                Self::Contradiction(r.clone())
            }
            (Self::IsNull, Self::Exact(v)) | (Self::Exact(v), Self::IsNull) => {
                Self::Contradiction(format!("cannot be both IS NULL and equal to {v}"))
            }
            (Self::IsNull, Self::IsNotNull) | (Self::IsNotNull, Self::IsNull) => {
                Self::Contradiction("cannot be both IS NULL and IS NOT NULL".to_string())
            }
            (Self::IsNull, Self::Range(_)) | (Self::Range(_), Self::IsNull) => Self::Contradiction(
                "cannot be both IS NULL and satisfy range constraint".to_string(),
            ),
            (Self::IsNull, Self::IsNull) => Self::IsNull,
            (Self::IsNotNull, Self::IsNotNull) => Self::IsNotNull,
            (Self::IsNotNull, Self::Exact(v)) | (Self::Exact(v), Self::IsNotNull) => {
                Self::Exact(v.clone())
            }
            (Self::IsNotNull, Self::Range(r)) | (Self::Range(r), Self::IsNotNull) => {
                Self::Range(r.clone())
            }
            (Self::Exact(v1), Self::Exact(v2)) => {
                if v1 == v2 {
                    Self::Exact(v1.clone())
                } else {
                    Self::Contradiction(format!("conflicting equality constraints: {v1} vs {v2}"))
                }
            }
            (Self::Exact(v), Self::Range(r)) | (Self::Range(r), Self::Exact(v)) => {
                if r.contains(v) {
                    Self::Exact(v.clone())
                } else {
                    Self::Contradiction(format!("value {v} is outside range constraint"))
                }
            }
            (Self::Range(r1), Self::Range(r2)) => match r1.intersect(r2) {
                Ok(int_range) => Self::Range(int_range),
                Err(reason) => Self::Contradiction(reason),
            },
        }
    }
}

/// Set of extracted and normalized logical constraints with equivalence classes.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintSet {
    /// Mapping from canonical column reference string (`"table.col"` or `"col"`) to column constraint.
    pub constraints: BTreeMap<String, ColumnConstraint>,
    /// Column equivalence classes for equi-join and transitive equality: mapping from column -> root representative.
    pub parent: BTreeMap<String, String>,
    /// Equivalence class root -> set of member columns.
    pub class_members: BTreeMap<String, BTreeSet<String>>,
    /// Active contradiction reason if any constraint is unsatisfiable.
    pub contradiction: Option<String>,
    /// Tracked inference logs and diagnostics.
    pub inferences: Vec<InferredPredicate>,
}

impl Default for ConstraintSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ConstraintSet {
    /// Creates an empty constraint set.
    pub fn new() -> Self {
        Self {
            constraints: BTreeMap::new(),
            parent: BTreeMap::new(),
            class_members: BTreeMap::new(),
            contradiction: None,
            inferences: Vec::new(),
        }
    }

    /// Returns `true` if the constraint set is logically impossible (unsatisfiable).
    pub fn is_unsatisfiable(&self) -> bool {
        self.contradiction.is_some()
            || self
                .constraints
                .values()
                .any(|c| matches!(c, ColumnConstraint::Contradiction(_)))
    }

    /// Returns the active contradiction reason if unsatisfiable.
    pub fn contradiction_reason(&self) -> Option<&str> {
        if let Some(r) = &self.contradiction {
            return Some(r);
        }
        for c in self.constraints.values() {
            if let ColumnConstraint::Contradiction(r) = c {
                return Some(r);
            }
        }
        None
    }

    /// Finds the root representative column of an equivalence class.
    pub fn find_root(&mut self, col: &str) -> String {
        if !self.parent.contains_key(col) {
            self.parent.insert(col.to_string(), col.to_string());
            let mut set = BTreeSet::new();
            set.insert(col.to_string());
            self.class_members.insert(col.to_string(), set);
            return col.to_string();
        }

        let p = self.parent[col].clone();
        if p != col {
            let root = self.find_root(&p);
            self.parent.insert(col.to_string(), root.clone());
            root
        } else {
            col.to_string()
        }
    }

    /// Merges two columns into the same equivalence class (`col1 = col2`).
    pub fn union_columns(&mut self, col1: &str, col2: &str, source: InferenceSource) {
        if self.is_unsatisfiable() {
            return;
        }

        let root1 = self.find_root(col1);
        let root2 = self.find_root(col2);

        if root1 == root2 {
            return;
        }

        // Merge smaller class into lexicographically smaller root for determinism
        let (survivor, absorbed) = if root1 < root2 {
            (root1, root2)
        } else {
            (root2, root1)
        };

        self.parent.insert(absorbed.clone(), survivor.clone());
        if let Some(absorbed_members) = self.class_members.remove(&absorbed) {
            let survivor_members = self.class_members.entry(survivor.clone()).or_default();
            survivor_members.extend(absorbed_members);
        }

        // Merge existing constraints of both classes
        let c_surv = self.constraints.get(&survivor).cloned();
        let c_abs = self.constraints.get(&absorbed).cloned();

        let merged_constraint = match (c_surv, c_abs) {
            (Some(c1), Some(c2)) => Some(c1.merge(&c2)),
            (Some(c), None) | (None, Some(c)) => Some(c),
            (None, None) => None,
        };

        if let Some(m) = merged_constraint {
            if let ColumnConstraint::Contradiction(ref reason) = m {
                self.contradiction = Some(reason.clone());
            }
            self.constraints.insert(survivor.clone(), m.clone());

            // Propagate merged constraint to all member columns in the class
            if let Some(members) = self.class_members.get(&survivor).cloned() {
                for mem in members {
                    self.constraints.insert(mem.clone(), m.clone());
                    if mem != col1 && mem != col2 {
                        let original_expr = Expr::Binary {
                            left: Box::new(Expr::Column(col1.to_string())),
                            op: BinaryOp::Eq,
                            right: Box::new(Expr::Column(col2.to_string())),
                        };
                        let inferred_expr = match &m {
                            ColumnConstraint::Exact(v) => Expr::Binary {
                                left: Box::new(Expr::Column(mem.clone())),
                                op: BinaryOp::Eq,
                                right: Box::new(Expr::Literal(v.clone())),
                            },
                            _ => Expr::Column(mem.clone()),
                        };
                        self.inferences.push(InferredPredicate {
                            original: original_expr,
                            inferred: inferred_expr,
                            source: source.clone(),
                            safety: RuleSafety::Safe,
                        });
                    }
                }
            }
        }
    }

    /// Adds or merges a constraint for a specific column.
    pub fn add_constraint(&mut self, col: &str, constraint: ColumnConstraint) {
        if self.is_unsatisfiable() {
            return;
        }

        let root = self.find_root(col);
        let existing = self.constraints.get(&root);
        let merged = match existing {
            Some(curr) => curr.merge(&constraint),
            None => constraint,
        };

        if let ColumnConstraint::Contradiction(ref reason) = merged {
            self.contradiction = Some(reason.clone());
        }

        self.constraints.insert(root.clone(), merged.clone());
        if let Some(members) = self.class_members.get(&root).cloned() {
            for mem in members {
                self.constraints.insert(mem, merged.clone());
            }
        }
    }

    /// Extracts constraints from a list of conjunction expressions (`AND` terms).
    pub fn extract_from_conjuncts(conjuncts: &[Expr], metrics: &mut InferenceMetrics) -> Self {
        let mut set = Self::new();
        for conj in conjuncts {
            set.extract_single_expr(conj, metrics);
            if set.is_unsatisfiable() {
                metrics.contradictions_detected += 1;
                break;
            }
        }
        set
    }

    /// Extracts constraints from an individual expression term.
    fn extract_single_expr(&mut self, expr: &Expr, metrics: &mut InferenceMetrics) {
        if self.is_unsatisfiable() {
            return;
        }

        match expr {
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => {
                    self.extract_single_expr(left, metrics);
                    self.extract_single_expr(right, metrics);
                }
                BinaryOp::Eq => {
                    match (left.as_ref(), right.as_ref()) {
                        // col = literal
                        (Expr::Column(col), Expr::Literal(val))
                        | (Expr::Literal(val), Expr::Column(col)) => {
                            if !val.is_null() {
                                metrics.constraints_extracted += 1;
                                self.add_constraint(col, ColumnConstraint::Exact(val.clone()));
                            }
                        }
                        // col = col
                        (Expr::Column(col1), Expr::Column(col2)) => {
                            metrics.constraints_extracted += 1;
                            self.union_columns(col1, col2, InferenceSource::TransitiveEquality);
                        }
                        // literal = literal
                        (Expr::Literal(v1), Expr::Literal(v2))
                            if !v1.is_null() && !v2.is_null() && v1 != v2 =>
                        {
                            self.contradiction =
                                Some(format!("constant contradiction: {v1} = {v2}"));
                        }
                        _ => {}
                    }
                }
                BinaryOp::Gt | BinaryOp::GtEq | BinaryOp::Lt | BinaryOp::LtEq => {
                    let (col_name, bound) = match (left.as_ref(), right.as_ref()) {
                        (Expr::Column(col), Expr::Literal(val)) => {
                            if val.is_null() {
                                return;
                            }
                            match op {
                                BinaryOp::Gt => (
                                    col,
                                    ColumnRange {
                                        lower: Some(RangeBound::Exclusive(val.clone())),
                                        upper: None,
                                    },
                                ),
                                BinaryOp::GtEq => (
                                    col,
                                    ColumnRange {
                                        lower: Some(RangeBound::Inclusive(val.clone())),
                                        upper: None,
                                    },
                                ),
                                BinaryOp::Lt => (
                                    col,
                                    ColumnRange {
                                        lower: None,
                                        upper: Some(RangeBound::Exclusive(val.clone())),
                                    },
                                ),
                                BinaryOp::LtEq => (
                                    col,
                                    ColumnRange {
                                        lower: None,
                                        upper: Some(RangeBound::Inclusive(val.clone())),
                                    },
                                ),
                                _ => unreachable!(),
                            }
                        }
                        (Expr::Literal(val), Expr::Column(col)) => {
                            if val.is_null() {
                                return;
                            }
                            // Normalize reversed comparison: `18 <= age` => `age >= 18`
                            match op {
                                BinaryOp::Gt => (
                                    col,
                                    ColumnRange {
                                        lower: None,
                                        upper: Some(RangeBound::Exclusive(val.clone())),
                                    },
                                ),
                                BinaryOp::GtEq => (
                                    col,
                                    ColumnRange {
                                        lower: None,
                                        upper: Some(RangeBound::Inclusive(val.clone())),
                                    },
                                ),
                                BinaryOp::Lt => (
                                    col,
                                    ColumnRange {
                                        lower: Some(RangeBound::Exclusive(val.clone())),
                                        upper: None,
                                    },
                                ),
                                BinaryOp::LtEq => (
                                    col,
                                    ColumnRange {
                                        lower: Some(RangeBound::Inclusive(val.clone())),
                                        upper: None,
                                    },
                                ),
                                _ => unreachable!(),
                            }
                        }
                        _ => return,
                    };

                    metrics.constraints_extracted += 1;
                    metrics.range_intersections += 1;
                    self.add_constraint(col_name, ColumnConstraint::Range(bound));
                }
                _ => {}
            },
            Expr::IsNull(inner) => {
                if let Expr::Column(col) = inner.as_ref() {
                    metrics.constraints_extracted += 1;
                    self.add_constraint(col, ColumnConstraint::IsNull);
                }
            }
            Expr::IsNotNull(inner) => {
                if let Expr::Column(col) = inner.as_ref() {
                    metrics.constraints_extracted += 1;
                    self.add_constraint(col, ColumnConstraint::IsNotNull);
                }
            }
            Expr::Unary {
                op: UnaryOp::Not,
                expr: inner,
            } => {
                if let Expr::IsNull(col_expr) = inner.as_ref() {
                    if let Expr::Column(col) = col_expr.as_ref() {
                        metrics.constraints_extracted += 1;
                        self.add_constraint(col, ColumnConstraint::IsNotNull);
                    }
                }
            }
            _ => {}
        }
    }

    /// Propagates constraints across INNER JOIN equality conditions (e.g. `u.id = o.user_id`).
    pub fn propagate_inner_joins(
        &mut self,
        inner_join_on_clauses: &[Expr],
        metrics: &mut InferenceMetrics,
    ) {
        if self.is_unsatisfiable() {
            return;
        }

        let mut round = 0;
        let mut changed = true;

        while changed && round < MAX_PROPAGATION_ROUNDS {
            round += 1;
            metrics.inference_rounds += 1;
            changed = false;

            for on_clause in inner_join_on_clauses {
                let on_conjuncts = split_conjunction_expr(on_clause);
                for conj in on_conjuncts {
                    if let Expr::Binary {
                        left,
                        op: BinaryOp::Eq,
                        right,
                    } = conj
                    {
                        if let (Expr::Column(col1), Expr::Column(col2)) =
                            (left.as_ref(), right.as_ref())
                        {
                            let r1 = self.find_root(col1);
                            let r2 = self.find_root(col2);

                            if r1 != r2 {
                                metrics.join_propagations += 1;
                                metrics.predicates_inferred += 1;
                                self.union_columns(col1, col2, InferenceSource::InnerJoinEquality);
                                changed = true;
                                if self.is_unsatisfiable() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Extracts common constraints that hold true across both branches of an OR disjunction.
    pub fn extract_common_or(left: &Self, right: &Self, metrics: &mut InferenceMetrics) -> Self {
        let mut common = Self::new();
        if left.is_unsatisfiable() {
            return right.clone();
        }
        if right.is_unsatisfiable() {
            return left.clone();
        }

        // Check common column constraints
        for (col, c_left) in &left.constraints {
            if let Some(c_right) = right.constraints.get(col) {
                if c_left == c_right {
                    metrics.constraints_extracted += 1;
                    metrics.predicates_inferred += 1;
                    common.add_constraint(col, c_left.clone());
                    let inferred_expr = match c_left {
                        ColumnConstraint::Exact(v) => Expr::Binary {
                            left: Box::new(Expr::Column(col.clone())),
                            op: BinaryOp::Eq,
                            right: Box::new(Expr::Literal(v.clone())),
                        },
                        ColumnConstraint::IsNull => {
                            Expr::IsNull(Box::new(Expr::Column(col.clone())))
                        }
                        ColumnConstraint::IsNotNull => {
                            Expr::IsNotNull(Box::new(Expr::Column(col.clone())))
                        }
                        _ => Expr::Column(col.clone()),
                    };
                    common.inferences.push(InferredPredicate {
                        original: inferred_expr.clone(),
                        inferred: inferred_expr,
                        source: InferenceSource::CommonOrExtraction,
                        safety: RuleSafety::Safe,
                    });
                }
            }
        }

        common
    }

    /// Converts normalized and inferred constraints into equivalent SQL `Expr` conjunction terms.
    pub fn to_conjuncts(&self, qualifier_filter: Option<&str>) -> Vec<Expr> {
        let mut conjuncts = Vec::new();
        if self.is_unsatisfiable() {
            // Contradiction: emit a known false literal
            conjuncts.push(Expr::Literal(Value::Boolean(false)));
            return conjuncts;
        }

        for (col, constraint) in &self.constraints {
            if let Some(qual) = qualifier_filter {
                if let Some((col_qual, _)) = col.split_once('.') {
                    if col_qual != qual {
                        continue;
                    }
                } else if qual != col {
                    // Unqualified column doesn't match qualifier
                }
            }

            match constraint {
                ColumnConstraint::Exact(val) => {
                    conjuncts.push(Expr::Binary {
                        left: Box::new(Expr::Column(col.clone())),
                        op: BinaryOp::Eq,
                        right: Box::new(Expr::Literal(val.clone())),
                    });
                }
                ColumnConstraint::Range(r) => {
                    if let Some(l) = &r.lower {
                        let op = if l.is_inclusive() {
                            BinaryOp::GtEq
                        } else {
                            BinaryOp::Gt
                        };
                        conjuncts.push(Expr::Binary {
                            left: Box::new(Expr::Column(col.clone())),
                            op,
                            right: Box::new(Expr::Literal(l.value().clone())),
                        });
                    }
                    if let Some(u) = &r.upper {
                        let op = if u.is_inclusive() {
                            BinaryOp::LtEq
                        } else {
                            BinaryOp::Lt
                        };
                        conjuncts.push(Expr::Binary {
                            left: Box::new(Expr::Column(col.clone())),
                            op,
                            right: Box::new(Expr::Literal(u.value().clone())),
                        });
                    }
                }
                ColumnConstraint::IsNull => {
                    conjuncts.push(Expr::IsNull(Box::new(Expr::Column(col.clone()))));
                }
                ColumnConstraint::IsNotNull => {
                    conjuncts.push(Expr::IsNotNull(Box::new(Expr::Column(col.clone()))));
                }
                ColumnConstraint::Contradiction(_) => {
                    conjuncts.push(Expr::Literal(Value::Boolean(false)));
                }
            }
        }

        conjuncts
    }
}

/// Helper to split a conjunction `Expr` into a list of constituent terms.
fn split_conjunction_expr(expr: &Expr) -> Vec<Expr> {
    let mut list = Vec::new();
    let mut stack = vec![expr];
    while let Some(curr) = stack.pop() {
        if let Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
        } = curr
        {
            stack.push(right);
            stack.push(left);
        } else {
            list.push(curr.clone());
        }
    }
    list
}
