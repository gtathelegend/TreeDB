use std::cmp::Ordering;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::query::expr::{BinaryOp, Expr};
use treedb::query::operator::{SortDirection, SortKey, SortOperator};

fn create_test_schema() -> Schema {
    let columns = vec![
        Column::new("id", DataType::Int64, false),
        Column::new("category", DataType::String, false),
        Column::new("val_num", DataType::Int64, false),
        Column::new("val_float", DataType::Float64, false),
        Column::new("val_opt", DataType::Int64, true),
    ];
    Schema::new(columns, 0).expect("create test schema")
}

#[test]
fn test_empty_and_single_row_sort() {
    let schema = create_test_schema();
    let sort_keys = vec![SortKey {
        expression: Expr::Column("id".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];

    // Empty input
    let empty_rows: Vec<Row> = vec![];
    let res = SortOperator::apply(empty_rows, &schema, &sort_keys).expect("sort empty rows");
    assert!(res.is_empty());

    // Single row
    let single = vec![Row::new(vec![
        Value::Int64(42),
        Value::String("cat".to_string()),
        Value::Int64(10),
        Value::Float64(3.5),
        Value::Null,
    ])];
    let res = SortOperator::apply(single.clone(), &schema, &sort_keys).expect("sort single row");
    assert_eq!(res, single);
}

#[test]
fn test_single_key_numeric_asc_and_desc() {
    let schema = create_test_schema();
    let rows = vec![
        Row::new(vec![
            Value::Int64(30),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(10),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(50),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(20),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(40),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
    ];

    // Ascending
    let asc_keys = vec![SortKey {
        expression: Expr::Column("id".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];
    let res_asc = SortOperator::apply(rows.clone(), &schema, &asc_keys).expect("sort asc");
    let ids_asc: Vec<i64> = res_asc
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ids_asc, vec![10, 20, 30, 40, 50]);

    // Descending
    let desc_keys = vec![SortKey {
        expression: Expr::Column("id".to_string()),
        direction: SortDirection::Descending,
        nulls_first: None,
    }];
    let res_desc = SortOperator::apply(rows, &schema, &desc_keys).expect("sort desc");
    let ids_desc: Vec<i64> = res_desc
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(v) => v,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ids_desc, vec![50, 40, 30, 20, 10]);
}

#[test]
fn test_string_sorting() {
    let schema = create_test_schema();
    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("banana".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("apple".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("cherry".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::String("date".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
    ];

    let keys = vec![SortKey {
        expression: Expr::Column("category".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];
    let res = SortOperator::apply(rows, &schema, &keys).expect("sort string");
    let names: Vec<&str> = res
        .iter()
        .map(|r| match &r.values[1] {
            Value::String(s) => s.as_str(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(names, vec!["apple", "banana", "cherry", "date"]);
}

#[test]
fn test_multi_key_composite_sorting() {
    let schema = create_test_schema();
    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("B".to_string()),
            Value::Int64(10),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("A".to_string()),
            Value::Int64(30),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("B".to_string()),
            Value::Int64(20),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::String("A".to_string()),
            Value::Int64(10),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::String("A".to_string()),
            Value::Int64(20),
            Value::Null,
            Value::Null,
        ]),
    ];

    // ORDER BY category ASC, val_num DESC
    let keys = vec![
        SortKey {
            expression: Expr::Column("category".to_string()),
            direction: SortDirection::Ascending,
            nulls_first: None,
        },
        SortKey {
            expression: Expr::Column("val_num".to_string()),
            direction: SortDirection::Descending,
            nulls_first: None,
        },
    ];

    let res = SortOperator::apply(rows, &schema, &keys).expect("sort multi-key");
    let pairs: Vec<(&str, i64)> = res
        .iter()
        .map(|r| {
            let cat = match &r.values[1] {
                Value::String(s) => s.as_str(),
                _ => unreachable!(),
            };
            let num = match r.values[2] {
                Value::Int64(n) => n,
                _ => unreachable!(),
            };
            (cat, num)
        })
        .collect();

    assert_eq!(
        pairs,
        vec![("A", 30), ("A", 20), ("A", 10), ("B", 20), ("B", 10)]
    );
}

#[test]
fn test_null_ordering_explicit_and_defaults() {
    let schema = create_test_schema();
    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int64(20),
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Int64(10),
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
    ];

    // Default ASC: NULLS FIRST
    let asc_default_keys = vec![SortKey {
        expression: Expr::Column("val_opt".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];
    let res = SortOperator::apply(rows.clone(), &schema, &asc_default_keys).expect("asc default");
    assert_eq!(res[0].values[4], Value::Null);
    assert_eq!(res[1].values[4], Value::Null);
    assert_eq!(res[2].values[4], Value::Int64(10));
    assert_eq!(res[3].values[4], Value::Int64(20));

    // ASC with NULLS LAST
    let asc_nulls_last_keys = vec![SortKey {
        expression: Expr::Column("val_opt".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: Some(false),
    }];
    let res =
        SortOperator::apply(rows.clone(), &schema, &asc_nulls_last_keys).expect("asc nulls last");
    assert_eq!(res[0].values[4], Value::Int64(10));
    assert_eq!(res[1].values[4], Value::Int64(20));
    assert_eq!(res[2].values[4], Value::Null);
    assert_eq!(res[3].values[4], Value::Null);

    // Default DESC: NULLS LAST
    let desc_default_keys = vec![SortKey {
        expression: Expr::Column("val_opt".to_string()),
        direction: SortDirection::Descending,
        nulls_first: None,
    }];
    let res = SortOperator::apply(rows.clone(), &schema, &desc_default_keys).expect("desc default");
    assert_eq!(res[0].values[4], Value::Int64(20));
    assert_eq!(res[1].values[4], Value::Int64(10));
    assert_eq!(res[2].values[4], Value::Null);
    assert_eq!(res[3].values[4], Value::Null);

    // DESC with NULLS FIRST
    let desc_nulls_first_keys = vec![SortKey {
        expression: Expr::Column("val_opt".to_string()),
        direction: SortDirection::Descending,
        nulls_first: Some(true),
    }];
    let res = SortOperator::apply(rows, &schema, &desc_nulls_first_keys).expect("desc nulls first");
    assert_eq!(res[0].values[4], Value::Null);
    assert_eq!(res[1].values[4], Value::Null);
    assert_eq!(res[2].values[4], Value::Int64(20));
    assert_eq!(res[3].values[4], Value::Int64(10));
}

#[test]
fn test_duplicate_keys_and_stability() {
    let schema = create_test_schema();
    // 5 rows with identical key value, distinct IDs representing insertion order
    let rows = vec![
        Row::new(vec![
            Value::Int64(1),
            Value::String("dup".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(2),
            Value::String("dup".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(3),
            Value::String("dup".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(4),
            Value::String("dup".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(5),
            Value::String("dup".to_string()),
            Value::Null,
            Value::Null,
            Value::Null,
        ]),
    ];

    let keys = vec![SortKey {
        expression: Expr::Column("category".to_string()),
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];

    let res = SortOperator::apply(rows, &schema, &keys).expect("sort duplicates");
    let ids: Vec<i64> = res
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(id) => id,
            _ => unreachable!(),
        })
        .collect();

    // Stability: sequence numbers ensure FIFO order on ties
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn test_expression_sort_keys() {
    let schema = create_test_schema();
    let rows = vec![
        Row::new(vec![
            Value::Int64(10),
            Value::Null,
            Value::Int64(5),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(20),
            Value::Null,
            Value::Int64(1),
            Value::Null,
            Value::Null,
        ]),
        Row::new(vec![
            Value::Int64(30),
            Value::Null,
            Value::Int64(4),
            Value::Null,
            Value::Null,
        ]),
    ];

    // ORDER BY (id + val_num) ASC
    // Row 1: 10 + 5 = 15
    // Row 2: 20 + 1 = 21
    // Row 3: 30 + 4 = 34
    let expr = Expr::Binary {
        left: Box::new(Expr::Column("id".to_string())),
        op: BinaryOp::Add,
        right: Box::new(Expr::Column("val_num".to_string())),
    };

    let keys = vec![SortKey {
        expression: expr,
        direction: SortDirection::Ascending,
        nulls_first: None,
    }];

    let res = SortOperator::apply(rows, &schema, &keys).expect("sort expression");
    let ids: Vec<i64> = res
        .iter()
        .map(|r| match r.values[0] {
            Value::Int64(id) => id,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ids, vec![10, 20, 30]);
}

#[test]
fn test_differential_oracle_large_dataset() {
    let schema = create_test_schema();
    const ROW_COUNT: usize = 2000;

    // Deterministic pseudo-random generation using LCG
    let mut state: u64 = 0x123456789ABCDEF;
    let mut next_u64 = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };

    let mut rows = Vec::with_capacity(ROW_COUNT);
    let mut oracle_data = Vec::with_capacity(ROW_COUNT);

    for i in 0..ROW_COUNT {
        let id = (next_u64() % 100000) as i64;
        let cat = format!("cat_{}", next_u64() % 50);
        let val = (next_u64() % 1000) as i64;
        rows.push(Row::new(vec![
            Value::Int64(id),
            Value::String(cat.clone()),
            Value::Int64(val),
            Value::Float64(0.0),
            Value::Null,
        ]));
        oracle_data.push((cat, val, i as u64, id));
    }

    // Sort using TreeDB SortOperator (ORDER BY category ASC, val_num DESC)
    let keys = vec![
        SortKey {
            expression: Expr::Column("category".to_string()),
            direction: SortDirection::Ascending,
            nulls_first: None,
        },
        SortKey {
            expression: Expr::Column("val_num".to_string()),
            direction: SortDirection::Descending,
            nulls_first: None,
        },
    ];

    let sorted_rows = SortOperator::apply(rows, &schema, &keys).expect("sort 2000 rows");

    // Independent reference sort in Rust
    oracle_data.sort_by(|a, b| match a.0.cmp(&b.0) {
        Ordering::Equal => match b.1.cmp(&a.1) {
            Ordering::Equal => a.2.cmp(&b.2),
            other => other,
        },
        other => other,
    });

    assert_eq!(sorted_rows.len(), ROW_COUNT);
    for (i, row) in sorted_rows.iter().enumerate() {
        let cat = match &row.values[1] {
            Value::String(s) => s.as_str(),
            _ => unreachable!(),
        };
        let val = match row.values[2] {
            Value::Int64(v) => v,
            _ => unreachable!(),
        };
        let id = match row.values[0] {
            Value::Int64(v) => v,
            _ => unreachable!(),
        };

        assert_eq!(cat, oracle_data[i].0);
        assert_eq!(val, oracle_data[i].1);
        assert_eq!(id, oracle_data[i].3);
    }
}

#[test]
fn test_sort_elimination_planner_properties() -> treedb::error::Result<()> {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let db_path = temp_dir.path().join("sort_elim.treedb");
    let mut db = treedb::TreeDb::create(&db_path, 64).unwrap();
    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, balance BIGINT);",
    )?;

    // 1. ORDER BY id ASC on primary key scan -> sort eliminated
    let plan1 = txn.plan_sql("SELECT * FROM users ORDER BY id ASC;")?;
    assert!(
        !plan1.order_by_sort_required,
        "indexed PK scan ASC must eliminate sort"
    );

    // 2. ORDER BY balance ASC on unindexed column -> sort required
    let plan2 = txn.plan_sql("SELECT * FROM users ORDER BY balance ASC;")?;
    assert!(
        plan2.order_by_sort_required,
        "unindexed column MUST require sort"
    );

    // 3. Explain text also verifies omission of Sort
    let explain1 = txn.explain_sql("SELECT * FROM users ORDER BY id ASC;")?;
    assert!(
        !explain1.contains("Sort ["),
        "explain should not contain Sort when eliminated"
    );

    let explain2 = txn.explain_sql("SELECT * FROM users ORDER BY balance ASC;")?;
    assert!(
        explain2.contains("Sort ["),
        "explain should contain Sort for unindexed column"
    );

    Ok(())
}
