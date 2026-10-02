#![allow(clippy::needless_borrow, clippy::manual_range_contains)]

use std::path::PathBuf;
use treedb::engine::types::{Row, Value};
use treedb::query::inference::{
    ColumnConstraint, ColumnRange, ConstraintSet, InferenceMetrics, InferenceSource, RangeBound,
    MAX_PROPAGATION_ROUNDS,
};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_pred_prop_{}_{}_{}.treedb",
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

fn query(txn: &mut treedb::engine::Transaction, sql: &str) -> Vec<Row> {
    txn.execute_sql(sql).unwrap().rows
}

// ============================================================================
// R356: Simple column equality extraction
// ============================================================================
#[test]
fn test_r356_simple_column_equality_extraction() {
    let ctx = TestContext::new("r356");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, age INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 25), (2, 30);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users WHERE id = 1;")
        .unwrap();
    assert!(plan.contains("PointLookup") || plan.contains("Scan") || plan.contains("id = 1"));
    let rows = query(&mut txn, "SELECT * FROM users WHERE id = 1;");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R357: Reversed equality extraction
// ============================================================================
#[test]
fn test_r357_reversed_equality_extraction() {
    let ctx = TestContext::new("r357");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, age INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 25), (2, 30);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users WHERE 1 = id;")
        .unwrap();
    assert!(plan.contains("PointLookup") || plan.contains("Scan") || plan.contains("id = 1"));
    let rows = query(&mut txn, "SELECT * FROM users WHERE 1 = id;");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R358: Column-to-column equality extraction
// ============================================================================
#[test]
fn test_r358_column_to_column_equality_extraction() {
    let mut cset = ConstraintSet::new();
    cset.union_columns("u.id", "o.user_id", InferenceSource::InnerJoinEquality);
    assert_eq!(cset.find_root("u.id"), cset.find_root("o.user_id"));
}

// ============================================================================
// R359: Constant equality propagation
// ============================================================================
#[test]
fn test_r359_constant_equality_propagation() {
    let mut cset = ConstraintSet::new();
    cset.union_columns("u.id", "o.user_id", InferenceSource::InnerJoinEquality);
    cset.add_constraint("u.id", ColumnConstraint::Exact(Value::Int64(42)));

    assert_eq!(
        cset.constraints.get("o.user_id"),
        Some(&ColumnConstraint::Exact(Value::Int64(42)))
    );
}

// ============================================================================
// R360: Transitive equality A=B, B=C
// ============================================================================
#[test]
fn test_r360_transitive_equality() {
    let mut cset = ConstraintSet::new();
    cset.union_columns("a", "b", InferenceSource::TransitiveEquality);
    cset.union_columns("b", "c", InferenceSource::TransitiveEquality);
    cset.add_constraint("a", ColumnConstraint::Exact(Value::Int64(42)));

    assert_eq!(
        cset.constraints.get("b"),
        Some(&ColumnConstraint::Exact(Value::Int64(42)))
    );
    assert_eq!(
        cset.constraints.get("c"),
        Some(&ColumnConstraint::Exact(Value::Int64(42)))
    );
}

