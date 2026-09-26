use std::fmt;

use crate::engine::types::DataType;

/// Top-level AST node representing an executable SQL statement.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// SQL `SELECT` query statement.
    Select(SelectStatement),
    /// SQL set-operation query statement (e.g. `UNION`, `INTERSECT`, `EXCEPT`).
    SetQuery(SetQuery),
    /// SQL `CREATE TABLE` DDL statement.
    CreateTable(CreateTableStatement),
    /// SQL `DROP TABLE` DDL statement.
    DropTable(DropTableStatement),
    /// SQL `CREATE INDEX` DDL statement.
    CreateIndex(CreateIndexStatement),
    /// SQL `DROP INDEX` DDL statement.
    DropIndex(DropIndexStatement),
    /// SQL `INSERT INTO` DML statement.
    Insert(InsertStatement),
    /// SQL `UPDATE` DML statement.
    Update(UpdateStatement),
    /// SQL `DELETE FROM` DML statement.
    Delete(DeleteStatement),
    /// SQL `EXPLAIN` query introspection statement.
    Explain(ExplainStatement),
    /// SQL `ANALYZE` statistics collection statement.
    Analyze(AnalyzeStatement),
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::Select(s) => write!(f, "{s}"),
            Statement::SetQuery(s) => write!(f, "{s}"),
            Statement::CreateTable(s) => write!(f, "{s}"),
            Statement::DropTable(s) => write!(f, "{s}"),
            Statement::CreateIndex(s) => write!(f, "{s}"),
            Statement::DropIndex(s) => write!(f, "{s}"),
            Statement::Insert(s) => write!(f, "{s}"),
            Statement::Update(s) => write!(f, "{s}"),
            Statement::Delete(s) => write!(f, "{s}"),
            Statement::Explain(s) => write!(f, "{s}"),
            Statement::Analyze(s) => write!(f, "{s}"),
        }
    }
}

/// Column definition within a `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column identifier name.
    pub name: String,
    /// Column data type.
    pub data_type: DataType,
    /// Whether this column is declared as the primary key.
    pub is_primary_key: bool,
    /// Whether this column permits `NULL` values.
    pub nullable: bool,
}

impl ColumnDef {
    /// Creates a new `ColumnDef`.
    pub fn new(
        name: impl Into<String>,
        data_type: DataType,
        is_primary_key: bool,
        nullable: bool,
    ) -> Self {
        Self {
            name: name.into(),
            data_type,
            is_primary_key,
            nullable,
        }
    }
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.data_type)?;
        if self.is_primary_key {
            write!(f, " PRIMARY KEY")?;
        }
        if !self.nullable && !self.is_primary_key {
            write!(f, " NOT NULL")?;
        }
        Ok(())
    }
}

/// AST node representing a `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    /// Target table name.
    pub table_name: String,
    /// List of declared column definitions.
    pub columns: Vec<ColumnDef>,
}

impl fmt::Display for CreateTableStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CREATE TABLE {} (", self.table_name)?;
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{col}")?;
        }
        write!(f, ")")
    }
}

/// AST node representing a `DROP TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTableStatement {
    /// Target table name to drop.
    pub table_name: String,
}

impl fmt::Display for DropTableStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DROP TABLE {}", self.table_name)
    }
}

/// AST node representing a `CREATE INDEX` or `CREATE UNIQUE INDEX` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndexStatement {
    /// Name of the index to create.
    pub name: String,
    /// Target table name.
    pub table_name: String,
    /// Target column name to index.
    pub column_name: String,
    /// Whether this index enforces uniqueness.
    pub unique: bool,
}

impl CreateIndexStatement {
    /// Creates a new `CreateIndexStatement`.
    pub fn new(
        name: impl Into<String>,
        table_name: impl Into<String>,
        column_name: impl Into<String>,
        unique: bool,
    ) -> Self {
        Self {
            name: name.into(),
            table_name: table_name.into(),
            column_name: column_name.into(),
            unique,
        }
    }
}

impl fmt::Display for CreateIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.unique {
            write!(
                f,
                "CREATE UNIQUE INDEX {} ON {} ({})",
                self.name, self.table_name, self.column_name
            )
        } else {
            write!(
                f,
                "CREATE INDEX {} ON {} ({})",
                self.name, self.table_name, self.column_name
            )
        }
    }
}

/// AST node representing a `DROP INDEX` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIndexStatement {
    /// Name of the index to drop.
    pub name: String,
}

impl DropIndexStatement {
    /// Creates a new `DropIndexStatement`.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl fmt::Display for DropIndexStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DROP INDEX {}", self.name)
    }
}

