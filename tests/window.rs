use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::error::Error;
use treedb::TreeDb;

fn col_names(res: &treedb::query::QueryResult) -> Vec<&str> {
    res.schema.columns.iter().map(|c| c.name.as_str()).collect()
}

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_window_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_window_{name}.treedb.wal"));
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
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
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

fn setup_employees_db(db: &mut TreeDb) {
    let mut txn = db.begin().unwrap();

    let employees_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
            Column::new("dept", DataType::String, true),
            Column::new("salary", DataType::Float64, true),
            Column::new("age", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("employees", employees_schema).unwrap();

    let depts_schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
            Column::new("location", DataType::String, false),
        ],
        0,
    )
    .unwrap();
    txn.create_table("departments", depts_schema).unwrap();

    // Insert departments
    txn.execute_statement(
        "INSERT INTO departments (id, name, location) VALUES (1, 'Engineering', 'Building A')",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO departments (id, name, location) VALUES (2, 'Sales', 'Building B')",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO departments (id, name, location) VALUES (3, 'HR', 'Building C')",
    )
    .unwrap();

    // Insert employees:
    // 1: Alice, Engineering, 1000.0, 30
    // 2: Bob, Engineering, 1200.0, 25
    // 3: Charlie, Engineering, 1000.0, 35 (Tie with Alice on salary)
    // 4: Dave, Sales, 800.0, 40
    // 5: Eve, Sales, 950.0, 28
    // 6: Frank, Sales, 950.0, 32 (Tie with Eve on salary)
    // 7: Grace, NULL, 500.0, 22 (NULL dept)
    // 8: Heidi, NULL, 700.0, 29 (NULL dept)
    // 9: Ivan, HR, NULL, 45 (NULL salary)
    txn.execute_statement("INSERT INTO employees (id, name, dept, salary, age) VALUES (1, 'Alice', 'Engineering', 1000.0, 30)").unwrap();
    txn.execute_statement("INSERT INTO employees (id, name, dept, salary, age) VALUES (2, 'Bob', 'Engineering', 1200.0, 25)").unwrap();
    txn.execute_statement("INSERT INTO employees (id, name, dept, salary, age) VALUES (3, 'Charlie', 'Engineering', 1000.0, 35)").unwrap();
    txn.execute_statement("INSERT INTO employees (id, name, dept, salary, age) VALUES (4, 'Dave', 'Sales', 800.0, 40)").unwrap();
    txn.execute_statement(
        "INSERT INTO employees (id, name, dept, salary, age) VALUES (5, 'Eve', 'Sales', 950.0, 28)",
    )
    .unwrap();
    txn.execute_statement("INSERT INTO employees (id, name, dept, salary, age) VALUES (6, 'Frank', 'Sales', 950.0, 32)").unwrap();
    txn.execute_statement(
        "INSERT INTO employees (id, name, dept, salary, age) VALUES (7, 'Grace', NULL, 500.0, 22)",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO employees (id, name, dept, salary, age) VALUES (8, 'Heidi', NULL, 700.0, 29)",
    )
    .unwrap();
    txn.execute_statement(
        "INSERT INTO employees (id, name, dept, salary, age) VALUES (9, 'Ivan', 'HR', NULL, 45)",
    )
    .unwrap();

    txn.commit().unwrap();
}

#[test]
fn test_window_row_number_basic() {
    let ctx = TestContext::new("row_number_basic");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id, dept, salary, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS rn FROM employees ORDER BY dept, rn, id",
        )
        .unwrap();

    assert_eq!(col_names(&res), vec!["id", "dept", "salary", "rn"]);

    // Check partition by dept, order by salary DESC
    let rows = res.rows;
    assert_eq!(rows.len(), 9);

    // Also test unpartitioned ROW_NUMBER() OVER (ORDER BY id ASC)
    let res2 = txn
        .execute_sql("SELECT id, ROW_NUMBER() OVER (ORDER BY id ASC) AS rn FROM employees")
        .unwrap();
    assert_eq!(res2.rows.len(), 9);
    for (i, row) in res2.rows.iter().enumerate() {
        assert_eq!(row.values[0], Value::UInt64((i + 1) as u64));
        assert_eq!(row.values[1], Value::UInt64((i + 1) as u64));
    }

    // Also test empty OVER ()
    let res3 = txn
        .execute_sql("SELECT id, ROW_NUMBER() OVER () AS rn FROM employees")
        .unwrap();
    assert_eq!(res3.rows.len(), 9);
    for (i, row) in res3.rows.iter().enumerate() {
        assert_eq!(row.values[1], Value::UInt64((i + 1) as u64));
    }
}