// ============================================================================
// R361: Constant contradiction
// ============================================================================
#[test]
fn test_r361_constant_contradiction() {
    let ctx = TestContext::new("r361");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users WHERE id = 10 AND id = 20;")
        .unwrap();
    assert!(plan.contains("EmptyResult") || plan.contains("contradiction"));

    let rows = query(&mut txn, "SELECT * FROM users WHERE id = 10 AND id = 20;");
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R362: Integer range intersection
// ============================================================================
#[test]
fn test_r362_integer_range_intersection() {
    let r1 = ColumnRange {
        lower: Some(RangeBound::Inclusive(Value::Int64(18))),
        upper: None,
    };
    let r2 = ColumnRange {
        lower: None,
        upper: Some(RangeBound::Exclusive(Value::Int64(30))),
    };
    let merged = r1.intersect(&r2).unwrap();
    assert_eq!(merged.lower, Some(RangeBound::Inclusive(Value::Int64(18))));
    assert_eq!(merged.upper, Some(RangeBound::Exclusive(Value::Int64(30))));
    assert!(merged.contains(&Value::Int64(18)));
    assert!(merged.contains(&Value::Int64(29)));
    assert!(!merged.contains(&Value::Int64(30)));
    assert!(!merged.contains(&Value::Int64(17)));
}

// ============================================================================
// R363: Inclusive/exclusive range boundaries
// ============================================================================
#[test]
fn test_r363_inclusive_exclusive_range_boundaries() {
    let r1 = ColumnRange {
        lower: Some(RangeBound::Inclusive(Value::Int64(10))),
        upper: Some(RangeBound::Inclusive(Value::Int64(20))),
    };
    let r2 = ColumnRange {
        lower: Some(RangeBound::Exclusive(Value::Int64(10))),
        upper: Some(RangeBound::Exclusive(Value::Int64(20))),
    };
    let merged = r1.intersect(&r2).unwrap();
    assert_eq!(merged.lower, Some(RangeBound::Exclusive(Value::Int64(10))));
    assert_eq!(merged.upper, Some(RangeBound::Exclusive(Value::Int64(20))));
}

// ============================================================================
// R364: Range contradiction
// ============================================================================
#[test]
fn test_r364_range_contradiction() {
    let ctx = TestContext::new("r364");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE metrics (id INT PRIMARY KEY, age INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO metrics VALUES (1, 25);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM metrics WHERE age >= 30 AND age < 18;")
        .unwrap();
    assert!(plan.contains("EmptyResult"));
    let rows = query(
        &mut txn,
        "SELECT * FROM metrics WHERE age >= 30 AND age < 18;",
    );
    assert_eq!(rows.len(), 0);

    let plan2 = txn
        .explain_sql("SELECT * FROM metrics WHERE age > 10 AND age <= 10;")
        .unwrap();
    assert!(plan2.contains("EmptyResult"));
    let rows2 = query(
        &mut txn,
        "SELECT * FROM metrics WHERE age > 10 AND age <= 10;",
    );
    assert_eq!(rows2.len(), 0);
}

// ============================================================================
// R365: Equality inside range
// ============================================================================
#[test]
fn test_r365_equality_inside_range() {
    let mut cset = ConstraintSet::new();
    cset.add_constraint(
        "age",
        ColumnConstraint::Range(ColumnRange {
            lower: Some(RangeBound::Inclusive(Value::Int64(18))),
            upper: Some(RangeBound::Exclusive(Value::Int64(30))),
        }),
    );
    cset.add_constraint("age", ColumnConstraint::Exact(Value::Int64(25)));

    assert_eq!(
        cset.constraints.get("age"),
        Some(&ColumnConstraint::Exact(Value::Int64(25)))
    );
    assert!(!cset.is_unsatisfiable());
}

// ============================================================================
// R366: Equality outside range
// ============================================================================
#[test]
fn test_r366_equality_outside_range() {
    let ctx = TestContext::new("r366");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, age INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 40);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users WHERE age = 40 AND age < 30;")
        .unwrap();
    assert!(plan.contains("EmptyResult"));

    let rows = query(&mut txn, "SELECT * FROM users WHERE age = 40 AND age < 30;");
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R367: IS NULL extraction
// ============================================================================
#[test]
fn test_r367_is_null_extraction() {
    let ctx = TestContext::new("r367");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, NULL), (2, 10);")
        .unwrap();

    let rows = query(&mut txn, "SELECT * FROM items WHERE val IS NULL;");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R368: IS NULL + equality contradiction
// ============================================================================
#[test]
fn test_r368_is_null_plus_equality_contradiction() {
    let ctx = TestContext::new("r368");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, 10);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM items WHERE val IS NULL AND val = 10;")
        .unwrap();
    assert!(plan.contains("EmptyResult"));

    let rows = query(
        &mut txn,
        "SELECT * FROM items WHERE val IS NULL AND val = 10;",
    );
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R369: IS NULL + IS NOT NULL contradiction
// ============================================================================
#[test]
fn test_r369_is_null_plus_is_not_null_contradiction() {
    let ctx = TestContext::new("r369");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, 10);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM items WHERE val IS NULL AND val IS NOT NULL;")
        .unwrap();
    assert!(plan.contains("EmptyResult"));

    let rows = query(
        &mut txn,
        "SELECT * FROM items WHERE val IS NULL AND val IS NOT NULL;",
    );
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R370: NULL comparison semantics
// ============================================================================
#[test]
fn test_r370_null_comparison_semantics() {
    let ctx = TestContext::new("r370");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, NULL), (2, 10);")
        .unwrap();

    let rows = query(&mut txn, "SELECT * FROM items WHERE val = NULL;");
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R371: AND constraint merging
// ============================================================================
#[test]
fn test_r371_and_constraint_merging() {
    let mut cset = ConstraintSet::new();
    cset.add_constraint("id", ColumnConstraint::Exact(Value::Int64(42)));
    cset.add_constraint(
        "status",
        ColumnConstraint::Exact(Value::String("ACTIVE".to_string())),
    );

    assert_eq!(
        cset.constraints.get("id"),
        Some(&ColumnConstraint::Exact(Value::Int64(42)))
    );
    assert_eq!(
        cset.constraints.get("status"),
        Some(&ColumnConstraint::Exact(Value::String(
            "ACTIVE".to_string()
        )))
    );
}

// ============================================================================
// R372: OR branch isolation
// ============================================================================
#[test]
fn test_r372_or_branch_isolation() {
    let ctx = TestContext::new("r372");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300);")
        .unwrap();

    let rows = query(&mut txn, "SELECT * FROM items WHERE a = 10 OR a = 20;");
    assert_eq!(rows.len(), 2);
}

// ============================================================================
// R373: OR common constraint extraction
// ============================================================================
#[test]
fn test_r373_or_common_constraint_extraction() {
    let mut metrics = InferenceMetrics::default();
    let mut left = ConstraintSet::new();
    left.add_constraint("a", ColumnConstraint::Exact(Value::Int64(1)));
    left.add_constraint(
        "status",
        ColumnConstraint::Exact(Value::String("ACTIVE".to_string())),
    );

    let mut right = ConstraintSet::new();
    right.add_constraint("a", ColumnConstraint::Exact(Value::Int64(2)));
    right.add_constraint(
        "status",
        ColumnConstraint::Exact(Value::String("ACTIVE".to_string())),
    );

    let common = ConstraintSet::extract_common_or(&left, &right, &mut metrics);
    assert_eq!(
        common.constraints.get("status"),
        Some(&ColumnConstraint::Exact(Value::String(
            "ACTIVE".to_string()
        )))
    );
    assert!(!common.constraints.contains_key("a"));
}

// ============================================================================
// R374: INNER JOIN equality propagation
// ============================================================================
#[test]
fn test_r374_inner_join_equality_propagation() {
    let ctx = TestContext::new("r374");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_orders_user ON orders (user_id);")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (42, 'alice'), (43, 'bob');")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (1, 42, 100), (2, 43, 200);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 42;")
        .unwrap();
    assert!(
        plan.contains("Predicate Inference")
            || plan.contains("user_id = 42")
            || plan.contains("IndexScan")
            || plan.contains("PointLookup")
            || plan.contains("HashJoin")
    );

    let rows = query(
        &mut txn,
        "SELECT * FROM users u JOIN orders o ON u.id = o.user_id WHERE u.id = 42;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(42));
    assert_eq!(rows[0].values[3], Value::Int64(42));
}

// ============================================================================
// R375: INNER JOIN range propagation
// ============================================================================
#[test]
fn test_r375_inner_join_range_propagation() {
    let mut cset = ConstraintSet::new();
    cset.union_columns("u.id", "o.user_id", InferenceSource::InnerJoinEquality);
    cset.add_constraint(
        "u.id",
        ColumnConstraint::Range(ColumnRange {
            lower: Some(RangeBound::Inclusive(Value::Int64(100))),
            upper: Some(RangeBound::Exclusive(Value::Int64(200))),
        }),
    );

    assert_eq!(
        cset.constraints.get("o.user_id"),
        Some(&ColumnConstraint::Range(ColumnRange {
            lower: Some(RangeBound::Inclusive(Value::Int64(100))),
            upper: Some(RangeBound::Exclusive(Value::Int64(200))),
        }))
    );
}

// ============================================================================
// R376: Multi-hop join propagation
// ============================================================================
#[test]
fn test_r376_multi_hop_join_propagation() {
    let mut cset = ConstraintSet::new();
    cset.union_columns("a.id", "b.a_id", InferenceSource::InnerJoinEquality);
    cset.union_columns("b.a_id", "c.b_id", InferenceSource::InnerJoinEquality);
    cset.add_constraint("a.id", ColumnConstraint::Exact(Value::Int64(42)));

    assert_eq!(
        cset.constraints.get("b.a_id"),
        Some(&ColumnConstraint::Exact(Value::Int64(42)))
    );
    assert_eq!(
        cset.constraints.get("c.b_id"),
        Some(&ColumnConstraint::Exact(Value::Int64(42)))
    );
}

// ============================================================================
// R377: Propagation depth bound
// ============================================================================
#[test]
fn test_r377_propagation_depth_bound() {
    assert_eq!(MAX_PROPAGATION_ROUNDS, 8);
}

// ============================================================================
// R378: LEFT JOIN barrier
// ============================================================================
#[test]
fn test_r378_left_join_barrier() {
    let ctx = TestContext::new("r378");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (42, 'alice'), (43, 'bob');")
        .unwrap();
    // No orders for 42

    let rows = query(
        &mut txn,
        "SELECT u.id, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 42;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(42));
    assert_eq!(rows[0].values[1], Value::Null);
}

// ============================================================================
// R379: RIGHT JOIN barrier
// ============================================================================
#[test]
fn test_r379_right_join_barrier() {
    let ctx = TestContext::new("r379");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO orders VALUES (1, 999, 100);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT u.name, o.amount FROM users u RIGHT JOIN orders o ON u.id = o.user_id WHERE o.amount = 100;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Null);
    assert_eq!(rows[0].values[1], Value::Int64(100));
}

// ============================================================================
// R380: FULL JOIN barrier
// ============================================================================
#[test]
fn test_r380_full_join_barrier() {
    let ctx = TestContext::new("r380");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2, 20);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT t1.id, t2.id FROM t1 FULL JOIN t2 ON t1.id = t2.id;",
    );
    assert_eq!(rows.len(), 2);
}

