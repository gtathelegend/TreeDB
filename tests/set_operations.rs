use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use treedb::engine::types::{DataType, Row, Value};
use treedb::engine::TreeDb;
use treedb::error::Error;
use treedb::sql::{parse_statement, SetOperator, Statement};

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let count = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let db_path = dir.path().join(format!(
            "test_set_ops_{name}_{}_{count}.treedb",
            std::process::id()
        ));
        let wal_path = dir.path().join(format!(
            "test_set_ops_{name}_{}_{count}.treedb.wal",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&wal_path);
        Self {
            _dir: dir,
            db_path,
            wal_path,
        }
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
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = std::fs::remove_file(&bak);
    }
}

fn row1_i64(v: i64) -> Row {
    Row::new(vec![Value::Int64(v)])
}

fn row1_null() -> Row {
    Row::new(vec![Value::Null])
}

// ============================================================================
// 1. Parser Tests
// ============================================================================

#[test]
fn test_parser_set_operations_basic() {
    let queries = [
        (
            "SELECT 1 FROM t1 UNION SELECT 2 FROM t2",
            SetOperator::Union,
        ),
        (
            "SELECT 1 FROM t1 UNION ALL SELECT 2 FROM t2",
            SetOperator::UnionAll,
        ),
        (
            "SELECT 1 FROM t1 INTERSECT SELECT 2 FROM t2",
            SetOperator::Intersect,
        ),
        (
            "SELECT 1 FROM t1 INTERSECT ALL SELECT 2 FROM t2",
            SetOperator::IntersectAll,
        ),
        (
            "SELECT 1 FROM t1 EXCEPT SELECT 2 FROM t2",
            SetOperator::Except,
        ),
        (
            "SELECT 1 FROM t1 EXCEPT ALL SELECT 2 FROM t2",
            SetOperator::ExceptAll,
        ),
    ];

    for (sql, expected_op) in queries {
        let stmt = parse_statement(sql).expect("parse failed");
        match stmt {
            Statement::SetQuery(q) => match q.body {
                treedb::sql::SetQueryBody::SetOperation { op, .. } => {
                    assert_eq!(op, expected_op);
                }
                other => panic!("expected SetOperation, got {other:?}"),
            },
            other => panic!("expected SetQuery, got {other:?}"),
        }
    }
}

#[test]
fn test_parser_case_insensitivity() {
    let sqls = [
        "select 1 from t1 union select 2 from t2",
        "Select 1 From t1 Union Select 2 From t2",
        "SELECT 1 FROM t1 UNION SELECT 2 FROM t2",
        "SELECT 1 FROM t1 union all SELECT 2 FROM t2",
        "SELECT 1 FROM t1 Intersect SELECT 2 FROM t2",
        "SELECT 1 FROM t1 except all SELECT 2 FROM t2",
    ];
    for sql in sqls {
        assert!(parse_statement(sql).is_ok());
    }
}

#[test]
fn test_parser_precedence_and_parentheses() {
    // INTERSECT has higher precedence than UNION and EXCEPT
    // A UNION B INTERSECT C -> A UNION (B INTERSECT C)
    let sql1 = "SELECT 1 FROM t1 UNION SELECT 2 FROM t2 INTERSECT SELECT 3 FROM t3";
    let stmt1 = parse_statement(sql1).expect("parse sql1");
    if let Statement::SetQuery(q1) = stmt1 {
        if let treedb::sql::SetQueryBody::SetOperation { op, left, right } = q1.body {
            assert_eq!(op, SetOperator::Union);
            assert!(matches!(*left, treedb::sql::SetQueryBody::Select(_)));
            assert!(matches!(
                *right,
                treedb::sql::SetQueryBody::SetOperation {
                    op: SetOperator::Intersect,
                    ..
                }
            ));
        } else {
            panic!("expected top-level Union");
        }
    } else {
        panic!("expected SetQuery");
    }

    // (A UNION B) INTERSECT C
    let sql2 = "(SELECT 1 FROM t1 UNION SELECT 2 FROM t2) INTERSECT SELECT 3 FROM t3";
    let stmt2 = parse_statement(sql2).expect("parse sql2");
    if let Statement::SetQuery(q2) = stmt2 {
        if let treedb::sql::SetQueryBody::SetOperation { op, left, right } = q2.body {
            assert_eq!(op, SetOperator::Intersect);
            assert!(matches!(*left, treedb::sql::SetQueryBody::Parenthesized(_)));
            assert!(matches!(*right, treedb::sql::SetQueryBody::Select(_)));
        } else {
            panic!("expected top-level Intersect");
        }
    } else {
        panic!("expected SetQuery");
    }
}

