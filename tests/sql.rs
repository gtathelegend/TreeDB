use treedb::error::Error;
use treedb::sql::{
    parse_sql, tokenize_sql, OrderByExpr, OrderDirection, SelectItem, SourceSpan, SqlBinaryOp,
    SqlExpr, SqlLiteral, SqlUnaryOp, TableRef, TokenKind,
};

// ============================================================================
// 1. Lexer Tests
// ============================================================================

#[test]
fn test_lexer_keywords_case_insensitivity() {
    let sql = "SELECT select SeLeCt FROM from WHERE where ORDER By asc DESC LIMIT offset AND Or NOT Is Null True FALSE AS";
    let tokens = tokenize_sql(sql).unwrap();

    let expected_kinds = vec![
        TokenKind::Select,
        TokenKind::Select,
        TokenKind::Select,
        TokenKind::From,
        TokenKind::From,
        TokenKind::Where,
        TokenKind::Where,
        TokenKind::Order,
        TokenKind::By,
        TokenKind::Asc,
        TokenKind::Desc,
        TokenKind::Limit,
        TokenKind::Offset,
        TokenKind::And,
        TokenKind::Or,
        TokenKind::Not,
        TokenKind::Is,
        TokenKind::Null,
        TokenKind::True,
        TokenKind::False,
        TokenKind::As,
        TokenKind::Eof,
    ];

    let actual_kinds: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();
    assert_eq!(actual_kinds, expected_kinds);
}

#[test]
fn test_lexer_identifiers_and_literals() {
    let sql = "user_id _count table123 42 12.34 'Alice' 'O''Reilly' true FALSE NULL";
    let tokens = tokenize_sql(sql).unwrap();

    assert_eq!(tokens[0].kind, TokenKind::Ident("user_id".into()));
    assert_eq!(tokens[1].kind, TokenKind::Ident("_count".into()));
    assert_eq!(tokens[2].kind, TokenKind::Ident("table123".into()));
    assert_eq!(tokens[3].kind, TokenKind::Integer(42));
    assert_eq!(tokens[4].kind, TokenKind::Float(12.34));
    assert_eq!(tokens[5].kind, TokenKind::String("Alice".into()));
    assert_eq!(tokens[6].kind, TokenKind::String("O'Reilly".into()));
    assert_eq!(tokens[7].kind, TokenKind::True);
    assert_eq!(tokens[8].kind, TokenKind::False);
    assert_eq!(tokens[9].kind, TokenKind::Null);
}

#[test]
fn test_lexer_operators_and_punctuation() {
    let sql = "= != <> < <= > >= - * , ( ) ;";
    let tokens = tokenize_sql(sql).unwrap();

    let expected = vec![
        TokenKind::Eq,
        TokenKind::NotEq,
        TokenKind::NotEq,
        TokenKind::Lt,
        TokenKind::LtEq,
        TokenKind::Gt,
        TokenKind::GtEq,
        TokenKind::Minus,
        TokenKind::Asterisk,
        TokenKind::Comma,
        TokenKind::LParen,
        TokenKind::RParen,
        TokenKind::Semicolon,
        TokenKind::Eof,
    ];

    let actual: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();
    assert_eq!(actual, expected);
}

#[test]
fn test_lexer_whitespace_and_line_comments() {
    let sql = r#"
        -- This is a comment
        SELECT   name, -- inline comment
        age FROM users;
    "#;

    let tokens = tokenize_sql(sql).unwrap();
    let kinds: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();

    assert_eq!(
        kinds,
        vec![
            TokenKind::Select,
            TokenKind::Ident("name".into()),
            TokenKind::Comma,
            TokenKind::Ident("age".into()),
            TokenKind::From,
            TokenKind::Ident("users".into()),
            TokenKind::Semicolon,
            TokenKind::Eof,
        ]
    );
}

#[test]
fn test_lexer_spans() {
    let sql = "SELECT id FROM users";
    let tokens = tokenize_sql(sql).unwrap();

    assert_eq!(tokens[0].span, SourceSpan::new(0, 6)); // SELECT
    assert_eq!(tokens[1].span, SourceSpan::new(7, 9)); // id
    assert_eq!(tokens[2].span, SourceSpan::new(10, 14)); // FROM
    assert_eq!(tokens[3].span, SourceSpan::new(15, 20)); // users
    assert_eq!(tokens[4].span, SourceSpan::new(20, 20)); // EOF
}