#[test]
fn test_window_rank_and_dense_rank_ties() {
    let ctx = TestContext::new("rank_dense_rank");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id, name, salary,
                    RANK() OVER (ORDER BY salary DESC) AS rnk,
                    DENSE_RANK() OVER (ORDER BY salary DESC) AS drnk
             FROM employees
             WHERE dept = 'Engineering'
             ORDER BY id",
        )
        .unwrap();

    // Engineering salaries: Bob=1200, Alice=1000, Charlie=1000
    // Bob (1200): rank 1, dense_rank 1
    // Alice (1000): rank 2, dense_rank 2
    // Charlie (1000): rank 2, dense_rank 2
    assert_eq!(res.rows.len(), 3);
    for row in &res.rows {
        match &row.values[1] {
            Value::String(n) if n == "Bob" => {
                assert_eq!(row.values[3], Value::UInt64(1)); // rnk
                assert_eq!(row.values[4], Value::UInt64(1)); // drnk
            }
            Value::String(n) if n == "Alice" || n == "Charlie" => {
                assert_eq!(row.values[3], Value::UInt64(2)); // rnk
                assert_eq!(row.values[4], Value::UInt64(2)); // drnk
            }
            _ => panic!("Unexpected employee"),
        }
    }

    // Now test a dataset with 4 rows where rank skips: 100, 100, 90, 80 -> ranks 1, 1, 3, 4; dense_ranks 1, 1, 2, 3
    let res_sales = txn
        .execute_sql(
            "SELECT id, name, salary,
                    RANK() OVER (ORDER BY salary DESC) AS rnk,
                    DENSE_RANK() OVER (ORDER BY salary DESC) AS drnk
             FROM employees
             WHERE dept = 'Sales'
             ORDER BY salary DESC, id ASC",
        )
        .unwrap();
    // Sales: Eve=950, Frank=950, Dave=800
    // Eve (950): rank 1, dense_rank 1
    // Frank (950): rank 1, dense_rank 1
    // Dave (800): rank 3, dense_rank 2
    assert_eq!(res_sales.rows[0].values[3], Value::UInt64(1));
    assert_eq!(res_sales.rows[0].values[4], Value::UInt64(1));
    assert_eq!(res_sales.rows[1].values[3], Value::UInt64(1));
    assert_eq!(res_sales.rows[1].values[4], Value::UInt64(1));
    assert_eq!(res_sales.rows[2].values[3], Value::UInt64(3)); // Gap from 1 -> 3
    assert_eq!(res_sales.rows[2].values[4], Value::UInt64(2)); // No gap: 1 -> 2
}

#[test]
fn test_window_null_ordering_and_partitions() {
    let ctx = TestContext::new("null_ordering");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    // In TreeDB: ASC puts NULL first, DESC puts NULL last.
    let res_asc = txn
        .execute_sql(
            "SELECT id, salary, ROW_NUMBER() OVER (ORDER BY salary ASC) AS rn FROM employees ORDER BY rn",
        )
        .unwrap();
    // Ivan has NULL salary, so Ivan gets rn 1 on ASC
    assert_eq!(res_asc.rows[0].values[0], Value::UInt64(9));
    assert_eq!(res_asc.rows[0].values[1], Value::Null);
    assert_eq!(res_asc.rows[0].values[2], Value::UInt64(1));

    // Test PARTITION BY with NULLs: Grace and Heidi have dept = NULL, so they are grouped together
    let res_null_part = txn
        .execute_sql(
            "SELECT id, name, dept, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY id) AS rn
             FROM employees
             WHERE dept IS NULL
             ORDER BY rn",
        )
        .unwrap();
    assert_eq!(res_null_part.rows.len(), 2);
    assert_eq!(res_null_part.rows[0].values[3], Value::UInt64(1));
    assert_eq!(res_null_part.rows[1].values[3], Value::UInt64(2));
}

