//! Example 04: Query Optimizer, EXPLAIN, and Execution Telemetry
//!
//! Demonstrates SQL statistics collection via `ANALYZE`, cost-based optimizer
//! access-path selection, query plan introspection using `EXPLAIN` and
//! `EXPLAIN ANALYZE`, and programmatic execution metrics inspection.
//!
//! Run with:
//! ```bash
//! cargo run --example 04_query_optimizer_and_metrics
//! ```

use tempfile::tempdir;
use treedb::{Result, TreeDb};

fn main() -> Result<()> {
    println!("=== TreeDB Example 04: Query Optimizer & Execution Metrics ===");

    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("optimizer_demo.treedb");

    let mut db = TreeDb::create(&db_path, 128)?;

    // 1. Setup Table and Secondary Index
    println!("\n--- 1. Setting up Schema & Secondary Index ---");
    db.execute_statement(
        "CREATE TABLE employees (
            id INT PRIMARY KEY,
            name TEXT,
            dept TEXT,
            salary INT,
            rating INT
        );",
    )?;

    db.execute_statement("CREATE INDEX idx_emp_dept ON employees (dept);")?;
    println!("Created table 'employees' and index 'idx_emp_dept' on (dept).");

    // 2. Populate Records
    println!("\n--- 2. Populating Sample Records ---");
    db.execute_statement(
        "INSERT INTO employees VALUES
            (1,  'Alice',   'Engineering', 120000, 5),
            (2,  'Bob',     'Engineering',  95000, 4),
            (3,  'Carol',   'Engineering', 140000, 5),
            (4,  'David',   'Product',     110000, 4),
            (5,  'Eve',     'Product',     130000, 5),
            (6,  'Frank',   'Sales',        85000, 3),
            (7,  'Grace',   'Sales',        90000, 4),
            (8,  'Heidi',   'HR',           75000, 4),
            (9,  'Ivan',    'Legal',       150000, 5),
            (10, 'Judy',    'Engineering', 105000, 4);",
    )?;
    println!("Inserted 10 employee records.");

    // 3. Collect Statistics via ANALYZE
    println!("\n--- 3. Collecting Statistics via ANALYZE ---");
    db.execute_statement("ANALYZE employees;")?;
    println!("Table and column histograms collected for cost-based optimization.");

    // 4. Query Plan Introspection with EXPLAIN
    println!("\n--- 4. Query Introspection with EXPLAIN ---");
    let explain_sql = "EXPLAIN SELECT name, salary FROM employees WHERE dept = 'Engineering' AND salary > 100000 ORDER BY salary DESC;";
    println!("SQL: {explain_sql}");
    let explain_res = db.execute_sql(explain_sql)?;
    println!("Physical Plan Chosen by Memo Optimizer:");
    for row in explain_res.rows() {
        if let Some(treedb::Value::String(plan_line)) = row.get(0) {
            println!("  {plan_line}");
        }
    }

    // 5. Runtime Execution Diagnostics with EXPLAIN ANALYZE
    println!("\n--- 5. Runtime Telemetry with EXPLAIN ANALYZE ---");
    let analyze_sql = "EXPLAIN ANALYZE SELECT name, salary FROM employees WHERE dept = 'Engineering' AND salary > 100000 ORDER BY salary DESC;";
    println!("SQL: {analyze_sql}");
    let analyze_res = db.execute_sql(analyze_sql)?;
    println!("Execution Telemetry (Estimated vs Actual Cardinality):");
    for row in analyze_res.rows() {
        if let Some(treedb::Value::String(diag_line)) = row.get(0) {
            println!("  {diag_line}");
        }
    }

    // 6. Programmatic Query Metrics Inspection
    println!("\n--- 6. Programmatic Query Execution Metrics API ---");
    let query_sql =
        "SELECT dept, COUNT(*), AVG(salary) FROM employees GROUP BY dept HAVING COUNT(*) >= 2;";
    let (res, metrics) = db.execute_sql_with_metrics(query_sql)?;
    println!("Query Output ({} rows):", res.len());
    for row in res.rows() {
        println!(
            "  Dept: {:<12} | Headcount: {} | Avg Salary: ${}",
            row.values[0], row.values[1], row.values[2]
        );
    }

    println!("\nTelemetry Summary:");
    println!("  Execution Elapsed Time: {:?}", metrics.total_time);
    println!("  Total Rows Returned:    {}", metrics.total_rows);
    println!("  Total Rows Scanned:     {}", metrics.total_rows_scanned);
    println!("  Plan Cache Hit:         {}", metrics.plan_cache_hit);
    if let Some(ref root_op) = metrics.root_operator {
        println!("  Root Operator Kind:     {:?}", root_op.kind);
        println!("  Actual Rows:            {}", root_op.actual_rows);
        if let Some(est) = root_op.estimated_rows {
            println!("  Estimated Rows:         {est:.2}");
        }
    }

    db.sync()?;
    println!("\nExample 04 completed successfully!");
    Ok(())
}
