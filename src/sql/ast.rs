use std::fmt;

/// Top-level AST node representing a SQL `SELECT` query.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    /// List of projection items in the `SELECT` clause (wildcard or expressions).
    pub projection: Vec<SelectItem>,
    /// Target table reference in the `FROM` clause.
    pub from: TableRef,
    /// Optional boolean filtering predicate in the `WHERE` clause.
    pub selection: Option<SqlExpr>,
    /// Optional list of grouping expressions in the `GROUP BY` clause.
    pub group_by: Vec<SqlExpr>,
    /// Optional boolean filtering predicate in the `HAVING` clause.
    pub having: Option<SqlExpr>,
    /// Ordered list of sort expressions in the `ORDER BY` clause.
    pub order_by: Vec<OrderByExpr>,
    /// Optional non-negative maximum row count in the `LIMIT` clause.
    pub limit: Option<u64>,
    /// Optional non-negative row skip count in the `OFFSET` clause.
    pub offset: Option<u64>,
}

/// An individual item in a `SELECT` projection list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// Unqualified wildcard (`*`) projecting all table columns.
    Wildcard,
    /// Qualified wildcard (`table.*` or `alias.*`) projecting all table columns.
    QualifiedWildcard(String),
    /// Projected expression with an optional column alias (`expr [AS alias]`).
    Expression {
        /// Evaluated scalar expression.
        expr: SqlExpr,
        /// Optional alias name for the projected output column.
        alias: Option<String>,
    },
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => write!(f, "*"),
            SelectItem::QualifiedWildcard(tbl) => write!(f, "{tbl}.*"),
            SelectItem::Expression { expr, alias } => match alias {
                Some(name) => write!(f, "{expr} AS {name}"),
                None => write!(f, "{expr}"),
            },
        }
    }
}

/// A referenced table in a `FROM` clause.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRef {
    /// Table name.
    pub name: String,
    /// Optional table alias.
    pub alias: Option<String>,
}

impl TableRef {
    /// Creates a new `TableRef` with the given table name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            alias: None,
        }
    }

    /// Creates a new `TableRef` with a table name and alias.
    pub fn with_alias(name: impl Into<String>, alias: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            alias: Some(alias.into()),
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.alias {
            Some(alias) => write!(f, "{} AS {}", self.name, alias),
            None => write!(f, "{}", self.name),
        }
    }
}

/// Binary operators supported in SQL expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlBinaryOp {
    /// Equality (`=`)
    Eq,
    /// Inequality (`!=` or `<>`)
    NotEq,
    /// Less than (`<`)
    Lt,
    /// Less than or equal (`<=`)
    LtEq,
    /// Greater than (`>`)
    Gt,
    /// Greater than or equal (`>=`)
    GtEq,
    /// Logical conjunction (`AND`)
    And,
    /// Logical disjunction (`OR`)
    Or,
}

impl fmt::Display for SqlBinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlBinaryOp::Eq => write!(f, "="),
            SqlBinaryOp::NotEq => write!(f, "!="),
            SqlBinaryOp::Lt => write!(f, "<"),
            SqlBinaryOp::LtEq => write!(f, "<="),
            SqlBinaryOp::Gt => write!(f, ">"),
            SqlBinaryOp::GtEq => write!(f, ">="),
            SqlBinaryOp::And => write!(f, "AND"),
            SqlBinaryOp::Or => write!(f, "OR"),
        }
    }
}

/// Unary operators supported in SQL expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlUnaryOp {
    /// Logical negation (`NOT`)
    Not,
    /// Arithmetic negation (`-`)
    Neg,
}

impl fmt::Display for SqlUnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlUnaryOp::Not => write!(f, "NOT"),
            SqlUnaryOp::Neg => write!(f, "-"),
        }
    }
}

/// Constant scalar literal in a SQL expression.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlLiteral {
    /// SQL `NULL`
    Null,
    /// SQL `TRUE` / `FALSE`
    Boolean(bool),
    /// 64-bit integer constant
    Integer(i64),
    /// 64-bit float constant
    Float(f64),
    /// UTF-8 string literal
    String(String),
}