#[test]
fn test_window_lag_and_lead() {
    let ctx = TestContext::new("lag_lead");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id, salary,
                    LAG(salary) OVER (ORDER BY id) AS prev_sal,
                    LEAD(salary) OVER (ORDER BY id) AS next_sal,
                    LAG(salary, 2, 0.0) OVER (ORDER BY id) AS lag2_sal,
                    LEAD(salary, 2, -1.0) OVER (ORDER BY id) AS lead2_sal
             FROM employees
             WHERE id <= 3
             ORDER BY id",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    // Row 1 (id 1, salary 1000.0):
    // prev_sal: NULL
    // next_sal: 1200.0
    // lag2_sal: 0.0 (default)
    // lead2_sal: 1000.0 (row 3 salary)
    assert_eq!(res.rows[0].values[2], Value::Null);
    assert_eq!(res.rows[0].values[3], Value::Float64(1200.0));
    assert_eq!(res.rows[0].values[4], Value::Float64(0.0));
    assert_eq!(res.rows[0].values[5], Value::Float64(1000.0));

    // Row 2 (id 2, salary 1200.0):
    // prev_sal: 1000.0
    // next_sal: 1000.0
    // lag2_sal: 0.0 (default)
    // lead2_sal: -1.0 (default)
    assert_eq!(res.rows[1].values[2], Value::Float64(1000.0));
    assert_eq!(res.rows[1].values[3], Value::Float64(1000.0));
    assert_eq!(res.rows[1].values[4], Value::Float64(0.0));
    assert_eq!(res.rows[1].values[5], Value::Float64(-1.0));

    // Row 3 (id 3, salary 1000.0):
    // prev_sal: 1200.0
    // next_sal: NULL
    // lag2_sal: 1000.0 (row 1 salary)
    // lead2_sal: -1.0 (default)
    assert_eq!(res.rows[2].values[2], Value::Float64(1200.0));
    assert_eq!(res.rows[2].values[3], Value::Null);
    assert_eq!(res.rows[2].values[4], Value::Float64(1000.0));
    assert_eq!(res.rows[2].values[5], Value::Float64(-1.0));
}

#[test]
fn test_window_first_value_and_last_value_frames() {
    let ctx = TestContext::new("first_last_value");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id, salary,
                    FIRST_VALUE(salary) OVER (ORDER BY id) AS fv,
                    LAST_VALUE(salary) OVER (ORDER BY id) AS lv_default,
                    LAST_VALUE(salary) OVER (
                        ORDER BY id
                        ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING
                    ) AS lv_full
             FROM employees
             WHERE id <= 3
             ORDER BY id",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    // Row 1 (id 1, sal 1000.0): fv=1000.0, lv_default=1000.0, lv_full=1000.0 (id 3)
    assert_eq!(res.rows[0].values[2], Value::Float64(1000.0));
    assert_eq!(res.rows[0].values[3], Value::Float64(1000.0));
    assert_eq!(res.rows[0].values[4], Value::Float64(1000.0));

    // Row 2 (id 2, sal 1200.0): fv=1000.0, lv_default=1200.0, lv_full=1000.0
    assert_eq!(res.rows[1].values[2], Value::Float64(1000.0));
    assert_eq!(res.rows[1].values[3], Value::Float64(1200.0));
    assert_eq!(res.rows[1].values[4], Value::Float64(1000.0));

    // Row 3 (id 3, sal 1000.0): fv=1000.0, lv_default=1000.0, lv_full=1000.0
    assert_eq!(res.rows[2].values[2], Value::Float64(1000.0));
    assert_eq!(res.rows[2].values[3], Value::Float64(1000.0));
    assert_eq!(res.rows[2].values[4], Value::Float64(1000.0));

    // Test sliding frame: ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING
    let res_sliding = txn
        .execute_sql(
            "SELECT id, salary,
                    FIRST_VALUE(salary) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS win_first,
                    LAST_VALUE(salary) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS win_last
             FROM employees
             WHERE id <= 3
             ORDER BY id",
        )
        .unwrap();

    // Row 1 (frame: 1 to 2): first=1000.0 (id 1), last=1200.0 (id 2)
    assert_eq!(res_sliding.rows[0].values[2], Value::Float64(1000.0));
    assert_eq!(res_sliding.rows[0].values[3], Value::Float64(1200.0));
    // Row 2 (frame: 1 to 3): first=1000.0 (id 1), last=1000.0 (id 3)
    assert_eq!(res_sliding.rows[1].values[2], Value::Float64(1000.0));
    assert_eq!(res_sliding.rows[1].values[3], Value::Float64(1000.0));
    // Row 3 (frame: 2 to 3): first=1200.0 (id 2), last=1000.0 (id 3)
    assert_eq!(res_sliding.rows[2].values[2], Value::Float64(1200.0));
    assert_eq!(res_sliding.rows[2].values[3], Value::Float64(1000.0));
}

