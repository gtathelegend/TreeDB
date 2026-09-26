use std::path::PathBuf;

use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::query::plan_cache::PlanCacheStats;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "test_pc_{}_{}_{}.treedb",
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

fn setup_test_schema(db: &mut TreeDb) -> Result<()> {
    let mut txn = db.begin_transaction()?;
    txn.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT, age BIGINT, dept_id BIGINT);",
    )?;
    txn.execute_statement(
        "CREATE TABLE departments (dept_id BIGINT PRIMARY KEY, name TEXT, budget DOUBLE);",
    )?;

    txn.execute_statement(
        "INSERT INTO users (id, name, age, dept_id) VALUES \
         (1, 'Alice', 25, 10), \
         (2, 'Bob', 30, 20), \
         (3, 'Charlie', 35, 10), \
         (4, 'Diana', 40, 20), \
         (5, 'Eve', 28, 30);",
    )?;

    txn.execute_statement(
        "INSERT INTO departments (dept_id, name, budget) VALUES \
         (10, 'Engineering', 100000.0), \
         (20, 'Marketing', 50000.0), \
         (30, 'Sales', 75000.0);",
    )?;

    txn.commit()?;
    Ok(())
}

#[test]
fn test_plan_cache_basic_miss_and_hit() -> Result<()> {
    let ctx = TestContext::new("basic_hit_miss");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name, age FROM users WHERE id = ?;")?;

    assert_eq!(
        txn.plan_cache_stats(),
        PlanCacheStats {
            hits: 0,
            misses: 0,
            evictions: 0,
            entries: 0,
            capacity: 128,
        }
    );

    // First execution: cache miss
    let (res1, metrics1) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(1)])?;
    assert!(!metrics1.plan_cache_hit);
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[1], Value::String("Alice".into()));

    assert_eq!(
        txn.plan_cache_stats(),
        PlanCacheStats {
            hits: 0,
            misses: 1,
            evictions: 0,
            entries: 1,
            capacity: 128,
        }
    );

    // Second execution with identical parameter: cache hit
    let (res2, metrics2) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(1)])?;
    assert!(metrics2.plan_cache_hit);
    assert_eq!(res2.rows.len(), 1);
    assert_eq!(res2.rows[0].values[1], Value::String("Alice".into()));

    assert_eq!(
        txn.plan_cache_stats(),
        PlanCacheStats {
            hits: 1,
            misses: 1,
            evictions: 0,
            entries: 1,
            capacity: 128,
        }
    );

    // Third execution via execute_prepared: cache hit
    let res3 = txn
        .execute_prepared(&stmt, &[Value::Int64(1)])?
        .into_query()?;
    assert_eq!(res3.rows.len(), 1);
    assert_eq!(
        txn.plan_cache_stats(),
        PlanCacheStats {
            hits: 2,
            misses: 1,
            evictions: 0,
            entries: 1,
            capacity: 128,
        }
    );

    txn.commit()?;
    Ok(())
}

