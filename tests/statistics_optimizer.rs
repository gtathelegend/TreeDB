use std::collections::HashMap;
use std::path::PathBuf;
use treedb::engine::transaction::StatementResult;
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::query::cardinality::CardinalityEstimator;
use treedb::query::expr::{BinaryOp, Expr};
use treedb::query::stats::{ColumnStatistics, TableStatistics};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_stats_{}_{}_{}.treedb",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&wal_path);
        Self { db_path, wal_path }
    }

    fn db(&self) -> TreeDb {
        if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.wal_path);
    }
}

// ---------------------------------------------------------------------------
// 1. Statistics Collection Tests
// ---------------------------------------------------------------------------

#[test]
fn test_statistics_empty_table() {
    let ctx = TestContext::new("stats_empty");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT, price INT);")
        .unwrap();

    let stats = txn.collect_statistics("items").unwrap();
    assert_eq!(stats.table_name, "items");
    assert_eq!(stats.row_count, 0);

    let id_stat = stats.column("id").unwrap();
    assert_eq!(id_stat.row_count, 0);
    assert_eq!(id_stat.null_count, 0);
    assert_eq!(id_stat.distinct_count, 0);
    assert_eq!(id_stat.min, None);
    assert_eq!(id_stat.max, None);

    let price_stat = stats.column("price").unwrap();
    assert_eq!(price_stat.row_count, 0);
    assert_eq!(price_stat.null_count, 0);
    assert_eq!(price_stat.distinct_count, 0);
}

#[test]
fn test_statistics_single_row_table() {
    let ctx = TestContext::new("stats_single");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, name TEXT, price INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, 'apple', 10);")
        .unwrap();

    let stats = txn.collect_statistics("items").unwrap();
    assert_eq!(stats.row_count, 1);

    let id_stat = stats.column("id").unwrap();
    assert_eq!(id_stat.row_count, 1);
    assert_eq!(id_stat.null_count, 0);
    assert_eq!(id_stat.distinct_count, 1);
    assert_eq!(id_stat.min, Some(Value::Int64(1)));
    assert_eq!(id_stat.max, Some(Value::Int64(1)));

    let name_stat = stats.column("name").unwrap();
    assert_eq!(name_stat.row_count, 1);
    assert_eq!(name_stat.null_count, 0);
    assert_eq!(name_stat.distinct_count, 1);
    assert_eq!(name_stat.min, Some(Value::String("apple".into())));
    assert_eq!(name_stat.max, Some(Value::String("apple".into())));
}

#[test]
fn test_statistics_multiple_rows_nulls_duplicates_and_all_nulls() {
    let ctx = TestContext::new("stats_multi");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, tag TEXT, score INT, note TEXT);",
    )
    .unwrap();
    txn.execute_statement("CREATE INDEX idx_tag ON users (tag);")
        .unwrap();

    // Insert rows:
    // id: 1..5
    // tag: 'a', 'b', 'a', NULL, 'b' (distinct: 'a', 'b', null_count: 1)
    // score: 10, 20, 10, 20, 30 (distinct: 10, 20, 30, min: 10, max: 30)
    // note: all NULL
    txn.execute_statement("INSERT INTO users VALUES (1, 'a', 10, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (2, 'b', 20, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (3, 'a', 10, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (4, NULL, 20, NULL);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (5, 'b', 30, NULL);")
        .unwrap();

    let stats = txn.collect_statistics("users").unwrap();
    assert_eq!(stats.row_count, 5);

    let tag_stat = stats.column("tag").unwrap();
    assert_eq!(tag_stat.null_count, 1);
    assert_eq!(tag_stat.distinct_count, 2);
    assert_eq!(tag_stat.min, Some(Value::String("a".into())));
    assert_eq!(tag_stat.max, Some(Value::String("b".into())));

    let score_stat = stats.column("score").unwrap();
    assert_eq!(score_stat.null_count, 0);
    assert_eq!(score_stat.distinct_count, 3);
    assert_eq!(score_stat.min, Some(Value::Int64(10)));
    assert_eq!(score_stat.max, Some(Value::Int64(30)));

    let note_stat = stats.column("note").unwrap();
    assert_eq!(note_stat.null_count, 5);
    assert_eq!(note_stat.distinct_count, 0);
    assert_eq!(note_stat.min, None);
    assert_eq!(note_stat.max, None);

    let idx_stat = stats.index("idx_tag").unwrap();
    assert_eq!(idx_stat.indexed_row_count, 4); // 4 non-null rows indexed
    assert_eq!(idx_stat.distinct_key_count, 2);
    assert_eq!(idx_stat.null_count, 1);
}