#[test]
fn test_lexer_error_handling() {
    // Unterminated string
    assert!(matches!(
        tokenize_sql("SELECT 'unterminated"),
        Err(Error::SqlLexError { .. })
    ));

    // Unexpected character
    assert!(matches!(
        tokenize_sql("SELECT @invalid"),
        Err(Error::SqlLexError { .. })
    ));

    // Single exclamation mark without =
    assert!(matches!(
        tokenize_sql("SELECT ! active"),
        Err(Error::SqlLexError { .. })
    ));
}

// ============================================================================
// 2. Parser Projection, FROM, & Basic Queries
// ============================================================================

#[test]
fn test_parser_select_wildcard() {
    let ast = parse_sql("SELECT * FROM users").unwrap();
    assert_eq!(ast.projection, vec![SelectItem::Wildcard]);
    assert_eq!(ast.from, TableRef::new("users"));
    assert_eq!(ast.selection, None);
    assert!(ast.order_by.is_empty());
    assert_eq!(ast.limit, None);
    assert_eq!(ast.offset, None);
}

#[test]
fn test_parser_select_columns_and_aliases() {
    let ast =
        parse_sql("SELECT name AS user_name, age, age > 18 AS is_adult FROM users u").unwrap();

    assert_eq!(ast.projection.len(), 3);
    assert_eq!(
        ast.projection[0],
        SelectItem::Expression {
            expr: SqlExpr::Identifier("name".into()),
            alias: Some("user_name".into()),
        }
    );
    assert_eq!(
        ast.projection[1],
        SelectItem::Expression {
            expr: SqlExpr::Identifier("age".into()),
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[2],
        SelectItem::Expression {
            expr: SqlExpr::Binary {
                left: Box::new(SqlExpr::Identifier("age".into())),
                op: SqlBinaryOp::Gt,
                right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(18))),
            },
            alias: Some("is_adult".into()),
        }
    );
    assert_eq!(ast.from, TableRef::with_alias("users", "u"));
}

// ============================================================================
// 3. Expression Precedence & Operator Parsing
// ============================================================================

#[test]
fn test_parser_expression_precedence_and_or() {
    // a = 1 OR b = 2 AND c = 3  ->  (a = 1) OR ((b = 2) AND (c = 3))
    let ast = parse_sql("SELECT * FROM t WHERE a = 1 OR b = 2 AND c = 3").unwrap();

    let expected_selection = SqlExpr::Binary {
        left: Box::new(SqlExpr::Binary {
            left: Box::new(SqlExpr::Identifier("a".into())),
            op: SqlBinaryOp::Eq,
            right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(1))),
        }),
        op: SqlBinaryOp::Or,
        right: Box::new(SqlExpr::Binary {
            left: Box::new(SqlExpr::Binary {
                left: Box::new(SqlExpr::Identifier("b".into())),
                op: SqlBinaryOp::Eq,
                right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(2))),
            }),
            op: SqlBinaryOp::And,
            right: Box::new(SqlExpr::Binary {
                left: Box::new(SqlExpr::Identifier("c".into())),
                op: SqlBinaryOp::Eq,
                right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(3))),
            }),
        }),
    };

    assert_eq!(ast.selection, Some(expected_selection));
}

#[test]
fn test_parser_parentheses_override_precedence() {
    // (a = 1 OR b = 2) AND c = 3
    let ast = parse_sql("SELECT * FROM t WHERE (a = 1 OR b = 2) AND c = 3").unwrap();

    let expected_selection = SqlExpr::Binary {
        left: Box::new(SqlExpr::Binary {
            left: Box::new(SqlExpr::Binary {
                left: Box::new(SqlExpr::Identifier("a".into())),
                op: SqlBinaryOp::Eq,
                right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(1))),
            }),
            op: SqlBinaryOp::Or,
            right: Box::new(SqlExpr::Binary {
                left: Box::new(SqlExpr::Identifier("b".into())),
                op: SqlBinaryOp::Eq,
                right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(2))),
            }),
        }),
        op: SqlBinaryOp::And,
        right: Box::new(SqlExpr::Binary {
            left: Box::new(SqlExpr::Identifier("c".into())),
            op: SqlBinaryOp::Eq,
            right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(3))),
        }),
    };

    assert_eq!(ast.selection, Some(expected_selection));
}