#[test]
fn test_plan_cache_multiple_prepared_statements() -> Result<()> {
    let ctx = TestContext::new("multi_stmt");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt1 = txn.prepare_sql("SELECT name FROM users WHERE id = ?;")?;
    let stmt2 = txn.prepare_sql("SELECT name, budget FROM departments WHERE dept_id = ?;")?;

    // Execute stmt1: miss
    let _ = txn.execute_prepared(&stmt1, &[Value::Int64(1)])?;
    assert_eq!(txn.plan_cache_stats().misses, 1);
    assert_eq!(txn.plan_cache_stats().entries, 1);

    // Execute stmt2: miss
    let _ = txn.execute_prepared(&stmt2, &[Value::Int64(10)])?;
    assert_eq!(txn.plan_cache_stats().misses, 2);
    assert_eq!(txn.plan_cache_stats().entries, 2);

    // Execute stmt1 again: hit
    let _ = txn.execute_prepared(&stmt1, &[Value::Int64(1)])?;
    assert_eq!(txn.plan_cache_stats().hits, 1);

    // Execute stmt2 again: hit
    let _ = txn.execute_prepared(&stmt2, &[Value::Int64(10)])?;
    assert_eq!(txn.plan_cache_stats().hits, 2);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_plan_cache_bounded_capacity_and_lru_eviction() -> Result<()> {
    let ctx = TestContext::new("bounded_lru");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // Set small cache capacity of 3 entries
    db.set_plan_cache_capacity(3);

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name FROM users WHERE id = ?;")?;

    // Execute with param 1: miss (entry 1 added)
    let _ = txn.execute_prepared(&stmt, &[Value::Int64(1)])?;
    // Execute with param 2: miss (entry 2 added)
    let _ = txn.execute_prepared(&stmt, &[Value::Int64(2)])?;
    // Execute with param 3: miss (entry 3 added)
    let _ = txn.execute_prepared(&stmt, &[Value::Int64(3)])?;

    assert_eq!(txn.plan_cache_stats().entries, 3);
    assert_eq!(txn.plan_cache_stats().evictions, 0);
    assert_eq!(txn.plan_cache_stats().misses, 3);

    // Touch param 1 so LRU order becomes: 2 (oldest), 3, 1 (newest)
    let (res1, m1) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(1)])?;
    assert!(m1.plan_cache_hit);
    assert_eq!(res1.rows[0].values[0], Value::Int64(1));

    // Execute with param 4: should cause eviction of param 2
    let (res4, m4) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(4)])?;
    assert!(!m4.plan_cache_hit);
    assert_eq!(res4.rows[0].values[0], Value::Int64(4));
    assert_eq!(txn.plan_cache_stats().evictions, 1);
    assert_eq!(txn.plan_cache_stats().entries, 3);

    // Param 1 should still be in cache: hit!
    let (_, m1_check) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(1)])?;
    assert!(m1_check.plan_cache_hit);

    // Param 2 was evicted: miss!
    let (_, m2_check) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(2)])?;
    assert!(!m2_check.plan_cache_hit);
    assert_eq!(txn.plan_cache_stats().evictions, 2);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_plan_cache_parameter_sensitivity() -> Result<()> {
    let ctx = TestContext::new("param_sensitivity");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name FROM users WHERE id >= ? AND id <= ?;")?;

    // Execution A: range [1, 2]
    let (res_a, ma1) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(1), Value::Int64(2)])?;
    assert!(!ma1.plan_cache_hit);
    assert_eq!(res_a.rows.len(), 2);

    // Execution A again: hit
    let (res_a2, ma2) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(1), Value::Int64(2)])?;
    assert!(ma2.plan_cache_hit);
    assert_eq!(res_a2.rows, res_a.rows);

    // Execution B: range [3, 5] -> distinct parameter values, cache miss
    let (res_b, mb1) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(3), Value::Int64(5)])?;
    assert!(!mb1.plan_cache_hit);
    assert_eq!(res_b.rows.len(), 3);

    // Execution B again: hit
    let (res_b2, mb2) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(3), Value::Int64(5)])?;
    assert!(mb2.plan_cache_hit);
    assert_eq!(res_b2.rows, res_b.rows);

    // Null parameter in comparison: produces 0 rows (SQL 3VL)
    let (res_null, m_null) =
        txn.execute_prepared_with_metrics(&stmt, &[Value::Null, Value::Int64(5)])?;
    assert!(!m_null.plan_cache_hit);
    assert_eq!(res_null.rows.len(), 0);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_plan_cache_schema_invalidation_table() -> Result<()> {
    let ctx = TestContext::new("schema_invalidation_table");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name FROM users WHERE id = ?;")?;

    // Fill cache
    let _ = txn.execute_prepared(&stmt, &[Value::Int64(1)])?;
    assert_eq!(txn.plan_cache_stats().entries, 1);
    txn.commit()?;

    // Create a new table
    let mut txn2 = db.begin_transaction()?;
    txn2.execute_statement("CREATE TABLE new_tbl (nid BIGINT PRIMARY KEY, val TEXT);")?;
    txn2.commit()?;

    // Verify cache was invalidated
    assert_eq!(db.plan_cache_stats().entries, 0);

    // Re-execute prepared statement on db: should be a cache miss
    let mut txn3 = db.begin_transaction()?;
    let (res, m) = txn3.execute_prepared_with_metrics(&stmt, &[Value::Int64(1)])?;
    assert!(!m.plan_cache_hit);
    assert_eq!(res.rows.len(), 1);
    txn3.commit()?;

    // Drop table
    let mut txn4 = db.begin_transaction()?;
    txn4.execute_statement("DROP TABLE new_tbl;")?;
    txn4.commit()?;

    assert_eq!(db.plan_cache_stats().entries, 0);

    Ok(())
}