#[test]
fn test_sql_analyze_statement() {
    let ctx = TestContext::new("sql_analyze");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, v INT);")
        .unwrap();
    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {v});", v = i % 5))
            .unwrap();
    }

    // Execute SQL ANALYZE
    let res = txn.execute_statement("ANALYZE t1;").unwrap();
    match res {
        StatementResult::AnalyzedTable { table } => {
            assert_eq!(table, "t1");
        }
        _ => panic!("Expected AnalyzedTable statement result"),
    }

    // Verify statistics registry is populated
    let cached = txn.get_statistics("t1").unwrap();
    assert_eq!(cached.row_count, 20);
    let v_stat = cached.column("v").unwrap();
    assert_eq!(v_stat.distinct_count, 5);
    assert_eq!(v_stat.min, Some(Value::Int64(0)));
    assert_eq!(v_stat.max, Some(Value::Int64(4)));
}

// ---------------------------------------------------------------------------
// 2. Cardinality Estimation Unit Tests
// ---------------------------------------------------------------------------

#[test]
fn test_cardinality_estimation_formulas() {
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("age", DataType::Int64, true),
            Column::new("category", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    let mut col_stats = HashMap::new();
    col_stats.insert(
        "id".into(),
        ColumnStatistics {
            row_count: 1000,
            null_count: 0,
            distinct_count: 1000,
            min: Some(Value::Int64(1)),
            max: Some(Value::Int64(1000)),
        },
    );
    col_stats.insert(
        "age".into(),
        ColumnStatistics {
            row_count: 1000,
            null_count: 50,
            distinct_count: 50,
            min: Some(Value::Int64(1)),
            max: Some(Value::Int64(100)),
        },
    );
    col_stats.insert(
        "category".into(),
        ColumnStatistics {
            row_count: 1000,
            null_count: 100,
            distinct_count: 10,
            min: Some(Value::String("a".into())),
            max: Some(Value::String("j".into())),
        },
    );

    let table_stats = TableStatistics {
        table_name: "people".into(),
        row_count: 1000,
        columns: col_stats,
        indexes: HashMap::new(),
        column_groups: HashMap::new(),
    };

    // 1. Full scan cardinality
    let full_rows =
        CardinalityEstimator::estimate_scan_cardinality(Some(&table_stats), None, &schema);
    assert_eq!(full_rows, 1000.0);

    // 2. Equality predicate: category = 'c' (NDV = 10 -> sel = 0.1 -> 100 rows)
    let eq_expr = Expr::Binary {
        left: Box::new(Expr::Column("category".into())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(Value::String("c".into()))),
    };
    let eq_rows = CardinalityEstimator::estimate_scan_cardinality(
        Some(&table_stats),
        Some(&eq_expr),
        &schema,
    );
    assert!((eq_rows - 100.0).abs() < 1e-3);

    // 3. IS NULL predicate: age IS NULL (50 / 1000 = 0.05 -> 50 rows)
    let is_null_expr = Expr::IsNull(Box::new(Expr::Column("age".into())));
    let null_rows = CardinalityEstimator::estimate_scan_cardinality(
        Some(&table_stats),
        Some(&is_null_expr),
        &schema,
    );
    assert!((null_rows - 50.0).abs() < 1e-3);

    // 4. IS NOT NULL predicate: age IS NOT NULL (1 - 0.05 = 0.95 -> 950 rows)
    let is_not_null_expr = Expr::IsNotNull(Box::new(Expr::Column("age".into())));
    let not_null_rows = CardinalityEstimator::estimate_scan_cardinality(
        Some(&table_stats),
        Some(&is_not_null_expr),
        &schema,
    );
    assert!((not_null_rows - 950.0).abs() < 1e-3);

    // 5. Range predicate: age <= 50 (range 1..100 -> (50 - 1) / (100 - 1) approx 0.495 -> 495 rows)
    let range_expr = Expr::Binary {
        left: Box::new(Expr::Column("age".into())),
        op: BinaryOp::LtEq,
        right: Box::new(Expr::Literal(Value::Int64(50))),
    };
    let range_rows = CardinalityEstimator::estimate_scan_cardinality(
        Some(&table_stats),
        Some(&range_expr),
        &schema,
    );
    assert!(range_rows > 400.0 && range_rows < 600.0);

    // 6. AND conjunction: category = 'c' AND age IS NOT NULL
    let and_expr = Expr::Binary {
        left: Box::new(eq_expr.clone()),
        op: BinaryOp::And,
        right: Box::new(is_not_null_expr.clone()),
    };
    let and_rows = CardinalityEstimator::estimate_scan_cardinality(
        Some(&table_stats),
        Some(&and_expr),
        &schema,
    );
    // 1000 * 0.1 * 0.95 = 95 rows
    assert!((and_rows - 95.0).abs() < 1e-3);

    // 7. OR disjunction: category = 'c' OR age IS NULL
    let or_expr = Expr::Binary {
        left: Box::new(eq_expr),
        op: BinaryOp::Or,
        right: Box::new(is_null_expr),
    };
    let or_rows = CardinalityEstimator::estimate_scan_cardinality(
        Some(&table_stats),
        Some(&or_expr),
        &schema,
    );
    // 0.1 + 0.05 - (0.1 * 0.05) = 0.145 -> 145 rows
    assert!((or_rows - 145.0).abs() < 1e-3);
}

// ---------------------------------------------------------------------------
// 3. Cost-Based Access Path Selection Tests
// ---------------------------------------------------------------------------

#[test]
fn test_access_path_cost_selection() {
    let ctx = TestContext::new("cost_access");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT, role TEXT);")
        .unwrap();
    txn.execute_statement("CREATE UNIQUE INDEX idx_email ON users (email);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_role ON users (role);")
        .unwrap();

    // Populate table with 300 rows
    for i in 1..=300 {
        let role = if i % 10 == 0 { "admin" } else { "user" };
        txn.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'user{i}@corp.com', '{role}');"
        ))
        .unwrap();
    }

    txn.execute_statement("ANALYZE users;").unwrap();

    // 1. Primary key lookup has lowest cost for id = 42
    let plan_pk = txn
        .explain_sql("SELECT * FROM users WHERE id = 42;")
        .unwrap();
    assert!(plan_pk.contains("PrimaryPointLookup(key=42)"));
    assert!(plan_pk.contains("estimated_rows: 1"));

    // 2. Unique secondary index point lookup for email = 'user50@corp.com'
    let plan_email = txn
        .explain_sql("SELECT * FROM users WHERE email = 'user50@corp.com';")
        .unwrap();
    assert!(plan_email.contains("SecondaryIndexScan(idx_email, PointLookup(\"user50@corp.com\"))"));
    assert!(plan_email.contains("estimated_rows: 1"));

    // 3. Non-unique secondary index with low distinct count (NDV=2, 150 matches out of 300)
    // Cost model correctly chooses FullTableScan over secondary index because fetching 150 tuples via index is more expensive!
    let plan_role = txn
        .explain_sql("SELECT * FROM users WHERE role = 'admin';")
        .unwrap();
    assert!(plan_role.contains("access: FullTableScan"));
}