#[test]
fn test_parser_unary_operators() {
    let ast = parse_sql("SELECT * FROM t WHERE NOT active AND val = -42").unwrap();

    let expected_selection = SqlExpr::Binary {
        left: Box::new(SqlExpr::Unary {
            op: SqlUnaryOp::Not,
            expr: Box::new(SqlExpr::Identifier("active".into())),
        }),
        op: SqlBinaryOp::And,
        right: Box::new(SqlExpr::Binary {
            left: Box::new(SqlExpr::Identifier("val".into())),
            op: SqlBinaryOp::Eq,
            right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(-42))),
        }),
    };

    assert_eq!(ast.selection, Some(expected_selection));

    // In SQL standard semantics, NOT binds looser than comparison: NOT x = 1 -> NOT (x = 1)
    let ast_not_eq = parse_sql("SELECT * FROM t WHERE NOT x = 1").unwrap();
    let expected_not_eq = SqlExpr::Unary {
        op: SqlUnaryOp::Not,
        expr: Box::new(SqlExpr::Binary {
            left: Box::new(SqlExpr::Identifier("x".into())),
            op: SqlBinaryOp::Eq,
            right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(1))),
        }),
    };
    assert_eq!(ast_not_eq.selection, Some(expected_not_eq));
}

#[test]
fn test_parser_qualified_identifiers_and_wildcards() {
    let ast =
        parse_sql("SELECT u.*, u.id, users.name AS user_name FROM users AS u WHERE u.age >= 18")
            .unwrap();

    assert_eq!(ast.projection.len(), 3);
    assert_eq!(ast.projection[0], SelectItem::QualifiedWildcard("u".into()));
    assert_eq!(
        ast.projection[1],
        SelectItem::Expression {
            expr: SqlExpr::QualifiedIdentifier {
                qualifier: "u".into(),
                name: "id".into(),
            },
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[2],
        SelectItem::Expression {
            expr: SqlExpr::QualifiedIdentifier {
                qualifier: "users".into(),
                name: "name".into(),
            },
            alias: Some("user_name".into()),
        }
    );

    let expected_selection = SqlExpr::Binary {
        left: Box::new(SqlExpr::QualifiedIdentifier {
            qualifier: "u".into(),
            name: "age".into(),
        }),
        op: SqlBinaryOp::GtEq,
        right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(18))),
    };
    assert_eq!(ast.selection, Some(expected_selection));
}

#[test]
fn test_parser_is_null_and_is_not_null() {
    let ast =
        parse_sql("SELECT * FROM users WHERE deleted_at IS NULL AND age IS NOT NULL").unwrap();

    let expected_selection = SqlExpr::Binary {
        left: Box::new(SqlExpr::IsNull(Box::new(SqlExpr::Identifier(
            "deleted_at".into(),
        )))),
        op: SqlBinaryOp::And,
        right: Box::new(SqlExpr::IsNotNull(Box::new(SqlExpr::Identifier(
            "age".into(),
        )))),
    };

    assert_eq!(ast.selection, Some(expected_selection));
}

#[test]
fn test_parser_inequality_normalization() {
    let ast_bang = parse_sql("SELECT * FROM users WHERE age != 18").unwrap();
    let ast_angle = parse_sql("SELECT * FROM users WHERE age <> 18").unwrap();

    assert_eq!(ast_bang, ast_angle);
}

// ============================================================================
// 4. ORDER BY, LIMIT, OFFSET, and Clause Ordering
// ============================================================================

#[test]
fn test_parser_order_by_multi_key() {
    let ast = parse_sql("SELECT * FROM users ORDER BY age DESC, name ASC, id").unwrap();

    assert_eq!(ast.order_by[0].direction, OrderDirection::Desc);
    assert_eq!(ast.order_by[1].direction, OrderDirection::Asc);
    assert_eq!(ast.order_by[2].direction, OrderDirection::Asc);

    assert_eq!(
        ast.order_by,
        vec![
            OrderByExpr::desc(SqlExpr::Identifier("age".into())),
            OrderByExpr::asc(SqlExpr::Identifier("name".into())),
            OrderByExpr::asc(SqlExpr::Identifier("id".into())),
        ]
    );
}

#[test]
fn test_parser_limit_and_offset() {
    let ast = parse_sql("SELECT * FROM users LIMIT 10 OFFSET 5;").unwrap();
    assert_eq!(ast.limit, Some(10));
    assert_eq!(ast.offset, Some(5));
}