/// AST node representing an `INSERT INTO` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    /// Target table name.
    pub table_name: String,
    /// Optional explicit column list mapping.
    pub columns: Option<Vec<String>>,
    /// List of row expressions to insert.
    pub values: Vec<Vec<SqlExpr>>,
}

impl fmt::Display for InsertStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {}", self.table_name)?;
        if let Some(cols) = &self.columns {
            write!(f, " ({})", cols.join(", "))?;
        }
        write!(f, " VALUES ")?;
        for (i, row) in self.values.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "(")?;
            for (j, expr) in row.iter().enumerate() {
                if j > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{expr}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

/// An individual assignment in an `UPDATE` statement (`col = expr`).
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateAssignment {
    /// Column name being assigned.
    pub column_name: String,
    /// Value expression assigned to the column.
    pub expr: SqlExpr,
}

impl fmt::Display for UpdateAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.column_name, self.expr)
    }
}

/// AST node representing an `UPDATE` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    /// Target table name.
    pub table_name: String,
    /// List of column assignments.
    pub assignments: Vec<UpdateAssignment>,
    /// Optional filtering predicate in `WHERE`.
    pub selection: Option<SqlExpr>,
}

impl fmt::Display for UpdateStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UPDATE {} SET ", self.table_name)?;
        for (i, assign) in self.assignments.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{assign}")?;
        }
        if let Some(sel) = &self.selection {
            write!(f, " WHERE {sel}")?;
        }
        Ok(())
    }
}

/// AST node representing a `DELETE FROM` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    /// Target table name.
    pub table_name: String,
    /// Optional filtering predicate in `WHERE`.
    pub selection: Option<SqlExpr>,
}

impl fmt::Display for DeleteStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", self.table_name)?;
        if let Some(sel) = &self.selection {
            write!(f, " WHERE {sel}")?;
        }
        Ok(())
    }
}

/// AST node representing an `EXPLAIN` or `EXPLAIN ANALYZE` query introspection statement.
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStatement {
    /// Inner query statement to explain.
    pub query: Box<Statement>,
    /// Whether runtime execution metrics should be gathered (`EXPLAIN ANALYZE`).
    pub analyze: bool,
}

impl fmt::Display for ExplainStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.analyze {
            write!(f, "EXPLAIN ANALYZE {}", self.query)
        } else {
            write!(f, "EXPLAIN {}", self.query)
        }
    }
}

/// AST node representing an `ANALYZE` statistics collection statement.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeStatement {
    /// Target table name to analyze and collect statistics for.
    pub table_name: String,
}

impl fmt::Display for AnalyzeStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ANALYZE {}", self.table_name)
    }
}

/// Query definition of a CTE (non-recursive query, set-operation query, or recursive anchor/member pair).
#[derive(Debug, Clone, PartialEq)]
pub enum CteQuery {
    /// Non-recursive single SELECT query.
    NonRecursive(Box<SelectStatement>),
    /// Non-recursive set-operation query.
    SetOperation(Box<SetQuery>),
    /// Recursive query with anchor and recursive member connected by `UNION ALL`.
    Recursive {
        /// Initial anchor relation query.
        anchor: Box<SelectStatement>,
        /// Recursive member relation query referencing the recursive CTE name.
        recursive: Box<SelectStatement>,
    },
}

impl fmt::Display for CteQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CteQuery::NonRecursive(q) => write!(f, "{q}"),
            CteQuery::SetOperation(q) => write!(f, "{q}"),
            CteQuery::Recursive { anchor, recursive } => {
                write!(f, "{anchor} UNION ALL {recursive}")
            }
        }
    }
}

/// Supported SQL relational set operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetOperator {
    /// `UNION` (duplicate-eliminating union).
    Union,
    /// `UNION ALL` (duplicate-preserving concatenation).
    UnionAll,
    /// `INTERSECT` (duplicate-eliminating intersection).
    Intersect,
    /// `INTERSECT ALL` (multiset intersection).
    IntersectAll,
    /// `EXCEPT` (duplicate-eliminating difference).
    Except,
    /// `EXCEPT ALL` (multiset difference).
    ExceptAll,
}

impl fmt::Display for SetOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetOperator::Union => write!(f, "UNION"),
            SetOperator::UnionAll => write!(f, "UNION ALL"),
            SetOperator::Intersect => write!(f, "INTERSECT"),
            SetOperator::IntersectAll => write!(f, "INTERSECT ALL"),
            SetOperator::Except => write!(f, "EXCEPT"),
            SetOperator::ExceptAll => write!(f, "EXCEPT ALL"),
        }
    }
}