#[test]
fn test_plan_cache_index_lifecycle_invalidation() -> Result<()> {
    let ctx = TestContext::new("index_lifecycle");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT id, name, age FROM users WHERE age = ?;")?;

    // Execution 1: FullTableScan (no index on age)
    let (res1, m1) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(30)])?;
    assert!(!m1.plan_cache_hit);
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(res1.rows[0].values[1], Value::String("Bob".into()));

    // Execution 2: Cache hit (still FullTableScan)
    let (_, m2) = txn.execute_prepared_with_metrics(&stmt, &[Value::Int64(30)])?;
    assert!(m2.plan_cache_hit);
    txn.commit()?;

    // Create secondary index on age
    let mut txn_idx = db.begin_transaction()?;
    txn_idx.execute_statement("CREATE INDEX idx_users_age ON users (age);")?;
    txn_idx.commit()?;

    // Cache should have been invalidated
    assert_eq!(db.plan_cache_stats().entries, 0);

    // Execution 3: Replan should now pick SecondaryIndexScan!
    let mut txn_query = db.begin_transaction()?;
    let plan_str = txn_query.explain_prepared(&stmt, &[Value::Int64(30)])?;
    assert!(
        plan_str.contains("SecondaryIndexScan"),
        "expected SecondaryIndexScan after index creation, got: {plan_str}"
    );

    let (res3, m3) = txn_query.execute_prepared_with_metrics(&stmt, &[Value::Int64(30)])?;
    assert!(!m3.plan_cache_hit);
    assert_eq!(res3.rows.len(), 1);
    assert_eq!(res3.rows[0].values[1], Value::String("Bob".into()));

    // Execution 4: Cache hit on SecondaryIndexScan
    let (res4, m4) = txn_query.execute_prepared_with_metrics(&stmt, &[Value::Int64(30)])?;
    assert!(m4.plan_cache_hit);
    assert_eq!(res4.rows, res3.rows);
    txn_query.commit()?;

    // Drop secondary index
    let mut txn_drop = db.begin_transaction()?;
    txn_drop.execute_statement("DROP INDEX idx_users_age;")?;
    txn_drop.commit()?;

    // Cache invalidated again
    assert_eq!(db.plan_cache_stats().entries, 0);

    // Execution 5: Replan falls back safely to FullTableScan
    let mut txn_after_drop = db.begin_transaction()?;
    let plan_after_drop = txn_after_drop.explain_prepared(&stmt, &[Value::Int64(30)])?;
    assert!(
        plan_after_drop.contains("FullTableScan"),
        "expected FullTableScan after drop index, got: {plan_after_drop}"
    );

    let (res5, m5) = txn_after_drop.execute_prepared_with_metrics(&stmt, &[Value::Int64(30)])?;
    assert!(!m5.plan_cache_hit);
    assert_eq!(res5.rows.len(), 1);
    assert_eq!(res5.rows[0].values[1], Value::String("Bob".into()));
    txn_after_drop.commit()?;

    Ok(())
}

#[test]
fn test_plan_cache_statistics_invalidation() -> Result<()> {
    let ctx = TestContext::new("stats_invalidation");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;
    let stmt = txn.prepare_sql("SELECT * FROM users WHERE dept_id = ?;")?;

    // Fill cache
    let _ = txn.execute_prepared(&stmt, &[Value::Int64(10)])?;
    assert_eq!(txn.plan_cache_stats().entries, 1);
    txn.commit()?;

    // Run ANALYZE on users table
    let mut txn_analyze = db.begin_transaction()?;
    txn_analyze.execute_statement("ANALYZE users;")?;
    txn_analyze.commit()?;

    // Statistics update must invalidate cached plans referencing users
    assert_eq!(db.plan_cache_stats().entries, 0);

    let mut txn2 = db.begin_transaction()?;
    let (res, m) = txn2.execute_prepared_with_metrics(&stmt, &[Value::Int64(10)])?;
    assert!(!m.plan_cache_hit);
    assert_eq!(res.rows.len(), 2);
    txn2.commit()?;

    Ok(())
}