#[test]
fn test_parser_golden_complete_statement() {
    let sql = r#"
        SELECT id, name, age >= 18 AS adult
        FROM users AS u
        WHERE (age > 21 OR active = TRUE)
          AND deleted_at IS NULL
        ORDER BY age DESC, name ASC
        LIMIT 25
        OFFSET 10;
    "#;

    let ast = parse_sql(sql).unwrap();

    assert_eq!(
        ast.projection,
        vec![
            SelectItem::Expression {
                expr: SqlExpr::Identifier("id".into()),
                alias: None,
            },
            SelectItem::Expression {
                expr: SqlExpr::Identifier("name".into()),
                alias: None,
            },
            SelectItem::Expression {
                expr: SqlExpr::Binary {
                    left: Box::new(SqlExpr::Identifier("age".into())),
                    op: SqlBinaryOp::GtEq,
                    right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(18))),
                },
                alias: Some("adult".into()),
            },
        ]
    );

    assert_eq!(ast.from, TableRef::with_alias("users", "u"));

    assert_eq!(
        ast.selection,
        Some(SqlExpr::Binary {
            left: Box::new(SqlExpr::Binary {
                left: Box::new(SqlExpr::Binary {
                    left: Box::new(SqlExpr::Identifier("age".into())),
                    op: SqlBinaryOp::Gt,
                    right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(21))),
                }),
                op: SqlBinaryOp::Or,
                right: Box::new(SqlExpr::Binary {
                    left: Box::new(SqlExpr::Identifier("active".into())),
                    op: SqlBinaryOp::Eq,
                    right: Box::new(SqlExpr::Literal(SqlLiteral::Boolean(true))),
                }),
            }),
            op: SqlBinaryOp::And,
            right: Box::new(SqlExpr::IsNull(Box::new(SqlExpr::Identifier(
                "deleted_at".into(),
            )))),
        })
    );

    assert_eq!(
        ast.order_by,
        vec![
            OrderByExpr::desc(SqlExpr::Identifier("age".into())),
            OrderByExpr::asc(SqlExpr::Identifier("name".into())),
        ]
    );

    assert_eq!(ast.limit, Some(25));
    assert_eq!(ast.offset, Some(10));
}

// ============================================================================
// 5. Parser Syntax Errors & Invalid SQL
// ============================================================================

#[test]
fn test_parser_invalid_syntax_matrix() {
    let invalid_queries = [
        "",                                        // Empty input
        "INSERT INTO users VALUES (1)",            // Non-SELECT statement
        "SELECT",                                  // Missing projection
        "SELECT FROM users",                       // Empty projection before FROM
        "SELECT name, FROM users",                 // Trailing comma in projection
        "SELECT name",                             // Missing FROM
        "SELECT name FROM",                        // Missing table name
        "SELECT * FROM users WHERE",               // Missing WHERE expression
        "SELECT * FROM users WHERE age >",         // Incomplete comparison
        "SELECT * FROM users WHERE (age > 18",     // Unclosed parenthesis
        "SELECT * FROM users ORDER",               // Missing BY
        "SELECT * FROM users ORDER BY",            // Missing ORDER BY expression
        "SELECT * FROM users ORDER BY age LIMIT",  // Missing LIMIT count
        "SELECT * FROM users LIMIT -1",            // Negative LIMIT
        "SELECT * FROM users LIMIT 3.14",          // Float LIMIT
        "SELECT * FROM users OFFSET -5",           // Negative OFFSET
        "SELECT * FROM users WHERE age > 10 FROM", // Invalid clause ordering
        "SELECT * FROM users; garbage",            // Trailing tokens
        "SELECT * FROM users WHERE x IS",          // Incomplete IS clause
        "SELECT * FROM users WHERE x IS UNKNOWN",  // Invalid IS target
    ];

    for &query in &invalid_queries {
        let result = parse_sql(query);
        assert!(
            result.is_err(),
            "Expected parse error for query: '{query}', got: {result:?}"
        );
    }
}

// ============================================================================
// 6. Robustness & Deterministic Fuzz Corpus
// ============================================================================