#[test]
fn test_parser_malformed_set_operations() {
    let malformed = [
        "UNION",
        "UNION ALL",
        "INTERSECT",
        "EXCEPT ALL",
        "SELECT 1 FROM t UNION",
        "SELECT 1 FROM t UNION ALL",
        "SELECT 1 FROM t INTERSECT",
        "SELECT 1 FROM t EXCEPT ALL",
        "UNION SELECT 1 FROM t",
        "INTERSECT ALL SELECT 1 FROM t",
        "SELECT 1 FROM t UNION UNION SELECT 2 FROM t",
        "SELECT 1 FROM t INTERSECT EXCEPT SELECT 2 FROM t",
    ];

    for &sql in &malformed {
        assert!(
            parse_statement(sql).is_err(),
            "expected error for malformed: {sql}"
        );
    }
}

// ============================================================================
// 2. Binding and Schema Compatibility Tests
// ============================================================================

#[test]
fn test_binding_column_count_mismatch() {
    let ctx = TestContext::new("bind_col_mismatch");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY);")
        .unwrap();

    let res = txn.execute_sql("SELECT id, val FROM t1 UNION SELECT id FROM t2;");
    assert!(matches!(
        res,
        Err(Error::SetOperationColumnCountMismatch {
            left_cols: 2,
            right_cols: 1
        })
    ));
}

