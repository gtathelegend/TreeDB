use crate::engine::types::DataType;
use crate::error::{Error, Result};
use crate::sql::ast::{
    AnalyzeStatement, ColumnDef, CreateIndexStatement, CreateTableStatement, Cte, CteQuery,
    DeleteStatement, DerivedQuery, DropIndexStatement, DropTableStatement, ExplainStatement,
    InsertStatement, JoinClause, JoinType, OrderByExpr, OrderDirection, SelectItem,
    SelectStatement, SetOperator, SetQuery, SetQueryBody, SqlBinaryOp, SqlExpr, SqlLiteral,
    SqlUnaryOp, Statement, TableRef, UpdateAssignment, UpdateStatement, WindowFrame,
    WindowFrameBound, WindowFunction, WindowSpec, WithClause,
};
use crate::sql::span::SourceSpan;
use crate::sql::token::{Token, TokenKind};

/// Precedence levels for SQL expression operators (lowest to highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Precedence {
    Lowest = 0,
    Or = 1,
    And = 2,
    Not = 3,
    Comparison = 4,
    IsNull = 5,
    AddSub = 6,
    MulDiv = 7,
    UnaryNeg = 8,
}

/// Recursive descent and Pratt precedence parser for SQL statements.
pub struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    param_counter: usize,
}