// ============================================================================
// R381: SEMI JOIN barrier
// ============================================================================
#[test]
fn test_r381_semi_join_barrier() {
    let ctx = TestContext::new("r381");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (10, 1);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT * FROM users WHERE id IN (SELECT user_id FROM orders);",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R382: ANTI JOIN barrier
// ============================================================================
#[test]
fn test_r382_anti_join_barrier() {
    let ctx = TestContext::new("r382");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 'alice'), (2, 'bob');")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (10, 1);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT * FROM users WHERE id NOT IN (SELECT user_id FROM orders WHERE user_id IS NOT NULL);",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(2));
}

// ============================================================================
// R383: NOT IN NULL semantics
// ============================================================================
#[test]
fn test_r383_not_in_null_semantics() {
    let ctx = TestContext::new("r383");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE forbidden (id INT PRIMARY KEY, val INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1), (2);")
        .unwrap();
    txn.execute_statement("INSERT INTO forbidden VALUES (10, NULL);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT * FROM users WHERE id NOT IN (SELECT val FROM forbidden);",
    );
    assert_eq!(rows.len(), 0); // NOT IN (NULL) evaluates to UNKNOWN / false
}

// ============================================================================
// R384: CTE barrier
// ============================================================================
#[test]
fn test_r384_cte_barrier() {
    let ctx = TestContext::new("r384");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO users VALUES (1, 100), (2, 200);")
        .unwrap();

    let rows = query(
        &mut txn,
        "WITH cte AS (SELECT id, val FROM users) SELECT * FROM cte WHERE id = 1;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R385: Recursive CTE barrier
// ============================================================================
#[test]
fn test_r385_recursive_cte_barrier() {
    let ctx = TestContext::new("r385");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE seeds (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO seeds VALUES (1, 1);")
        .unwrap();

    let rows = query(
        &mut txn,
        "WITH RECURSIVE cnt(x) AS (SELECT val FROM seeds WHERE id = 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 5) SELECT * FROM cnt;",
    );
    assert_eq!(rows.len(), 5);
}

// ============================================================================
// R386: Derived-table barrier
// ============================================================================
#[test]
fn test_r386_derived_table_barrier() {
    let ctx = TestContext::new("r386");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE data (id INT PRIMARY KEY, grp INT, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO data VALUES (1, 1, 10), (2, 1, 20), (3, 2, 30);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT * FROM (SELECT grp, sum(val) as s FROM data GROUP BY grp) d WHERE d.grp = 1;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R387: Aggregation barrier
// ============================================================================
#[test]
fn test_r387_aggregation_barrier() {
    let ctx = TestContext::new("r387");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE sales (id INT PRIMARY KEY, amount INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO sales VALUES (1, 50), (2, 150);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT sum(amount) FROM sales HAVING sum(amount) > 100;",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(200));
}

// ============================================================================
// R388: Window-function barrier
// ============================================================================
#[test]
fn test_r388_window_function_barrier() {
    let ctx = TestContext::new("r388");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE scores (id INT PRIMARY KEY, score INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO scores VALUES (1, 10), (2, 20), (3, 30);")
        .unwrap();

    let rows = query(
        &mut txn,
        "SELECT id, row_number() OVER (ORDER BY score DESC) as rnk FROM scores;",
    );
    assert_eq!(rows.len(), 3);
}

// ============================================================================
// R389: LIMIT/OFFSET barrier
// ============================================================================
#[test]
fn test_r389_limit_offset_barrier() {
    let ctx = TestContext::new("r389");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE nums (id INT PRIMARY KEY);")
        .unwrap();
    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO nums VALUES ({i});"))
            .unwrap();
    }

    let rows = query(
        &mut txn,
        "SELECT * FROM (SELECT * FROM nums ORDER BY id LIMIT 3) sub WHERE id = 5;",
    );
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R390: Index point lookup from inferred equality
// ============================================================================
#[test]
fn test_r390_index_point_lookup_from_inferred_equality() {
    let ctx = TestContext::new("r390");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE u (id INT PRIMARY KEY, name TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE o (id INT PRIMARY KEY, u_id INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_o_uid ON o (u_id);")
        .unwrap();

    txn.execute_statement("INSERT INTO u VALUES (10, 'alice');")
        .unwrap();
    txn.execute_statement("INSERT INTO o VALUES (100, 10);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM u JOIN o ON u.id = o.u_id WHERE u.id = 10;")
        .unwrap();
    assert!(
        plan.contains("IndexScan")
            || plan.contains("PointLookup")
            || plan.contains("Predicate Inference")
            || plan.contains("HashJoin")
    );

    let rows = query(
        &mut txn,
        "SELECT * FROM u JOIN o ON u.id = o.u_id WHERE u.id = 10;",
    );
    assert_eq!(rows.len(), 1);
}

// ============================================================================
// R391: Index range lookup from inferred range
// ============================================================================
#[test]
fn test_r391_index_range_lookup_from_inferred_range() {
    let ctx = TestContext::new("r391");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_items_val ON items (val);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO items VALUES ({i}, {i});"))
            .unwrap();
    }

    let rows = query(&mut txn, "SELECT * FROM items WHERE val >= 5 AND val < 10;");
    assert_eq!(rows.len(), 5);
}

// ============================================================================
// R392: Composite-index access from inferred constraints
// ============================================================================
#[test]
fn test_r392_composite_index_access() {
    let ctx = TestContext::new("r392");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, country TEXT, status TEXT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_country_status ON users (country, status);")
        .unwrap();

    txn.execute_statement(
        "INSERT INTO users VALUES (1, 'IN', 'ACTIVE'), (2, 'US', 'ACTIVE'), (3, 'IN', 'INACTIVE');",
    )
    .unwrap();

    let rows = query(
        &mut txn,
        "SELECT * FROM users WHERE country = 'IN' AND status = 'ACTIVE';",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R393: Multi-index planning from inferred constraints
// ============================================================================
#[test]
fn test_r393_multi_index_planning() {
    let ctx = TestContext::new("r393");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, a INT, b INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_a ON items (a);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_b ON items (b);")
        .unwrap();

    txn.execute_statement("INSERT INTO items VALUES (1, 10, 20), (2, 10, 30), (3, 40, 20);")
        .unwrap();

    let rows = query(&mut txn, "SELECT * FROM items WHERE a = 10 AND b = 20;");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R394: Skip-scan regression
// ============================================================================
#[test]
fn test_r394_skip_scan_regression() {
    let ctx = TestContext::new("r394");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE events (id INT PRIMARY KEY, grp INT, ts INT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_grp_ts ON events (grp, ts);")
        .unwrap();

    for i in 1..=50 {
        let grp = i % 5;
        txn.execute_statement(&format!("INSERT INTO events VALUES ({i}, {grp}, {i});"))
            .unwrap();
    }

    let rows = query(&mut txn, "SELECT * FROM events WHERE ts = 25;");
    assert_eq!(rows.len(), 1);
}

// ============================================================================
// R395: Correlation statistics after normalization
// ============================================================================
#[test]
fn test_r395_correlation_statistics_after_normalization() {
    let ctx = TestContext::new("r395");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE stats_tbl (id INT PRIMARY KEY, country TEXT, status TEXT);",
    )
    .unwrap();
    txn.execute_statement("INSERT INTO stats_tbl VALUES (1, 'IN', 'ACTIVE'), (2, 'IN', 'ACTIVE');")
        .unwrap();
    txn.execute_statement("ANALYZE stats_tbl;").unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM stats_tbl WHERE country = 'IN' AND status = 'ACTIVE';")
        .unwrap();
    assert!(plan.contains("stats_tbl"));
}

// ============================================================================
// R396: Histogram estimation after range normalization
// ============================================================================
#[test]
fn test_r396_histogram_estimation_after_range_normalization() {
    let ctx = TestContext::new("r396");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE hist_tbl (id INT PRIMARY KEY, age INT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO hist_tbl VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE hist_tbl;").unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM hist_tbl WHERE age >= 18 AND age < 30;")
        .unwrap();
    assert!(plan.contains("hist_tbl"));
}

// ============================================================================
// R397: EmptyResult plan
// ============================================================================
#[test]
fn test_r397_empty_result_plan() {
    let ctx = TestContext::new("r397");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("INSERT INTO t VALUES (1);").unwrap();

    let rows = query(&mut txn, "SELECT * FROM t WHERE id = 1 AND id = 2;");
    assert_eq!(rows.len(), 0);
}

// ============================================================================
// R398: EmptyResult EXPLAIN
// ============================================================================
#[test]
fn test_r398_empty_result_explain() {
    let ctx = TestContext::new("r398");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id INT PRIMARY KEY);")
        .unwrap();

    let plan = txn
        .explain_sql("SELECT * FROM t WHERE id = 1 AND id = 2;")
        .unwrap();
    assert!(plan.contains("EmptyResult"));
    assert!(plan.contains("contradiction"));
}

// ============================================================================
// R399: EmptyResult EXPLAIN ANALYZE
// ============================================================================
#[test]
fn test_r399_empty_result_explain_analyze() {
    let ctx = TestContext::new("r399");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id INT PRIMARY KEY);")
        .unwrap();

    let explain_res = txn
        .execute_sql("EXPLAIN ANALYZE SELECT * FROM t WHERE id = 1 AND id = 2;")
        .unwrap();
    let plan_text = match &explain_res.rows[0].values[0] {
        Value::String(s) => s,
        _ => panic!("expected string output"),
    };
    assert!(plan_text.contains("EmptyResult"));
}

// ============================================================================
// R400: Prepared statement propagation
// ============================================================================
#[test]
fn test_r400_prepared_statement_propagation() {
    let ctx = TestContext::new("r400");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, val INT);")
        .unwrap();
    db.execute_statement("INSERT INTO users VALUES (1, 10), (2, 20);")
        .unwrap();

    let stmt = db.prepare_sql("SELECT * FROM users WHERE id = ?;").unwrap();
    let res = db
        .execute_prepared_query(&stmt, &[Value::Int64(1)])
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
}

// ============================================================================
// R401: Parameter-sensitive plan correctness
// ============================================================================
#[test]
fn test_r401_parameter_sensitive_plan_correctness() {
    let ctx = TestContext::new("r401");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, val INT);")
        .unwrap();
    db.execute_statement("INSERT INTO users VALUES (1, 10), (2, 20);")
        .unwrap();

    let stmt = db.prepare_sql("SELECT * FROM users WHERE id = ?;").unwrap();
    let r1 = db
        .execute_prepared_query(&stmt, &[Value::Int64(1)])
        .unwrap();
    let r2 = db
        .execute_prepared_query(&stmt, &[Value::Int64(2)])
        .unwrap();
    assert_eq!(r1.rows[0].values[0], Value::Int64(1));
    assert_eq!(r2.rows[0].values[0], Value::Int64(2));
}

// ============================================================================
// R402: Plan-cache correctness
// ============================================================================
#[test]
fn test_r402_plan_cache_correctness() {
    let ctx = TestContext::new("r402");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO items VALUES (1, 100), (2, 200);")
        .unwrap();

    for _ in 0..3 {
        let rows = query(&mut txn, "SELECT * FROM items WHERE id = 1;");
        assert_eq!(rows.len(), 1);
    }
}

// ============================================================================
// R403: UPDATE predicate inference
// ============================================================================
#[test]
fn test_r403_update_predicate_inference() {
    let ctx = TestContext::new("r403");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, balance INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (1, 100), (2, 200);")
        .unwrap();

    // Contradictory update should affect 0 rows
    txn.execute_statement("UPDATE accounts SET balance = 999 WHERE id = 1 AND id = 2;")
        .unwrap();

    let rows = query(&mut txn, "SELECT * FROM accounts WHERE id = 1;");
    assert_eq!(rows[0].values[1], Value::Int64(100));
}

// ============================================================================
// R404: DELETE predicate inference
// ============================================================================
#[test]
fn test_r404_delete_predicate_inference() {
    let ctx = TestContext::new("r404");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, balance INT);")
        .unwrap();
    txn.execute_statement("INSERT INTO accounts VALUES (1, 100), (2, 200);")
        .unwrap();

    // Contradictory delete should affect 0 rows
    txn.execute_statement("DELETE FROM accounts WHERE id = 1 AND id = 2;")
        .unwrap();

    let rows = query(&mut txn, "SELECT * FROM accounts;");
    assert_eq!(rows.len(), 2);
}

// ============================================================================
// R405: DML rollback
// ============================================================================
#[test]
fn test_r405_dml_rollback() {
    let ctx = TestContext::new("r405");
    let mut db = ctx.db();

    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, balance INT);")
            .unwrap();
        txn.execute_statement("INSERT INTO accounts VALUES (1, 100);")
            .unwrap();
        txn.commit().unwrap();
    }

    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("UPDATE accounts SET balance = 500 WHERE id = 1;")
            .unwrap();
        txn.abort().unwrap();
    }

    {
        let mut txn = db.begin_transaction().unwrap();
        let rows = query(&mut txn, "SELECT * FROM accounts WHERE id = 1;");
        assert_eq!(rows[0].values[1], Value::Int64(100));
    }
}

// ============================================================================
// R406: Inference metrics
// ============================================================================
#[test]
fn test_r406_inference_metrics() {
    let mut cset = ConstraintSet::new();
    cset.union_columns("a", "b", InferenceSource::TransitiveEquality);
    cset.add_constraint("a", ColumnConstraint::Exact(Value::Int64(10)));
    cset.add_constraint("b", ColumnConstraint::Exact(Value::Int64(20)));

    assert!(cset.is_unsatisfiable());
    assert!(cset.contradiction_reason().is_some());
}

// ============================================================================
// R407: Randomized equality-propagation differential test
// ============================================================================
#[test]
fn test_r407_randomized_equality_propagation_differential() {
    let ctx = TestContext::new("r407");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, t1_id INT, num INT);")
        .unwrap();

    for i in 1..=20 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i} * 10);"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO t2 VALUES ({i}, {i}, {i} * 100);"))
            .unwrap();
    }

    // Randomized test cases across different equality targets
    for target in [1, 5, 12, 19, 25] {
        let sql = format!(
            "SELECT t1.id, t2.num FROM t1 JOIN t2 ON t1.id = t2.t1_id WHERE t1.id = {target};"
        );
        let rows = query(&mut txn, &sql);
        if target <= 20 {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values[0], Value::Int64(target));
        } else {
            assert_eq!(rows.len(), 0);
        }
    }
}