// ---------------------------------------------------------------------------
// 4. Bounded Inner Join Ordering Tests
// ---------------------------------------------------------------------------

#[test]
fn test_cost_based_inner_join_ordering() {
    let ctx = TestContext::new("join_ordering");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    // Create small table (countries: 5 rows) and large table (cities: 500 rows)
    txn.execute_statement("CREATE TABLE countries (c_id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement(
        "CREATE TABLE cities (city_id BIGINT PRIMARY KEY, c_id BIGINT, name TEXT);",
    )
    .unwrap();

    for i in 1..=5 {
        txn.execute_statement(&format!(
            "INSERT INTO countries VALUES ({i}, 'Country_{i}');"
        ))
        .unwrap();
    }
    for i in 1..=500 {
        let c_id = (i % 5) + 1;
        txn.execute_statement(&format!(
            "INSERT INTO cities VALUES ({i}, {c_id}, 'City_{i}');"
        ))
        .unwrap();
    }

    txn.execute_statement("ANALYZE countries;").unwrap();
    txn.execute_statement("ANALYZE cities;").unwrap();

    // Query written with large table first: SELECT * FROM cities JOIN countries ON cities.c_id = countries.c_id
    // Cost-based optimizer should order smaller relation (countries, 5 rows) as outer loop base table!
    let plan = txn
        .explain_sql("SELECT * FROM cities c JOIN countries k ON c.c_id = k.c_id;")
        .unwrap();

    // Base table scan should be countries (outer loop) because 5 * 500 is much cheaper nested loop
    assert!(plan.contains("TableScan (table: countries AS k"));
    assert!(plan.contains("TableScan (table: cities AS c"));
}

#[test]
fn test_outer_joins_never_reordered() {
    let ctx = TestContext::new("outer_join_order");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO a VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("INSERT INTO b VALUES (1, 100);")
        .unwrap();

    txn.execute_statement("ANALYZE a;").unwrap();
    txn.execute_statement("ANALYZE b;").unwrap();

    // LEFT JOIN must preserve 'a' as left relation despite 'b' being smaller
    let plan = txn
        .explain_sql("SELECT * FROM a LEFT JOIN b ON a.id = b.id;")
        .unwrap();
    assert!(plan.contains("Left: TableScan (table: a"));
    assert!(plan.contains("Right: TableScan (table: b"));
}

