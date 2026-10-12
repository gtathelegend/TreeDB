use std::collections::HashMap;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Result;
use treedb::query::expr::{BinaryOp, Expr};
use treedb::query::external_hash_join::{analyze_equi_join, ExternalHashJoiner, JoinConfig};
use treedb::sql::ast::JoinType;
use treedb::TreeDb;

fn create_schema_pair() -> (Schema, Schema) {
    let left_schema = Schema::new_projected(vec![
        Column::new("t1.id", DataType::Int64, false),
        Column::new("t1.key", DataType::Int64, true),
        Column::new("t1.tag", DataType::String, false),
    ]);
    let right_schema = Schema::new_projected(vec![
        Column::new("t2.id", DataType::Int64, false),
        Column::new("t2.key", DataType::Int64, true),
        Column::new("t2.payload", DataType::String, false),
    ]);
    (left_schema, right_schema)
}

fn create_equi_expr() -> Expr {
    Expr::Binary {
        left: Box::new(Expr::Column("t1.key".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("t2.key".to_string())),
    }
}

/// Helper for multiset row comparison
fn assert_multiset_equivalent(actual: &[Row], expected: &[Row]) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "Row count mismatch: actual {}, expected {}",
        actual.len(),
        expected.len()
    );

    let mut actual_counts: HashMap<String, usize> = HashMap::new();
    for r in actual {
        let key = format!("{r:?}");
        *actual_counts.entry(key).or_insert(0) += 1;
    }

    let mut expected_counts: HashMap<String, usize> = HashMap::new();
    for r in expected {
        let key = format!("{r:?}");
        *expected_counts.entry(key).or_insert(0) += 1;
    }

    assert_eq!(
        actual_counts, expected_counts,
        "Multiset mismatch between actual and expected rows"
    );
}

#[test]
fn test_empty_left_or_right_inputs() -> Result<()> {
    let (left_schema, right_schema) = create_schema_pair();
    let equi_plan = analyze_equi_join(&create_equi_expr(), &left_schema, &right_schema).unwrap();

    let joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan.clone(),
        JoinConfig::default(),
    );

    let left_rows = vec![Row::new(vec![
        Value::Int64(1),
        Value::Int64(10),
        Value::String("A".to_string()),
    ])];
    let right_rows = vec![Row::new(vec![
        Value::Int64(100),
        Value::Int64(10),
        Value::String("Payload".to_string()),
    ])];

    // Empty left
    let (_, out, metrics) = joiner.execute_ref(&[], &right_rows)?;
    assert!(out.is_empty());
    assert_eq!(metrics.rows_left, 0);
    assert_eq!(metrics.rows_output, 0);

    // Empty right
    let (_, out, metrics) = joiner.execute_ref(&left_rows, &[])?;
    assert!(out.is_empty());
    assert_eq!(metrics.rows_right, 0);
    assert_eq!(metrics.rows_output, 0);

    // Empty left with LEFT JOIN
    let left_joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Left,
        equi_plan.clone(),
        JoinConfig::default(),
    );
    let (_, out, _) = left_joiner.execute_ref(&[], &right_rows)?;
    assert!(out.is_empty());

    // Empty right with LEFT JOIN (null padding)
    let (_, out, _) = left_joiner.execute_ref(&left_rows, &[])?;
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].values,
        vec![
            Value::Int64(1),
            Value::Int64(10),
            Value::String("A".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]
    );

    // Empty right with ANTI JOIN (preserves left)
    let anti_joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Anti,
        equi_plan,
        JoinConfig::default(),
    );
    let (_, out, _) = anti_joiner.execute_ref(&left_rows, &[])?;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], left_rows[0]);

    Ok(())
}

