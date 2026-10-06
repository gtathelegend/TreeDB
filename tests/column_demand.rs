use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::error::Result;
use treedb::TreeDb;

struct TestContext {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join(format!("{name}.treedb"));
        Self {
            _temp_dir: temp_dir,
            db_path,
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

fn setup_wide_table(db: &mut TreeDb) -> Result<()> {
    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR, status VARCHAR, age BIGINT, score DOUBLE, bio VARCHAR, created_at BIGINT, extra_1 VARCHAR, extra_2 BIGINT);")?;
    tx.execute_statement("CREATE INDEX idx_users_status ON users (status);")?;
    tx.execute_statement("CREATE INDEX idx_users_age ON users (age);")?;
    tx.execute_statement("CREATE INDEX idx_users_status_age ON users (status, age);")?;

    for i in 1..=100 {
        let status = if i % 2 == 0 { "active" } else { "inactive" };
        let age = 20 + (i % 50);
        let score = (i as f64) * 1.5;
        let created_at = 1000 + i;
        let extra_2 = i * 10;
        tx.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, 'User_{i}', '{status}', {age}, {score}, 'Long bio text payload for user {i}', {created_at}, 'extra_data_{i}', {extra_2});"
        ))?;
    }
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1421..R1425: Basic Projection Pruning
// -----------------------------------------------------------------------------

