use std::cmp::Ordering;
use std::path::PathBuf;
use tempfile::TempDir;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::query::expr::Expr;
use treedb::query::operator::{SortDirection, SortKey};
use treedb::TreeDb;

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("query_test.treedb");
    (dir, path)
}

fn create_test_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("name", DataType::String, false),
            Column::new("age", DataType::Int64, true),
            Column::new("score", DataType::Float64, true),
            Column::new("active", DataType::Boolean, true),
        ],
        0,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// 1. 3VL Truth Tables & Boolean Logic
// ---------------------------------------------------------------------------

#[test]
fn test_3vl_truth_tables_and() {
    let schema = create_test_schema();
    let dummy_row = Row::new(vec![
        Value::Int64(1),
        Value::String("Alice".into()),
        Value::Null,
        Value::Null,
        Value::Null,
    ]);

    let t = Expr::literal(Value::Boolean(true));
    let f = Expr::literal(Value::Boolean(false));
    let n = Expr::literal(Value::Null);

    // TRUE AND ...
    assert_eq!(
        Expr::and(t.clone(), t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::and(t.clone(), f.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::and(t.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );

    // FALSE AND ...
    assert_eq!(
        Expr::and(f.clone(), t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::and(f.clone(), f.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::and(f.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );

    // NULL AND ...
    assert_eq!(
        Expr::and(n.clone(), t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        Expr::and(n.clone(), f.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::and(n.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
}

#[test]
fn test_3vl_truth_tables_or() {
    let schema = create_test_schema();
    let dummy_row = Row::new(vec![
        Value::Int64(1),
        Value::String("Alice".into()),
        Value::Null,
        Value::Null,
        Value::Null,
    ]);

    let t = Expr::literal(Value::Boolean(true));
    let f = Expr::literal(Value::Boolean(false));
    let n = Expr::literal(Value::Null);

    // TRUE OR ...
    assert_eq!(
        Expr::or(t.clone(), t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::or(t.clone(), f.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::or(t.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );

    // FALSE OR ...
    assert_eq!(
        Expr::or(f.clone(), t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::or(f.clone(), f.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::or(f.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );

    // NULL OR ...
    assert_eq!(
        Expr::or(n.clone(), t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::or(n.clone(), f.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        Expr::or(n.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
}

#[test]
fn test_3vl_truth_tables_not_and_is_null() {
    let schema = create_test_schema();
    let dummy_row = Row::new(vec![
        Value::Int64(1),
        Value::String("Alice".into()),
        Value::Null,
        Value::Null,
        Value::Null,
    ]);

    let t = Expr::literal(Value::Boolean(true));
    let f = Expr::literal(Value::Boolean(false));
    let n = Expr::literal(Value::Null);

    // NOT
    assert_eq!(
        Expr::not(t.clone()).evaluate(&dummy_row, &schema).unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::not(f.clone()).evaluate(&dummy_row, &schema).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::not(n.clone()).evaluate(&dummy_row, &schema).unwrap(),
        Value::Null
    );

    // IS NULL / IS NOT NULL
    assert_eq!(
        Expr::is_null(n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::is_null(t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::is_not_null(n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(false)
    );
    assert_eq!(
        Expr::is_not_null(t.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Boolean(true)
    );
}

// ---------------------------------------------------------------------------
// 2. Comparison Semantics & NULL Comparison
// ---------------------------------------------------------------------------

#[test]
fn test_null_comparisons_produce_null() {
    let schema = create_test_schema();
    let dummy_row = Row::new(vec![
        Value::Int64(1),
        Value::String("Alice".into()),
        Value::Null,
        Value::Null,
        Value::Null,
    ]);

    let n = Expr::literal(Value::Null);
    let one = Expr::literal(Value::Int64(1));

    assert_eq!(
        Expr::eq(n.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        Expr::not_eq(n.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        Expr::lt(n.clone(), one.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
    assert_eq!(
        Expr::gt(one.clone(), n.clone())
            .evaluate(&dummy_row, &schema)
            .unwrap(),
        Value::Null
    );
}

#[test]
fn test_comparison_all_types() {
    let schema = create_test_schema();
    let dummy_row = Row::new(vec![
        Value::Int64(1),
        Value::String("Alice".into()),
        Value::Null,
        Value::Null,
        Value::Null,
    ]);

    // Int64
    assert_eq!(
        Expr::eq(
            Expr::literal(Value::Int64(10)),
            Expr::literal(Value::Int64(10))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(
        Expr::lt(
            Expr::literal(Value::Int64(5)),
            Expr::literal(Value::Int64(10))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(true)
    );

    // UInt64
    assert_eq!(
        Expr::gt_eq(
            Expr::literal(Value::UInt64(100)),
            Expr::literal(Value::UInt64(100))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(true)
    );

    // Float64
    assert_eq!(
        Expr::lt(
            Expr::literal(Value::Float64(12.34)),
            Expr::literal(Value::Float64(12.35))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(true)
    );

    // Float64 NaN (IEEE 754: NaN == NaN is false)
    assert_eq!(
        Expr::eq(
            Expr::literal(Value::Float64(f64::NAN)),
            Expr::literal(Value::Float64(f64::NAN))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(false)
    );

    // String
    assert_eq!(
        Expr::lt(
            Expr::literal(Value::String("apple".into())),
            Expr::literal(Value::String("banana".into()))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(true)
    );

    // Bytes
    assert_eq!(
        Expr::lt_eq(
            Expr::literal(Value::Bytes(vec![1, 2, 3])),
            Expr::literal(Value::Bytes(vec![1, 2, 4]))
        )
        .evaluate(&dummy_row, &schema)
        .unwrap(),
        Value::Boolean(true)
    );
}

// ---------------------------------------------------------------------------
// 3. Type Checking & Error Model
// ---------------------------------------------------------------------------

#[test]
fn test_type_checking_and_error_handling() {
    let schema = create_test_schema();

    // Valid boolean infer
    assert_eq!(
        Expr::eq(Expr::column("id"), Expr::literal(Value::Int64(42)))
            .infer_type(&schema)
            .unwrap(),
        DataType::Boolean
    );

    // Invalid column
    assert!(matches!(
        Expr::column("non_existent").infer_type(&schema),
        Err(Error::ColumnNotFound(_))
    ));

    // Type mismatch comparison (String vs Int64)
    assert!(matches!(
        Expr::eq(Expr::column("name"), Expr::literal(Value::Int64(42))).infer_type(&schema),
        Err(Error::InvalidComparison { .. })
    ));

    // Boolean inequality (Boolean < Boolean is invalid)
    assert!(matches!(
        Expr::lt(Expr::column("active"), Expr::literal(Value::Boolean(true))).infer_type(&schema),
        Err(Error::InvalidComparison { .. })
    ));

    // Invalid boolean operand (Int64 AND Boolean)
    assert!(matches!(
        Expr::and(Expr::column("id"), Expr::column("active")).infer_type(&schema),
        Err(Error::InvalidBooleanExpression { .. })
    ));
}

// ---------------------------------------------------------------------------
// 4. CRUD Query Pipeline Operations
// ---------------------------------------------------------------------------

fn setup_populated_db() -> (TreeDb, TempDir) {
    let (dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 64).unwrap();
    let schema = create_test_schema();
    db.create_table("users", schema).unwrap();

    let mut table = db.open_table("users").unwrap();
    let rows = vec![
        Row::new(vec![
            Value::Int64(10),
            Value::String("Alice".into()),
            Value::Int64(25),
            Value::Float64(95.5),
            Value::Boolean(true),
        ]),
        Row::new(vec![
            Value::Int64(20),
            Value::String("Bob".into()),
            Value::Int64(30),
            Value::Float64(88.0),
            Value::Boolean(false),
        ]),
        Row::new(vec![
            Value::Int64(30),
            Value::String("Charlie".into()),
            Value::Int64(25),
            Value::Null,
            Value::Boolean(true),
        ]),
        Row::new(vec![
            Value::Int64(40),
            Value::String("Diana".into()),
            Value::Null,
            Value::Float64(99.0),
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(50),
            Value::String("Eve".into()),
            Value::Int64(40),
            Value::Float64(72.5),
            Value::Boolean(true),
        ]),
    ];

    for r in &rows {
        table.insert(r).unwrap();
    }

    (db, dir)
}

#[test]
fn test_query_filter_matching_and_null_discard() {
    let (mut db, _dir) = setup_populated_db();

    // age == 25 (Alice, Charlie)
    let res = db
        .query("users")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("age"),
            Expr::literal(Value::Int64(25)),
        ))
        .execute()
        .unwrap();

    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(10));
    assert_eq!(res.rows[1].values[0], Value::Int64(30));

    // score > 90.0 (Alice 95.5, Diana 99.0; Charlie NULL is discarded)
    let res = db
        .query("users")
        .unwrap()
        .filter(Expr::gt(
            Expr::column("score"),
            Expr::literal(Value::Float64(90.0)),
        ))
        .execute()
        .unwrap();

    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(10));
    assert_eq!(res.rows[1].values[0], Value::Int64(40));
}

#[test]
fn test_query_projection_and_schema_derivation() {
    let (mut db, _dir) = setup_populated_db();

    let res = db
        .query("users")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("id"),
            Expr::literal(Value::Int64(10)),
        ))
        .project(vec![
            Expr::column("name"),
            Expr::column("age"),
            Expr::gt(Expr::column("age"), Expr::literal(Value::Int64(18))),
        ])
        .execute()
        .unwrap();

    assert_eq!(res.len(), 1);
    assert_eq!(res.schema.columns.len(), 3);
    assert_eq!(res.schema.columns[0].name, "name");
    assert_eq!(res.schema.columns[0].data_type, DataType::String);
    assert_eq!(res.schema.columns[1].name, "age");
    assert_eq!(res.schema.columns[1].data_type, DataType::Int64);
    assert_eq!(res.schema.columns[2].name, "col_2");
    assert_eq!(res.schema.columns[2].data_type, DataType::Boolean);

    let row = &res.rows[0];
    assert_eq!(row.values[0], Value::String("Alice".into()));
    assert_eq!(row.values[1], Value::Int64(25));
    assert_eq!(row.values[2], Value::Boolean(true));
}

#[test]
fn test_query_sort_multi_key_and_null_ordering() {
    let (mut db, _dir) = setup_populated_db();

    // ORDER BY age ASC, score DESC
    // age: NULL (Diana), 25 (Alice, Charlie), 30 (Bob), 40 (Eve)
    // Default NULL ordering in ASC is NULLs first.
    let res = db
        .query("users")
        .unwrap()
        .sort(vec![
            SortKey::asc(Expr::column("age")),
            SortKey::desc(Expr::column("score")),
        ])
        .execute()
        .unwrap();

    assert_eq!(res.len(), 5);
    // 1st: Diana (age NULL)
    assert_eq!(res.rows[0].values[0], Value::Int64(40));
    // 2nd: Alice (age 25, score 95.5)
    assert_eq!(res.rows[1].values[0], Value::Int64(10));
    // 3rd: Charlie (age 25, score NULL - NULLs last in DESC)
    assert_eq!(res.rows[2].values[0], Value::Int64(30));
    // 4th: Bob (age 30, score 88.0)
    assert_eq!(res.rows[3].values[0], Value::Int64(20));
    // 5th: Eve (age 40, score 72.5)
    assert_eq!(res.rows[4].values[0], Value::Int64(50));
}

#[test]
fn test_query_offset_and_limit() {
    let (mut db, _dir) = setup_populated_db();

    // Scan all, offset 2, limit 2
    let res = db
        .query("users")
        .unwrap()
        .offset(2)
        .limit(2)
        .execute()
        .unwrap();

    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(30)); // Charlie
    assert_eq!(res.rows[1].values[0], Value::Int64(40)); // Diana

    // Limit 0 returns empty
    let res_empty = db.query("users").unwrap().limit(0).execute().unwrap();
    assert_eq!(res_empty.len(), 0);

    // Offset beyond end returns empty
    let res_out = db.query("users").unwrap().offset(100).execute().unwrap();
    assert_eq!(res_out.len(), 0);
}

#[test]
fn test_query_full_pipeline_composition() {
    let (mut db, _dir) = setup_populated_db();

    let res = db
        .query("users")
        .unwrap()
        .filter(Expr::is_not_null(Expr::column("age")))
        .project(vec![Expr::column("id"), Expr::column("name")])
        .sort_by(Expr::column("id"), SortDirection::Descending)
        .offset(1)
        .limit(2)
        .execute()
        .unwrap();

    // Eligible by age is not null: [10 (Alice), 20 (Bob), 30 (Charlie), 50 (Eve)]
    // Sorted DESC by id: [50, 30, 20, 10]
    // Offset 1, Limit 2: [30 (Charlie), 20 (Bob)]
    assert_eq!(res.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(30));
    assert_eq!(res.rows[0].values[1], Value::String("Charlie".into()));
    assert_eq!(res.rows[1].values[0], Value::Int64(20));
    assert_eq!(res.rows[1].values[1], Value::String("Bob".into()));
}

// ---------------------------------------------------------------------------
// 5. Primary-Key Optimizations & Equivalence
// ---------------------------------------------------------------------------

#[test]
fn test_primary_key_point_lookup_optimization_equivalence() {
    let (mut db, _dir) = setup_populated_db();

    // Target PK = 20
    let opt_res = db
        .query("users")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("id"),
            Expr::literal(Value::Int64(20)),
        ))
        .execute()
        .unwrap();

    // Target PK = 999 (missing)
    let missing_res = db
        .query("users")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("id"),
            Expr::literal(Value::Int64(999)),
        ))
        .execute()
        .unwrap();

    assert_eq!(opt_res.len(), 1);
    assert_eq!(opt_res.rows[0].values[0], Value::Int64(20));
    assert_eq!(opt_res.rows[0].values[1], Value::String("Bob".into()));
    assert_eq!(missing_res.len(), 0);
}

#[test]
fn test_primary_key_range_scan_optimization_equivalence() {
    let (mut db, _dir) = setup_populated_db();

    // Range: id >= 20 AND id < 50 -> [20, 30, 40]
    let res = db
        .query("users")
        .unwrap()
        .filter(Expr::and(
            Expr::gt_eq(Expr::column("id"), Expr::literal(Value::Int64(20))),
            Expr::lt(Expr::column("id"), Expr::literal(Value::Int64(50))),
        ))
        .execute()
        .unwrap();

    assert_eq!(res.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(20));
    assert_eq!(res.rows[1].values[0], Value::Int64(30));
    assert_eq!(res.rows[2].values[0], Value::Int64(40));
}

// ---------------------------------------------------------------------------
// 6. Transaction Integration & Read-Your-Writes
// ---------------------------------------------------------------------------

#[test]
fn test_query_transaction_lifecycle_and_visibility() {
    let (mut db, _dir) = setup_populated_db();

    let mut txn = db.begin().unwrap();

    // Insert within txn
    let new_row = Row::new(vec![
        Value::Int64(60),
        Value::String("Frank".into()),
        Value::Int64(50),
        Value::Float64(85.0),
        Value::Boolean(true),
    ]);
    {
        let mut table = txn.open_table("users").unwrap();
        table.insert(&new_row).unwrap();
    }

    // Query sees inserted row
    let res = txn
        .query("users")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("id"),
            Expr::literal(Value::Int64(60)),
        ))
        .execute()
        .unwrap();

    assert_eq!(res.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::String("Frank".into()));

    // Abort transaction
    txn.abort().unwrap();

    // After rollback, row 60 is gone
    let res_post = db
        .query("users")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("id"),
            Expr::literal(Value::Int64(60)),
        ))
        .execute()
        .unwrap();

    assert_eq!(res_post.len(), 0);
}

// ---------------------------------------------------------------------------
// 7. Persistence & Crash Recovery Verification
// ---------------------------------------------------------------------------

#[test]
fn test_query_persistence_and_recovery() {
    let (dir, db_path) = temp_db_path();

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        let schema = create_test_schema();
        db.create_table("users", schema).unwrap();
        let mut txn = db.begin().unwrap();
        {
            let mut table = txn.open_table("users").unwrap();
            table
                .insert(&Row::new(vec![
                    Value::Int64(1),
                    Value::String("Persisted".into()),
                    Value::Int64(99),
                    Value::Float64(100.0),
                    Value::Boolean(true),
                ]))
                .unwrap();
        }
        txn.commit().unwrap();
    }

    // Reopen and query
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let res = db
            .query("users")
            .unwrap()
            .filter(Expr::eq(Expr::column("id"), Expr::literal(Value::Int64(1))))
            .execute()
            .unwrap();

        assert_eq!(res.len(), 1);
        assert_eq!(res.rows[0].values[1], Value::String("Persisted".into()));
    }

    drop(dir);
}

// ---------------------------------------------------------------------------
// 8. Deterministic Randomized Expression Oracle Testing
// ---------------------------------------------------------------------------

#[test]
fn test_randomized_expression_oracle() {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val_a", DataType::Int64, true),
            Column::new("val_b", DataType::Int64, true),
            Column::new("flag", DataType::Boolean, true),
        ],
        0,
    )
    .unwrap();

    // Deterministic pseudo-random number generator (LCG)
    let mut state: u64 = 0xDEADBEEFCAFE;
    let mut next_u32 = || -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 32) as u32
    };

    for i in 0..100 {
        let val_a = if next_u32() % 5 == 0 {
            Value::Null
        } else {
            Value::Int64((next_u32() % 50) as i64)
        };
        let val_b = if next_u32() % 5 == 0 {
            Value::Null
        } else {
            Value::Int64((next_u32() % 50) as i64)
        };
        let flag = if next_u32() % 5 == 0 {
            Value::Null
        } else {
            Value::Boolean(next_u32() % 2 == 0)
        };

        let row = Row::new(vec![
            Value::Int64(i),
            val_a.clone(),
            val_b.clone(),
            flag.clone(),
        ]);

        // Independent 3VL reference evaluator for: (val_a > 20) AND (flag IS NOT NULL)
        let left_val = match &val_a {
            Value::Int64(a) => Value::Boolean(*a > 20),
            Value::Null => Value::Null,
            _ => unreachable!(),
        };
        let right_val = Value::Boolean(!flag.is_null());

        let expected_pred = match (left_val, right_val) {
            (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Value::Boolean(false),
            (Value::Boolean(true), Value::Boolean(true)) => Value::Boolean(true),
            _ => Value::Null,
        };

        let expr = Expr::and(
            Expr::gt(Expr::column("val_a"), Expr::literal(Value::Int64(20))),
            Expr::is_not_null(Expr::column("flag")),
        );

        let actual = expr.evaluate(&row, &schema).unwrap();
        assert_eq!(actual, expected_pred, "Mismatch on iteration {i}");
    }
}

// ---------------------------------------------------------------------------
// 9. Deterministic Randomized Query Pipeline Oracle Comparison
// ---------------------------------------------------------------------------

#[test]
fn test_randomized_query_pipeline_oracle() {
    let (dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 64).unwrap();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("category", DataType::String, false),
            Column::new("value", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    db.create_table("items", schema.clone()).unwrap();
    let mut table = db.open_table("items").unwrap();

    let mut state: u64 = 0xCAFEBABE1234;
    let mut next_u32 = || -> u32 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 32) as u32
    };

    let mut ref_rows = Vec::new();

    for i in 0..100 {
        let cat = format!("cat_{}", next_u32() % 5);
        let val = if next_u32() % 4 == 0 {
            Value::Null
        } else {
            Value::Int64((next_u32() % 100) as i64)
        };

        let row = Row::new(vec![Value::Int64(i as i64), Value::String(cat), val]);
        table.insert(&row).unwrap();
        ref_rows.push(row);
    }

    // Query: category == "cat_1" AND value > 30, ORDER BY value DESC, offset 1, limit 3
    let res = db
        .query("items")
        .unwrap()
        .filter(Expr::and(
            Expr::eq(
                Expr::column("category"),
                Expr::literal(Value::String("cat_1".into())),
            ),
            Expr::gt(Expr::column("value"), Expr::literal(Value::Int64(30))),
        ))
        .sort_by(Expr::column("value"), SortDirection::Descending)
        .offset(1)
        .limit(3)
        .project(vec![Expr::column("id"), Expr::column("value")])
        .execute()
        .unwrap();

    // In-memory reference execution directly on `ref_rows`
    let mut ref_filtered: Vec<Row> = ref_rows
        .into_iter()
        .filter(|r| {
            let cat_match = match &r.values[1] {
                Value::String(s) => s == "cat_1",
                _ => false,
            };
            let val_match = match &r.values[2] {
                Value::Int64(v) => *v > 30,
                _ => false,
            };
            cat_match && val_match
        })
        .collect();

    // Sort DESC by value (NULLs last in DESC)
    ref_filtered.sort_by(|a, b| match (&a.values[2], &b.values[2]) {
        (Value::Int64(x), Value::Int64(y)) => y.cmp(x), // DESC
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater, // NULLs last
        (_, Value::Null) => Ordering::Less,
        _ => unreachable!(),
    });

    let ref_paginated: Vec<Row> = ref_filtered.into_iter().skip(1).take(3).collect();
    let ref_projected: Vec<Row> = ref_paginated
        .into_iter()
        .map(|r| Row::new(vec![r.values[0].clone(), r.values[2].clone()]))
        .collect();

    assert_eq!(res.rows, ref_projected);

    drop(dir);
}

// ---------------------------------------------------------------------------
// 10. Additional Edge Cases, Float Total Ordering & Multi-Table Tests
// ---------------------------------------------------------------------------

#[test]
fn test_query_float_sorting_with_nan_and_infinities() {
    let (dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 64).unwrap();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Float64, true),
        ],
        0,
    )
    .unwrap();

    db.create_table("floats", schema).unwrap();
    let mut table = db.open_table("floats").unwrap();

    // Insert -inf, 0.0, +inf, NaN, NULL
    table
        .insert(&Row::new(vec![
            Value::Int64(1),
            Value::Float64(f64::NEG_INFINITY),
        ]))
        .unwrap();
    table
        .insert(&Row::new(vec![Value::Int64(2), Value::Float64(0.0)]))
        .unwrap();
    table
        .insert(&Row::new(vec![
            Value::Int64(3),
            Value::Float64(f64::INFINITY),
        ]))
        .unwrap();
    table
        .insert(&Row::new(vec![Value::Int64(4), Value::Float64(f64::NAN)]))
        .unwrap();
    table
        .insert(&Row::new(vec![Value::Int64(5), Value::Null]))
        .unwrap();

    // ASC order: NULL (1st), -inf (2nd), 0.0 (3rd), +inf (4th), NaN (5th)
    let res = db
        .query("floats")
        .unwrap()
        .sort_by(Expr::column("val"), SortDirection::Ascending)
        .execute()
        .unwrap();

    assert_eq!(res.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(5)); // NULL
    assert_eq!(res.rows[1].values[0], Value::Int64(1)); // -inf
    assert_eq!(res.rows[2].values[0], Value::Int64(2)); // 0.0
    assert_eq!(res.rows[3].values[0], Value::Int64(3)); // +inf
    assert_eq!(res.rows[4].values[0], Value::Int64(4)); // NaN

    drop(dir);
}

#[test]
fn test_query_explicit_nulls_first_and_last() {
    let (mut db, _dir) = setup_populated_db();

    // ASC with explicit NULLS LAST
    let res = db
        .query("users")
        .unwrap()
        .sort(vec![
            SortKey::asc(Expr::column("age")).with_nulls_first(false)
        ])
        .execute()
        .unwrap();

    assert_eq!(res.len(), 5);
    // Non-null ages: 25, 25, 30, 40, followed by NULL (Diana)
    assert_eq!(res.rows[4].values[0], Value::Int64(40)); // Diana

    // DESC with explicit NULLS FIRST
    let res_desc = db
        .query("users")
        .unwrap()
        .sort(vec![
            SortKey::desc(Expr::column("age")).with_nulls_first(true)
        ])
        .execute()
        .unwrap();

    assert_eq!(res_desc.len(), 5);
    assert_eq!(res_desc.rows[0].values[0], Value::Int64(40)); // Diana (NULL first)
}

#[test]
fn test_query_empty_table() {
    let (dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 64).unwrap();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("data", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    db.create_table("empty_tbl", schema).unwrap();

    let res = db
        .query("empty_tbl")
        .unwrap()
        .filter(Expr::eq(
            Expr::column("id"),
            Expr::literal(Value::UInt64(10)),
        ))
        .execute()
        .unwrap();

    assert_eq!(res.len(), 0);
    assert!(res.is_empty());
    assert_eq!(res.schema.columns.len(), 2);

    drop(dir);
}

#[test]
fn test_query_multi_table_atomic_transaction() {
    let (dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 64).unwrap();

    let schema_a = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val_a", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    let schema_b = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val_b", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    db.create_table("tbl_a", schema_a).unwrap();
    db.create_table("tbl_b", schema_b).unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl_a = txn.open_table("tbl_a").unwrap();
        tbl_a
            .insert(&Row::new(vec![Value::Int64(1), Value::String("A1".into())]))
            .unwrap();
    }
    {
        let mut tbl_b = txn.open_table("tbl_b").unwrap();
        tbl_b
            .insert(&Row::new(vec![Value::Int64(1), Value::Int64(100)]))
            .unwrap();
    }

    // Query both inside txn
    let res_a = txn
        .query("tbl_a")
        .unwrap()
        .filter(Expr::eq(Expr::column("id"), Expr::literal(Value::Int64(1))))
        .execute()
        .unwrap();
    let res_b = txn
        .query("tbl_b")
        .unwrap()
        .filter(Expr::eq(Expr::column("id"), Expr::literal(Value::Int64(1))))
        .execute()
        .unwrap();

    assert_eq!(res_a.len(), 1);
    assert_eq!(res_a.rows[0].values[1], Value::String("A1".into()));
    assert_eq!(res_b.len(), 1);
    assert_eq!(res_b.rows[0].values[1], Value::Int64(100));

    txn.commit().unwrap();

    // Verify after commit
    let res_a_committed = db.query("tbl_a").unwrap().execute().unwrap();
    let res_b_committed = db.query("tbl_b").unwrap().execute().unwrap();
    assert_eq!(res_a_committed.len(), 1);
    assert_eq!(res_b_committed.len(), 1);

    drop(dir);
}