#[test]
fn test_single_row_matching_and_unmatched() -> Result<()> {
    let (left_schema, right_schema) = create_schema_pair();
    let equi_plan = analyze_equi_join(&create_equi_expr(), &left_schema, &right_schema).unwrap();

    let joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan,
        JoinConfig::default(),
    );

    let l_match = vec![Row::new(vec![
        Value::Int64(1),
        Value::Int64(42),
        Value::String("L".to_string()),
    ])];
    let r_match = vec![Row::new(vec![
        Value::Int64(2),
        Value::Int64(42),
        Value::String("R".to_string()),
    ])];
    let r_unmatch = vec![Row::new(vec![
        Value::Int64(3),
        Value::Int64(999),
        Value::String("R_unmatch".to_string()),
    ])];

    // Matching single row
    let (_, out, metrics) = joiner.execute_ref(&l_match, &r_match)?;
    assert_eq!(out.len(), 1);
    assert_eq!(metrics.matched_rows, 1);
    assert_eq!(
        out[0].values,
        vec![
            Value::Int64(1),
            Value::Int64(42),
            Value::String("L".to_string()),
            Value::Int64(2),
            Value::Int64(42),
            Value::String("R".to_string()),
        ]
    );

    // Unmatched single row
    let (_, out, metrics) = joiner.execute_ref(&l_match, &r_unmatch)?;
    assert!(out.is_empty());
    assert_eq!(metrics.matched_rows, 0);

    Ok(())
}

#[test]
fn test_duplicate_multiplicity_on_both_sides() -> Result<()> {
    let (left_schema, right_schema) = create_schema_pair();
    let equi_plan = analyze_equi_join(&create_equi_expr(), &left_schema, &right_schema).unwrap();

    let joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan,
        JoinConfig::default(),
    );

    // Key 10 appears 3 times in left, 2 times in right -> 3 * 2 = 6 output rows
    // Key 20 appears 1 time in left, 4 times in right -> 1 * 4 = 4 output rows
    // Key 30 appears 2 times in left, 0 times in right -> 0 output rows
    // Total = 10 output rows
    let left_rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Int64(10),
            Value::String("L1".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Int64(10),
            Value::String("L2".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::Int64(10),
            Value::String("L3".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::Int64(20),
            Value::String("L4".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::Int64(30),
            Value::String("L5".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(6),
            Value::Int64(30),
            Value::String("L6".to_string()),
        ]),
    ];

    let right_rows = vec![
        Row::new(vec![
            Value::Int64(101),
            Value::Int64(10),
            Value::String("R1".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(102),
            Value::Int64(10),
            Value::String("R2".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(103),
            Value::Int64(20),
            Value::String("R3".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(104),
            Value::Int64(20),
            Value::String("R4".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(105),
            Value::Int64(20),
            Value::String("R5".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(106),
            Value::Int64(20),
            Value::String("R6".to_string()),
        ]),
    ];

    let (_, out, metrics) = joiner.execute_ref(&left_rows, &right_rows)?;
    assert_eq!(out.len(), 10);
    assert_eq!(metrics.matched_rows, 10);

    // Verify exact cross-product multiplicity for key 10
    let k10_rows: Vec<&Row> = out
        .iter()
        .filter(|r| r.values[1] == Value::Int64(10) && r.values[4] == Value::Int64(10))
        .collect();
    assert_eq!(k10_rows.len(), 6);

    // Verify exact cross-product multiplicity for key 20
    let k20_rows: Vec<&Row> = out
        .iter()
        .filter(|r| r.values[1] == Value::Int64(20) && r.values[4] == Value::Int64(20))
        .collect();
    assert_eq!(k20_rows.len(), 4);

    Ok(())
}

#[test]
fn test_null_join_keys_sql_semantics() -> Result<()> {
    let (left_schema, right_schema) = create_schema_pair();
    let equi_plan = analyze_equi_join(&create_equi_expr(), &left_schema, &right_schema).unwrap();

    let joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan.clone(),
        JoinConfig::default(),
    );

    // Under SQL 3VL, NULL = NULL is UNKNOWN (false in filter).
    // Rows with NULL join key must NEVER match each other.
    let left_rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Null,
            Value::String("L_Null1".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Null,
            Value::String("L_Null2".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::Int64(5),
            Value::String("L_Valid".to_string()),
        ]),
    ];

    let right_rows = vec![
        Row::new(vec![
            Value::Int64(10),
            Value::Null,
            Value::String("R_Null1".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(20),
            Value::Null,
            Value::String("R_Null2".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(30),
            Value::Int64(5),
            Value::String("R_Valid".to_string()),
        ]),
    ];

    let (_, out, metrics) = joiner.execute_ref(&left_rows, &right_rows)?;
    // Only the valid key (5) matches!
    assert_eq!(out.len(), 1);
    assert_eq!(metrics.matched_rows, 1);
    assert_eq!(out[0].values[0], Value::Int64(3));
    assert_eq!(out[0].values[3], Value::Int64(30));

    // For LEFT JOIN, the 2 NULL left rows must be preserved with NULL padding
    let left_joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Left,
        equi_plan,
        JoinConfig::default(),
    );
    let (_, out_left, _) = left_joiner.execute_ref(&left_rows, &right_rows)?;
    assert_eq!(out_left.len(), 3);
    let null_padded: Vec<&Row> = out_left
        .iter()
        .filter(|r| r.values[1] == Value::Null)
        .collect();
    assert_eq!(null_padded.len(), 2);
    for r in null_padded {
        assert_eq!(r.values[3], Value::Null);
        assert_eq!(r.values[4], Value::Null);
        assert_eq!(r.values[5], Value::Null);
    }

    Ok(())
}

#[test]
fn test_string_keys_and_composite_keys() -> Result<()> {
    let schema_left = Schema::new_projected(vec![
        Column::new("users.region", DataType::String, false),
        Column::new("users.dept", DataType::String, false),
        Column::new("users.name", DataType::String, false),
    ]);
    let schema_right = Schema::new_projected(vec![
        Column::new("salaries.reg", DataType::String, false),
        Column::new("salaries.department", DataType::String, false),
        Column::new("salaries.amount", DataType::Float64, false),
    ]);

    // Composite equi-join: users.region = salaries.reg AND users.dept = salaries.department
    let on_expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("users.region".to_string())),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Column("salaries.reg".to_string())),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("users.dept".to_string())),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Column("salaries.department".to_string())),
        }),
    };

    let equi_plan = analyze_equi_join(&on_expr, &schema_left, &schema_right).unwrap();
    assert_eq!(equi_plan.left_keys.len(), 2);
    assert_eq!(equi_plan.right_keys.len(), 2);

    let joiner = ExternalHashJoiner::new(
        &schema_left,
        &schema_right,
        JoinType::Inner,
        equi_plan,
        JoinConfig::default(),
    );

    let left_rows = vec![
        Row::new(vec![
            Value::String("US".to_string()),
            Value::String("Eng".to_string()),
            Value::String("Alice".to_string()),
        ]),
        Row::new(vec![
            Value::String("EU".to_string()),
            Value::String("Eng".to_string()),
            Value::String("Bob".to_string()),
        ]),
        Row::new(vec![
            Value::String("US".to_string()),
            Value::String("Sales".to_string()),
            Value::String("Charlie".to_string()),
        ]),
    ];

    let right_rows = vec![
        Row::new(vec![
            Value::String("US".to_string()),
            Value::String("Eng".to_string()),
            Value::Float64(150_000.0),
        ]),
        Row::new(vec![
            Value::String("US".to_string()),
            Value::String("Eng".to_string()),
            Value::Float64(160_000.0),
        ]),
        Row::new(vec![
            Value::String("APAC".to_string()),
            Value::String("Eng".to_string()),
            Value::Float64(120_000.0),
        ]),
    ];

    let (_, out, metrics) = joiner.execute_ref(&left_rows, &right_rows)?;
    // Alice matches 2 salary rows; Bob and Charlie don't match APAC
    assert_eq!(out.len(), 2);
    assert_eq!(metrics.matched_rows, 2);
    assert_eq!(out[0].values[2], Value::String("Alice".to_string()));
    assert_eq!(out[1].values[2], Value::String("Alice".to_string()));

    Ok(())
}