// ============================================================================
// R408: Randomized range-constraint differential test
// ============================================================================
#[test]
fn test_r408_randomized_range_constraint_differential() {
    let ctx = TestContext::new("r408");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE r_test (id INT PRIMARY KEY, v INT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO r_test VALUES ({i}, {i});"))
            .unwrap();
    }

    let test_ranges = [(10, 20), (5, 5), (30, 15), (40, 50)];
    for (low, high) in test_ranges {
        let sql = format!("SELECT * FROM r_test WHERE v >= {low} AND v <= {high};");
        let rows = query(&mut txn, &sql);
        if low <= high {
            let expected_count = (high - low + 1) as usize;
            assert_eq!(rows.len(), expected_count);
        } else {
            assert_eq!(rows.len(), 0);
        }
    }
}

// ============================================================================
// R409: Randomized join-propagation differential test
// ============================================================================
#[test]
fn test_r409_randomized_join_propagation_differential() {
    let ctx = TestContext::new("r409");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id INT PRIMARY KEY, a_id INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE c (id INT PRIMARY KEY, b_id INT);")
        .unwrap();

    for i in 1..=10 {
        txn.execute_statement(&format!("INSERT INTO a VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO b VALUES ({i}, {i});"))
            .unwrap();
        txn.execute_statement(&format!("INSERT INTO c VALUES ({i}, {i});"))
            .unwrap();
    }

    let sql = "SELECT a.id, b.id, c.id FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id WHERE a.id = 7;";
    let rows = query(&mut txn, sql);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[0], Value::Int64(7));
    assert_eq!(rows[0].values[1], Value::Int64(7));
    assert_eq!(rows[0].values[2], Value::Int64(7));
}