#[test]
fn test_window_multiple_different_specs() {
    let ctx = TestContext::new("multiple_specs");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id, dept, salary,
                    ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS dept_rn,
                    RANK() OVER (ORDER BY salary DESC) AS global_salary_rank,
                    LAG(salary) OVER (ORDER BY id ASC) AS prev_id_salary
             FROM employees
             WHERE dept = 'Engineering'
             ORDER BY id",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    assert_eq!(
        col_names(&res),
        vec![
            "id",
            "dept",
            "salary",
            "dept_rn",
            "global_salary_rank",
            "prev_id_salary"
        ]
    );
}

#[test]
fn test_window_expression_arguments() {
    let ctx = TestContext::new("expr_args");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT id,
                    LAG(salary IS NOT NULL) OVER (ORDER BY id) AS prev_sal_not_null,
                    LEAD(dept = 'Engineering') OVER (ORDER BY id) AS next_is_eng
             FROM employees
             WHERE id <= 3
             ORDER BY id",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    // Row 1 (id 1, Alice, salary 1000.0, Engineering):
    // prev_sal_not_null = NULL (no row 0)
    // next_is_eng = true (row 2 Bob is Engineering)
    assert_eq!(res.rows[0].values[1], Value::Null);
    assert_eq!(res.rows[0].values[2], Value::Boolean(true));

    // Row 2 (id 2, Bob, salary 1200.0, Engineering):
    // prev_sal_not_null = true (row 1 Alice has non-null salary)
    // next_is_eng = true (row 3 Charlie is Engineering)
    assert_eq!(res.rows[1].values[1], Value::Boolean(true));
    assert_eq!(res.rows[1].values[2], Value::Boolean(true));
}

#[test]
fn test_window_with_cte() {
    let ctx = TestContext::new("with_cte");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "WITH ranked AS (
                SELECT id, name, dept, salary,
                       ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS rn
                FROM employees
             )
             SELECT id, name, dept, salary, rn
             FROM ranked
             WHERE rn <= 1
             ORDER BY dept, id",
        )
        .unwrap();

    // Top 1 earner in each department:
    // Engineering: Bob (1200)
    // HR: Ivan (NULL)
    // Sales: Eve (950, id 5)
    // NULL dept: Heidi (700)
    assert_eq!(res.rows.len(), 4);
}

#[test]
fn test_window_with_joins() {
    let ctx = TestContext::new("with_joins");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT e.id, e.name, d.name AS dept_name, d.location,
                    ROW_NUMBER() OVER (PARTITION BY d.name ORDER BY e.salary DESC) AS loc_rn
             FROM employees e
             JOIN departments d ON e.dept = d.name
             ORDER BY dept_name, loc_rn",
        )
        .unwrap();

    assert_eq!(
        col_names(&res),
        vec!["id", "name", "dept_name", "location", "loc_rn"]
    );
    // 7 employees belong to matching departments (Grace & Heidi have NULL dept)
    assert_eq!(res.rows.len(), 7);
}