impl<'a> Parser<'a> {
    /// Creates a new `Parser` over the provided slice of tokens.
    pub fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            cursor: 0,
            param_counter: 0,
        }
    }

    /// Parses an arbitrary top-level SQL statement (`SELECT`, set-operation query, `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, `DELETE`, `EXPLAIN`, `ANALYZE`).
    pub fn parse_statement(&mut self) -> Result<Statement> {
        let token = self.peek();
        match token.kind {
            TokenKind::Explain => Ok(Statement::Explain(self.parse_explain()?)),
            TokenKind::Analyze => Ok(Statement::Analyze(self.parse_analyze()?)),
            TokenKind::Select | TokenKind::With | TokenKind::LParen => {
                self.parse_query_statement()
            }
            TokenKind::Create => match self.peek_ahead(1).kind {
                TokenKind::Table => Ok(Statement::CreateTable(self.parse_create_table()?)),
                TokenKind::Index | TokenKind::Unique => {
                    Ok(Statement::CreateIndex(self.parse_create_index()?))
                }
                _ => {
                    let tok = self.peek_ahead(1).clone();
                    Err(Error::SqlParseError {
                        message: format!(
                            "expected TABLE, INDEX, or UNIQUE after CREATE, found {}",
                            tok.kind
                        ),
                        start: tok.span.start,
                        end: tok.span.end,
                    })
                }
            },
            TokenKind::Drop => match self.peek_ahead(1).kind {
                TokenKind::Table => Ok(Statement::DropTable(self.parse_drop_table()?)),
                TokenKind::Index => Ok(Statement::DropIndex(self.parse_drop_index()?)),
                _ => {
                    let tok = self.peek_ahead(1).clone();
                    Err(Error::SqlParseError {
                        message: format!(
                            "expected TABLE or INDEX after DROP, found {}",
                            tok.kind
                        ),
                        start: tok.span.start,
                        end: tok.span.end,
                    })
                }
            },
            TokenKind::Insert => Ok(Statement::Insert(self.parse_insert()?)),
            TokenKind::Update => Ok(Statement::Update(self.parse_update()?)),
            TokenKind::Delete => Ok(Statement::Delete(self.parse_delete()?)),
            _ => Err(Error::SqlParseError {
                message: format!(
                    "unexpected token {}, expected SQL statement (SELECT, CREATE, DROP, INSERT, UPDATE, DELETE, EXPLAIN, ANALYZE)",
                    token.kind
                ),
                start: token.span.start,
                end: token.span.end,
            }),
        }
    }

    /// Parses a complete SQL `CREATE TABLE` statement.
    ///
    /// # Syntax
    /// `CREATE TABLE <table_name> ( <col_def> [, <col_def> ...] [, PRIMARY KEY (<col>)] ) [;]`
    pub fn parse_create_table(&mut self) -> Result<CreateTableStatement> {
        let start_span = self.peek().span;
        if self.peek().kind != TokenKind::Create {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected CREATE statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume CREATE

        if self.peek().kind != TokenKind::Table {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected TABLE after CREATE, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume TABLE

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in CREATE TABLE, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        if self.peek().kind != TokenKind::LParen {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected '(' after table name in CREATE TABLE, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume '('

        let mut columns: Vec<ColumnDef> = Vec::new();
        let mut table_pk_col: Option<String> = None;

        loop {
            if self.peek().kind == TokenKind::RParen {
                break;
            }

            if self.peek().kind == TokenKind::Primary {
                let primary_span = self.peek().span;
                self.advance(); // consume PRIMARY
                if self.peek().kind != TokenKind::Key {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!("expected KEY after PRIMARY, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume KEY

                if self.peek().kind != TokenKind::LParen {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!("expected '(' after PRIMARY KEY, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume '('

                let pk_col_name = match &self.peek().kind {
                    TokenKind::Ident(s) => s.clone(),
                    _ => {
                        let tok = self.peek().clone();
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected column name in PRIMARY KEY (...), found {}",
                                tok.kind
                            ),
                            start: tok.span.start,
                            end: tok.span.end,
                        });
                    }
                };
                self.advance(); // consume column name

                if self.peek().kind != TokenKind::RParen {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected ')' after column name in PRIMARY KEY, found {}",
                            tok.kind
                        ),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                let end_pk_span = self.peek().span;
                self.advance(); // consume ')'

                if table_pk_col.is_some() {
                    return Err(Error::SqlParseError {
                        message: "multiple PRIMARY KEY constraints defined in CREATE TABLE"
                            .to_string(),
                        start: primary_span.start,
                        end: end_pk_span.end,
                    });
                }
                table_pk_col = Some(pk_col_name);
            } else {
                let col_name = match &self.peek().kind {
                    TokenKind::Ident(s) => s.clone(),
                    _ => {
                        let tok = self.peek().clone();
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected column name in CREATE TABLE, found {}",
                                tok.kind
                            ),
                            start: tok.span.start,
                            end: tok.span.end,
                        });
                    }
                };
                self.advance(); // consume column name

                let data_type = match self.peek().kind {
                    TokenKind::BooleanType => DataType::Boolean,
                    TokenKind::BigIntType => DataType::Int64,
                    TokenKind::UBigIntType => DataType::UInt64,
                    TokenKind::DoubleType => DataType::Float64,
                    TokenKind::TextType => DataType::String,
                    TokenKind::BlobType => DataType::Bytes,
                    _ => {
                        let tok = self.peek().clone();
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected data type for column '{col_name}', found {}",
                                tok.kind
                            ),
                            start: tok.span.start,
                            end: tok.span.end,
                        });
                    }
                };
                self.advance(); // consume type

                let mut is_pk = false;
                let mut nullable = true;

                loop {
                    match self.peek().kind {
                        TokenKind::Primary => {
                            self.advance(); // consume PRIMARY
                            if self.peek().kind != TokenKind::Key {
                                let tok = self.peek().clone();
                                return Err(Error::SqlParseError {
                                    message: format!(
                                        "expected KEY after PRIMARY, found {}",
                                        tok.kind
                                    ),
                                    start: tok.span.start,
                                    end: tok.span.end,
                                });
                            }
                            self.advance(); // consume KEY
                            is_pk = true;
                            nullable = false;
                        }
                        TokenKind::Not => {
                            self.advance(); // consume NOT
                            if self.peek().kind != TokenKind::Null {
                                let tok = self.peek().clone();
                                return Err(Error::SqlParseError {
                                    message: format!("expected NULL after NOT, found {}", tok.kind),
                                    start: tok.span.start,
                                    end: tok.span.end,
                                });
                            }
                            self.advance(); // consume NULL
                            nullable = false;
                        }
                        TokenKind::Null => {
                            self.advance(); // consume NULL
                            nullable = true;
                        }
                        _ => break,
                    }
                }

                columns.push(ColumnDef::new(col_name, data_type, is_pk, nullable));
            }

            if self.peek().kind == TokenKind::Comma {
                self.advance(); // consume ','
            } else if self.peek().kind == TokenKind::RParen {
                break;
            } else {
                let err_tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected ',' or ')' in CREATE TABLE column definitions, found {}",
                        err_tok.kind
                    ),
                    start: err_tok.span.start,
                    end: err_tok.span.end,
                });
            }
        }

        if self.peek().kind != TokenKind::RParen {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected ')' at end of CREATE TABLE column definitions, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        let end_span = self.peek().span;
        self.advance(); // consume ')'

        // Apply table-level primary key constraint if specified
        if let Some(pk_name) = table_pk_col {
            let col = columns
                .iter_mut()
                .find(|c| c.name == pk_name)
                .ok_or_else(|| Error::SqlParseError {
                    message: format!(
                        "PRIMARY KEY column '{pk_name}' not found in CREATE TABLE column list"
                    ),
                    start: start_span.start,
                    end: end_span.end,
                })?;
            if col.is_primary_key {
                return Err(Error::SqlParseError {
                    message: format!("column '{pk_name}' is already defined as PRIMARY KEY"),
                    start: start_span.start,
                    end: end_span.end,
                });
            }
            col.is_primary_key = true;
            col.nullable = false;
        }

        // Validate that exactly one column is primary key
        let pk_count = columns.iter().filter(|c| c.is_primary_key).count();
        if pk_count == 0 {
            return Err(Error::SqlParseError {
                message: "table must contain a PRIMARY KEY column".to_string(),
                start: start_span.start,
                end: end_span.end,
            });
        }
        if pk_count > 1 {
            return Err(Error::SqlParseError {
                message: "multiple PRIMARY KEY columns are not supported".to_string(),
                start: start_span.start,
                end: end_span.end,
            });
        }

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after CREATE TABLE statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(CreateTableStatement {
            table_name,
            columns,
        })
    }

    /// Parses a complete SQL `DROP TABLE` statement.
    ///
    /// # Syntax
    /// `DROP TABLE <table_name> [;]`
    pub fn parse_drop_table(&mut self) -> Result<DropTableStatement> {
        if self.peek().kind != TokenKind::Drop {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected DROP statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume DROP

        if self.peek().kind != TokenKind::Table {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected TABLE after DROP, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume TABLE

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in DROP TABLE, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after DROP TABLE statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(DropTableStatement { table_name })
    }

    /// Parses a complete SQL `CREATE INDEX` or `CREATE UNIQUE INDEX` statement.
    ///
    /// # Syntax
    /// `CREATE [UNIQUE] INDEX <index_name> ON <table_name> ( <col_name> ) [;]`
    pub fn parse_create_index(&mut self) -> Result<CreateIndexStatement> {
        if self.peek().kind != TokenKind::Create {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected CREATE statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume CREATE

        let mut unique = false;
        if self.peek().kind == TokenKind::Unique {
            self.advance(); // consume UNIQUE
            unique = true;
        }

        if self.peek().kind != TokenKind::Index {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected INDEX in CREATE INDEX statement, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume INDEX

        let name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected index name in CREATE INDEX, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume index name

        if self.peek().kind != TokenKind::On {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected ON after index name in CREATE INDEX, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume ON

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in CREATE INDEX, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        if self.peek().kind != TokenKind::LParen {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected '(' after table name in CREATE INDEX, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume '('

        if self.peek().kind == TokenKind::RParen {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: "CREATE INDEX requires at least one column name, found empty column list"
                    .to_string(),
                start: tok.span.start,
                end: tok.span.end,
            });
        }

        let mut column_names = Vec::new();
        let mut seen = std::collections::HashSet::new();

        loop {
            let col_tok = self.peek().clone();
            let col = match &col_tok.kind {
                TokenKind::Ident(s) => s.clone(),
                _ => {
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected column name in CREATE INDEX, expressions and function calls are not supported; found {}",
                            col_tok.kind
                        ),
                        start: col_tok.span.start,
                        end: col_tok.span.end,
                    });
                }
            };
            self.advance(); // consume column name

            if !seen.insert(col.clone()) {
                return Err(Error::SqlParseError {
                    message: format!("duplicate column '{col}' in CREATE INDEX"),
                    start: col_tok.span.start,
                    end: col_tok.span.end,
                });
            }

            column_names.push(col);

            if column_names.len() > crate::index::composite::MAX_INDEX_COLUMNS {
                return Err(Error::SqlParseError {
                    message: format!(
                        "CREATE INDEX exceeds maximum column limit of {}",
                        crate::index::composite::MAX_INDEX_COLUMNS
                    ),
                    start: col_tok.span.start,
                    end: col_tok.span.end,
                });
            }

            if self.peek().kind == TokenKind::Comma {
                self.advance(); // consume comma
                if self.peek().kind == TokenKind::RParen {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: "trailing comma in CREATE INDEX column list is not allowed"
                            .to_string(),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
            } else if self.peek().kind == TokenKind::RParen {
                break;
            } else {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected ',' or ')' in CREATE INDEX, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        }

        self.advance(); // consume ')'

        let mut include_column_names = Vec::new();
        if self.peek().kind == TokenKind::Include {
            self.advance(); // consume INCLUDE

            if self.peek().kind != TokenKind::LParen {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected '(' after INCLUDE, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
            self.advance(); // consume '('

            loop {
                let col_tok = self.peek().clone();
                let col = match &col_tok.kind {
                    TokenKind::Ident(name) => name.clone(),
                    _ => {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected column name in INCLUDE clause, found {}",
                                col_tok.kind
                            ),
                            start: col_tok.span.start,
                            end: col_tok.span.end,
                        });
                    }
                };
                self.advance(); // consume column name

                if include_column_names.contains(&col) {
                    return Err(Error::SqlParseError {
                        message: format!("duplicate column '{col}' in INCLUDE clause"),
                        start: col_tok.span.start,
                        end: col_tok.span.end,
                    });
                }

                include_column_names.push(col);

                if include_column_names.len() > crate::index::covering::MAX_INDEX_INCLUDE_COLUMNS {
                    return Err(Error::SqlParseError {
                        message: format!(
                            "INCLUDE clause exceeds maximum column limit of {}",
                            crate::index::covering::MAX_INDEX_INCLUDE_COLUMNS
                        ),
                        start: col_tok.span.start,
                        end: col_tok.span.end,
                    });
                }

                if self.peek().kind == TokenKind::Comma {
                    self.advance(); // consume comma
                    if self.peek().kind == TokenKind::RParen {
                        let tok = self.peek().clone();
                        return Err(Error::SqlParseError {
                            message: "trailing comma in INCLUDE column list is not allowed"
                                .to_string(),
                            start: tok.span.start,
                            end: tok.span.end,
                        });
                    }
                } else if self.peek().kind == TokenKind::RParen {
                    break;
                } else {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected ',' or ')' in INCLUDE clause, found {}",
                            tok.kind
                        ),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
            }

            self.advance(); // consume ')'
        }

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after CREATE INDEX statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(CreateIndexStatement::new_covering(
            name,
            table_name,
            column_names,
            include_column_names,
            unique,
        ))
    }

    /// Parses a complete SQL `DROP INDEX` statement.
    ///
    /// # Syntax
    /// `DROP INDEX <index_name> [;]`
    pub fn parse_drop_index(&mut self) -> Result<DropIndexStatement> {
        if self.peek().kind != TokenKind::Drop {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected DROP statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume DROP

        if self.peek().kind != TokenKind::Index {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected INDEX after DROP, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume INDEX

        let name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected index name in DROP INDEX, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume index name

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after DROP INDEX statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(DropIndexStatement { name })
    }

    /// Parses a complete SQL `EXPLAIN` statement.
    ///
    /// # Syntax
    /// `EXPLAIN <query> [;]`
    pub fn parse_explain(&mut self) -> Result<ExplainStatement> {
        let start_span = self.peek().span;
        if self.peek().kind != TokenKind::Explain {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected EXPLAIN statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume EXPLAIN

        let mut analyze = false;
        if self.peek().kind == TokenKind::Analyze {
            analyze = true;
            self.advance(); // consume ANALYZE
        }

        let next_tok = self.peek().clone();
        match next_tok.kind {
            TokenKind::Insert => Err(Error::UnsupportedExplainTarget("INSERT".to_string())),
            TokenKind::Update => Err(Error::UnsupportedExplainTarget("UPDATE".to_string())),
            TokenKind::Delete => Err(Error::UnsupportedExplainTarget("DELETE".to_string())),
            TokenKind::Analyze => Err(Error::UnsupportedExplainTarget("ANALYZE".to_string())),
            TokenKind::Create => match self.peek_ahead(1).kind {
                TokenKind::Table => {
                    Err(Error::UnsupportedExplainTarget("CREATE TABLE".to_string()))
                }
                TokenKind::Index | TokenKind::Unique => {
                    Err(Error::UnsupportedExplainTarget("CREATE INDEX".to_string()))
                }
                _ => Err(Error::UnsupportedExplainTarget("CREATE".to_string())),
            },
            TokenKind::Drop => match self.peek_ahead(1).kind {
                TokenKind::Table => Err(Error::UnsupportedExplainTarget("DROP TABLE".to_string())),
                TokenKind::Index => Err(Error::UnsupportedExplainTarget("DROP INDEX".to_string())),
                _ => Err(Error::UnsupportedExplainTarget("DROP".to_string())),
            },
            TokenKind::Select | TokenKind::With | TokenKind::LParen => {
                let query_stmt = self.parse_query_statement()?;
                Ok(ExplainStatement {
                    query: Box::new(query_stmt),
                    analyze,
                })
            }
            TokenKind::Eof => {
                let prefix = if analyze {
                    "EXPLAIN ANALYZE"
                } else {
                    "EXPLAIN"
                };
                Err(Error::SqlParseError {
                    message: format!("expected query statement after {prefix}, found EOF"),
                    start: start_span.start,
                    end: next_tok.span.end,
                })
            }
            _ => {
                let prefix = if analyze {
                    "EXPLAIN ANALYZE"
                } else {
                    "EXPLAIN"
                };
                Err(Error::SqlParseError {
                    message: format!(
                        "expected query statement after {prefix}, found {}",
                        next_tok.kind
                    ),
                    start: next_tok.span.start,
                    end: next_tok.span.end,
                })
            }
        }
    }

    /// Parses a complete SQL `ANALYZE` statement.
    ///
    /// # Syntax
    /// `ANALYZE <table_name> [;]`
    pub fn parse_analyze(&mut self) -> Result<AnalyzeStatement> {
        if self.peek().kind != TokenKind::Analyze {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected ANALYZE statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume ANALYZE

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in ANALYZE, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after ANALYZE statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(AnalyzeStatement { table_name })
    }

    /// Parses a complete SQL `INSERT INTO` statement.
    ///
    /// # Syntax
    /// `INSERT INTO <table_name> [(col1, ...)] VALUES (val1, ...), (val2, ...) [;]`
    pub fn parse_insert(&mut self) -> Result<InsertStatement> {
        let start_span = self.peek().span;
        if self.peek().kind != TokenKind::Insert {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected INSERT statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume INSERT

        if self.peek().kind != TokenKind::Into {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected INTO after INSERT, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume INTO

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in INSERT INTO, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        let columns = if self.peek().kind == TokenKind::LParen {
            self.advance(); // consume '('
            let mut cols = Vec::new();
            loop {
                let col_name = match &self.peek().kind {
                    TokenKind::Ident(s) => s.clone(),
                    _ => {
                        let tok = self.peek().clone();
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected column identifier in INSERT INTO column list, found {}",
                                tok.kind
                            ),
                            start: tok.span.start,
                            end: tok.span.end,
                        });
                    }
                };
                self.advance();
                cols.push(col_name);

                if self.peek().kind == TokenKind::Comma {
                    self.advance();
                } else if self.peek().kind == TokenKind::RParen {
                    break;
                } else {
                    let err_tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected ',' or ')' in INSERT INTO column list, found {}",
                            err_tok.kind
                        ),
                        start: err_tok.span.start,
                        end: err_tok.span.end,
                    });
                }
            }
            self.advance(); // consume ')'
            Some(cols)
        } else {
            None
        };

        if self.peek().kind != TokenKind::Values {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected VALUES in INSERT INTO statement, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        let values_span = self.peek().span;
        self.advance(); // consume VALUES

        let mut values = Vec::new();
        loop {
            if self.peek().kind != TokenKind::LParen {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected '(' before row values in INSERT, found {}",
                        tok.kind
                    ),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
            self.advance(); // consume '('

            let mut row = Vec::new();
            loop {
                let expr = self.parse_expr()?;
                row.push(expr);

                if self.peek().kind == TokenKind::Comma {
                    self.advance();
                } else if self.peek().kind == TokenKind::RParen {
                    break;
                } else {
                    let err_tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected ',' or ')' in INSERT row values, found {}",
                            err_tok.kind
                        ),
                        start: err_tok.span.start,
                        end: err_tok.span.end,
                    });
                }
            }
            self.advance(); // consume ')'
            values.push(row);

            if self.peek().kind == TokenKind::Comma {
                self.advance();
            } else {
                break;
            }
        }

        if values.is_empty() {
            return Err(Error::SqlParseError {
                message: "INSERT INTO must have at least one VALUES row".to_string(),
                start: start_span.start,
                end: values_span.end,
            });
        }

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after INSERT statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(InsertStatement {
            table_name,
            columns,
            values,
        })
    }

    /// Parses a complete SQL `UPDATE` statement.
    ///
    /// # Syntax
    /// `UPDATE <table_name> SET <col1> = <expr1> [, <col2> = <expr2> ...] [WHERE <expr>] [;]`
    pub fn parse_update(&mut self) -> Result<UpdateStatement> {
        if self.peek().kind != TokenKind::Update {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected UPDATE statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume UPDATE

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in UPDATE, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        if self.peek().kind != TokenKind::Set {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected SET after table name in UPDATE, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume SET

        let mut assignments = Vec::new();
        loop {
            let column_name = match &self.peek().kind {
                TokenKind::Ident(s) => s.clone(),
                _ => {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected column identifier in UPDATE SET, found {}",
                            tok.kind
                        ),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
            };
            self.advance(); // consume column name

            if self.peek().kind != TokenKind::Eq {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected '=' after column in UPDATE SET, found {}",
                        tok.kind
                    ),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
            self.advance(); // consume '='

            let expr = self.parse_expr()?;
            assignments.push(UpdateAssignment { column_name, expr });

            if self.peek().kind == TokenKind::Comma {
                self.advance();
            } else {
                break;
            }
        }

        let selection = if self.peek().kind == TokenKind::Where {
            self.advance(); // consume WHERE
            Some(self.parse_expr()?)
        } else {
            None
        };

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after UPDATE statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(UpdateStatement {
            table_name,
            assignments,
            selection,
        })
    }

    /// Parses a complete SQL `DELETE FROM` statement.
    ///
    /// # Syntax
    /// `DELETE FROM <table_name> [WHERE <expr>] [;]`
    pub fn parse_delete(&mut self) -> Result<DeleteStatement> {
        if self.peek().kind != TokenKind::Delete {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected DELETE statement, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume DELETE

        if self.peek().kind != TokenKind::From {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected FROM after DELETE, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume FROM

        let table_name = match &self.peek().kind {
            TokenKind::Ident(s) => s.clone(),
            _ => {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected table name in DELETE FROM, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
        };
        self.advance(); // consume table name

        let selection = if self.peek().kind == TokenKind::Where {
            self.advance(); // consume WHERE
            Some(self.parse_expr()?)
        } else {
            None
        };

        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        if self.peek().kind != TokenKind::Eof {
            let trailing = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "unexpected trailing token after DELETE statement: {}",
                    trailing.kind
                ),
                start: trailing.span.start,
                end: trailing.span.end,
            });
        }

        Ok(DeleteStatement {
            table_name,
            selection,
        })
    }

    /// Parses a complete SQL `SELECT` statement.
    ///
    /// # Grammatical Structure
    /// `SELECT <projection> FROM <table_ref> [WHERE <expr>] [ORDER BY <order_by>] [LIMIT <n>] [OFFSET <n>] [;]`
    pub fn parse_select(&mut self) -> Result<SelectStatement> {
        let stmt = self.parse_select_clauses()?;

        // Optional trailing semicolon
        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        // Enforce EOF (no unexpected trailing tokens)
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

        Ok(stmt)
    }

    /// Parses the core clauses of a SQL `SELECT` statement without trailing ORDER BY, LIMIT, OFFSET or statement delimiters.
    pub fn parse_select_core_clauses(&mut self) -> Result<SelectStatement> {
        let with_clause = if self.peek().kind == TokenKind::With {
            Some(self.parse_with_clause()?)
        } else {
            None
        };

        let select_token = self.peek().clone();
        if select_token.kind != TokenKind::Select {
            if matches!(
                select_token.kind,
                TokenKind::Insert | TokenKind::Update | TokenKind::Delete | TokenKind::Create
            ) {
                return Err(Error::UnsupportedStatement(format!(
                    "CTE is not supported with {}",
                    select_token.kind
                )));
            }
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

        // 3. Optional JOIN clauses
        let mut joins = Vec::new();
        loop {
            let next_token = self.peek();
            let join_type = match next_token.kind {
                TokenKind::Inner => {
                    self.advance(); // consume INNER
                    let join_token = self.peek();
                    if join_token.kind != TokenKind::Join {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected JOIN after INNER, found {}",
                                join_token.kind
                            ),
                            start: join_token.span.start,
                            end: join_token.span.end,
                        });
                    }
                    self.advance(); // consume JOIN
                    JoinType::Inner
                }
                TokenKind::Join => {
                    self.advance(); // consume JOIN
                    JoinType::Inner
                }
                TokenKind::Left => {
                    self.advance(); // consume LEFT
                    if self.peek().kind == TokenKind::Outer {
                        self.advance(); // consume OUTER
                    }
                    let join_token = self.peek();
                    if join_token.kind != TokenKind::Join {
                        return Err(Error::SqlParseError {
                            message: format!("expected JOIN after LEFT, found {}", join_token.kind),
                            start: join_token.span.start,
                            end: join_token.span.end,
                        });
                    }
                    self.advance(); // consume JOIN
                    JoinType::Left
                }
                TokenKind::Right => {
                    self.advance(); // consume RIGHT
                    if self.peek().kind == TokenKind::Outer {
                        self.advance(); // consume OUTER
                    }
                    let join_token = self.peek();
                    if join_token.kind != TokenKind::Join {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected JOIN after RIGHT, found {}",
                                join_token.kind
                            ),
                            start: join_token.span.start,
                            end: join_token.span.end,
                        });
                    }
                    self.advance(); // consume JOIN
                    JoinType::Right
                }
                TokenKind::Full => {
                    self.advance(); // consume FULL
                    if self.peek().kind == TokenKind::Outer {
                        self.advance(); // consume OUTER
                    }
                    let join_token = self.peek();
                    if join_token.kind != TokenKind::Join {
                        return Err(Error::SqlParseError {
                            message: format!("expected JOIN after FULL, found {}", join_token.kind),
                            start: join_token.span.start,
                            end: join_token.span.end,
                        });
                    }
                    self.advance(); // consume JOIN
                    JoinType::Full
                }
                _ => break,
            };

            let table = self.parse_table_ref()?;
            let on_token = self.peek();
            if on_token.kind != TokenKind::On {
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected ON after table in JOIN clause, found {}",
                        on_token.kind
                    ),
                    start: on_token.span.start,
                    end: on_token.span.end,
                });
            }
            self.advance(); // consume ON
            let on = self.parse_expr()?;
            joins.push(JoinClause {
                join_type,
                table,
                on,
            });
        }

        // 4. Optional WHERE clause
        let selection = if self.peek().kind == TokenKind::Where {
            self.advance(); // consume WHERE
            Some(self.parse_expr()?)
        } else {
            None
        };

        // 5. Optional GROUP BY clause
        let group_by = if self.peek().kind == TokenKind::Group {
            self.advance(); // consume GROUP
            let by_token = self.peek();
            if by_token.kind != TokenKind::By {
                return Err(Error::SqlParseError {
                    message: format!("expected BY after GROUP, found {}", by_token.kind),
                    start: by_token.span.start,
                    end: by_token.span.end,
                });
            }
            self.advance(); // consume BY
            self.parse_group_by_list()?
        } else {
            Vec::new()
        };

        // 6. Optional HAVING clause
        let having = if self.peek().kind == TokenKind::Having {
            self.advance(); // consume HAVING
            Some(self.parse_expr()?)
        } else {
            None
        };

        Ok(SelectStatement {
            with: with_clause,
            projection,
            from,
            joins,
            selection,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
    }

    /// Parses the clauses of a SQL `SELECT` statement including trailing ORDER BY, LIMIT, and OFFSET.
    pub fn parse_select_clauses(&mut self) -> Result<SelectStatement> {
        let mut stmt = self.parse_select_core_clauses()?;

        // 7. Optional ORDER BY clause
        if self.peek().kind == TokenKind::Order {
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
            stmt.order_by = self.parse_order_by_list()?;
        }

        // 8. Optional LIMIT clause
        if self.peek().kind == TokenKind::Limit {
            self.advance(); // consume LIMIT
            stmt.limit = Some(self.parse_non_negative_integer("LIMIT")?);
        }

        // 9. Optional OFFSET clause
        if self.peek().kind == TokenKind::Offset {
            self.advance(); // consume OFFSET
            stmt.offset = Some(self.parse_non_negative_integer("OFFSET")?);
        }

        Ok(stmt)
    }

    /// Checks if the next tokens form a set operator without consuming them.
    fn peek_set_operator(&self) -> Option<(SetOperator, usize)> {
        match self.peek().kind {
            TokenKind::Union => {
                if self.peek_ahead(1).kind == TokenKind::All {
                    Some((SetOperator::UnionAll, 2))
                } else {
                    Some((SetOperator::Union, 1))
                }
            }
            TokenKind::Intersect => {
                if self.peek_ahead(1).kind == TokenKind::All {
                    Some((SetOperator::IntersectAll, 2))
                } else {
                    Some((SetOperator::Intersect, 1))
                }
            }
            TokenKind::Except => {
                if self.peek_ahead(1).kind == TokenKind::All {
                    Some((SetOperator::ExceptAll, 2))
                } else {
                    Some((SetOperator::Except, 1))
                }
            }
            _ => None,
        }
    }

    /// Consumes a set operator that was previously peeked.
    fn consume_set_operator(&mut self, token_count: usize) {
        for _ in 0..token_count {
            self.advance();
        }
    }

    fn set_operator_precedence(op: SetOperator) -> u8 {
        match op {
            SetOperator::Intersect | SetOperator::IntersectAll => 2,
            SetOperator::Union
            | SetOperator::UnionAll
            | SetOperator::Except
            | SetOperator::ExceptAll => 1,
        }
    }

    fn parse_set_query_primary(&mut self) -> Result<SetQueryBody> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::LParen => {
                self.advance(); // consume '('
                let inner = self.parse_query_expression()?;
                let close = self.peek();
                if close.kind != TokenKind::RParen {
                    return Err(Error::SqlParseError {
                        message: format!("expected ')' to close query, found {}", close.kind),
                        start: close.span.start,
                        end: close.span.end,
                    });
                }
                self.advance(); // consume ')'
                Ok(SetQueryBody::Parenthesized(Box::new(inner)))
            }
            TokenKind::Select => {
                let stmt = self.parse_select_core_clauses()?;
                Ok(SetQueryBody::Select(Box::new(stmt)))
            }
            _ => Err(Error::SqlParseError {
                message: format!("expected SELECT or '(' in query, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            }),
        }
    }

    fn parse_set_query_body_precedence(&mut self, min_prec: u8) -> Result<SetQueryBody> {
        let mut left = self.parse_set_query_primary()?;

        while let Some((op, count)) = self.peek_set_operator() {
            let prec = Self::set_operator_precedence(op);
            if prec < min_prec {
                break;
            }

            self.consume_set_operator(count);

            let next_tok = self.peek().clone();
            if next_tok.kind != TokenKind::Select
                && next_tok.kind != TokenKind::LParen
                && next_tok.kind != TokenKind::With
            {
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected query operand after {}, found {}",
                        op, next_tok.kind
                    ),
                    start: next_tok.span.start,
                    end: next_tok.span.end,
                });
            }

            let right = self.parse_set_query_body_precedence(prec + 1)?;
            left = SetQueryBody::SetOperation {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    /// Parses a complete SQL query expression including optional WITH, set operations, ORDER BY, LIMIT, and OFFSET.
    pub fn parse_query_expression(&mut self) -> Result<SetQuery> {
        let with = if self.peek().kind == TokenKind::With {
            Some(self.parse_with_clause()?)
        } else {
            None
        };

        let body = self.parse_set_query_body_precedence(0)?;

        let mut order_by = Vec::new();
        if self.peek().kind == TokenKind::Order {
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
            order_by = self.parse_order_by_list()?;
        }

        let limit = if self.peek().kind == TokenKind::Limit {
            self.advance(); // consume LIMIT
            Some(self.parse_non_negative_integer("LIMIT")?)
        } else {
            None
        };

        let offset = if self.peek().kind == TokenKind::Offset {
            self.advance(); // consume OFFSET
            Some(self.parse_non_negative_integer("OFFSET")?)
        } else {
            None
        };

        Ok(SetQuery {
            with,
            body,
            order_by,
            limit,
            offset,
        })
    }

    /// Parses an arbitrary query statement (SELECT or SetQuery) ending with an optional semicolon and EOF.
    pub fn parse_query_statement(&mut self) -> Result<Statement> {
        let query = self.parse_query_expression()?;

        // Optional trailing semicolon
        if self.peek().kind == TokenKind::Semicolon {
            self.advance();
        }

        // Enforce EOF
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

        match query.body {
            SetQueryBody::Select(mut select_stmt) => {
                if !query.order_by.is_empty() {
                    select_stmt.order_by = query.order_by;
                }
                if query.limit.is_some() {
                    select_stmt.limit = query.limit;
                }
                if query.offset.is_some() {
                    select_stmt.offset = query.offset;
                }
                if query.with.is_some() {
                    select_stmt.with = query.with;
                }
                Ok(Statement::Select(*select_stmt))
            }
            _ => Ok(Statement::SetQuery(query)),
        }
    }

    /// Parses a SQL `WITH` clause containing one or more CTE definitions.
    pub fn parse_with_clause(&mut self) -> Result<WithClause> {
        let with_tok = self.peek().clone();
        if with_tok.kind != TokenKind::With {
            return Err(Error::SqlParseError {
                message: format!("expected WITH, found {}", with_tok.kind),
                start: with_tok.span.start,
                end: with_tok.span.end,
            });
        }
        self.advance(); // consume WITH

        // Check for WITH RECURSIVE
        let recursive = if self.peek().kind == TokenKind::Recursive {
            self.advance();
            true
        } else if let TokenKind::Ident(s) = &self.peek().kind {
            if s.eq_ignore_ascii_case("RECURSIVE") {
                self.advance();
                true
            } else {
                false
            }
        } else {
            false
        };

        let mut ctes = Vec::new();
        loop {
            let name_tok = self.peek().clone();
            let cte_name = match &name_tok.kind {
                TokenKind::Ident(s) => s.clone(),
                _ => {
                    return Err(Error::SqlParseError {
                        message: format!("expected CTE name after WITH, found {}", name_tok.kind),
                        start: name_tok.span.start,
                        end: name_tok.span.end,
                    });
                }
            };
            self.advance(); // consume CTE name

            // Check for duplicate CTE name in the same WITH clause
            if ctes.iter().any(|c: &Cte| c.name == cte_name) {
                return Err(Error::DuplicateCteName(cte_name));
            }

            // Optional column list: `cte_name(col1, col2, ...)`
            let columns = if self.peek().kind == TokenKind::LParen {
                self.advance(); // consume '('
                let mut cols = Vec::new();
                loop {
                    let col_tok = self.peek().clone();
                    let col_name = match &col_tok.kind {
                        TokenKind::Ident(s) => s.clone(),
                        _ => {
                            return Err(Error::SqlParseError {
                                message: format!(
                                    "expected column identifier in CTE column list, found {}",
                                    col_tok.kind
                                ),
                                start: col_tok.span.start,
                                end: col_tok.span.end,
                            });
                        }
                    };
                    self.advance();

                    if cols.contains(&col_name) {
                        return Err(Error::DuplicateCteColumnName {
                            cte: cte_name.clone(),
                            column: col_name,
                        });
                    }
                    cols.push(col_name);

                    if self.peek().kind == TokenKind::Comma {
                        self.advance();
                    } else if self.peek().kind == TokenKind::RParen {
                        break;
                    } else {
                        let err_tok = self.peek().clone();
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ',' or ')' in CTE column list, found {}",
                                err_tok.kind
                            ),
                            start: err_tok.span.start,
                            end: err_tok.span.end,
                        });
                    }
                }
                self.advance(); // consume ')'
                Some(cols)
            } else {
                None
            };

            // Expect `AS`
            let as_tok = self.peek().clone();
            if as_tok.kind != TokenKind::As {
                return Err(Error::SqlParseError {
                    message: format!("expected AS after CTE name, found {}", as_tok.kind),
                    start: as_tok.span.start,
                    end: as_tok.span.end,
                });
            }
            self.advance(); // consume AS

            // Expect `(`
            let lparen_tok = self.peek().clone();
            if lparen_tok.kind != TokenKind::LParen {
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected '(' after AS in CTE definition, found {}",
                        lparen_tok.kind
                    ),
                    start: lparen_tok.span.start,
                    end: lparen_tok.span.end,
                });
            }
            self.advance(); // consume '('

            // Reject empty body `()`
            if self.peek().kind == TokenKind::RParen {
                let rparen_tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: "empty CTE body () is not allowed".to_string(),
                    start: lparen_tok.span.start,
                    end: rparen_tok.span.end,
                });
            }

            // Parse CTE query body
            let query = if recursive {
                let first_query = self.parse_select_clauses()?;
                if self.peek().kind == TokenKind::Union {
                    self.advance(); // consume UNION
                    if self.peek().kind == TokenKind::All {
                        self.advance(); // consume ALL
                    } else {
                        return Err(Error::UnsupportedSetOperation(
                            "UNION without ALL is not supported".to_string(),
                        ));
                    }

                    let body_tok = self.peek().clone();
                    if body_tok.kind != TokenKind::Select && body_tok.kind != TokenKind::With {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected SELECT after UNION ALL in CTE body, found {}",
                                body_tok.kind
                            ),
                            start: body_tok.span.start,
                            end: body_tok.span.end,
                        });
                    }

                    let second_query = self.parse_select_clauses()?;

                    if self.peek().kind == TokenKind::Union {
                        return Err(Error::InvalidRecursiveCteShape(cte_name.clone()));
                    }

                    CteQuery::Recursive {
                        anchor: Box::new(first_query),
                        recursive: Box::new(second_query),
                    }
                } else {
                    CteQuery::NonRecursive(Box::new(first_query))
                }
            } else {
                let query_expr = self.parse_query_expression()?;
                match query_expr.body {
                    SetQueryBody::Select(mut select_stmt) if query_expr.with.is_none() => {
                        if !query_expr.order_by.is_empty() {
                            select_stmt.order_by = query_expr.order_by;
                        }
                        if query_expr.limit.is_some() {
                            select_stmt.limit = query_expr.limit;
                        }
                        if query_expr.offset.is_some() {
                            select_stmt.offset = query_expr.offset;
                        }
                        CteQuery::NonRecursive(select_stmt)
                    }
                    _ => CteQuery::SetOperation(Box::new(query_expr)),
                }
            };

            // Expect `)`
            let rparen_tok = self.peek().clone();
            if rparen_tok.kind != TokenKind::RParen {
                return Err(Error::SqlParseError {
                    message: format!("expected ')' after CTE query, found {}", rparen_tok.kind),
                    start: rparen_tok.span.start,
                    end: rparen_tok.span.end,
                });
            }
            self.advance(); // consume ')'

            ctes.push(Cte {
                name: cte_name,
                columns,
                query,
            });

            if self.peek().kind == TokenKind::Comma {
                self.advance(); // consume ','
            } else {
                break;
            }
        }

        if ctes.is_empty() {
            return Err(Error::SqlParseError {
                message: "WITH clause must define at least one CTE".to_string(),
                start: with_tok.span.start,
                end: with_tok.span.end,
            });
        }

        Ok(WithClause { recursive, ctes })
    }

    fn parse_projection_list(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();

        if self.peek().kind == TokenKind::Asterisk {
            self.advance();
            items.push(SelectItem::Wildcard);
            return Ok(items);
        }

        loop {
            // Check for qualified wildcard: `tbl.*`
            if let TokenKind::Ident(tbl) = &self.peek().kind {
                if self.peek_ahead(1).kind == TokenKind::Dot
                    && self.peek_ahead(2).kind == TokenKind::Asterisk
                {
                    let tbl_name = tbl.clone();
                    self.advance(); // consume ident
                    self.advance(); // consume '.'
                    self.advance(); // consume '*'
                    items.push(SelectItem::QualifiedWildcard(tbl_name));

                    if self.peek().kind == TokenKind::Comma {
                        self.advance(); // consume comma
                        if self.peek().kind == TokenKind::From || self.peek().kind == TokenKind::Eof
                        {
                            let err_token = self.peek();
                            return Err(Error::SqlParseError {
                                message: "expected expression after comma in SELECT list"
                                    .to_string(),
                                start: err_token.span.start,
                                end: err_token.span.end,
                            });
                        }
                        continue;
                    } else {
                        break;
                    }
                }
            }

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
        if self.peek().kind == TokenKind::LParen {
            let lparen_span = self.peek().span;
            self.advance(); // consume '('

            let query_expr = self.parse_query_expression()?;
            let rparen_tok = self.peek().clone();
            if rparen_tok.kind != TokenKind::RParen {
                return Err(Error::SqlParseError {
                    message: format!(
                        "expected ')' after derived table query, found {}",
                        rparen_tok.kind
                    ),
                    start: rparen_tok.span.start,
                    end: rparen_tok.span.end,
                });
            }
            self.advance(); // consume ')'

            let alias = self.parse_optional_alias()?;
            let alias = match alias {
                Some(a) => a,
                None => {
                    let end_span = self.peek().span;
                    return Err(Error::SqlParseError {
                        message: "every derived table must have an alias".to_string(),
                        start: lparen_span.start,
                        end: end_span.end,
                    });
                }
            };

            let derived_query = match query_expr.body {
                SetQueryBody::Select(mut select_stmt) => {
                    if !query_expr.order_by.is_empty() {
                        select_stmt.order_by = query_expr.order_by;
                    }
                    if query_expr.limit.is_some() {
                        select_stmt.limit = query_expr.limit;
                    }
                    if query_expr.offset.is_some() {
                        select_stmt.offset = query_expr.offset;
                    }
                    if query_expr.with.is_some() {
                        select_stmt.with = query_expr.with;
                    }
                    DerivedQuery::Select(select_stmt)
                }
                _ => DerivedQuery::SetOperation(Box::new(query_expr)),
            };

            return Ok(TableRef::Derived {
                query: derived_query,
                alias,
            });
        }

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

        let alias = self.parse_optional_alias()?;

        Ok(TableRef::Table { name, alias })
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

    fn parse_group_by_list(&mut self) -> Result<Vec<SqlExpr>> {
        let mut list = Vec::new();

        loop {
            let expr = self.parse_expr()?;
            list.push(expr);

            if self.peek().kind == TokenKind::Comma {
                self.advance(); // consume comma
            } else {
                break;
            }
        }

        if list.is_empty() {
            let span = self.peek().span;
            return Err(Error::SqlParseError {
                message: "GROUP BY list cannot be empty".to_string(),
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
            TokenKind::QuestionMark => {
                self.advance();
                self.param_counter += 1;
                Ok(SqlExpr::Parameter(self.param_counter))
            }
            TokenKind::Ident(name) => {
                let val = name.clone();
                if self.peek_ahead(1).kind == TokenKind::Dot {
                    if let TokenKind::Ident(col) = &self.peek_ahead(2).kind {
                        let col_name = col.clone();
                        self.advance(); // consume qualifier
                        self.advance(); // consume '.'
                        self.advance(); // consume column ident
                        return Ok(SqlExpr::QualifiedIdentifier {
                            qualifier: val,
                            name: col_name,
                        });
                    }
                }

                if self.peek_ahead(1).kind == TokenKind::LParen {
                    let upper = val.to_ascii_uppercase();
                    match upper.as_str() {
                        "COUNT" => {
                            self.advance(); // consume 'COUNT'
                            self.advance(); // consume '('
                            if self.peek().kind == TokenKind::Asterisk {
                                self.advance(); // consume '*'
                                let close = self.peek();
                                if close.kind != TokenKind::RParen {
                                    return Err(Error::SqlParseError {
                                        message: format!(
                                            "expected ')' after COUNT(*), found {}",
                                            close.kind
                                        ),
                                        start: close.span.start,
                                        end: close.span.end,
                                    });
                                }
                                self.advance(); // consume ')'
                                return Ok(SqlExpr::CountAll);
                            } else if self.peek().kind == TokenKind::RParen {
                                return Err(Error::InvalidAggregateArgument(
                                    "COUNT() requires an argument or '*'".to_string(),
                                ));
                            } else {
                                let arg = self.parse_expr()?;
                                if matches!(&arg, SqlExpr::CountAll | SqlExpr::Aggregate { .. }) {
                                    return Err(Error::NestedAggregate(format!(
                                        "nested aggregate inside COUNT: {arg}"
                                    )));
                                }
                                let close = self.peek();
                                if close.kind != TokenKind::RParen {
                                    return Err(Error::SqlParseError {
                                        message: format!(
                                            "expected ')' after function argument, found {}",
                                            close.kind
                                        ),
                                        start: close.span.start,
                                        end: close.span.end,
                                    });
                                }
                                self.advance(); // consume ')'
                                return Ok(SqlExpr::Aggregate {
                                    function: crate::sql::ast::AggregateFunction::Count,
                                    arg: Box::new(arg),
                                });
                            }
                        }
                        "SUM" | "AVG" | "MIN" | "MAX" => {
                            let func = match upper.as_str() {
                                "SUM" => crate::sql::ast::AggregateFunction::Sum,
                                "AVG" => crate::sql::ast::AggregateFunction::Avg,
                                "MIN" => crate::sql::ast::AggregateFunction::Min,
                                "MAX" => crate::sql::ast::AggregateFunction::Max,
                                _ => unreachable!(),
                            };
                            self.advance(); // consume function name
                            self.advance(); // consume '('
                            if self.peek().kind == TokenKind::Asterisk {
                                return Err(Error::InvalidAggregateArgument(format!(
                                    "{func}(*) is not valid"
                                )));
                            } else if self.peek().kind == TokenKind::RParen {
                                return Err(Error::InvalidAggregateArgument(format!(
                                    "{func}() requires an argument"
                                )));
                            } else {
                                let arg = self.parse_expr()?;
                                if matches!(&arg, SqlExpr::CountAll | SqlExpr::Aggregate { .. }) {
                                    return Err(Error::NestedAggregate(format!(
                                        "nested aggregate inside {func}: {arg}"
                                    )));
                                }
                                let close = self.peek();
                                if close.kind != TokenKind::RParen {
                                    return Err(Error::SqlParseError {
                                        message: format!(
                                            "expected ')' after function argument, found {}",
                                            close.kind
                                        ),
                                        start: close.span.start,
                                        end: close.span.end,
                                    });
                                }
                                self.advance(); // consume ')'
                                return Ok(SqlExpr::Aggregate {
                                    function: func,
                                    arg: Box::new(arg),
                                });
                            }
                        }
                        "ROW_NUMBER" => {
                            return self.parse_window_function(WindowFunction::RowNumber);
                        }
                        "RANK" => {
                            return self.parse_window_function(WindowFunction::Rank);
                        }
                        "DENSE_RANK" => {
                            return self.parse_window_function(WindowFunction::DenseRank);
                        }
                        "LAG" => {
                            return self.parse_window_function(WindowFunction::Lag);
                        }
                        "LEAD" => {
                            return self.parse_window_function(WindowFunction::Lead);
                        }
                        "FIRST_VALUE" => {
                            return self.parse_window_function(WindowFunction::FirstValue);
                        }
                        "LAST_VALUE" => {
                            return self.parse_window_function(WindowFunction::LastValue);
                        }
                        _ => {
                            return Err(Error::UnknownAggregateFunction(val));
                        }
                    }
                }

                self.advance();
                Ok(SqlExpr::Identifier(val))
            }
            TokenKind::LParen => {
                self.advance(); // consume '('
                if self.peek().kind == TokenKind::Select || self.peek().kind == TokenKind::With {
                    let subquery = self.parse_query_expression()?;
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ')' to close subquery, found {}",
                                close.kind
                            ),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    match subquery.body {
                        SetQueryBody::Select(mut select_stmt) if subquery.with.is_none() => {
                            if !subquery.order_by.is_empty() {
                                select_stmt.order_by = subquery.order_by;
                            }
                            if subquery.limit.is_some() {
                                select_stmt.limit = subquery.limit;
                            }
                            if subquery.offset.is_some() {
                                select_stmt.offset = subquery.offset;
                            }
                            Ok(SqlExpr::Subquery(select_stmt))
                        }
                        _ => Ok(SqlExpr::SetSubquery(Box::new(subquery))),
                    }
                } else {
                    let inner = self.parse_expr()?;
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ')' to close parenthesis, found {}",
                                close.kind
                            ),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    Ok(inner)
                }
            }
            TokenKind::Exists => {
                self.advance(); // consume EXISTS
                if self.peek().kind != TokenKind::LParen {
                    let tok = self.peek();
                    return Err(Error::SqlParseError {
                        message: format!("expected '(' after EXISTS, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume '('
                let subquery = self.parse_query_expression()?;
                let close = self.peek();
                if close.kind != TokenKind::RParen {
                    return Err(Error::SqlParseError {
                        message: format!(
                            "expected ')' to close EXISTS subquery, found {}",
                            close.kind
                        ),
                        start: close.span.start,
                        end: close.span.end,
                    });
                }
                self.advance(); // consume ')'
                match subquery.body {
                    SetQueryBody::Select(mut select_stmt) if subquery.with.is_none() => {
                        if !subquery.order_by.is_empty() {
                            select_stmt.order_by = subquery.order_by;
                        }
                        if subquery.limit.is_some() {
                            select_stmt.limit = subquery.limit;
                        }
                        if subquery.offset.is_some() {
                            select_stmt.offset = subquery.offset;
                        }
                        Ok(SqlExpr::Exists {
                            subquery: select_stmt,
                            negated: false,
                        })
                    }
                    _ => Ok(SqlExpr::SetExists {
                        subquery: Box::new(subquery),
                        negated: false,
                    }),
                }
            }
            TokenKind::Not => {
                self.advance(); // consume NOT
                if self.peek().kind == TokenKind::Exists {
                    self.advance(); // consume EXISTS
                    if self.peek().kind != TokenKind::LParen {
                        let tok = self.peek();
                        return Err(Error::SqlParseError {
                            message: format!("expected '(' after NOT EXISTS, found {}", tok.kind),
                            start: tok.span.start,
                            end: tok.span.end,
                        });
                    }
                    self.advance(); // consume '('
                    let subquery = self.parse_query_expression()?;
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ')' to close NOT EXISTS subquery, found {}",
                                close.kind
                            ),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    match subquery.body {
                        SetQueryBody::Select(mut select_stmt) if subquery.with.is_none() => {
                            if !subquery.order_by.is_empty() {
                                select_stmt.order_by = subquery.order_by;
                            }
                            if subquery.limit.is_some() {
                                select_stmt.limit = subquery.limit;
                            }
                            if subquery.offset.is_some() {
                                select_stmt.offset = subquery.offset;
                            }
                            Ok(SqlExpr::Exists {
                                subquery: select_stmt,
                                negated: true,
                            })
                        }
                        _ => Ok(SqlExpr::SetExists {
                            subquery: Box::new(subquery),
                            negated: true,
                        }),
                    }
                } else {
                    let expr = self.parse_precedence(Precedence::Not)?;
                    Ok(SqlExpr::Unary {
                        op: SqlUnaryOp::Not,
                        expr: Box::new(expr),
                    })
                }
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
                    let expr = self.parse_precedence(Precedence::UnaryNeg)?;
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
            TokenKind::In => {
                self.advance(); // consume IN
                if self.peek().kind != TokenKind::LParen {
                    let tok = self.peek();
                    return Err(Error::SqlParseError {
                        message: format!("expected '(' after IN, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume '('
                if self.peek().kind == TokenKind::Select || self.peek().kind == TokenKind::With {
                    let subquery = self.parse_query_expression()?;
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ')' after IN (SELECT ...), found {}",
                                close.kind
                            ),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    match subquery.body {
                        SetQueryBody::Select(mut select_stmt) if subquery.with.is_none() => {
                            if !subquery.order_by.is_empty() {
                                select_stmt.order_by = subquery.order_by;
                            }
                            if subquery.limit.is_some() {
                                select_stmt.limit = subquery.limit;
                            }
                            if subquery.offset.is_some() {
                                select_stmt.offset = subquery.offset;
                            }
                            Ok(SqlExpr::InSubquery {
                                expr: Box::new(left),
                                subquery: select_stmt,
                                negated: false,
                            })
                        }
                        _ => Ok(SqlExpr::InSetSubquery {
                            expr: Box::new(left),
                            subquery: Box::new(subquery),
                            negated: false,
                        }),
                    }
                } else if self.peek().kind == TokenKind::RParen {
                    let tok = self.peek();
                    Err(Error::SqlParseError {
                        message: "IN list cannot be empty".to_string(),
                        start: tok.span.start,
                        end: tok.span.end,
                    })
                } else {
                    let mut list = Vec::new();
                    loop {
                        let item = self.parse_expr()?;
                        list.push(item);
                        if self.peek().kind == TokenKind::Comma {
                            self.advance(); // consume comma
                        } else {
                            break;
                        }
                    }
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!("expected ')' after IN list, found {}", close.kind),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    Ok(SqlExpr::InList {
                        expr: Box::new(left),
                        list,
                        negated: false,
                    })
                }
            }
            TokenKind::Not if self.peek_ahead(1).kind == TokenKind::In => {
                self.advance(); // consume NOT
                self.advance(); // consume IN
                if self.peek().kind != TokenKind::LParen {
                    let tok = self.peek();
                    return Err(Error::SqlParseError {
                        message: format!("expected '(' after NOT IN, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume '('
                if self.peek().kind == TokenKind::Select || self.peek().kind == TokenKind::With {
                    let subquery = self.parse_query_expression()?;
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ')' after NOT IN (SELECT ...), found {}",
                                close.kind
                            ),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    match subquery.body {
                        SetQueryBody::Select(mut select_stmt) if subquery.with.is_none() => {
                            if !subquery.order_by.is_empty() {
                                select_stmt.order_by = subquery.order_by;
                            }
                            if subquery.limit.is_some() {
                                select_stmt.limit = subquery.limit;
                            }
                            if subquery.offset.is_some() {
                                select_stmt.offset = subquery.offset;
                            }
                            Ok(SqlExpr::InSubquery {
                                expr: Box::new(left),
                                subquery: select_stmt,
                                negated: true,
                            })
                        }
                        _ => Ok(SqlExpr::InSetSubquery {
                            expr: Box::new(left),
                            subquery: Box::new(subquery),
                            negated: true,
                        }),
                    }
                } else if self.peek().kind == TokenKind::RParen {
                    let tok = self.peek();
                    Err(Error::SqlParseError {
                        message: "NOT IN list cannot be empty".to_string(),
                        start: tok.span.start,
                        end: tok.span.end,
                    })
                } else {
                    let mut list = Vec::new();
                    loop {
                        let item = self.parse_expr()?;
                        list.push(item);
                        if self.peek().kind == TokenKind::Comma {
                            self.advance(); // consume comma
                        } else {
                            break;
                        }
                    }
                    let close = self.peek();
                    if close.kind != TokenKind::RParen {
                        return Err(Error::SqlParseError {
                            message: format!(
                                "expected ')' after NOT IN list, found {}",
                                close.kind
                            ),
                            start: close.span.start,
                            end: close.span.end,
                        });
                    }
                    self.advance(); // consume ')'
                    Ok(SqlExpr::InList {
                        expr: Box::new(left),
                        list,
                        negated: true,
                    })
                }
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
            TokenKind::Plus => {
                self.advance();
                let right = self.parse_precedence(Precedence::AddSub)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Add,
                    right: Box::new(right),
                })
            }
            TokenKind::Minus => {
                self.advance();
                let right = self.parse_precedence(Precedence::AddSub)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Sub,
                    right: Box::new(right),
                })
            }
            TokenKind::Asterisk => {
                self.advance();
                let right = self.parse_precedence(Precedence::MulDiv)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Mul,
                    right: Box::new(right),
                })
            }
            TokenKind::Slash => {
                self.advance();
                let right = self.parse_precedence(Precedence::MulDiv)?;
                Ok(SqlExpr::Binary {
                    left: Box::new(left),
                    op: SqlBinaryOp::Div,
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
            TokenKind::In => Precedence::Comparison,
            TokenKind::Not if self.peek_ahead(1).kind == TokenKind::In => Precedence::Comparison,
            TokenKind::Eq
            | TokenKind::NotEq
            | TokenKind::Lt
            | TokenKind::LtEq
            | TokenKind::Gt
            | TokenKind::GtEq => Precedence::Comparison,
            TokenKind::Is => Precedence::IsNull,
            TokenKind::Plus | TokenKind::Minus => Precedence::AddSub,
            TokenKind::Asterisk | TokenKind::Slash => Precedence::MulDiv,
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

    fn peek_ahead(&self, offset: usize) -> &Token {
        self.tokens.get(self.cursor + offset).unwrap_or_else(|| {
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

    fn parse_window_function(&mut self, func: WindowFunction) -> Result<SqlExpr> {
        self.advance(); // consume function identifier
        self.advance(); // consume '('
        let mut args = Vec::new();
        if self.peek().kind != TokenKind::RParen {
            loop {
                let arg = self.parse_expr()?;
                args.push(arg);
                if self.peek().kind == TokenKind::Comma {
                    self.advance(); // consume comma
                } else {
                    break;
                }
            }
        }
        if self.peek().kind != TokenKind::RParen {
            let close = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected ')' after {func} arguments, found {}", close.kind),
                start: close.span.start,
                end: close.span.end,
            });
        }
        self.advance(); // consume ')'

        if self.peek().kind != TokenKind::Over {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "window function {func} requires an OVER clause, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume OVER

        let window_spec = self.parse_window_spec()?;
        Ok(SqlExpr::Window {
            function: func,
            args,
            window_spec,
        })
    }

    fn parse_window_spec(&mut self) -> Result<WindowSpec> {
        if self.peek().kind != TokenKind::LParen {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!("expected '(' after OVER, found {}", tok.kind),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume '('

        let mut partition_by = Vec::new();
        if self.peek().kind == TokenKind::Partition {
            self.advance(); // consume PARTITION
            if self.peek().kind != TokenKind::By {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected BY after PARTITION, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
            self.advance(); // consume BY
            loop {
                let expr = self.parse_expr()?;
                partition_by.push(expr);
                if self.peek().kind == TokenKind::Comma {
                    self.advance(); // consume comma
                } else {
                    break;
                }
            }
        }

        let mut order_by = Vec::new();
        if self.peek().kind == TokenKind::Order {
            self.advance(); // consume ORDER
            if self.peek().kind != TokenKind::By {
                let tok = self.peek().clone();
                return Err(Error::SqlParseError {
                    message: format!("expected BY after ORDER, found {}", tok.kind),
                    start: tok.span.start,
                    end: tok.span.end,
                });
            }
            self.advance(); // consume BY
            order_by = self.parse_order_by_list()?;
        }

        let mut frame = None;
        if self.peek().kind == TokenKind::Rows {
            self.advance(); // consume ROWS
            if self.peek().kind == TokenKind::Between {
                self.advance(); // consume BETWEEN
                let start = self.parse_window_frame_bound()?;
                if self.peek().kind != TokenKind::And {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!("expected AND in ROWS BETWEEN frame, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume AND
                let end = self.parse_window_frame_bound()?;
                frame = Some(WindowFrame { start, end });
            } else {
                let start = self.parse_window_frame_bound()?;
                frame = Some(WindowFrame {
                    start,
                    end: WindowFrameBound::CurrentRow,
                });
            }
        } else if self.peek().kind == TokenKind::Range {
            return Err(Error::InvalidWindowFrame(
                "RANGE frames are not supported in this phase".to_string(),
            ));
        } else if self.peek().kind == TokenKind::Groups {
            return Err(Error::InvalidWindowFrame(
                "GROUPS frames are not supported in this phase".to_string(),
            ));
        }

        if self.peek().kind != TokenKind::RParen {
            let tok = self.peek().clone();
            return Err(Error::SqlParseError {
                message: format!(
                    "expected ')' at end of OVER specification, found {}",
                    tok.kind
                ),
                start: tok.span.start,
                end: tok.span.end,
            });
        }
        self.advance(); // consume ')'

        Ok(WindowSpec {
            partition_by,
            order_by,
            frame,
        })
    }

    fn parse_window_frame_bound(&mut self) -> Result<WindowFrameBound> {
        match self.peek().kind {
            TokenKind::Unbounded => {
                self.advance(); // consume UNBOUNDED
                match self.peek().kind {
                    TokenKind::Preceding => {
                        self.advance();
                        Ok(WindowFrameBound::UnboundedPreceding)
                    }
                    TokenKind::Following => {
                        self.advance();
                        Ok(WindowFrameBound::UnboundedFollowing)
                    }
                    ref other => {
                        let tok = self.peek().clone();
                        Err(Error::SqlParseError {
                            message: format!(
                                "expected PRECEDING or FOLLOWING after UNBOUNDED, found {other}"
                            ),
                            start: tok.span.start,
                            end: tok.span.end,
                        })
                    }
                }
            }
            TokenKind::Current => {
                self.advance(); // consume CURRENT
                if self.peek().kind != TokenKind::Row {
                    let tok = self.peek().clone();
                    return Err(Error::SqlParseError {
                        message: format!("expected ROW after CURRENT, found {}", tok.kind),
                        start: tok.span.start,
                        end: tok.span.end,
                    });
                }
                self.advance(); // consume ROW
                Ok(WindowFrameBound::CurrentRow)
            }
            TokenKind::Integer(n) => {
                if n < 0 {
                    return Err(Error::InvalidWindowFrame(
                        "negative frame bound is not allowed".to_string(),
                    ));
                }
                let bound_n = n as u64;
                self.advance(); // consume integer
                match self.peek().kind {
                    TokenKind::Preceding => {
                        self.advance();
                        Ok(WindowFrameBound::Preceding(bound_n))
                    }
                    TokenKind::Following => {
                        self.advance();
                        Ok(WindowFrameBound::Following(bound_n))
                    }
                    ref other => {
                        let tok = self.peek().clone();
                        Err(Error::SqlParseError {
                            message: format!(
                                "expected PRECEDING or FOLLOWING after frame offset, found {other}"
                            ),
                            start: tok.span.start,
                            end: tok.span.end,
                        })
                    }
                }
            }
            ref other => {
                let tok = self.peek().clone();
                Err(Error::SqlParseError {
                    message: format!("invalid window frame boundary specification, found {other}"),
                    start: tok.span.start,
                    end: tok.span.end,
                })
            }
        }
    }
}