impl fmt::Display for SqlLiteral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlLiteral::Null => write!(f, "NULL"),
            SqlLiteral::Boolean(b) => write!(f, "{b}"),
            SqlLiteral::Integer(i) => write!(f, "{i}"),
            SqlLiteral::Float(v) => write!(f, "{v}"),
            SqlLiteral::String(s) => write!(f, "'{}'", s.replace('\'', "''")),
        }
    }
}

/// Supported relational aggregate functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AggregateFunction {
    /// COUNT
    Count,
    /// SUM
    Sum,
    /// AVG
    Avg,
    /// MIN
    Min,
    /// MAX
    Max,
}

impl fmt::Display for AggregateFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AggregateFunction::Count => write!(f, "COUNT"),
            AggregateFunction::Sum => write!(f, "SUM"),
            AggregateFunction::Avg => write!(f, "AVG"),
            AggregateFunction::Min => write!(f, "MIN"),
            AggregateFunction::Max => write!(f, "MAX"),
        }
    }
}

/// Abstract Syntax Tree (AST) node for SQL scalar expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlExpr {
    /// Constant literal value.
    Literal(SqlLiteral),
    /// Unqualified column or identifier reference (`column`).
    Identifier(String),
    /// Qualified column reference (`table.column` or `alias.column`).
    QualifiedIdentifier {
        /// Table name or table alias qualifier.
        qualifier: String,
        /// Column name.
        name: String,
    },
    /// Binary operation on two sub-expressions (`left op right`).
    Binary {
        /// Left-hand operand.
        left: Box<SqlExpr>,
        /// Binary operator.
        op: SqlBinaryOp,
        /// Right-hand operand.
        right: Box<SqlExpr>,
    },
    /// Unary operation on a sub-expression (`op expr`).
    Unary {
        /// Unary operator.
        op: SqlUnaryOp,
        /// Sub-expression operand.
        expr: Box<SqlExpr>,
    },
    /// `expr IS NULL` predicate.
    IsNull(Box<SqlExpr>),
    /// `expr IS NOT NULL` predicate.
    IsNotNull(Box<SqlExpr>),
    /// Special wildcard count aggregate `COUNT(*)`.
    CountAll,
    /// Aggregate function applied to a single scalar expression (`FUNC(expr)`).
    Aggregate {
        /// Aggregate function name.
        function: AggregateFunction,
        /// Inner argument expression.
        arg: Box<SqlExpr>,
    },
}

impl fmt::Display for SqlExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlExpr::Literal(lit) => write!(f, "{lit}"),
            SqlExpr::Identifier(name) => write!(f, "{name}"),
            SqlExpr::QualifiedIdentifier { qualifier, name } => write!(f, "{qualifier}.{name}"),
            SqlExpr::Binary { left, op, right } => write!(f, "({left} {op} {right})"),
            SqlExpr::Unary { op, expr } => write!(f, "({op} {expr})"),
            SqlExpr::IsNull(expr) => write!(f, "({expr} IS NULL)"),
            SqlExpr::IsNotNull(expr) => write!(f, "({expr} IS NOT NULL)"),
            SqlExpr::CountAll => write!(f, "COUNT(*)"),
            SqlExpr::Aggregate { function, arg } => write!(f, "{function}({arg})"),
        }
    }
}

/// Sort direction for an `ORDER BY` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OrderDirection {
    /// Ascending sort order (default).
    #[default]
    Asc,
    /// Descending sort order.
    Desc,
}

impl fmt::Display for OrderDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrderDirection::Asc => write!(f, "ASC"),
            OrderDirection::Desc => write!(f, "DESC"),
        }
    }
}

/// An individual sorting expression in an `ORDER BY` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByExpr {
    /// Evaluated expression to sort by.
    pub expr: SqlExpr,
    /// Sort direction (`ASC` or `DESC`).
    pub direction: OrderDirection,
}

impl OrderByExpr {
    /// Creates a new `OrderByExpr` with the given expression and direction.
    pub const fn new(expr: SqlExpr, direction: OrderDirection) -> Self {
        Self { expr, direction }
    }

    /// Creates an ascending `OrderByExpr`.
    pub const fn asc(expr: SqlExpr) -> Self {
        Self::new(expr, OrderDirection::Asc)
    }

    /// Creates a descending `OrderByExpr`.
    pub const fn desc(expr: SqlExpr) -> Self {
        Self::new(expr, OrderDirection::Desc)
    }
}

impl fmt::Display for OrderByExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.expr, self.direction)
    }
}