#[test]
fn test_window_with_aggregation() {
    let ctx = TestContext::new("with_aggregation");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let res = txn
        .execute_sql(
            "SELECT dept, COUNT(*) AS emp_cnt,
                    RANK() OVER (ORDER BY COUNT(*) DESC) AS dept_rank
             FROM employees
             WHERE dept IS NOT NULL
             GROUP BY dept
             ORDER BY dept_rank, dept",
        )
        .unwrap();

    // Engineering: 3 emps -> rank 1
    // Sales: 3 emps -> rank 1
    // HR: 1 emp -> rank 3
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[1], Value::UInt64(3));
    assert_eq!(res.rows[0].values[2], Value::UInt64(1));
    assert_eq!(res.rows[1].values[1], Value::UInt64(3));
    assert_eq!(res.rows[1].values[2], Value::UInt64(1));
    assert_eq!(res.rows[2].values[1], Value::UInt64(1));
    assert_eq!(res.rows[2].values[2], Value::UInt64(3));
}

#[test]
fn test_window_with_query_order_by_limit_offset() {
    let ctx = TestContext::new("order_limit_offset");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Window numbers by salary DESC, but final query sorts by id ASC with LIMIT 3 OFFSET 1
    let res = txn
        .execute_sql(
            "SELECT id, salary, ROW_NUMBER() OVER (ORDER BY salary DESC) AS sal_rn
             FROM employees
             WHERE id <= 5
             ORDER BY id ASC
             LIMIT 3 OFFSET 1",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::UInt64(2)); // Bob
    assert_eq!(res.rows[0].values[2], Value::UInt64(1));
    assert_eq!(res.rows[1].values[0], Value::UInt64(3)); // Charlie
    assert_eq!(res.rows[1].values[2], Value::UInt64(3));
    assert_eq!(res.rows[2].values[0], Value::UInt64(4)); // Dave
    assert_eq!(res.rows[2].values[2], Value::UInt64(5));
}

#[test]
fn test_window_transaction_visibility_and_rollback() {
    let ctx = TestContext::new("txn_visibility");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    // Insert new employee in active transaction
    txn.execute_statement(
        "INSERT INTO employees (id, name, dept, salary, age) VALUES (10, 'Jack', 'Engineering', 2000.0, 50)",
    )
    .unwrap();

    let res = txn
        .execute_sql(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS rn
             FROM employees
             WHERE dept = 'Engineering'
             ORDER BY rn",
        )
        .unwrap();

    // Jack has highest salary (2000), so Jack gets rn 1
    assert_eq!(res.rows.len(), 4);
    assert_eq!(res.rows[0].values[0], Value::UInt64(10));
    assert_eq!(res.rows[0].values[1], Value::UInt64(1));

    txn.abort().unwrap();

    // Verify rollback
    let mut txn2 = db.begin().unwrap();
    let res2 = txn2
        .execute_sql(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS rn
             FROM employees
             WHERE dept = 'Engineering'
             ORDER BY rn",
        )
        .unwrap();
    assert_eq!(res2.rows.len(), 3);
    assert_eq!(res2.rows[0].values[0], Value::UInt64(2)); // Bob
}

#[test]
fn test_window_read_only_and_persistence() {
    let ctx = TestContext::new("read_only_persistence");
    {
        let mut db = ctx.db();
        setup_employees_db(&mut db);

        let mut txn = db.begin().unwrap();
        let res = txn
            .execute_sql("SELECT id, ROW_NUMBER() OVER (ORDER BY salary DESC) AS rn FROM employees")
            .unwrap();
        assert_eq!(res.rows.len(), 9);
        txn.commit().unwrap();
    }

    // Reopen DB and verify recovery / identical query results
    {
        let mut db = ctx.db();
        let mut txn = db.begin().unwrap();
        let res = txn
            .execute_sql("SELECT id, ROW_NUMBER() OVER (ORDER BY salary DESC) AS rn FROM employees")
            .unwrap();
        assert_eq!(res.rows.len(), 9);
    }
}

#[test]
fn test_window_edge_cases() {
    let ctx = TestContext::new("edge_cases");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("t_edge", schema).unwrap();

    // 1. Empty table
    let res_empty = txn
        .execute_sql(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY val), LAG(val) OVER (ORDER BY id) FROM t_edge",
        )
        .unwrap();
    assert_eq!(res_empty.rows.len(), 0);

    // 2. Single row
    txn.execute_statement("INSERT INTO t_edge (id, val) VALUES (1, 100)")
        .unwrap();
    let res_single = txn
        .execute_sql(
            "SELECT id, ROW_NUMBER() OVER (), LAG(val, 5, -99) OVER (ORDER BY id), FIRST_VALUE(val) OVER (ORDER BY id), LAST_VALUE(val) OVER (ORDER BY id) FROM t_edge",
        )
        .unwrap();
    assert_eq!(res_single.rows.len(), 1);
    assert_eq!(res_single.rows[0].values[1], Value::UInt64(1)); // rn
    assert_eq!(res_single.rows[0].values[2], Value::Int64(-99)); // lag with offset > len
    assert_eq!(res_single.rows[0].values[3], Value::Int64(100)); // first_value
    assert_eq!(res_single.rows[0].values[4], Value::Int64(100)); // last_value

    // 3. Offset zero
    let res_offset_zero = txn
        .execute_sql("SELECT id, LAG(val, 0) OVER (ORDER BY id) FROM t_edge")
        .unwrap();
    assert_eq!(res_offset_zero.rows[0].values[1], Value::Int64(100));
}

