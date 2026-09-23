use crate::error::{Error, Result};
use crate::sql::ast::{
    OrderByExpr, OrderDirection, SelectItem, SelectStatement, SqlBinaryOp, SqlExpr, SqlLiteral,
    SqlUnaryOp, TableRef,
};
use crate::sql::span::SourceSpan;
use crate::sql::token::{Token, TokenKind};

/// Precedence levels for SQL expression operators (lowest to highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    Lowest = 0,
    Or = 1,
    And = 2,
    Comparison = 3,
    IsNull = 4,
    Unary = 5,
}

/// Recursive descent and Pratt precedence parser for SQL `SELECT` queries.
pub struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
}

impl<'a> Parser<'a> {
    /// Creates a new `Parser` over the provided slice of tokens.
    pub fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, cursor: 0 }
    }

    /// Parses a complete SQL `SELECT` statement.
    ///
    /// # Grammatical Structure
    /// `SELECT <projection> FROM <table_ref> [WHERE <expr>] [ORDER BY <order_by>] [LIMIT <n>] [OFFSET <n>] [;]`
    pub fn parse_select(&mut self) -> Result<SelectStatement> {
        let select_token = self.peek();
        if select_token.kind != TokenKind::Select {
            return Err(Error::SqlParseError {
                message: format!("expected SELECT statement, found {}", select_token.kind),
                start: select_token.span.start,
                end: select_token.span.end,
            });
        }
        self.advance(); // consume SELECT

        // 1. Projection list
        let projection = self.parse_projection_list()?;

        // 2. FROM clause (mandatory)
        let from_token = self.peek();
        if from_token.kind != TokenKind::From {
            return Err(Error::SqlParseError {
                message: format!(
                    "expected FROM clause after SELECT list, found {}",
                    from_token.kind
                ),
                start: from_token.span.start,
                end: from_token.span.end,
            });
        }
        self.advance(); // consume FROM

        let from = self.parse_table_ref()?;

        // 3. Optional WHERE clause
        let selection = if self.peek().kind == TokenKind::Where {
            self.advance(); // consume WHERE
            Some(self.parse_expr()?)
        } else {
            None
        };

        // 4. Optional ORDER BY clause
        let order_by = if self.peek().kind == TokenKind::Order {
            self.advance(); // consume ORDER
            let by_token = self.peek();
            if by_token.kind != TokenKind::By {
                return Err(Error::SqlParseError {
                    message: format!("expected BY after ORDER, found {}", by_token.kind),
                    start: by_token.span.start,
                    end: by_token.span.end,
                });
            }
            self.advance(); // consume BY
            self.parse_order_by_list()?
        } else {
            Vec::new()
        };

        // 5. Optional LIMIT clause
        let limit = if self.peek().kind == TokenKind::Limit {
            self.advance(); // consume LIMIT
            Some(self.parse_non_negative_integer("LIMIT")?)
        } else {
            None
        };

        // 6. Optional OFFSET clause
        let offset = if self.peek().kind == TokenKind::Offset {
            self.advance(); // consume OFFSET
            Some(self.parse_non_negative_integer("OFFSET")?)
        } else {
            None
        };

        // 7. Optional trailing semicolon
        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        // 8. Enforce EOF (no unexpected trailing tokens)
        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(SelectStatement {
            projection,
            from,
            selection,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_projection_list(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();

        if self.peek().kind == TokenKind::Asterisk {
            self.advance();
            items.push(SelectItem::Wildcard);
            return Ok(items);
        }

        loop {
            let expr = self.parse_expr()?;
            let alias = self.parse_optional_alias()?;

            items.push(SelectItem::Expression { expr, alias });

            if self.peek().kind == TokenKind::Comma {
                self.advance(); // consume comma
                if self.peek().kind == TokenKind::From || self.peek().kind == TokenKind::Eof {
                    let err_token = self.peek();
                    return Err(Error::SqlParseError {
                        message: "expected expression after comma in SELECT list".to_string(),
                        start: err_token.span.start,
                        end: err_token.span.end,
                    });
                }
            } else {
                break;
            }
        }

        if items.is_empty() {
            let span = self.peek().span;
            return Err(Error::SqlParseError {
                message: "SELECT list cannot be empty".to_string(),
                start: span.start,
                end: span.end,
            });
        }

        Ok(items)
    }

    fn parse_optional_alias(&mut self) -> Result<Option<String>> {
        if self.peek().kind == TokenKind::As {
            self.advance(); // consume AS
            let ident_token = self.peek();
            match &ident_token.kind {
                TokenKind::Ident(name) => {
                    let name_clone = name.clone();
                    self.advance();
                    Ok(Some(name_clone))
                }
                other => Err(Error::SqlParseError {
                    message: format!("expected identifier after AS, found {other}"),
                    start: ident_token.span.start,
                    end: ident_token.span.end,
                }),
            }
        } else if let TokenKind::Ident(name) = &self.peek().kind {
            // Check if next is a keyword or punctuation starting another clause
            let name_clone = name.clone();
            self.advance();
            Ok(Some(name_clone))
        } else {
            Ok(None)
        }
    }

    fn parse_table_ref(&mut self) -> Result<TableRef> {
        let table_token = self.peek();
        let name = match &table_token.kind {
            TokenKind::Ident(s) => s.clone(),
            other => {
                return Err(Error::SqlParseError {
                    message: format!("expected table name in FROM clause, found {other}"),
                    start: table_token.span.start,
                    end: table_token.span.end,
                })
            }
        };
        self.advance();

        let alias = if self.peek().kind == TokenKind::As {
            self.advance(); // consume AS
            let alias_token = self.peek();
            match &alias_token.kind {
                TokenKind::Ident(s) => {
                    let a = s.clone();
                    self.advance();
                    Some(a)
                }
                other => {
                    return Err(Error::SqlParseError {
                        message: format!("expected table alias after AS, found {other}"),
                        start: alias_token.span.start,
                        end: alias_token.span.end,
                    })
                }
            }
        } else if let TokenKind::Ident(s) = &self.peek().kind {
            let a = s.clone();
            self.advance();
            Some(a)
        } else {
            None
        };

        Ok(TableRef { name, alias })
    }

    fn parse_order_by_list(&mut self) -> Result<Vec<OrderByExpr>> {
        let mut list = Vec::new();

        loop {
            let expr = self.parse_expr()?;
            let direction = if self.peek().kind == TokenKind::Asc {
                self.advance();
                OrderDirection::Asc
            } else if self.peek().kind == TokenKind::Desc {
                self.advance();
                OrderDirection::Desc
            } else {
                OrderDirection::Asc
            };

            list.push(OrderByExpr::new(expr, direction));

            if self.peek().kind == TokenKind::Comma {
                self.advance(); // consume comma
            } else {
                break;
            }
        }

        if list.is_empty() {
            let span = self.peek().span;
            return Err(Error::SqlParseError {
                message: "ORDER BY list cannot be empty".to_string(),
                start: span.start,
                end: span.end,
            });
        }

        Ok(list)
    }

    fn parse_non_negative_integer(&mut self, clause: &str) -> Result<u64> {
        let token = self.peek();
        match token.kind {
            TokenKind::Integer(n) => {
                if n < 0 {
                    return Err(Error::SqlParseError {
                        message: format!("{clause} value cannot be negative: {n}"),
                        start: token.span.start,
                        end: token.span.end,
                    });
                }
                self.advance();
                Ok(n as u64)
            }
            ref other => Err(Error::SqlParseError {
                message: format!("expected non-negative integer for {clause}, found {other}"),
                start: token.span.start,
                end: token.span.end,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Expression Parsing (Pratt Precedence Climbing)
    // -----------------------------------------------------------------------

    /// Parses a scalar SQL expression.
    pub fn parse_expr(&mut self) -> Result<SqlExpr> {
        self.parse_precedence(Precedence::Lowest)
    }

    fn parse_precedence(&mut self, prec: Precedence) -> Result<SqlExpr> {
        let mut left = self.parse_prefix()?;

        while prec < self.current_precedence() {
            left = self.parse_infix(left)?;
        }

        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<SqlExpr> {
        let token = self.peek();
        let span = token.span;

        match &token.kind {
            TokenKind::Integer(i) => {
                let val = *i;
                self.advance();
                Ok(SqlExpr::Literal(SqlLiteral::Integer(val)))
            }
            TokenKind::Float(v) => {
                let val = *v;
                self.advance();
                Ok(SqlExpr::Literal(SqlLiteral::Float(val)))
            }
            TokenKind::String(s) => {
                let val = s.clone();
                self.advance();
                Ok(SqlExpr::Literal(SqlLiteral::String(val)))
            }
            TokenKind::True => {
                self.advance();
                Ok(SqlExpr::Literal(SqlLiteral::Boolean(true)))
            }
            TokenKind::False => {
                self.advance();
                Ok(SqlExpr::Literal(SqlLiteral::Boolean(false)))
            }
            TokenKind::Null => {
                self.advance();
                Ok(SqlExpr::Literal(SqlLiteral::Null))
            }
            TokenKind::Ident(name) => {
                let val = name.clone();
                self.advance();
                Ok(SqlExpr::Identifier(val))
            }
            TokenKind::LParen => {
                self.advance(); // consume '('
                let inner = self.parse_expr()?;
                let close = self.peek();
                if close.kind != TokenKind::RParen {
                    return Err(Error::SqlParseError {
                        message: format!("expected ')' to close parenthesis, found {}", close.kind),
                        start: close.span.start,
                        end: close.span.end,
                    });
                }
                self.advance(); // consume ')'
                Ok(inner)
            }
            TokenKind::Not => {
                self.advance(); // consume NOT
                let expr = self.parse_precedence(Precedence::Unary)?;
                Ok(SqlExpr::Unary {
                    op: SqlUnaryOp::Not,
                    expr: Box::new(expr),
                })
            }
            TokenKind::Minus => {
                self.advance(); // consume '-'
                                // If followed immediately by an integer or float, fold into negative literal
                if let TokenKind::Integer(i) = self.peek().kind {
                    let val = i;
                    self.advance();
                    Ok(SqlExpr::Literal(SqlLiteral::Integer(-val)))
                } else if let TokenKind::Float(v) = self.peek().kind {
                    let val = v;
                    self.advance();
                    Ok(SqlExpr::Literal(SqlLiteral::Float(-val)))
                } else {
                    let expr = self.parse_precedence(Precedence::Unary)?;
                    Ok(SqlExpr::Unary {
                        op: SqlUnaryOp::Neg,
                        expr: Box::new(expr),
                    })
                }
            }
            other => Err(Error::SqlParseError {
                message: format!("expected expression, found {other}"),
                start: span.start,
                end: span.end,
            }),
        }
    }

    fn parse_infix(&mut self, left: SqlExpr) -> Result<SqlExpr> {
        let token = self.peek();

        match token.kind {
            TokenKind::Or => {
                self.advance();
                let right = self.parse_precedence(Precedence::Or)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Or,
                    right: Box::new(right),
                })
            }
            TokenKind::And => {
                self.advance();
                let right = self.parse_precedence(Precedence::And)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::And,
                    right: Box::new(right),
                })
            }
            TokenKind::Eq => {
                self.advance();
                let right = self.parse_precedence(Precedence::Comparison)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Eq,
                    right: Box::new(right),
                })
            }
            TokenKind::NotEq => {
                self.advance();
                let right = self.parse_precedence(Precedence::Comparison)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::NotEq,
                    right: Box::new(right),
                })
            }
            TokenKind::Lt => {
                self.advance();
                let right = self.parse_precedence(Precedence::Comparison)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Lt,
                    right: Box::new(right),
                })
            }
            TokenKind::LtEq => {
                self.advance();
                let right = self.parse_precedence(Precedence::Comparison)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::LtEq,
                    right: Box::new(right),
                })
            }
            TokenKind::Gt => {
                self.advance();
                let right = self.parse_precedence(Precedence::Comparison)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Gt,
                    right: Box::new(right),
                })
            }
            TokenKind::GtEq => {
                self.advance();
                let right = self.parse_precedence(Precedence::Comparison)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::GtEq,
                    right: Box::new(right),
                })
            }
            TokenKind::Is => {
                self.advance(); // consume IS
                let next = self.peek();
                if next.kind == TokenKind::Not {
                    self.advance(); // consume NOT
                    let null_token = self.peek();
                    if null_token.kind == TokenKind::Null {
                        self.advance(); // consume NULL
                        Ok(SqlExpr::IsNotNull(Box::new(left)))
                    } else {
                        Err(Error::SqlParseError {
                            message: format!(
                                "expected NULL after IS NOT, found {}",
                                null_token.kind
                            ),
                            start: null_token.span.start,
                            end: null_token.span.end,
                        })
                    }
                } else if next.kind == TokenKind::Null {
                    self.advance(); // consume NULL
                    Ok(SqlExpr::IsNull(Box::new(left)))
                } else {
                    Err(Error::SqlParseError {
                        message: format!("expected NULL or NOT NULL after IS, found {}", next.kind),
                        start: next.span.start,
                        end: next.span.end,
                    })
                }
            }
            _ => Ok(left),
        }
    }

    fn current_precedence(&self) -> Precedence {
        match self.peek().kind {
            TokenKind::Or => Precedence::Or,
            TokenKind::And => Precedence::And,
            TokenKind::Eq
            | TokenKind::NotEq
            | TokenKind::Lt
            | TokenKind::LtEq
            | TokenKind::Gt
            | TokenKind::GtEq => Precedence::Comparison,
            TokenKind::Is => Precedence::IsNull,
            _ => Precedence::Lowest,
        }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.cursor).unwrap_or_else(|| {
            static EOF_TOKEN: Token = Token {
                kind: TokenKind::Eof,
                span: SourceSpan { start: 0, end: 0 },
            };
            &EOF_TOKEN
        })
    }

    fn advance(&mut self) -> &Token {
        let prev = self.cursor;
        if self.cursor < self.tokens.len() {
            self.cursor += 1;
        }
        self.tokens.get(prev).unwrap_or_else(|| {
            static EOF_TOKEN: Token = Token {
                kind: TokenKind::Eof,
                span: SourceSpan { start: 0, end: 0 },
            };
            &EOF_TOKEN
        })
    }
}