/// Recursive set-operation query body representation.
#[derive(Debug, Clone, PartialEq)]
pub enum SetQueryBody {
    /// Single `SELECT` query operand.
    Select(Box<SelectStatement>),
    /// Binary set operation combining left and right query operands.
    SetOperation {
        /// Set operator kind.
        op: SetOperator,
        /// Left-hand query operand.
        left: Box<SetQueryBody>,
        /// Right-hand query operand.
        right: Box<SetQueryBody>,
    },
    /// Parenthesized query operand (preserves explicit grouping and inner modifiers).
    Parenthesized(Box<SetQuery>),
}

impl fmt::Display for SetQueryBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetQueryBody::Select(s) => write!(f, "{s}"),
            SetQueryBody::SetOperation { op, left, right } => {
                write!(f, "{left} {op} {right}")
            }
            SetQueryBody::Parenthesized(q) => write!(f, "({q})"),
        }
    }
}

/// Complete SQL set-operation query statement with optional CTEs, sort, and pagination.
#[derive(Debug, Clone, PartialEq)]
pub struct SetQuery {
    /// Optional Common Table Expression (CTE) `WITH` clause.
    pub with: Option<WithClause>,
    /// Inner set-operation query body.
    pub body: SetQueryBody,
    /// Ordered list of sort expressions in the final `ORDER BY` clause.
    pub order_by: Vec<OrderByExpr>,
    /// Optional non-negative maximum row count in the final `LIMIT` clause.
    pub limit: Option<u64>,
    /// Optional non-negative row skip count in the final `OFFSET` clause.
    pub offset: Option<u64>,
}

impl fmt::Display for SetQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(with) = &self.with {
            write!(f, "{with} ")?;
        }
        write!(f, "{}", self.body)?;
        if !self.order_by.is_empty() {
            write!(f, " ORDER BY ")?;
            for (i, item) in self.order_by.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{item}")?;
            }
        }
        if let Some(limit) = self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        if let Some(offset) = self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
}

/// A Common Table Expression (`[WITH [RECURSIVE]] cte_name [(col1, ...)] AS (...)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    /// Name of the CTE relation.
    pub name: String,
    /// Optional explicit column name list.
    pub columns: Option<Vec<String>>,
    /// Inner query defining the CTE relation.
    pub query: CteQuery,
}

impl fmt::Display for Cte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(cols) = &self.columns {
            write!(f, "({})", cols.join(", "))?;
        }
        write!(f, " AS ({})", self.query)
    }
}

/// A `WITH [RECURSIVE]` clause containing one or more CTE definitions.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    /// Whether `RECURSIVE` was specified.
    pub recursive: bool,
    /// CTE definitions in declaration order.
    pub ctes: Vec<Cte>,
}

impl fmt::Display for WithClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.recursive {
            write!(f, "WITH RECURSIVE ")?;
        } else {
            write!(f, "WITH ")?;
        }
        for (i, cte) in self.ctes.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{cte}")?;
        }
        Ok(())
    }
}

/// Top-level AST node representing a SQL `SELECT` query.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    /// Optional Common Table Expression (CTE) `WITH` clause.
    pub with: Option<WithClause>,
    /// List of projection items in the `SELECT` clause (wildcard or expressions).
    pub projection: Vec<SelectItem>,
    /// Target table reference in the `FROM` clause.
    pub from: TableRef,
    /// Joined table clauses in order of declaration.
    pub joins: Vec<JoinClause>,
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

impl fmt::Display for SelectStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(with) = &self.with {
            write!(f, "{with} ")?;
        }
        write!(f, "SELECT ")?;
        for (i, item) in self.projection.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{item}")?;
        }
        write!(f, " FROM {}", self.from)?;
        for join in &self.joins {
            write!(f, " {join}")?;
        }
        if let Some(sel) = &self.selection {
            write!(f, " WHERE {sel}")?;
        }
        if !self.group_by.is_empty() {
            write!(f, " GROUP BY ")?;
            for (i, item) in self.group_by.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{item}")?;
            }
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        if !self.order_by.is_empty() {
            write!(f, " ORDER BY ")?;
            for (i, item) in self.order_by.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{item}")?;
            }
        }
        if let Some(limit) = self.limit {
            write!(f, " LIMIT {limit}")?;
        }
        if let Some(offset) = self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
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