#[test]
fn test_residual_predicates() -> Result<()> {
    let (left_schema, right_schema) = create_schema_pair();
    // ON t1.key = t2.key AND t1.id < t2.id
    let on_expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("t1.key".to_string())),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Column("t2.key".to_string())),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("t1.id".to_string())),
            op: BinaryOp::Lt,
            right: Box::new(Expr::Column("t2.id".to_string())),
        }),
    };

    let equi_plan = analyze_equi_join(&on_expr, &left_schema, &right_schema).unwrap();
    assert!(equi_plan.residual_predicate.is_some());

    let joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan,
        JoinConfig::default(),
    );

    let left_rows = vec![
        Row::new(vec![
            Value::Int64(10),
            Value::Int64(1),
            Value::String("L1".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(50),
            Value::Int64(1),
            Value::String("L2".to_string()),
        ]),
    ];

    let right_rows = vec![Row::new(vec![
        Value::Int64(20),
        Value::Int64(1),
        Value::String("R1".to_string()),
    ])];

    let (_, out, metrics) = joiner.execute_ref(&left_rows, &right_rows)?;
    // Only L1 (id 10 < 20) satisfies residual predicate; L2 (id 50 < 20) is rejected
    assert_eq!(out.len(), 1);
    assert_eq!(metrics.candidate_pairs, 2);
    assert_eq!(metrics.matched_rows, 1);
    assert_eq!(out[0].values[0], Value::Int64(10));

    Ok(())
}