#[test]
fn test_window_error_rejections() {
    let ctx = TestContext::new("error_rejections");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();

    // 1. Wrong argument counts
    assert!(txn
        .execute_sql("SELECT ROW_NUMBER(1) OVER () FROM employees")
        .is_err());
    assert!(txn
        .execute_sql("SELECT RANK(1) OVER () FROM employees")
        .is_err());
    assert!(txn
        .execute_sql("SELECT FIRST_VALUE() OVER () FROM employees")
        .is_err());
    assert!(txn
        .execute_sql("SELECT FIRST_VALUE(salary, 2) OVER () FROM employees")
        .is_err());

    // 2. Invalid LAG/LEAD offsets
    assert!(txn
        .execute_sql("SELECT LAG(salary, -1) OVER (ORDER BY id) FROM employees")
        .is_err());
    assert!(txn
        .execute_sql("SELECT LAG(salary, NULL) OVER (ORDER BY id) FROM employees")
        .is_err());
    assert!(txn
        .execute_sql("SELECT LAG(salary, 'abc') OVER (ORDER BY id) FROM employees")
        .is_err());

    // 3. Window in WHERE clause
    let err_where =
        txn.execute_sql("SELECT id FROM employees WHERE ROW_NUMBER() OVER (ORDER BY id) = 1");
    assert!(matches!(
        err_where,
        Err(Error::InvalidWindowPlacement(msg)) if msg.contains("WHERE")
    ));

    // 4. Window in GROUP BY
    let err_gb =
        txn.execute_sql("SELECT id FROM employees GROUP BY ROW_NUMBER() OVER (ORDER BY id)");
    assert!(matches!(
        err_gb,
        Err(Error::InvalidWindowPlacement(msg)) if msg.contains("GROUP BY")
    ));

    // 5. Window in HAVING
    let err_having = txn.execute_sql(
        "SELECT dept, COUNT(*) FROM employees GROUP BY dept HAVING ROW_NUMBER() OVER (ORDER BY dept) > 1",
    );
    assert!(matches!(
        err_having,
        Err(Error::InvalidWindowPlacement(msg)) if msg.contains("HAVING")
    ));

    // 6. Nested window functions
    let err_nested = txn.execute_sql(
        "SELECT ROW_NUMBER() OVER (ORDER BY RANK() OVER (ORDER BY salary)) FROM employees",
    );
    assert!(matches!(err_nested, Err(Error::NestedWindowFunction(_))));

    // 7. Invalid frames: FOLLOWING before PRECEDING
    assert!(txn
        .execute_sql(
            "SELECT LAST_VALUE(salary) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING) FROM employees"
        )
        .is_err());
    assert!(txn
        .execute_sql(
            "SELECT LAST_VALUE(salary) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM employees"
        )
        .is_err());
}