/// Type of relational JOIN operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinType {
    /// INNER JOIN: Only pairs of rows satisfying the join predicate are emitted.
    Inner,
    /// LEFT OUTER JOIN: All left-side rows are emitted, with NULL-padded right side when no match.
    Left,
}

impl fmt::Display for JoinType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinType::Inner => write!(f, "INNER JOIN"),
            JoinType::Left => write!(f, "LEFT JOIN"),
        }
    }
}

/// A single `JOIN` clause in a `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    /// Relational join strategy (Inner or Left).
    pub join_type: JoinType,
    /// Right-hand table reference being joined.
    pub table: TableRef,
    /// Join condition boolean predicate (`ON <expr>`).
    pub on: SqlExpr,
}

impl fmt::Display for JoinClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ON {}", self.join_type, self.table, self.on)
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
    /// Addition (`+`)
    Add,
    /// Subtraction (`-`)
    Sub,
    /// Multiplication (`*`)
    Mul,
    /// Division (`/`)
    Div,
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
            SqlBinaryOp::Add => write!(f, "+"),
            SqlBinaryOp::Sub => write!(f, "-"),
            SqlBinaryOp::Mul => write!(f, "*"),
            SqlBinaryOp::Div => write!(f, "/"),
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

/// Supported SQL window functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowFunction {
    /// ROW_NUMBER()
    RowNumber,
    /// RANK()
    Rank,
    /// DENSE_RANK()
    DenseRank,
    /// LAG(expr [, offset [, default]])
    Lag,
    /// LEAD(expr [, offset [, default]])
    Lead,
    /// FIRST_VALUE(expr)
    FirstValue,
    /// LAST_VALUE(expr)
    LastValue,
}

impl fmt::Display for WindowFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowFunction::RowNumber => write!(f, "ROW_NUMBER"),
            WindowFunction::Rank => write!(f, "RANK"),
            WindowFunction::DenseRank => write!(f, "DENSE_RANK"),
            WindowFunction::Lag => write!(f, "LAG"),
            WindowFunction::Lead => write!(f, "LEAD"),
            WindowFunction::FirstValue => write!(f, "FIRST_VALUE"),
            WindowFunction::LastValue => write!(f, "LAST_VALUE"),
        }
    }
}

/// Boundary bound for a `ROWS` window frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowFrameBound {
    /// UNBOUNDED PRECEDING
    UnboundedPreceding,
    /// <n> PRECEDING
    Preceding(u64),
    /// CURRENT ROW
    CurrentRow,
    /// <n> FOLLOWING
    Following(u64),
    /// UNBOUNDED FOLLOWING
    UnboundedFollowing,
}

impl fmt::Display for WindowFrameBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowFrameBound::UnboundedPreceding => write!(f, "UNBOUNDED PRECEDING"),
            WindowFrameBound::Preceding(n) => write!(f, "{n} PRECEDING"),
            WindowFrameBound::CurrentRow => write!(f, "CURRENT ROW"),
            WindowFrameBound::Following(n) => write!(f, "{n} FOLLOWING"),
            WindowFrameBound::UnboundedFollowing => write!(f, "UNBOUNDED FOLLOWING"),
        }
    }
}

/// A window frame specification (`ROWS BETWEEN <start> AND <end>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowFrame {
    /// Frame starting boundary.
    pub start: WindowFrameBound,
    /// Frame ending boundary.
    pub end: WindowFrameBound,
}

impl fmt::Display for WindowFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ROWS BETWEEN {} AND {}", self.start, self.end)
    }
}

/// A window specification (`OVER ([PARTITION BY ...] [ORDER BY ...] [ROWS ...])`).
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// Optional partitioning expressions (`PARTITION BY ...`).
    pub partition_by: Vec<SqlExpr>,
    /// Optional ordering expressions (`ORDER BY ...`).
    pub order_by: Vec<OrderByExpr>,
    /// Optional explicit framing specification (`ROWS ...`).
    pub frame: Option<WindowFrame>,
}

impl fmt::Display for WindowSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OVER (")?;
        let mut parts = Vec::new();
        if !self.partition_by.is_empty() {
            let p: Vec<String> = self.partition_by.iter().map(|e| e.to_string()).collect();
            parts.push(format!("PARTITION BY {}", p.join(", ")));
        }
        if !self.order_by.is_empty() {
            let o: Vec<String> = self.order_by.iter().map(|e| e.to_string()).collect();
            parts.push(format!("ORDER BY {}", o.join(", ")));
        }
        if let Some(frame) = &self.frame {
            parts.push(format!("{frame}"));
        }
        write!(f, "{})", parts.join(" "))
    }
}

