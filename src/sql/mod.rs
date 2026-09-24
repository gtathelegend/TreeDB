pub mod ast;
pub mod binder;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod token;

pub use ast::{
    AggregateFunction, OrderByExpr, OrderDirection, SelectItem, SelectStatement, SqlBinaryOp,
    SqlExpr, SqlLiteral, SqlUnaryOp, TableRef,
};
pub use binder::{bind_expr, bind_select, BindingContext, BoundSelect};
pub use lexer::{Lexer, MAX_IDENTIFIER_LEN};
pub use parser::Parser;
pub use span::SourceSpan;
pub use token::{Token, TokenKind};

use crate::error::Result;

/// Tokenizes an SQL query string into a vector of lexical `Token`s.
pub fn tokenize(sql: &str) -> Result<Vec<Token>> {
    let lexer = Lexer::new(sql);
    lexer.tokenize()
}

/// Parses an SQL query string into a `SelectStatement` AST.
pub fn parse(sql: &str) -> Result<SelectStatement> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser::new(&tokens);
    parser.parse_select()
}

pub use parse as parse_sql;
pub use tokenize as tokenize_sql;