#[test]
fn test_window_explain() {
    let ctx = TestContext::new("explain");
    let mut db = ctx.db();
    setup_employees_db(&mut db);

    let mut txn = db.begin().unwrap();
    let explain_text = txn
        .explain_sql(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) AS rn FROM employees",
        )
        .unwrap();

    assert!(explain_text.contains("Window"));
    assert!(explain_text.contains("ROW_NUMBER()"));
    assert!(explain_text.contains("Partition By:"));
    assert!(explain_text.contains("dept"));
    assert!(explain_text.contains("Order By:"));
    assert!(explain_text.contains("salary Descending"));
    assert!(explain_text.contains("Frame:"));
    assert!(explain_text.contains("UNBOUNDED PRECEDING -> CURRENT ROW"));
}

// ---------------------------------------------------------------------------
// Randomized Reference Oracle Test
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SampleRow {
    id: u64,
    dept: Option<String>,
    score: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OracleRowResult {
    row_number: u64,
    rank: u64,
    dense_rank: u64,
    lag: Option<i64>,
    lead: Option<i64>,
    first_value: Option<i64>,
    last_value: Option<i64>,
}

#[test]
fn test_randomized_window_oracle() {
    let ctx = TestContext::new("randomized_oracle");
    let mut db = ctx.db();
    let mut txn = db.begin().unwrap();

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("dept", DataType::String, true),
            Column::new("score", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("t_oracle", schema).unwrap();

    // Generate pseudo-random rows with duplicate departments, duplicate scores, NULL depts, NULL scores
    let depts = [Some("Alpha"), Some("Beta"), None, Some("Gamma")];
    let scores = [Some(10), Some(20), Some(20), None, Some(30), Some(10)];

    let mut dataset = Vec::new();
    let mut lcg_state = 123456789u64;
    let mut next_rand = || {
        lcg_state = lcg_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        lcg_state
    };

    for id in 1..=40 {
        let dept = depts[(next_rand() as usize) % depts.len()].map(|s| s.to_string());
        let score = scores[(next_rand() as usize) % scores.len()];
        dataset.push(SampleRow {
            id,
            dept: dept.clone(),
            score,
        });

        let dept_str = match &dept {
            Some(d) => format!("'{d}'"),
            None => "NULL".to_string(),
        };
        let score_str = match score {
            Some(s) => s.to_string(),
            None => "NULL".to_string(),
        };
        txn.execute_statement(&format!(
            "INSERT INTO t_oracle (id, dept, score) VALUES ({id}, {dept_str}, {score_str})"
        ))
        .unwrap();
    }

    // Build Oracle calculation for ROW_NUMBER, RANK, DENSE_RANK, LAG, LEAD, FIRST_VALUE, LAST_VALUE
    // Window: PARTITION BY dept ORDER BY score DESC
    // Frame for LAST_VALUE: ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING

    // Group rows by dept (where NULL == NULL)
    let mut partitions: HashMap<Option<String>, Vec<SampleRow>> = HashMap::new();
    for row in &dataset {
        partitions
            .entry(row.dept.clone())
            .or_default()
            .push(row.clone());
    }

    // For each partition, sort by score DESC, then tie-break by original id (insertion index)
    let mut oracle_results: HashMap<u64, OracleRowResult> = HashMap::new();

    for (_, mut part_rows) in partitions {
        part_rows.sort_by(|a, b| {
            // SQL NULLs ordering for DESC: NULL is last
            let cmp = match (a.score, b.score) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(sa), Some(sb)) => sa.cmp(&sb),
            };
            let desc_cmp = cmp.reverse();
            if desc_cmp != std::cmp::Ordering::Equal {
                desc_cmp
            } else {
                a.id.cmp(&b.id)
            }
        });

        let len = part_rows.len();
        let mut current_rank = 1u64;
        let mut current_dense_rank = 1u64;

        for i in 0..len {
            let row = &part_rows[i];
            let row_number = (i + 1) as u64;

            if i > 0 {
                let prev_row = &part_rows[i - 1];
                if row.score != prev_row.score {
                    current_rank = (i + 1) as u64;
                    current_dense_rank += 1;
                }
            }

            let lag_val = if i > 0 { part_rows[i - 1].score } else { None };
            let lead_val = if i + 1 < len {
                part_rows[i + 1].score
            } else {
                None
            };
            let first_val = part_rows[0].score;
            let last_val = part_rows[len - 1].score;

            oracle_results.insert(
                row.id,
                OracleRowResult {
                    row_number,
                    rank: current_rank,
                    dense_rank: current_dense_rank,
                    lag: lag_val,
                    lead: lead_val,
                    first_value: first_val,
                    last_value: last_val,
                },
            );
        }
    }

    // Execute SQL query against TreeDB
    let res = txn
        .execute_sql(
            "SELECT id,
                    ROW_NUMBER() OVER (PARTITION BY dept ORDER BY score DESC) AS rn,
                    RANK() OVER (PARTITION BY dept ORDER BY score DESC) AS rnk,
                    DENSE_RANK() OVER (PARTITION BY dept ORDER BY score DESC) AS drnk,
                    LAG(score) OVER (PARTITION BY dept ORDER BY score DESC) AS lag_score,
                    LEAD(score) OVER (PARTITION BY dept ORDER BY score DESC) AS lead_score,
                    FIRST_VALUE(score) OVER (PARTITION BY dept ORDER BY score DESC) AS fv,
                    LAST_VALUE(score) OVER (
                        PARTITION BY dept ORDER BY score DESC
                        ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING
                    ) AS lv
             FROM t_oracle
             ORDER BY id",
        )
        .unwrap();

    assert_eq!(res.rows.len(), 40);

    for row in res.rows {
        let id = match row.values[0] {
            Value::UInt64(id) => id,
            _ => panic!("Expected UInt64 id"),
        };
        let expected = oracle_results.get(&id).unwrap();

        assert_eq!(
            row.values[1],
            Value::UInt64(expected.row_number),
            "mismatch in ROW_NUMBER for id {id}"
        );
        assert_eq!(
            row.values[2],
            Value::UInt64(expected.rank),
            "mismatch in RANK for id {id}"
        );
        assert_eq!(
            row.values[3],
            Value::UInt64(expected.dense_rank),
            "mismatch in DENSE_RANK for id {id}"
        );

        let actual_lag = match &row.values[4] {
            Value::Null => None,
            Value::Int64(v) => Some(*v),
            _ => panic!("Invalid lag value"),
        };
        assert_eq!(actual_lag, expected.lag, "mismatch in LAG for id {id}");

        let actual_lead = match &row.values[5] {
            Value::Null => None,
            Value::Int64(v) => Some(*v),
            _ => panic!("Invalid lead value"),
        };
        assert_eq!(actual_lead, expected.lead, "mismatch in LEAD for id {id}");

        let actual_fv = match &row.values[6] {
            Value::Null => None,
            Value::Int64(v) => Some(*v),
            _ => panic!("Invalid FIRST_VALUE for id {id}"),
        };
        assert_eq!(
            actual_fv, expected.first_value,
            "mismatch in FIRST_VALUE for id {id}"
        );

        let actual_lv = match &row.values[7] {
            Value::Null => None,
            Value::Int64(v) => Some(*v),
            _ => panic!("Invalid LAST_VALUE for id {id}"),
        };
        assert_eq!(
            actual_lv, expected.last_value,
            "mismatch in LAST_VALUE for id {id}"
        );
    }
}