// ---------------------------------------------------------------------------
// 5. Missing Statistics & Stale Fallback Tests
// ---------------------------------------------------------------------------

#[test]
fn test_missing_statistics_safe_fallback() {
    let ctx = TestContext::new("missing_stats");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE unanalyzed (id BIGINT PRIMARY KEY, val TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO unanalyzed VALUES (1, 'hello');")
        .unwrap();

    // Explain query on table with no statistics
    let plan = txn
        .explain_sql("SELECT * FROM unanalyzed WHERE id = 1;")
        .unwrap();
    assert!(plan.contains("PrimaryPointLookup(key=1)"));
    assert!(plan.contains("estimated_rows: 1"));
    assert!(plan.contains("cost:"));

    // Result correctness without statistics
    let res = txn
        .execute_sql("SELECT * FROM unanalyzed WHERE id = 1;")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[1], Value::String("hello".into()));
}

// ---------------------------------------------------------------------------
// 6. Execution Equivalence & Result Correctness Tests
// ---------------------------------------------------------------------------

#[test]
fn test_execution_equivalence_analyzed_vs_unanalyzed() {
    let ctx = TestContext::new("exec_equiv");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, a INT, b TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id BIGINT PRIMARY KEY, t1_id BIGINT, c INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!(
            "INSERT INTO t1 VALUES ({i}, {}, 'str_{}');",
            i * 2,
            i % 7
        ))
        .unwrap();
    }
    for j in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO t2 VALUES ({j}, {}, {});",
            (j % 50) + 1,
            j * 3
        ))
        .unwrap();
    }

    let query = "SELECT t1.b, SUM(t2.c), COUNT(*) FROM t1 JOIN t2 ON t1.id = t2.t1_id WHERE t1.a > 20 GROUP BY t1.b ORDER BY t1.b;";

    // Run query before ANALYZE
    let res_before = txn.execute_sql(query).unwrap();

    // ANALYZE both tables
    txn.execute_statement("ANALYZE t1;").unwrap();
    txn.execute_statement("ANALYZE t2;").unwrap();

    // Run query after ANALYZE
    let res_after = txn.execute_sql(query).unwrap();

    assert_eq!(res_before.schema.columns, res_after.schema.columns);
    assert_eq!(res_before.rows.len(), res_after.rows.len());
    for (r1, r2) in res_before.rows.iter().zip(&res_after.rows) {
        assert_eq!(r1, r2);
    }
}

// ---------------------------------------------------------------------------
// 7. Randomized Differential Testing
// ---------------------------------------------------------------------------

#[test]
fn test_randomized_statistics_and_query_oracle() {
    let ctx = TestContext::new("random_oracle");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE rand_t (id BIGINT PRIMARY KEY, val INT, note TEXT);")
        .unwrap();

    // Pseudo-random data generator with skewed values and NULLs
    let mut rng_state: u64 = 123456789;
    let mut next_rand = || -> u64 {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };

    for i in 1..=80 {
        let r = next_rand();
        let val_str = if r % 5 == 0 {
            "NULL".to_string()
        } else {
            ((r % 20) as i64).to_string()
        };
        let note_str = if r % 7 == 0 {
            "NULL".to_string()
        } else {
            format!("'note_{}'", r % 4)
        };
        txn.execute_statement(&format!(
            "INSERT INTO rand_t VALUES ({i}, {val_str}, {note_str});"
        ))
        .unwrap();
    }

    let test_queries = [
        "SELECT * FROM rand_t WHERE id = 42;",
        "SELECT * FROM rand_t WHERE val = 5;",
        "SELECT * FROM rand_t WHERE val IS NULL;",
        "SELECT * FROM rand_t WHERE val IS NOT NULL;",
        "SELECT note, COUNT(*), AVG(val) FROM rand_t GROUP BY note ORDER BY note;",
        "SELECT * FROM rand_t WHERE val >= 5 AND val <= 15 ORDER BY id;",
        "SELECT * FROM rand_t WHERE val = 3 OR note = 'note_1' ORDER BY id;",
    ];

    for sql in test_queries {
        let res_unanalyzed = txn.execute_sql(sql).unwrap();

        txn.execute_statement("ANALYZE rand_t;").unwrap();
        let res_analyzed = txn.execute_sql(sql).unwrap();

        assert_eq!(
            res_unanalyzed.rows, res_analyzed.rows,
            "Query result mismatch for: {sql}"
        );
    }
}