#[test]
fn test_plan_cache_transaction_isolation_uncommitted_ddl() -> Result<()> {
    let ctx = TestContext::new("txn_isolation_ddl");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    // 1. Transaction A creates an uncommitted index then aborts
    let mut txn_a = db.begin_transaction()?;
    txn_a.execute_statement("CREATE INDEX idx_users_temp ON users (name);")?;
    let stmt = txn_a.prepare_sql("SELECT id FROM users WHERE name = ?;")?;

    // In transaction A, execute with uncommitted index
    let res_a = txn_a.execute_prepared(&stmt, &[Value::String("Alice".into())])?;
    assert_eq!(res_a.rows_affected(), 1);

    // Abort transaction A
    txn_a.abort()?;

    // 2. Transaction B must NOT use plan with uncommitted (now aborted) index
    let mut txn_b = db.begin_transaction()?;
    let stmt_b = txn_b.prepare_sql("SELECT id FROM users WHERE name = ?;")?;
    let plan_b = txn_b.explain_prepared(&stmt_b, &[Value::String("Alice".into())])?;
    assert!(
        !plan_b.contains("idx_users_temp"),
        "aborted index must not be present in plan: {plan_b}"
    );

    let (res_b, m_b) =
        txn_b.execute_prepared_with_metrics(&stmt_b, &[Value::String("Alice".into())])?;
    assert!(!m_b.plan_cache_hit);
    assert_eq!(res_b.rows.len(), 1);
    txn_b.commit()?;

    Ok(())
}

#[test]
fn test_plan_cache_advanced_queries_join_cte_window_set() -> Result<()> {
    let ctx = TestContext::new("advanced_queries");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut txn = db.begin_transaction()?;

    // 1. JOIN query
    let join_stmt = txn.prepare_sql(
        "SELECT u.name, d.name \
         FROM users u \
         INNER JOIN departments d ON u.dept_id = d.dept_id \
         WHERE u.age >= ? \
         ORDER BY u.id ASC;",
    )?;

    let (join_res1, jm1) = txn.execute_prepared_with_metrics(&join_stmt, &[Value::Int64(30)])?;
    assert!(!jm1.plan_cache_hit);
    assert_eq!(join_res1.rows.len(), 3);

    let (join_res2, jm2) = txn.execute_prepared_with_metrics(&join_stmt, &[Value::Int64(30)])?;
    assert!(jm2.plan_cache_hit);
    assert_eq!(join_res2.rows, join_res1.rows);

    // 2. CTE query
    let cte_stmt = txn.prepare_sql(
        "WITH older_users AS (SELECT * FROM users WHERE age > ?) \
         SELECT name FROM older_users WHERE dept_id = ?;",
    )?;

    let (cte_res1, cm1) =
        txn.execute_prepared_with_metrics(&cte_stmt, &[Value::Int64(25), Value::Int64(10)])?;
    assert!(!cm1.plan_cache_hit);
    assert_eq!(cte_res1.rows.len(), 1);
    assert_eq!(cte_res1.rows[0].values[0], Value::String("Charlie".into()));

    let (cte_res2, cm2) =
        txn.execute_prepared_with_metrics(&cte_stmt, &[Value::Int64(25), Value::Int64(10)])?;
    assert!(cm2.plan_cache_hit);
    assert_eq!(cte_res2.rows, cte_res1.rows);

    // 3. Window function query
    let win_stmt = txn.prepare_sql(
        "SELECT id, name, age, ROW_NUMBER() OVER (PARTITION BY dept_id ORDER BY age ASC) as rn \
         FROM users \
         WHERE age >= ?;",
    )?;

    let (win_res1, wm1) = txn.execute_prepared_with_metrics(&win_stmt, &[Value::Int64(20)])?;
    assert!(!wm1.plan_cache_hit);
    assert_eq!(win_res1.rows.len(), 5);

    let (win_res2, wm2) = txn.execute_prepared_with_metrics(&win_stmt, &[Value::Int64(20)])?;
    assert!(wm2.plan_cache_hit);
    assert_eq!(win_res2.rows, win_res1.rows);

    // 4. Set operation query
    let set_stmt = txn.prepare_sql(
        "SELECT id, name FROM users WHERE dept_id = ? \
         UNION ALL \
         SELECT id, name FROM users WHERE age > ?;",
    )?;

    let (set_res1, sm1) =
        txn.execute_prepared_with_metrics(&set_stmt, &[Value::Int64(10), Value::Int64(35)])?;
    assert!(!sm1.plan_cache_hit);

    let (set_res2, sm2) =
        txn.execute_prepared_with_metrics(&set_stmt, &[Value::Int64(10), Value::Int64(35)])?;
    assert!(sm2.plan_cache_hit);
    assert_eq!(set_res2.rows, set_res1.rows);

    txn.commit()?;
    Ok(())
}