#[test]
fn test_memory_budget_spill_equivalence() -> Result<()> {
    let (left_schema, right_schema) = create_schema_pair();
    let equi_plan = analyze_equi_join(&create_equi_expr(), &left_schema, &right_schema).unwrap();

    let n = 200;
    let left_rows: Vec<Row> = (1..=n)
        .map(|i| {
            Row::new(vec![
                Value::Int64(i),
                Value::Int64(i % 20),
                Value::String(format!("LeftPayload_{i}")),
            ])
        })
        .collect();

    let right_rows: Vec<Row> = (1..=n)
        .map(|i| {
            Row::new(vec![
                Value::Int64(i + 1000),
                Value::Int64(i % 20),
                Value::String(format!("RightPayload_{i}")),
            ])
        })
        .collect();

    // 1. In-memory execution (2 MB budget)
    let in_mem_config = JoinConfig::new().with_memory_budget_bytes(2 * 1024 * 1024);
    let in_mem_joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan.clone(),
        in_mem_config,
    );
    let (_, in_mem_out, in_mem_metrics) = in_mem_joiner.execute_ref(&left_rows, &right_rows)?;
    assert_eq!(in_mem_metrics.bytes_spilled, 0);
    assert_eq!(in_mem_metrics.runs_created, 0);

    // 2. Forced external spill execution (tiny budget: 512 bytes)
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_config = JoinConfig::new()
        .with_memory_budget_bytes(512)
        .with_temp_dir(temp_dir.path());
    let spill_joiner = ExternalHashJoiner::new(
        &left_schema,
        &right_schema,
        JoinType::Inner,
        equi_plan,
        spill_config,
    );
    let (_, spill_out, spill_metrics) = spill_joiner.execute_ref(&left_rows, &right_rows)?;
    assert!(spill_metrics.bytes_spilled > 0);
    assert!(spill_metrics.runs_created > 0);

    // 3. Multiset equivalence verification
    assert_multiset_equivalent(&spill_out, &in_mem_out);

    Ok(())
}

#[test]
fn test_transaction_and_mvcc_visibility_in_joins() -> Result<()> {
    let temp_dir = tempfile::tempdir().unwrap();
    let mut db = TreeDb::create(temp_dir.path().join("join_mvcc.db"), 64).unwrap();

    let mut txn1 = db.begin_transaction().unwrap();
    txn1.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")?;
    txn1.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")?;

    txn1.execute_statement("INSERT INTO t1 VALUES (1, 100);")?;
    txn1.execute_statement("INSERT INTO t1 VALUES (2, 200);")?;
    txn1.execute_statement("INSERT INTO t2 VALUES (10, 100);")?;
    txn1.execute_statement("INSERT INTO t2 VALUES (20, 200);")?;
    txn1.commit()?;

    // Read initial 2 join matches
    {
        let mut txn = db.begin_transaction().unwrap();
        let res = txn.execute_sql("SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.val = t2.val;")?;
        assert_eq!(res.rows.len(), 2);
    }

    // Uncommitted insert in txn and immediate join visibility, followed by rollback
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("INSERT INTO t1 VALUES (3, 100);")?;
        let res = txn.execute_sql("SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.val = t2.val;")?;
        assert_eq!(res.rows.len(), 3);
        txn.abort()?;
    }

    // After rollback, join sees only the original 2 matches
    {
        let mut txn = db.begin_transaction().unwrap();
        let res = txn.execute_sql("SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.val = t2.val;")?;
        assert_eq!(res.rows.len(), 2);
    }

    // Committed insert in txn
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("INSERT INTO t1 VALUES (3, 100);")?;
        txn.commit()?;
    }

    // After commit, new txn sees 3 matches
    {
        let mut txn = db.begin_transaction().unwrap();
        let res = txn.execute_sql("SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.val = t2.val;")?;
        assert_eq!(res.rows.len(), 3);
    }

    Ok(())
}