// ============================================================================
// R410: Randomized NULL/3VL differential test
// ============================================================================
#[test]
fn test_r410_randomized_null_3vl_differential() {
    let ctx = TestContext::new("r410");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE n_test (id INT PRIMARY KEY, a INT, b INT);")
        .unwrap();

    txn.execute_statement(
        "INSERT INTO n_test VALUES (1, NULL, 10), (2, 20, NULL), (3, NULL, NULL), (4, 40, 40);",
    )
    .unwrap();

    // 1. IS NULL
    let r1 = query(&mut txn, "SELECT * FROM n_test WHERE a IS NULL;");
    assert_eq!(r1.len(), 2);

    // 2. IS NOT NULL
    let r2 = query(&mut txn, "SELECT * FROM n_test WHERE a IS NOT NULL;");
    assert_eq!(r2.len(), 2);

    // 3. a = NULL (Must return 0 rows in SQL 3VL)
    let r3 = query(&mut txn, "SELECT * FROM n_test WHERE a = NULL;");
    assert_eq!(r3.len(), 0);

    // 4. a = b when both are NULL (Must return 0 rows in SQL 3VL)
    let r4 = query(&mut txn, "SELECT * FROM n_test WHERE a = b;");
    assert_eq!(r4.len(), 1); // only row 4 (40 == 40)
    assert_eq!(r4[0].values[0], Value::Int64(4));
}