#[test]
fn test_parser_robustness_malformed_corpus() {
    // Malformed strings that must never cause panics or infinite loops
    let corpus = [
        "SELECT * FROM",
        "SELECT ,,, FROM t",
        "SELECT (((( FROM t",
        "SELECT * FROM t WHERE ()",
        "SELECT * FROM t WHERE (a AND)",
        "SELECT * FROM t WHERE a IS NOT",
        "SELECT * FROM t ORDER BY ,,,",
        "SELECT * FROM t LIMIT 999999999999999999999999999999",
        "SELECT * FROM t OFFSET 999999999999999999999999999999",
        "-- line comment without newline",
        ";;;;;",
        "SELECT 'unterminated string literal",
        "SELECT * FROM t WHERE a = 'unterminated",
        "SELECT 12.34.56 FROM t",
        "SELECT ! FROM t",
    ];

    for &sql in &corpus {
        let _ = parse_sql(sql); // Must safely return Ok or Err, never panic
    }
}

// ============================================================================
// 7. Integration Boundary Test (Standalone Parsing without Database)
// ============================================================================

#[test]
fn test_sql_parsing_standalone_boundary() {
    let sql = "SELECT id, name FROM users WHERE active = TRUE ORDER BY id ASC LIMIT 10;";
    let ast = parse_sql(sql).expect("standalone parse should succeed");

    assert_eq!(ast.projection.len(), 2);
    assert_eq!(ast.from.name, "users");
    assert!(ast.selection.is_some());
    assert_eq!(ast.order_by.len(), 1);
    assert_eq!(ast.limit, Some(10));
}

// ============================================================================
// 8. Aggregation, GROUP BY & HAVING Parsing Tests (Phase 18)
// ============================================================================