/// Abstract Syntax Tree (AST) node for SQL scalar expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlExpr {
    /// Constant literal value.
    Literal(SqlLiteral),
    /// Positional statement parameter placeholder (`?` with 1-indexed identifier).
    Parameter(usize),
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
    /// Window function applied to arguments with an OVER specification (`FUNC(...) OVER (...)`).
    Window {
        /// Window function kind.
        function: WindowFunction,
        /// Function arguments.
        args: Vec<SqlExpr>,
        /// Window specification.
        window_spec: WindowSpec,
    },
    /// Scalar subquery: `(SELECT ...)`.
    Subquery(Box<SelectStatement>),
    /// Scalar set-operation subquery: `(SELECT ... UNION SELECT ...)`.
    SetSubquery(Box<SetQuery>),
    /// `EXISTS (SELECT ...)` or `NOT EXISTS (SELECT ...)`.
    Exists {
        /// Inner select statement.
        subquery: Box<SelectStatement>,
        /// Whether the predicate is negated (`NOT EXISTS`).
        negated: bool,
    },
    /// `EXISTS (SELECT ... UNION ...)` or `NOT EXISTS (...)`.
    SetExists {
        /// Inner set query.
        subquery: Box<SetQuery>,
        /// Whether the predicate is negated (`NOT EXISTS`).
        negated: bool,
    },
    /// `expr [NOT] IN (SELECT ...)`.
    InSubquery {
        /// Evaluated scalar expression.
        expr: Box<SqlExpr>,
        /// Subquery producing set of candidate values.
        subquery: Box<SelectStatement>,
        /// Whether the condition is negated (`NOT IN`).
        negated: bool,
    },
    /// `expr [NOT] IN (SELECT ... UNION ...)`.
    InSetSubquery {
        /// Evaluated scalar expression.
        expr: Box<SqlExpr>,
        /// Subquery producing set of candidate values.
        subquery: Box<SetQuery>,
        /// Whether the condition is negated (`NOT IN`).
        negated: bool,
    },
    /// `expr [NOT] IN (val1, val2, ...)`.
    InList {
        /// Evaluated scalar expression.
        expr: Box<SqlExpr>,
        /// List of expressions.
        list: Vec<SqlExpr>,
        /// Whether the condition is negated (`NOT IN`).
        negated: bool,
    },
}

impl fmt::Display for SqlExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlExpr::Literal(lit) => write!(f, "{lit}"),
            SqlExpr::Parameter(_) => write!(f, "?"),
            SqlExpr::Identifier(name) => write!(f, "{name}"),
            SqlExpr::QualifiedIdentifier { qualifier, name } => write!(f, "{qualifier}.{name}"),
            SqlExpr::Binary { left, op, right } => write!(f, "({left} {op} {right})"),
            SqlExpr::Unary { op, expr } => write!(f, "({op} {expr})"),
            SqlExpr::IsNull(expr) => write!(f, "({expr} IS NULL)"),
            SqlExpr::IsNotNull(expr) => write!(f, "({expr} IS NOT NULL)"),
            SqlExpr::CountAll => write!(f, "COUNT(*)"),
            SqlExpr::Aggregate { function, arg } => write!(f, "{function}({arg})"),
            SqlExpr::Window {
                function,
                args,
                window_spec,
            } => {
                let args_str: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                write!(f, "{function}({}) {window_spec}", args_str.join(", "))
            }
            SqlExpr::Subquery(sub) => write!(f, "({sub})"),
            SqlExpr::SetSubquery(sub) => write!(f, "({sub})"),
            SqlExpr::Exists { subquery, negated } => {
                if *negated {
                    write!(f, "NOT EXISTS ({subquery})")
                } else {
                    write!(f, "EXISTS ({subquery})")
                }
            }
            SqlExpr::SetExists { subquery, negated } => {
                if *negated {
                    write!(f, "NOT EXISTS ({subquery})")
                } else {
                    write!(f, "EXISTS ({subquery})")
                }
            }
            SqlExpr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                if *negated {
                    write!(f, "({expr} NOT IN ({subquery}))")
                } else {
                    write!(f, "({expr} IN ({subquery}))")
                }
            }
            SqlExpr::InSetSubquery {
                expr,
                subquery,
                negated,
            } => {
                if *negated {
                    write!(f, "({expr} NOT IN ({subquery}))")
                } else {
                    write!(f, "({expr} IN ({subquery}))")
                }
            }
            SqlExpr::InList {
                expr,
                list,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                write!(f, "({expr} {not}IN (")?;
                for (i, item) in list.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "))")
            }
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