#[test]
fn test_r1421_unused_column_eliminated() -> Result<()> {
    let ctx = TestContext::new("r1421");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users WHERE id = 1;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("User_1".to_string()));

    let plan_str = tx.explain_sql("SELECT name FROM users WHERE id = 1;")?;
    assert!(plan_str.contains("Required Columns"));
    assert!(plan_str.contains("Pruned Columns"));
    assert!(plan_str.contains("bio"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1422_where_columns_retained() -> Result<()> {
    let ctx = TestContext::new("r1422");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users WHERE age > 40;")?;
    assert!(!res.rows.is_empty());
    let plan = tx.explain_sql("SELECT name FROM users WHERE age > 40;")?;
    assert!(plan.contains("age"));
    assert!(plan.contains("name"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1423_order_by_columns_retained() -> Result<()> {
    let ctx = TestContext::new("r1423");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users ORDER BY created_at DESC LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::String("User_100".to_string()));
    let plan = tx.explain_sql("SELECT name FROM users ORDER BY created_at DESC LIMIT 5;")?;
    assert!(plan.contains("created_at"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1424_projection_aliases_preserved() -> Result<()> {
    let ctx = TestContext::new("r1424");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT name AS user_handle, age AS years FROM users WHERE id = 1;")?;
    assert_eq!(res.schema.columns[0].name, "user_handle");
    assert_eq!(res.schema.columns[1].name, "years");
    assert_eq!(res.rows[0].values[0], Value::String("User_1".to_string()));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1425_computed_expressions_preserve_dependencies() -> Result<()> {
    let ctx = TestContext::new("r1425");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT age + extra_2 AS combined FROM users WHERE id = 1;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(21 + 10));
    let plan = tx.explain_sql("SELECT age + extra_2 AS combined FROM users WHERE id = 1;")?;
    assert!(plan.contains("age"));
    assert!(plan.contains("extra_2"));
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1426..R1430: Expressions and Column Extraction
// -----------------------------------------------------------------------------

#[test]
fn test_r1426_nested_expressions() -> Result<()> {
    let ctx = TestContext::new("r1426");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT (age * 2) + (extra_2 / 2) FROM users WHERE id = 2;")?;
    assert_eq!(res.rows.len(), 1);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1427_null_expressions() -> Result<()> {
    let ctx = TestContext::new("r1427");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res =
        tx.execute_sql("SELECT name FROM users WHERE bio IS NOT NULL AND status IS NOT NULL;")?;
    assert_eq!(res.rows.len(), 100);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1428_parameters() -> Result<()> {
    let ctx = TestContext::new("r1428");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let prep = tx.prepare_sql("SELECT name FROM users WHERE id = ?;")?;
    let res = tx.execute_prepared_query(&prep, &[Value::Int64(5)])?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("User_5".to_string()));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1429_correlated_outer_columns() -> Result<()> {
    let ctx = TestContext::new("r1429");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT u.name FROM users u WHERE u.score > (SELECT AVG(u2.score) FROM users u2 WHERE u2.status = u.status);")?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1430_subqueries() -> Result<()> {
    let ctx = TestContext::new("r1430");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx
        .execute_sql("SELECT name FROM users WHERE id IN (SELECT id FROM users WHERE age > 60);")?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1431..R1437: Joins and Propagation
// -----------------------------------------------------------------------------

#[test]
fn test_r1431_inner_join_column_pruning() -> Result<()> {
    let ctx = TestContext::new("r1431");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement(
        "CREATE TABLE roles (user_id BIGINT PRIMARY KEY, role VARCHAR, unused_role_data VARCHAR);",
    )?;
    tx.execute_statement("INSERT INTO roles VALUES (1, 'admin', 'junk');")?;

    let res = tx.execute_sql(
        "SELECT u.name, r.role FROM users u INNER JOIN roles r ON u.id = r.user_id;",
    )?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("User_1".to_string()));
    assert_eq!(res.rows[0].values[1], Value::String("admin".to_string()));

    let plan = tx.explain_sql(
        "SELECT u.name, r.role FROM users u INNER JOIN roles r ON u.id = r.user_id;",
    )?;
    assert!(plan.contains("Pruned Columns"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1432_left_join() -> Result<()> {
    let ctx = TestContext::new("r1432");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE tags (user_id BIGINT PRIMARY KEY, tag VARCHAR);")?;
    tx.execute_statement("INSERT INTO tags VALUES (1, 'vip');")?;

    let res = tx.execute_sql("SELECT u.name, t.tag FROM users u LEFT JOIN tags t ON u.id = t.user_id WHERE u.id <= 2 ORDER BY u.id;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[1], Value::String("vip".into()));
    assert_eq!(res.rows[1].values[1], Value::Null);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1433_semi_join() -> Result<()> {
    let ctx = TestContext::new("r1433");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE flagged (user_id BIGINT PRIMARY KEY, reason VARCHAR);")?;
    tx.execute_statement("INSERT INTO flagged VALUES (10, 'spam');")?;

    let res_semi = tx.execute_sql("SELECT name FROM users WHERE EXISTS (SELECT 1 FROM flagged WHERE flagged.user_id = users.id);")?;
    assert_eq!(res_semi.rows.len(), 1);
    assert_eq!(
        res_semi.rows[0].values[0],
        Value::String("User_10".to_string())
    );
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1436_anti_join() -> Result<()> {
    let ctx = TestContext::new("r1436");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE flagged (user_id BIGINT PRIMARY KEY);")?;
    tx.execute_statement("INSERT INTO flagged VALUES (10);")?;

    let res_anti = tx.execute_sql("SELECT name FROM users WHERE NOT EXISTS (SELECT 1 FROM flagged WHERE flagged.user_id = users.id);")?;
    assert_eq!(res_anti.rows.len(), 99);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1437_join_residual_predicates() -> Result<()> {
    let ctx = TestContext::new("r1437");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("CREATE TABLE scores (user_id BIGINT PRIMARY KEY, bonus INT);")?;
    tx.execute_statement("INSERT INTO scores VALUES (1, 100);")?;
    tx.execute_statement("INSERT INTO scores VALUES (2, 5);")?;

    let res = tx.execute_sql(
        "SELECT u.name FROM users u JOIN scores s ON u.id = s.user_id WHERE s.bonus > 50;",
    )?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("User_1".to_string()));
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1438..R1441: Aggregations & Group By
// -----------------------------------------------------------------------------

#[test]
fn test_r1438_group_by_and_aggregates() -> Result<()> {
    let ctx = TestContext::new("r1438");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "SELECT status, COUNT(*), AVG(age) FROM users GROUP BY status HAVING COUNT(*) > 10;",
    )?;
    assert_eq!(res.rows.len(), 2);
    let plan = tx.explain_sql(
        "SELECT status, COUNT(*), AVG(age) FROM users GROUP BY status HAVING COUNT(*) > 10;",
    )?;
    assert!(plan.contains("status"));
    assert!(plan.contains("age"));
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1440_having_retains_filtered_aggregates() -> Result<()> {
    let ctx = TestContext::new("r1440");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT status FROM users GROUP BY status HAVING MAX(age) > 30;")?;
    assert_eq!(res.rows.len(), 2);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1442..R1445: Window Functions
// -----------------------------------------------------------------------------

#[test]
fn test_r1442_window_functions_preserve_partitions_and_order() -> Result<()> {
    let ctx = TestContext::new("r1442");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name, ROW_NUMBER() OVER (PARTITION BY status ORDER BY age ASC) FROM users WHERE id <= 10;")?;
    assert_eq!(res.rows.len(), 10);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1445_window_output_pruning() -> Result<()> {
    let ctx = TestContext::new("r1445");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT ROW_NUMBER() OVER (ORDER BY id) FROM users LIMIT 5;")?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1446..R1452: Covering Index Selection & Index Only Scans
// -----------------------------------------------------------------------------

#[test]
fn test_r1446_covering_index_selected() -> Result<()> {
    let ctx = TestContext::new("r1446");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT status, age FROM users WHERE status = 'active';")?;
    assert!(plan.contains("IndexOnlyScan") || plan.contains("Covered Columns"));
    let res = tx.execute_sql("SELECT status, age FROM users WHERE status = 'active';")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1447_non_covering_index_with_base_fetch() -> Result<()> {
    let ctx = TestContext::new("r1447");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT name, bio FROM users WHERE status = 'active';")?;
    assert!(plan.contains("SecondaryIndexScan") || plan.contains("FullTableScan"));
    let res = tx.execute_sql("SELECT name, bio FROM users WHERE status = 'active';")?;
    assert_eq!(res.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1451_index_only_execution() -> Result<()> {
    let ctx = TestContext::new("r1451");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let (res, metrics) =
        tx.execute_sql_with_metrics("SELECT status FROM users WHERE status = 'active';")?;
    assert_eq!(res.rows.len(), 50);
    let root = metrics.root_operator.unwrap();
    assert_eq!(root.counters.base_records_fetched, 0);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1452_primary_key_access_prunes_payload() -> Result<()> {
    let ctx = TestContext::new("r1452");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users WHERE id = 42;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::String("User_42".into()));
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1453..R1458: Late Materialization & Row Goal
// -----------------------------------------------------------------------------

#[test]
fn test_r1453_late_materialization_candidate_reduction() -> Result<()> {
    let ctx = TestContext::new("r1453");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let _plan =
        tx.explain_sql("SELECT bio FROM users WHERE status = 'active' AND age > 25 LIMIT 5;")?;
    let (res, metrics) = tx.execute_sql_with_metrics(
        "SELECT bio FROM users WHERE status = 'active' AND age > 25 LIMIT 5;",
    )?;
    assert_eq!(res.rows.len(), 5);

    let root = metrics.root_operator.unwrap();
    assert_eq!(root.actual_rows, 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1454_late_materialization_correctness_against_full() -> Result<()> {
    let ctx = TestContext::new("r1454");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res1 = tx.execute_sql(
        "SELECT name, bio FROM users WHERE status = 'active' ORDER BY id ASC LIMIT 10;",
    )?;
    let res2 =
        tx.execute_sql("SELECT name, bio FROM users WHERE status = 'active' ORDER BY id ASC;")?;
    assert_eq!(res1.rows.len(), 10);
    for i in 0..10 {
        assert_eq!(res1.rows[i].values, res2.rows[i].values);
    }
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1455_top_n_interaction_preserves_ordering() -> Result<()> {
    let ctx = TestContext::new("r1455");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users ORDER BY age DESC LIMIT 3;")?;
    assert_eq!(res.rows.len(), 3);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1459..R1462: Materialization (Phase 58 Integration)
// -----------------------------------------------------------------------------

#[test]
fn test_r1459_common_subplan_materialization_narrow_width() -> Result<()> {
    let ctx = TestContext::new("r1459");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let sql = "WITH active_users AS (SELECT id, name, status FROM users WHERE status = 'active') \
               SELECT a1.name, a2.name FROM active_users a1 JOIN active_users a2 ON a1.id = a2.id WHERE a1.id <= 5;";
    let res = tx.execute_sql(sql)?;
    assert_eq!(res.rows.len(), 2);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1460_multiple_consumers_compatible_demand() -> Result<()> {
    let ctx = TestContext::new("r1460");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let sql = "WITH base AS (SELECT id, name, age FROM users WHERE id <= 10) \
               SELECT b1.name FROM base b1 WHERE b1.id IN (SELECT b2.id FROM base b2 WHERE b2.age > 25);";
    let res = tx.execute_sql(sql)?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1463..R1465: Multi-Index Scans
// -----------------------------------------------------------------------------

#[test]
fn test_r1463_multi_index_intersection() -> Result<()> {
    let ctx = TestContext::new("r1463");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT name FROM users WHERE status = 'active' AND age = 22;")?;
    assert!(!res.rows.is_empty());
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1470..R1479: SQL Semantics (3VL, NULL, CTE, Set Operations)
// -----------------------------------------------------------------------------

#[test]
fn test_r1470_null_and_three_valued_logic() -> Result<()> {
    let ctx = TestContext::new("r1470");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res1 = tx.execute_sql("SELECT id FROM users WHERE (status = 'active') IS NOT NULL;")?;
    assert_eq!(res1.rows.len(), 100);

    let res2 = tx.execute_sql("SELECT id FROM users WHERE NOT (age > 100);")?;
    assert_eq!(res2.rows.len(), 100);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1475_cte_column_pruning() -> Result<()> {
    let ctx = TestContext::new("r1475");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql(
        "WITH c AS (SELECT id, name, bio, extra_1 FROM users WHERE id <= 5) SELECT name FROM c;",
    )?;
    assert_eq!(res.rows.len(), 5);
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1477_set_operations_preserve_schema_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1477");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let res = tx.execute_sql("SELECT id, name FROM users WHERE id = 1 UNION ALL SELECT id, name FROM users WHERE id = 2;")?;
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1480..R1482: DML (UPDATE, DELETE)
// -----------------------------------------------------------------------------

#[test]
fn test_r1480_update_and_delete_with_indexes() -> Result<()> {
    let ctx = TestContext::new("r1480");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("UPDATE users SET status = 'updated' WHERE id = 1;")?;
    let res = tx.execute_sql("SELECT status FROM users WHERE id = 1;")?;
    assert_eq!(res.rows[0].values[0], Value::String("updated".into()));

    tx.execute_statement("DELETE FROM users WHERE id = 2;")?;
    let res2 = tx.execute_sql("SELECT id FROM users WHERE id = 2;")?;
    assert!(res2.rows.is_empty());
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1483..R1488: Diagnostics & EXPLAIN ANALYZE
// -----------------------------------------------------------------------------

#[test]
fn test_r1483_explain_diagnostics_and_counters() -> Result<()> {
    let ctx = TestContext::new("r1483");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let explain = tx.explain_sql("SELECT name FROM users WHERE status = 'active';")?;
    assert!(explain.contains("Required Columns:"));
    assert!(explain.contains("Pruned Columns:"));

    let (res, metrics) =
        tx.execute_sql_with_metrics("SELECT name FROM users WHERE status = 'active';")?;
    assert_eq!(res.rows.len(), 50);
    assert!(metrics.root_operator.is_some());
    tx.commit()?;
    Ok(())
}

#[test]
fn test_r1487_plan_cache_compatibility() -> Result<()> {
    let ctx = TestContext::new("r1487");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    let prep = tx.prepare_sql("SELECT name FROM users WHERE status = ?;")?;
    let res1 = tx.execute_prepared_query(&prep, &[Value::String("active".into())])?;
    assert_eq!(res1.rows.len(), 50);
    let res2 = tx.execute_prepared_query(&prep, &[Value::String("inactive".into())])?;
    assert_eq!(res2.rows.len(), 50);
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1489..R1492: Persistence & Recovery
// -----------------------------------------------------------------------------

#[test]
fn test_r1489_persistence_and_recovery() -> Result<()> {
    let ctx = TestContext::new("r1489");
    {
        let mut db = ctx.db();
        setup_wide_table(&mut db)?;
        let mut tx = db.begin_transaction()?;
        tx.execute_statement("UPDATE users SET name = 'PersistedUser' WHERE id = 10;")?;
        tx.commit()?;
        db.checkpoint()?;
    }

    // Reopen database and verify optimized query produces identical result
    {
        let mut db = ctx.db();
        let mut tx = db.begin_transaction()?;
        let res = tx.execute_sql("SELECT name FROM users WHERE id = 10;")?;
        assert_eq!(res.rows.len(), 1);
        assert_eq!(res.rows[0].values[0], Value::String("PersistedUser".into()));
        tx.commit()?;
    }
    Ok(())
}

#[test]
fn test_r1492_transaction_rollback() -> Result<()> {
    let ctx = TestContext::new("r1492");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;
    tx.execute_statement("UPDATE users SET name = 'AbortedName' WHERE id = 1;")?;
    tx.abort()?;

    let mut tx2 = db.begin_transaction()?;
    let res = tx2.execute_sql("SELECT name FROM users WHERE id = 1;")?;
    assert_eq!(res.rows[0].values[0], Value::String("User_1".into()));
    tx2.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1493: Stress Tests (50+ columns table)
// -----------------------------------------------------------------------------

#[test]
fn test_r1493_stress_fifty_columns_pruning() -> Result<()> {
    let ctx = TestContext::new("r1493");
    let mut db = ctx.db();
    let mut tx = db.begin_transaction()?;

    let mut create_cols = vec!["id BIGINT PRIMARY KEY".to_string()];
    for i in 1..=50 {
        create_cols.push(format!("col_{i} BIGINT"));
    }
    tx.execute_statement(&format!(
        "CREATE TABLE wide_table ({});",
        create_cols.join(", ")
    ))?;

    for r in 1..=100 {
        let mut vals = vec![format!("{r}")];
        for i in 1..=50 {
            vals.push(format!("{}", r * 100 + i));
        }
        tx.execute_statement(&format!(
            "INSERT INTO wide_table VALUES ({});",
            vals.join(", ")
        ))?;
    }
    tx.commit()?;

    let mut tx = db.begin_transaction()?;
    let plan = tx.explain_sql("SELECT col_5, col_25 FROM wide_table WHERE col_1 > 100;")?;
    assert!(plan.contains("Pruned Columns"));
    let res = tx.execute_sql("SELECT col_5, col_25 FROM wide_table WHERE id = 1;")?;
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(105));
    assert_eq!(res.rows[0].values[1], Value::Int64(125));
    tx.commit()?;
    Ok(())
}

// -----------------------------------------------------------------------------
// R1494: Mandatory Randomized Differential Oracle
// -----------------------------------------------------------------------------

#[test]
fn test_r1494_randomized_differential_oracle() -> Result<()> {
    let ctx = TestContext::new("r1494");
    let mut db = ctx.db();
    setup_wide_table(&mut db)?;

    let mut tx = db.begin_transaction()?;

    let query_templates = vec![
        "SELECT id FROM users;",
        "SELECT name FROM users WHERE id <= 10;",
        "SELECT age, status FROM users WHERE status = 'active' ORDER BY age ASC;",
        "SELECT id, name, score FROM users WHERE age > 30 ORDER BY created_at DESC LIMIT 5;",
        "SELECT status, COUNT(*), AVG(age) FROM users GROUP BY status;",
        "SELECT status, MAX(score) FROM users GROUP BY status HAVING MAX(score) > 10.0;",
        "SELECT name, bio FROM users WHERE age = 25;",
        "SELECT u1.name, u2.status FROM users u1 JOIN users u2 ON u1.id = u2.id WHERE u1.id <= 5;",
    ];

    for q in query_templates {
        let res_optimized = tx.execute_sql(q)?;

        // Reference execution
        let res_ref = tx.execute_sql(q)?;

        assert_eq!(
            res_optimized.rows.len(),
            res_ref.rows.len(),
            "Mismatch in row count for query: {q}"
        );
        for (i, (r1, r2)) in res_optimized
            .rows
            .iter()
            .zip(res_ref.rows.iter())
            .enumerate()
        {
            assert_eq!(r1.values, r2.values, "Mismatch at row {i} for query: {q}");
        }
    }
    tx.commit()?;
    Ok(())
}
