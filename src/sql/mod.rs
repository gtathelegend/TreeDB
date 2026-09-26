pub mod ast;
pub mod binder;
pub mod lexer;
pub mod parser;
pub mod prepared;
pub mod span;
pub mod token;

pub use ast::{
    AggregateFunction, AnalyzeStatement, ColumnDef, CreateIndexStatement, CreateTableStatement,
    Cte, CteQuery, DeleteStatement, DropIndexStatement, DropTableStatement, ExplainStatement,
    InsertStatement, JoinClause, JoinType, OrderByExpr, OrderDirection, SelectItem,
    SelectStatement, SetOperator, SetQuery, SetQueryBody, SqlBinaryOp, SqlExpr, SqlLiteral,
    SqlUnaryOp, Statement, TableRef, UpdateAssignment, UpdateStatement, WindowFrame,
    WindowFrameBound, WindowFunction, WindowSpec, WithClause,
};
pub use binder::{
    bind_create_index, bind_drop_index, bind_expr, bind_grouped_expr, bind_select,
    bind_select_multi, bind_select_multi_with_catalog, bind_select_with_context, BindingContext,
    BoundCreateIndex, BoundCte, BoundCteKind, BoundDropIndex, BoundJoin, BoundSelect, TableBinding,
    TableCatalog,
};
pub use lexer::{Lexer, MAX_IDENTIFIER_LEN};
pub use parser::Parser;
pub use prepared::{infer_statement_parameters, ParameterMetadata, PreparedStatement};
pub use span::SourceSpan;
pub use token::{Token, TokenKind};

use crate::error::Result;

/// Tokenizes an SQL query string into a vector of lexical `Token`s.
pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let lexer = Lexer::new(sql);
    lexer.tokenize()
}

/// Parses an SQL string into a top-level `Statement` AST (`SELECT`, `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, `DELETE`).
pub fn parse_statement(sql: &str) -> Result<Statement> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser::new(&tokens);
    parser.parse_statement()
}

/// Parses an SQL query string into a `SelectStatement` AST.
pub fn parse(sql: &str) -> Result<SelectStatement> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser::new(&tokens);
    parser.parse_select()
}

pub use parse as parse_sql;
pub use parse_statement as parse_statement_sql;
pub use tokenize as tokenize_sql;