#[test]
fn test_plan_cache_differential_oracle_against_uncached() -> Result<()> {
    let ctx = TestContext::new("differential_oracle");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let test_queries = [
        ("SELECT * FROM users WHERE id = ?;", vec![Value::Int64(2)]),
        (
            "SELECT * FROM users WHERE age >= ? AND age <= ?;",
            vec![Value::Int64(25), Value::Int64(35)],
        ),
        (
            "SELECT u.name, d.name FROM users u JOIN departments d ON u.dept_id = d.dept_id WHERE u.dept_id = ?;",
            vec![Value::Int64(20)],
        ),
        (
            "SELECT dept_id, COUNT(*) FROM users WHERE age >= ? GROUP BY dept_id HAVING COUNT(*) >= ?;",
            vec![Value::Int64(20), Value::Int64(2)],
        ),
        (
            "SELECT name FROM users WHERE dept_id IN (SELECT dept_id FROM departments WHERE budget >= ?);",
            vec![Value::Float64(70000.0)],
        ),
    ];

    for (sql, params) in &test_queries {
        let mut txn = db.begin_transaction()?;
        let stmt = txn.prepare_sql(sql)?;

        // Run 1: with cache enabled (miss)
        let res_cached1 = txn.execute_prepared(&stmt, params)?.into_query()?;

        // Run 2: with cache enabled (hit)
        let res_cached2 = txn.execute_prepared(&stmt, params)?.into_query()?;

        // Run 3: with cache explicitly cleared
        txn.clear_plan_cache();
        let res_uncached = txn.execute_prepared(&stmt, params)?.into_query()?;

        assert_eq!(
            res_cached1.rows, res_uncached.rows,
            "mismatch between cached and uncached result for: {sql}"
        );
        assert_eq!(
            res_cached2.rows, res_uncached.rows,
            "mismatch on second cached execution for: {sql}"
        );

        txn.commit()?;
    }

    Ok(())
}

#[test]
fn test_plan_cache_reopen_starts_empty() -> Result<()> {
    let ctx = TestContext::new("reopen_empty");
    {
        let mut db = ctx.db();
        setup_test_schema(&mut db)?;

        let mut txn = db.begin_transaction()?;
        let stmt = txn.prepare_sql("SELECT * FROM users WHERE id = ?;")?;
        let _ = txn.execute_prepared(&stmt, &[Value::Int64(1)])?;
        assert_eq!(txn.plan_cache_stats().entries, 1);
        txn.commit()?;
    }

    // Reopen database
    {
        let db = ctx.db();
        assert_eq!(db.plan_cache_stats().entries, 0);
        assert_eq!(db.plan_cache_stats().hits, 0);
        assert_eq!(db.plan_cache_stats().misses, 0);
    }

    Ok(())
}

#[test]
fn test_randomized_plan_cache_oracle() -> Result<()> {
    let ctx = TestContext::new("random_oracle");
    let mut db = ctx.db();
    setup_test_schema(&mut db)?;

    let mut rng_state: u64 = 0xDEADBEEF_CAFE1234;
    let mut next_rand = || -> u64 {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };

    let sql_templates = [
        "SELECT id, name FROM users WHERE id = ?;",
        "SELECT id, name, age FROM users WHERE age >= ?;",
        "SELECT name, dept_id FROM users WHERE dept_id = ?;",
    ];

    for iter in 0..50 {
        let tmpl_idx = (next_rand() as usize) % sql_templates.len();
        let sql = sql_templates[tmpl_idx];

        let param = match tmpl_idx {
            0 => Value::Int64(((next_rand() % 5) + 1) as i64),
            1 => Value::Int64((20 + (next_rand() % 25)) as i64),
            2 => Value::Int64((10 * ((next_rand() % 3) + 1)) as i64),
            _ => unreachable!(),
        };

        let mut txn = db.begin_transaction()?;
        let stmt = txn.prepare_sql(sql)?;

        // Execute via cache
        let cached_res = txn
            .execute_prepared(&stmt, std::slice::from_ref(&param))?
            .into_query()?;

        // Execute via uncached forced plan
        let uncached_plan = txn.plan_select_with_params(
            match &stmt.statement {
                treedb::sql::Statement::Select(s) => s,
                _ => unreachable!(),
            },
            Some(std::slice::from_ref(&param)),
        )?;
        let uncached_res = txn.execute_physical_plan(&uncached_plan)?;

        assert_eq!(
            cached_res.rows, uncached_res.rows,
            "randomized mismatch at iteration {iter} for {sql} with param {param:?}"
        );

        txn.commit()?;
    }

    Ok(())
}