#[test]
fn test_parser_aggregates_and_case_insensitivity() {
    let sql = "SELECT COUNT(*), count(age), Sum(salary), AVG(rating), min(score), MAX(height) FROM users;";
    let ast = parse_sql(sql).expect("parsing aggregates should succeed");

    assert_eq!(ast.projection.len(), 6);
    assert_eq!(
        ast.projection[0],
        SelectItem::Expression {
            expr: SqlExpr::CountAll,
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[1],
        SelectItem::Expression {
            expr: SqlExpr::Aggregate {
                function: treedb::sql::AggregateFunction::Count,
                arg: Box::new(SqlExpr::Identifier("age".into())),
            },
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[2],
        SelectItem::Expression {
            expr: SqlExpr::Aggregate {
                function: treedb::sql::AggregateFunction::Sum,
                arg: Box::new(SqlExpr::Identifier("salary".into())),
            },
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[3],
        SelectItem::Expression {
            expr: SqlExpr::Aggregate {
                function: treedb::sql::AggregateFunction::Avg,
                arg: Box::new(SqlExpr::Identifier("rating".into())),
            },
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[4],
        SelectItem::Expression {
            expr: SqlExpr::Aggregate {
                function: treedb::sql::AggregateFunction::Min,
                arg: Box::new(SqlExpr::Identifier("score".into())),
            },
            alias: None,
        }
    );
    assert_eq!(
        ast.projection[5],
        SelectItem::Expression {
            expr: SqlExpr::Aggregate {
                function: treedb::sql::AggregateFunction::Max,
                arg: Box::new(SqlExpr::Identifier("height".into())),
            },
            alias: None,
        }
    );
}

#[test]
fn test_parser_group_by_and_having() {
    let sql = "SELECT department, role, COUNT(*) AS emp_cnt FROM employees WHERE active = TRUE GROUP BY department, role HAVING COUNT(*) >= 5 ORDER BY emp_cnt DESC LIMIT 10 OFFSET 5;";
    let ast = parse_sql(sql).expect("parsing grouped query should succeed");

    assert_eq!(ast.projection.len(), 3);
    assert_eq!(ast.from.name, "employees");
    assert!(ast.selection.is_some());
    assert_eq!(
        ast.group_by,
        vec![
            SqlExpr::Identifier("department".into()),
            SqlExpr::Identifier("role".into())
        ]
    );
    assert_eq!(
        ast.having,
        Some(SqlExpr::Binary {
            left: Box::new(SqlExpr::CountAll),
            op: SqlBinaryOp::GtEq,
            right: Box::new(SqlExpr::Literal(SqlLiteral::Integer(5))),
        })
    );
    assert_eq!(ast.order_by.len(), 1);
    assert_eq!(ast.limit, Some(10));
    assert_eq!(ast.offset, Some(5));
}

#[test]
fn test_parser_invalid_aggregate_and_group_by_syntax() {
    let invalid = [
        "SELECT COUNT() FROM users;",           // COUNT without args
        "SELECT SUM(*) FROM users;",            // SUM(*) invalid
        "SELECT AVG(*) FROM users;",            // AVG(*) invalid
        "SELECT MIN(*) FROM users;",            // MIN(*) invalid
        "SELECT MAX(*) FROM users;",            // MAX(*) invalid
        "SELECT SUM() FROM users;",             // SUM() empty
        "SELECT AVG() FROM users;",             // AVG() empty
        "SELECT MIN() FROM users;",             // MIN() empty
        "SELECT MAX() FROM users;",             // MAX() empty
        "SELECT SUM(COUNT(*)) FROM users;",     // Nested aggregate
        "SELECT AVG(SUM(salary)) FROM users;",  // Nested aggregate
        "SELECT UNKNOWN_FUNC(age) FROM users;", // Unknown function
        "SELECT * FROM users GROUP;",           // GROUP without BY
        "SELECT * FROM users GROUP BY;",        // GROUP BY without expr
        "SELECT * FROM users HAVING;",          // HAVING without expr
        "SELECT * FROM users GROUP BY department WHERE active = TRUE;", // Out of order: GROUP BY before WHERE
        "SELECT * FROM users ORDER BY age GROUP BY department;", // Out of order: ORDER BY before GROUP BY
        "SELECT * FROM users LIMIT 10 GROUP BY department;", // Out of order: LIMIT before GROUP BY
    ];

    for &sql in &invalid {
        let result = parse_sql(sql);
        assert!(
            result.is_err(),
            "Expected parse error for query: '{sql}', but succeeded: {result:?}"
        );
    }
}

#[test]
fn test_parser_joins_and_case_insensitivity() {
    let sql = "SELECT u.name, o.amount FROM users u INNER JOIN orders o ON u.id = o.user_id LEFT OUTER JOIN products AS p ON o.prod_id = p.id WHERE u.active = true;";
    let ast = parse_sql(sql).unwrap();

    assert_eq!(ast.from.name, "users");
    assert_eq!(ast.from.alias, Some("u".to_string()));
    assert_eq!(ast.joins.len(), 2);

    assert_eq!(ast.joins[0].join_type, treedb::sql::JoinType::Inner);
    assert_eq!(ast.joins[0].table.name, "orders");
    assert_eq!(ast.joins[0].table.alias, Some("o".to_string()));
    assert_eq!(
        ast.joins[0].on,
        SqlExpr::Binary {
            left: Box::new(SqlExpr::QualifiedIdentifier {
                qualifier: "u".to_string(),
                name: "id".to_string(),
            }),
            op: SqlBinaryOp::Eq,
            right: Box::new(SqlExpr::QualifiedIdentifier {
                qualifier: "o".to_string(),
                name: "user_id".to_string(),
            }),
        }
    );

    assert_eq!(ast.joins[1].join_type, treedb::sql::JoinType::Left);
    assert_eq!(ast.joins[1].table.name, "products");
    assert_eq!(ast.joins[1].table.alias, Some("p".to_string()));

    // Case insensitivity: 'join', 'left join', 'inner join'
    let sql2 = "SELECT * FROM a join b on a.id = b.a_id left join c on b.id = c.b_id;";
    let ast2 = parse_sql(sql2).unwrap();
    assert_eq!(ast2.joins.len(), 2);
    assert_eq!(ast2.joins[0].join_type, treedb::sql::JoinType::Inner);
    assert_eq!(ast2.joins[1].join_type, treedb::sql::JoinType::Left);
}

#[test]
fn test_parser_invalid_join_syntax() {
    let invalid = [
        "SELECT * FROM users JOIN;",                             // Missing table
        "SELECT * FROM users JOIN orders;",                      // Missing ON
        "SELECT * FROM users JOIN orders ON;",                   // Missing ON expr
        "SELECT * FROM users INNER orders ON u.id = o.id;",      // Missing JOIN after INNER
        "SELECT * FROM users LEFT orders ON u.id = o.id;",       // Missing JOIN after LEFT
        "SELECT * FROM users LEFT OUTER orders ON u.id = o.id;", // Missing JOIN after LEFT OUTER
        "SELECT * FROM users WHERE active = true JOIN orders ON u.id = o.id;", // JOIN after WHERE
    ];

    for &sql in &invalid {
        let result = parse_sql(sql);
        assert!(
            result.is_err(),
            "Expected parse error for query: '{sql}', but succeeded: {result:?}"
        );
    }
}