#[test]
fn test_binding_type_compatibility_and_coercion() {
    let ctx = TestContext::new("bind_type_compat");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t_int (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_float (id INT PRIMARY KEY, val FLOAT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_str (id INT PRIMARY KEY, name VARCHAR);")
        .unwrap();

    txn.execute_statement("INSERT INTO t_int VALUES (10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t_float VALUES (1, 20.5);")
        .unwrap();

    // INT and FLOAT unify to FLOAT
    let res = txn
        .execute_sql("SELECT id FROM t_int UNION SELECT val FROM t_float;")
        .unwrap();
    assert_eq!(res.schema.columns[0].data_type, DataType::Float64);
    assert_eq!(res.rows.len(), 2);

    // Incompatible types fail at binding
    let err = txn.execute_sql("SELECT id FROM t_int UNION SELECT name FROM t_str;");
    assert!(matches!(err, Err(Error::SetOperationTypeMismatch { .. })));
}

#[test]
fn test_output_column_naming_from_left_operand() {
    let ctx = TestContext::new("col_naming");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE a (left_name INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (right_name INT PRIMARY KEY);")
        .unwrap();

    let res = txn
        .execute_sql("SELECT left_name FROM a UNION ALL SELECT right_name FROM b;")
        .unwrap();
    assert_eq!(res.schema.columns[0].name, "left_name");
}

// ============================================================================
// 3. UNION and UNION ALL Semantics
// ============================================================================

#[test]
fn test_union_deduplication_and_union_all_preservation() {
    let ctx = TestContext::new("union_semantics");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE left_t (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE right_t (id INT PRIMARY KEY, v INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO left_t VALUES (1, 10), (2, 20), (3, 20);")
        .unwrap();
    txn.execute_statement("INSERT INTO right_t VALUES (1, 20), (2, 30);")
        .unwrap();

    // UNION (deduplicating) on column v: left has 10, 20, 20; right has 20, 30 -> result: 10, 20, 30
    let res_union = txn
        .execute_sql("SELECT v FROM left_t UNION SELECT v FROM right_t ORDER BY v ASC;")
        .unwrap();
    assert_eq!(
        res_union.rows,
        vec![row1_i64(10), row1_i64(20), row1_i64(30)]
    );

    // UNION ALL (duplicate preserving): left has 10, 20, 20; right has 20, 30 -> total 5 rows
    let res_union_all = txn
        .execute_sql("SELECT v FROM left_t UNION ALL SELECT v FROM right_t ORDER BY v ASC;")
        .unwrap();
    assert_eq!(
        res_union_all.rows,
        vec![
            row1_i64(10),
            row1_i64(20),
            row1_i64(20),
            row1_i64(20),
            row1_i64(30)
        ]
    );
}

#[test]
fn test_union_empty_operands() {
    let ctx = TestContext::new("union_empty");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE empty_a (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE empty_b (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE non_empty (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("INSERT INTO non_empty VALUES (1), (2);")
        .unwrap();

    // empty UNION empty
    let res = txn
        .execute_sql("SELECT id FROM empty_a UNION SELECT id FROM empty_b;")
        .unwrap();
    assert!(res.rows.is_empty());

    // empty UNION non_empty
    let res = txn
        .execute_sql("SELECT id FROM empty_a UNION SELECT id FROM non_empty ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res.rows, vec![row1_i64(1), row1_i64(2)]);

    // non_empty UNION empty
    let res = txn
        .execute_sql("SELECT id FROM non_empty UNION SELECT id FROM empty_a ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res.rows, vec![row1_i64(1), row1_i64(2)]);
}

// ============================================================================
// 4. INTERSECT and INTERSECT ALL Semantics
// ============================================================================

#[test]
fn test_intersect_and_intersect_all() {
    let ctx = TestContext::new("intersect_semantics");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t_left (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_right (id INT PRIMARY KEY, v INT);")
        .unwrap();

    // left: 2, 2, 2, 3
    txn.execute_statement("INSERT INTO t_left VALUES (1, 2), (2, 2), (3, 2), (4, 3);")
        .unwrap();
    // right: 2, 2, 4
    txn.execute_statement("INSERT INTO t_right VALUES (1, 2), (2, 2), (3, 4);")
        .unwrap();

    // INTERSECT (deduplicating) -> { 2 }
    let res = txn
        .execute_sql("SELECT v FROM t_left INTERSECT SELECT v FROM t_right;")
        .unwrap();
    assert_eq!(res.rows, vec![row1_i64(2)]);

    // INTERSECT ALL (multiset min count): min(3, 2) = 2 -> { 2, 2 }
    let res_all = txn
        .execute_sql("SELECT v FROM t_left INTERSECT ALL SELECT v FROM t_right ORDER BY v;")
        .unwrap();
    assert_eq!(res_all.rows, vec![row1_i64(2), row1_i64(2)]);
}

#[test]
fn test_intersect_no_overlap_and_complete_overlap() {
    let ctx = TestContext::new("intersect_overlap");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1), (2);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (3), (4);")
        .unwrap();

    // No overlap
    let res = txn
        .execute_sql("SELECT id FROM t1 INTERSECT SELECT id FROM t2;")
        .unwrap();
    assert!(res.rows.is_empty());

    // Complete overlap
    let res_overlap = txn
        .execute_sql("SELECT id FROM t1 INTERSECT SELECT id FROM t1 ORDER BY id ASC;")
        .unwrap();
    assert_eq!(res_overlap.rows, vec![row1_i64(1), row1_i64(2)]);
}

// ============================================================================
// 5. EXCEPT and EXCEPT ALL Semantics
// ============================================================================

#[test]
fn test_except_and_except_all() {
    let ctx = TestContext::new("except_semantics");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t_left (id INT PRIMARY KEY, v INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t_right (id INT PRIMARY KEY, v INT);")
        .unwrap();

    // left: 1, 2, 2, 2, 3
    txn.execute_statement("INSERT INTO t_left VALUES (1, 1), (2, 2), (3, 2), (4, 2), (5, 3);")
        .unwrap();
    // right: 2, 3
    txn.execute_statement("INSERT INTO t_right VALUES (1, 2), (2, 3);")
        .unwrap();

    // EXCEPT (distinct subtraction): left has 1, 2, 3; right has 2, 3 -> { 1 }
    let res = txn
        .execute_sql("SELECT v FROM t_left EXCEPT SELECT v FROM t_right ORDER BY v;")
        .unwrap();
    assert_eq!(res.rows, vec![row1_i64(1)]);

    // EXCEPT ALL (multiset subtraction): 1: 1-0=1; 2: 3-1=2; 3: 1-1=0 -> { 1, 2, 2 }
    let res_all = txn
        .execute_sql("SELECT v FROM t_left EXCEPT ALL SELECT v FROM t_right ORDER BY v ASC;")
        .unwrap();
    assert_eq!(res_all.rows, vec![row1_i64(1), row1_i64(2), row1_i64(2)]);
}

// ============================================================================
// 6. NULL Semantics (NULL = NULL in set operations)
// ============================================================================

#[test]
fn test_null_row_equivalence_in_set_operations() {
    let ctx = TestContext::new("null_semantics");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, NULL), (2, NULL), (3, 10);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (1, NULL), (2, 20);")
        .unwrap();

    // UNION: NULLs deduplicate to a single NULL
    let res_union = txn
        .execute_sql("SELECT val FROM t1 UNION SELECT val FROM t2 ORDER BY val ASC;")
        .unwrap();
    assert_eq!(
        res_union.rows,
        vec![row1_null(), row1_i64(10), row1_i64(20)]
    );

    // INTERSECT: NULL matches NULL -> { NULL }
    let res_intersect = txn
        .execute_sql("SELECT val FROM t1 INTERSECT SELECT val FROM t2;")
        .unwrap();
    assert_eq!(res_intersect.rows, vec![row1_null()]);

    // INTERSECT ALL: left has 2 NULLs, right has 1 NULL -> min(2, 1) = 1 NULL
    let res_intersect_all = txn
        .execute_sql("SELECT val FROM t1 INTERSECT ALL SELECT val FROM t2;")
        .unwrap();
    assert_eq!(res_intersect_all.rows, vec![row1_null()]);

    // EXCEPT: left { NULL, 10 }, right { NULL, 20 } -> { 10 }
    let res_except = txn
        .execute_sql("SELECT val FROM t1 EXCEPT SELECT val FROM t2;")
        .unwrap();
    assert_eq!(res_except.rows, vec![row1_i64(10)]);

    // EXCEPT ALL: left { NULL, NULL, 10 }, right { NULL, 20 } -> { NULL, 10 }
    let res_except_all = txn
        .execute_sql("SELECT val FROM t1 EXCEPT ALL SELECT val FROM t2 ORDER BY val ASC;")
        .unwrap();
    assert_eq!(res_except_all.rows, vec![row1_null(), row1_i64(10)]);
}

// ============================================================================
// 7. Composition: JOINs, Aggregation, Window Functions, CTEs, Subqueries
// ============================================================================

#[test]
fn test_set_operation_with_joins() {
    let ctx = TestContext::new("set_joins");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR);")
        .unwrap();
    txn.execute_statement("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE refunds (id INT PRIMARY KEY, user_id INT, amount INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');")
        .unwrap();
    txn.execute_statement("INSERT INTO orders VALUES (10, 1, 100), (20, 2, 200);")
        .unwrap();
    txn.execute_statement("INSERT INTO refunds VALUES (100, 1, 50);")
        .unwrap();

    let sql = "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id \
               UNION \
               SELECT u.name, r.amount FROM users u JOIN refunds r ON u.id = r.user_id \
               ORDER BY name ASC, amount ASC;";
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(
        res.rows,
        vec![
            Row::new(vec![Value::String("Alice".into()), Value::Int64(50)]),
            Row::new(vec![Value::String("Alice".into()), Value::Int64(100)]),
            Row::new(vec![Value::String("Bob".into()), Value::Int64(200)]),
        ]
    );
}

#[test]
fn test_set_operation_with_aggregations() {
    let ctx = TestContext::new("set_aggs");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement(
        "CREATE TABLE sales_2022 (id INT PRIMARY KEY, dept VARCHAR, revenue INT);",
    )
    .unwrap();
    txn.execute_statement(
        "CREATE TABLE sales_2023 (id INT PRIMARY KEY, dept VARCHAR, revenue INT);",
    )
    .unwrap();

    txn.execute_statement("INSERT INTO sales_2022 VALUES (1, 'HR', 100), (2, 'IT', 300);")
        .unwrap();
    txn.execute_statement("INSERT INTO sales_2023 VALUES (1, 'HR', 150), (2, 'IT', 300);")
        .unwrap();

    let sql = "SELECT dept, SUM(revenue) FROM sales_2022 GROUP BY dept \
               INTERSECT \
               SELECT dept, SUM(revenue) FROM sales_2023 GROUP BY dept;";
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(
        res.rows,
        vec![Row::new(vec![
            Value::String("IT".into()),
            Value::Int64(300)
        ])]
    );
}

#[test]
fn test_set_operation_with_window_functions() {
    let ctx = TestContext::new("set_window");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE scores1 (id INT PRIMARY KEY, score INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE scores2 (id INT PRIMARY KEY, score INT);")
        .unwrap();

    txn.execute_statement("INSERT INTO scores1 VALUES (1, 80), (2, 90);")
        .unwrap();
    txn.execute_statement("INSERT INTO scores2 VALUES (1, 85), (2, 90);")
        .unwrap();

    let sql = "SELECT score, ROW_NUMBER() OVER (ORDER BY score ASC) AS rnk FROM scores1 \
               UNION \
               SELECT score, ROW_NUMBER() OVER (ORDER BY score ASC) AS rnk FROM scores2 \
               ORDER BY score ASC, rnk ASC;";
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(
        res.rows,
        vec![
            Row::new(vec![Value::Int64(80), Value::UInt64(1)]),
            Row::new(vec![Value::Int64(85), Value::UInt64(1)]),
            Row::new(vec![Value::Int64(90), Value::UInt64(2)]),
        ]
    );
}

#[test]
fn test_set_operation_inside_cte() {
    let ctx = TestContext::new("set_cte");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t1 (v INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (v INT PRIMARY KEY);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1), (2);")
        .unwrap();
    txn.execute_statement("INSERT INTO t2 VALUES (2), (3);")
        .unwrap();

    let sql = "WITH combined AS ( \
                   SELECT v FROM t1 \
                   UNION ALL \
                   SELECT v FROM t2 \
               ) \
               SELECT v, COUNT(v) FROM combined GROUP BY v ORDER BY v ASC;";
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(
        res.rows,
        vec![
            Row::new(vec![Value::Int64(1), Value::UInt64(1)]),
            Row::new(vec![Value::Int64(2), Value::UInt64(2)]),
            Row::new(vec![Value::Int64(3), Value::UInt64(1)]),
        ]
    );
}

#[test]
fn test_set_operation_in_subqueries() {
    let ctx = TestContext::new("set_subqueries");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE base (x INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE s1 (x INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE s2 (x INT PRIMARY KEY);")
        .unwrap();

    txn.execute_statement("INSERT INTO base VALUES (1), (2), (3), (4);")
        .unwrap();
    txn.execute_statement("INSERT INTO s1 VALUES (2);").unwrap();
    txn.execute_statement("INSERT INTO s2 VALUES (3);").unwrap();

    // IN (SetSubquery)
    let sql_in =
        "SELECT x FROM base WHERE x IN (SELECT x FROM s1 UNION SELECT x FROM s2) ORDER BY x ASC;";
    let res_in = txn.execute_sql(sql_in).unwrap();
    assert_eq!(res_in.rows, vec![row1_i64(2), row1_i64(3)]);

    // EXISTS (SetSubquery)
    let sql_exists = "SELECT x FROM base WHERE EXISTS (SELECT x FROM s1 WHERE s1.x = base.x UNION SELECT x FROM s2 WHERE s2.x = base.x) ORDER BY x ASC;";
    let res_exists = txn.execute_sql(sql_exists).unwrap();
    assert_eq!(res_exists.rows, vec![row1_i64(2), row1_i64(3)]);

    // Scalar SetSubquery
    let sql_scalar =
        "SELECT (SELECT x FROM s1 UNION SELECT x FROM s1) AS val FROM base WHERE x = 1;";
    let res_scalar = txn.execute_sql(sql_scalar).unwrap();
    assert_eq!(res_scalar.rows, vec![row1_i64(2)]);
}

// ============================================================================
// 8. Ordering, Limit, and Offset
// ============================================================================

#[test]
fn test_set_operation_order_by_limit_offset() {
    let ctx = TestContext::new("set_order_limit");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE a (v INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (v INT PRIMARY KEY);")
        .unwrap();

    txn.execute_statement("INSERT INTO a VALUES (10), (30), (50);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (20), (40), (60);")
        .unwrap();

    // Overall ORDER BY + LIMIT + OFFSET
    let sql = "SELECT v FROM a UNION SELECT v FROM b ORDER BY v DESC LIMIT 3 OFFSET 1;";
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows, vec![row1_i64(50), row1_i64(40), row1_i64(30)]);
}

// ============================================================================
// 9. Recursive CTE Distinction
// ============================================================================

#[test]
fn test_recursive_cte_distinction_preserved() {
    let ctx = TestContext::new("rec_cte_distinction");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();

    txn.execute_statement("CREATE TABLE anchor (x INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("INSERT INTO anchor VALUES (1);")
        .unwrap();

    let sql_rec = "WITH RECURSIVE cnt(x) AS ( \
                       SELECT x FROM anchor \
                       UNION ALL \
                       SELECT x + 1 FROM cnt WHERE x < 5 \
                   ) \
                   SELECT x FROM cnt ORDER BY x ASC;";
    let res = txn.execute_sql(sql_rec).unwrap();
    assert_eq!(
        res.rows,
        vec![
            row1_i64(1),
            row1_i64(2),
            row1_i64(3),
            row1_i64(4),
            row1_i64(5),
        ]
    );

    // Non-recursive UNION in recursive CTE is rejected
    let sql_invalid = "WITH RECURSIVE cnt(x) AS ( \
                           SELECT x FROM anchor \
                           UNION \
                           SELECT x + 1 FROM cnt WHERE x < 5 \
                       ) \
                       SELECT x FROM cnt;";
    assert!(txn.execute_sql(sql_invalid).is_err());
}

// ============================================================================
// 10. Transaction Visibility and Rollback
// ============================================================================

#[test]
fn test_set_operation_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("set_tx_visibility");
    let mut db = ctx.db();

    // Txn 1: create and insert initial data
    {
        let mut txn1 = db.begin().unwrap();
        txn1.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY, val INT);")
            .unwrap();
        txn1.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY, val INT);")
            .unwrap();
        txn1.execute_statement("INSERT INTO t1 VALUES (1, 100);")
            .unwrap();
        txn1.commit().unwrap();
    }

    // Txn 2: uncommitted inserts/updates/deletes are visible to set operations
    {
        let mut txn2 = db.begin().unwrap();
        txn2.execute_statement("INSERT INTO t2 VALUES (1, 200);")
            .unwrap();
        txn2.execute_statement("UPDATE t1 SET val = 150 WHERE id = 1;")
            .unwrap();

        let res = txn2
            .execute_sql("SELECT val FROM t1 UNION ALL SELECT val FROM t2 ORDER BY val ASC;")
            .unwrap();
        assert_eq!(res.rows, vec![row1_i64(150), row1_i64(200)]);

        // Rollback / Abort
        txn2.abort().unwrap();
    }

    // Txn 3: verify rollback cleaned up uncommitted mutations
    {
        let mut txn3 = db.begin().unwrap();
        let res = txn3
            .execute_sql("SELECT val FROM t1 UNION ALL SELECT val FROM t2;")
            .unwrap();
        assert_eq!(res.rows, vec![row1_i64(100)]);
    }
}

// ============================================================================
// 11. Persistence and Recovery Equivalence
// ============================================================================

#[test]
fn test_set_operation_persistence_across_reopen() {
    let ctx = TestContext::new("set_persistence");

    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();
        txn.execute_statement("CREATE TABLE left_p (id INT PRIMARY KEY, v INT);")
            .unwrap();
        txn.execute_statement("CREATE TABLE right_p (id INT PRIMARY KEY, v INT);")
            .unwrap();
        txn.execute_statement("INSERT INTO left_p VALUES (1, 10), (2, 20);")
            .unwrap();
        txn.execute_statement("INSERT INTO right_p VALUES (1, 20), (2, 30);")
            .unwrap();
        txn.commit().unwrap();
        db.checkpoint().unwrap();
    }

    // Reopen DB and execute set operations
    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();

        let res = txn
            .execute_sql("SELECT v FROM left_p INTERSECT SELECT v FROM right_p;")
            .unwrap();
        assert_eq!(res.rows, vec![row1_i64(20)]);

        let res_except = txn
            .execute_sql("SELECT v FROM left_p EXCEPT SELECT v FROM right_p;")
            .unwrap();
        assert_eq!(res_except.rows, vec![row1_i64(10)]);
    }
}

// ============================================================================
// 12. Optimizer and EXPLAIN Output
// ============================================================================

#[test]
fn test_set_operation_explain_output() {
    let ctx = TestContext::new("set_explain");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();
    txn.execute_statement("CREATE TABLE t1 (id INT PRIMARY KEY);")
        .unwrap();
    txn.execute_statement("CREATE TABLE t2 (id INT PRIMARY KEY);")
        .unwrap();

    let explain_union = txn
        .explain_sql("SELECT id FROM t1 UNION ALL SELECT id FROM t2;")
        .unwrap();
    assert!(explain_union.contains("Set Operation: UNION ALL"));
    assert!(explain_union.contains("Left"));
    assert!(explain_union.contains("Right"));

    let explain_intersect = txn
        .explain_sql("SELECT id FROM t1 INTERSECT SELECT id FROM t2;")
        .unwrap();
    assert!(explain_intersect.contains("Set Operation: INTERSECT"));
}

// ============================================================================
// 13. Reference Oracle and Randomized Testing
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum OracleValue {
    Null,
    Int64(i64),
    UInt64(u64),
    Float64(u64), // bitwise representation for exact hashing/equality
    String(String),
}

impl OracleValue {
    fn from_val(v: &Value) -> Self {
        match v {
            Value::Null => OracleValue::Null,
            Value::Int64(i) => OracleValue::Int64(*i),
            Value::UInt64(u) => OracleValue::UInt64(*u),
            Value::Float64(f) => OracleValue::Float64(f.to_bits()),
            Value::String(s) => OracleValue::String(s.clone()),
            _ => panic!("unsupported oracle type"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OracleRow(Vec<OracleValue>);

impl OracleRow {
    fn from_row(r: &Row) -> Self {
        OracleRow(r.values.iter().map(OracleValue::from_val).collect())
    }
}

/// Pure-Rust independent reference evaluator for relational set operations.
fn evaluate_reference_set_operation(
    op: SetOperator,
    left: &[Row],
    right: &[Row],
) -> HashMap<OracleRow, usize> {
    let mut left_counts: HashMap<OracleRow, usize> = HashMap::new();
    for r in left {
        *left_counts.entry(OracleRow::from_row(r)).or_insert(0) += 1;
    }

    let mut right_counts: HashMap<OracleRow, usize> = HashMap::new();
    for r in right {
        *right_counts.entry(OracleRow::from_row(r)).or_insert(0) += 1;
    }

    let mut result_counts = HashMap::new();

    match op {
        SetOperator::Union => {
            let mut all_keys: std::collections::HashSet<OracleRow> =
                left_counts.keys().cloned().collect();
            all_keys.extend(right_counts.keys().cloned());
            for k in all_keys {
                result_counts.insert(k, 1);
            }
        }
        SetOperator::UnionAll => {
            for (k, v) in left_counts {
                *result_counts.entry(k).or_insert(0) += v;
            }
            for (k, v) in right_counts {
                *result_counts.entry(k).or_insert(0) += v;
            }
        }
        SetOperator::Intersect => {
            for (k, _) in left_counts {
                if right_counts.contains_key(&k) {
                    result_counts.insert(k, 1);
                }
            }
        }
        SetOperator::IntersectAll => {
            for (k, l_cnt) in left_counts {
                if let Some(&r_cnt) = right_counts.get(&k) {
                    let min_cnt = std::cmp::min(l_cnt, r_cnt);
                    if min_cnt > 0 {
                        result_counts.insert(k, min_cnt);
                    }
                }
            }
        }
        SetOperator::Except => {
            for (k, _) in left_counts {
                if !right_counts.contains_key(&k) {
                    result_counts.insert(k, 1);
                }
            }
        }
        SetOperator::ExceptAll => {
            for (k, l_cnt) in left_counts {
                let r_cnt = right_counts.get(&k).copied().unwrap_or(0);
                if l_cnt > r_cnt {
                    result_counts.insert(k, l_cnt - r_cnt);
                }
            }
        }
    }

    result_counts
}

#[test]
fn test_randomized_set_operation_oracle() {
    let ctx = TestContext::new("oracle");
    let mut db = ctx.db();

    // Pseudo-random linear congruential generator for reproducible testing
    let mut rng_state = 123456789u64;
    let mut next_rand = || {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };

    let operators = [
        (SetOperator::Union, "UNION"),
        (SetOperator::UnionAll, "UNION ALL"),
        (SetOperator::Intersect, "INTERSECT"),
        (SetOperator::IntersectAll, "INTERSECT ALL"),
        (SetOperator::Except, "EXCEPT"),
        (SetOperator::ExceptAll, "EXCEPT ALL"),
    ];

    for iteration in 0..20 {
        let mut txn = db.begin().unwrap();
        let t1_name = format!("t1_{iteration}");
        let t2_name = format!("t2_{iteration}");

        txn.execute_statement(&format!(
            "CREATE TABLE {t1_name} (id INT PRIMARY KEY, val INT, tag VARCHAR);"
        ))
        .unwrap();
        txn.execute_statement(&format!(
            "CREATE TABLE {t2_name} (id INT PRIMARY KEY, val INT, tag VARCHAR);"
        ))
        .unwrap();

        let num_left = (next_rand() % 15) as usize;
        let num_right = (next_rand() % 15) as usize;

        let mut left_rows = Vec::new();
        let mut right_rows = Vec::new();

        let possible_tags = ["A", "B", "C", "NULL"];
        let possible_vals = [Some(1), Some(2), Some(3), None];

        for i in 0..num_left {
            let val_choice = possible_vals[(next_rand() % 4) as usize];
            let tag_choice = possible_tags[(next_rand() % 4) as usize];

            let val_sql = match val_choice {
                Some(v) => v.to_string(),
                None => "NULL".to_string(),
            };
            let tag_sql = if tag_choice == "NULL" {
                "NULL".to_string()
            } else {
                format!("'{tag_choice}'")
            };

            txn.execute_statement(&format!(
                "INSERT INTO {t1_name} VALUES ({i}, {val_sql}, {tag_sql});"
            ))
            .unwrap();

            left_rows.push(Row::new(vec![
                match val_choice {
                    Some(v) => Value::Int64(v),
                    None => Value::Null,
                },
                if tag_choice == "NULL" {
                    Value::Null
                } else {
                    Value::String(tag_choice.to_string())
                },
            ]));
        }

        for i in 0..num_right {
            let val_choice = possible_vals[(next_rand() % 4) as usize];
            let tag_choice = possible_tags[(next_rand() % 4) as usize];

            let val_sql = match val_choice {
                Some(v) => v.to_string(),
                None => "NULL".to_string(),
            };
            let tag_sql = if tag_choice == "NULL" {
                "NULL".to_string()
            } else {
                format!("'{tag_choice}'")
            };

            txn.execute_statement(&format!(
                "INSERT INTO {t2_name} VALUES ({i}, {val_sql}, {tag_sql});"
            ))
            .unwrap();

            right_rows.push(Row::new(vec![
                match val_choice {
                    Some(v) => Value::Int64(v),
                    None => Value::Null,
                },
                if tag_choice == "NULL" {
                    Value::Null
                } else {
                    Value::String(tag_choice.to_string())
                },
            ]));
        }

        for (op, op_str) in &operators {
            let expected_counts = evaluate_reference_set_operation(*op, &left_rows, &right_rows);

            let sql =
                format!("SELECT val, tag FROM {t1_name} {op_str} SELECT val, tag FROM {t2_name};");
            let actual_res = txn.execute_sql(&sql).unwrap();

            let mut actual_counts = HashMap::new();
            for r in &actual_res.rows {
                *actual_counts.entry(OracleRow::from_row(r)).or_insert(0) += 1;
            }

            assert_eq!(
                actual_counts, expected_counts,
                "Oracle mismatch on iteration {iteration} for operator {op_str}"
            );
        }

        txn.commit().unwrap();
    }
}
