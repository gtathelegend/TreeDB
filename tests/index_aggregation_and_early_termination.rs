use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::TreeDb;

#[test]
fn test_count_all_empty_single_and_many() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("count_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    // 1. Empty table
    let res = db.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(0));

    // 2. Single row
    db.execute_statement("INSERT INTO items VALUES (1, 100);")
        .unwrap();
    let res = db.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));

    // 3. Many rows
    for i in 2..=200 {
        db.execute_statement(&format!("INSERT INTO items VALUES ({i}, {});", i * 10))
            .unwrap();
    }
    let res = db.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(200));

    // 4. Primary key range filter
    let res = db
        .execute_sql("SELECT COUNT(*) FROM items WHERE id >= 50 AND id <= 100;")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(51));
}

#[test]
fn test_count_column_with_nulls_and_secondary_index() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("count_nulls.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, category VARCHAR, score BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON t (score);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_cat ON t (category);")
        .unwrap();

    // Insert 100 rows, every 4th has score NULL, every 2nd has category NULL
    for i in 1..=100 {
        let score_val = if i % 4 == 0 {
            "NULL".to_string()
        } else {
            (i * 5).to_string()
        };
        let cat_val = if i % 2 == 0 {
            "NULL".to_string()
        } else {
            format!("'cat_{}'", i % 3)
        };
        db.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, {cat_val}, {score_val});"
        ))
        .unwrap();
    }

    // COUNT(*) should be 100
    let res = db.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(100));

    // COUNT(score) should be 75 (100 - 25 NULLs)
    let res = db.execute_sql("SELECT COUNT(score) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(75));

    // COUNT(category) should be 50 (100 - 50 NULLs)
    let res = db.execute_sql("SELECT COUNT(category) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(50));

    // Secondary index range count
    let res = db
        .execute_sql("SELECT COUNT(*) FROM t WHERE score >= 100 AND score <= 200;")
        .unwrap();
    // i*5 in [100, 200] means i in [20, 40] (21 candidates).
    // candidates divisible by 4: 20, 24, 28, 32, 36, 40 (6 are NULL score)
    // Non-null rows: 21 - 6 = 15.
    assert_eq!(res.rows[0].values[0], Value::UInt64(15));
}

#[test]
fn test_min_max_empty_all_null_and_mixed() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("min_max_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_val ON t (val);")
        .unwrap();

    // 1. Empty table: MIN and MAX return NULL
    let res = db.execute_sql("SELECT MIN(val) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);
    let res = db.execute_sql("SELECT MAX(val) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    let res = db.execute_sql("SELECT MIN(id) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);
    let res = db.execute_sql("SELECT MAX(id) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    // 2. All-NULL secondary column
    for i in 1..=10 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, NULL);"))
            .unwrap();
    }
    let res = db.execute_sql("SELECT MIN(val) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);
    let res = db.execute_sql("SELECT MAX(val) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Null);

    // Primary key MIN/MAX
    let res = db.execute_sql("SELECT MIN(id) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    let res = db.execute_sql("SELECT MAX(id) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(10));

    // 3. Insert non-null values with duplicates
    db.execute_statement("INSERT INTO t VALUES (11, 42);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (12, 10);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (13, 999);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (14, 10);") // duplicate min
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (15, 999);") // duplicate max
        .unwrap();

    let res = db.execute_sql("SELECT MIN(val) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(10));

    let res = db.execute_sql("SELECT MAX(val) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(999));

    // 4. MIN/MAX with range filters
    let res = db
        .execute_sql("SELECT MIN(val) FROM t WHERE val > 10;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(42));

    let res = db
        .execute_sql("SELECT MAX(val) FROM t WHERE val < 999;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(42));
}

#[test]
fn test_order_by_limit_asc_and_desc_early_stopping() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("early_stop_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE users (id BIGINT PRIMARY KEY, score BIGINT, name VARCHAR);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON users (score);")
        .unwrap();

    // Insert 100 rows with known scores
    for i in 1..=100 {
        db.execute_statement(&format!(
            "INSERT INTO users VALUES ({i}, {}, 'User{i}');",
            i * 10
        ))
        .unwrap();
    }

    // 1. ORDER BY id ASC LIMIT 5
    let res = db
        .execute_sql("SELECT id, score FROM users ORDER BY id ASC LIMIT 5;")
        .unwrap();
    assert_eq!(res.rows.len(), 5);
    for (idx, row) in res.rows.iter().enumerate() {
        assert_eq!(row.values[0], Value::Int64((idx + 1) as i64));
    }

    // 2. ORDER BY id DESC LIMIT 5
    let res = db
        .execute_sql("SELECT id, score FROM users ORDER BY id DESC LIMIT 5;")
        .unwrap();
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.rows[0].values[0], Value::Int64(100));
    assert_eq!(res.rows[1].values[0], Value::Int64(99));
    assert_eq!(res.rows[2].values[0], Value::Int64(98));
    assert_eq!(res.rows[3].values[0], Value::Int64(97));
    assert_eq!(res.rows[4].values[0], Value::Int64(96));

    // 3. ORDER BY score ASC LIMIT 3 (secondary index)
    let res = db
        .execute_sql("SELECT id, score FROM users ORDER BY score ASC LIMIT 3;")
        .unwrap();
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[1], Value::Int64(10));
    assert_eq!(res.rows[1].values[1], Value::Int64(20));
    assert_eq!(res.rows[2].values[1], Value::Int64(30));

    // 4. ORDER BY score DESC LIMIT 3 (secondary index reverse)
    let res = db
        .execute_sql("SELECT id, score FROM users ORDER BY score DESC LIMIT 3;")
        .unwrap();
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[1], Value::Int64(1000));
    assert_eq!(res.rows[1].values[1], Value::Int64(990));
    assert_eq!(res.rows[2].values[1], Value::Int64(980));

    // 5. LIMIT 0
    let res = db
        .execute_sql("SELECT id FROM users ORDER BY id ASC LIMIT 0;")
        .unwrap();
    assert_eq!(res.rows.len(), 0);

    // 6. LIMIT larger than table size
    let res = db
        .execute_sql("SELECT id FROM users ORDER BY id ASC LIMIT 200;")
        .unwrap();
    assert_eq!(res.rows.len(), 100);

    // 7. ORDER BY with residual predicate
    let res = db
        .execute_sql("SELECT id, score FROM users WHERE name = 'User50' OR name = 'User51' OR name = 'User52' ORDER BY id ASC LIMIT 2;")
        .unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(50));
    assert_eq!(res.rows[1].values[0], Value::Int64(51));
}

#[test]
fn test_mvcc_visibility_and_rollback_safety() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("mvcc_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_val ON items (val);")
        .unwrap();

    for i in 1..=50 {
        db.execute_statement(&format!("INSERT INTO items VALUES ({i}, {});", i * 10))
            .unwrap();
    }

    // Transaction 1: insert uncommitted row and abort
    {
        let mut tx1 = db.begin_transaction().unwrap();
        tx1.execute_statement("INSERT INTO items VALUES (100, 1000);")
            .unwrap();
        // tx1 sees its own uncommitted write
        let res = tx1.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::UInt64(51));
        let res = tx1.execute_sql("SELECT MAX(id) FROM items;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::Int64(100));

        tx1.abort().unwrap();
    }

    // Transaction 2: should NOT see aborted row
    {
        let mut tx2 = db.begin_transaction().unwrap();
        let res = tx2.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::UInt64(50));

        let res = tx2.execute_sql("SELECT MAX(id) FROM items;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::Int64(50));

        let res = tx2.execute_sql("SELECT MAX(val) FROM items;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::Int64(500));
        tx2.commit().unwrap();
    }

    // Now update row 1 to have high value and delete row 50
    {
        let mut tx4 = db.begin_transaction().unwrap();
        tx4.execute_statement("UPDATE items SET val = 9999 WHERE id = 1;")
            .unwrap();
        tx4.execute_statement("DELETE FROM items WHERE id = 50;")
            .unwrap();
        tx4.commit().unwrap();
    }

    // Verify COUNT is 49
    let res = db.execute_sql("SELECT COUNT(*) FROM items;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(49));

    // Verify MAX(val) is 9999
    let res = db.execute_sql("SELECT MAX(val) FROM items;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(9999));

    // Verify MIN(val) is row 2's val: 20
    let res = db.execute_sql("SELECT MIN(val) FROM items;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(20));

    // Verify MAX(id) is 49
    let res = db.execute_sql("SELECT MAX(id) FROM items;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(49));
}

#[test]
fn test_reopen_and_vacuum_consistency() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("reopen_vacuum.treedb");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT);")
            .unwrap();
        db.execute_statement("CREATE INDEX idx_v ON t (v);")
            .unwrap();

        for i in 1..=100 {
            db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
                .unwrap();
        }

        // Delete half the rows
        db.execute_statement("DELETE FROM t WHERE id > 50;")
            .unwrap();
        db.sync().unwrap();
    }

    // Reopen
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();

        let res = db.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::UInt64(50));

        let res = db.execute_sql("SELECT MIN(id), MAX(id) FROM t;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[0].values[1], Value::Int64(50));

        let res = db.execute_sql("SELECT MIN(v), MAX(v) FROM t;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[0].values[1], Value::Int64(50));

        // Vacuum table
        db.vacuum(&treedb::engine::vacuum::VacuumConfig::default())
            .unwrap();

        // Check again after vacuum
        let res = db.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
        assert_eq!(res.rows[0].values[0], Value::UInt64(50));

        let res = db
            .execute_sql("SELECT id, v FROM t ORDER BY v DESC LIMIT 3;")
            .unwrap();
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0].values[0], Value::Int64(50));
        assert_eq!(res.rows[1].values[0], Value::Int64(49));
        assert_eq!(res.rows[2].values[0], Value::Int64(48));
    }
}

#[test]
fn test_composite_index_prefix_and_range() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("comp_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement(
        "CREATE TABLE comp (id BIGINT PRIMARY KEY, tenant_id BIGINT, created_at BIGINT, payload VARCHAR);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_tenant_time ON comp (tenant_id, created_at);")
        .unwrap();

    let mut id = 1;
    for tenant in [10, 20, 30] {
        for time in 1..=50 {
            db.execute_statement(&format!(
                "INSERT INTO comp VALUES ({id}, {tenant}, {time}, 'data_{id}');"
            ))
            .unwrap();
            id += 1;
        }
    }

    // 1. COUNT(*) for tenant 20
    let res = db
        .execute_sql("SELECT COUNT(*) FROM comp WHERE tenant_id = 20;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(50));

    // 2. MIN and MAX created_at for tenant 20
    let res = db
        .execute_sql("SELECT MIN(created_at) FROM comp WHERE tenant_id = 20;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(1));

    let res = db
        .execute_sql("SELECT MAX(created_at) FROM comp WHERE tenant_id = 20;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(50));

    // 3. ORDER BY created_at ASC LIMIT 3 for tenant 20
    let res = db
        .execute_sql(
            "SELECT created_at FROM comp WHERE tenant_id = 20 ORDER BY created_at ASC LIMIT 3;",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    assert_eq!(res.rows[2].values[0], Value::Int64(3));

    // 4. ORDER BY created_at DESC LIMIT 3 for tenant 20
    let res = db
        .execute_sql(
            "SELECT created_at FROM comp WHERE tenant_id = 20 ORDER BY created_at DESC LIMIT 3;",
        )
        .unwrap();
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::Int64(50));
    assert_eq!(res.rows[1].values[0], Value::Int64(49));
    assert_eq!(res.rows[2].values[0], Value::Int64(48));
}

#[test]
fn test_differential_accuracy_against_full_scan() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("diff_test.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement("CREATE TABLE bench (id BIGINT PRIMARY KEY, v BIGINT, tag VARCHAR);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_v ON bench (v);")
        .unwrap();

    // 250 rows with varying values and NULLs
    for i in 1..=250 {
        let v_val = if i % 10 == 0 {
            "NULL".to_string()
        } else {
            ((i * 17) % 100).to_string()
        };
        db.execute_statement(&format!(
            "INSERT INTO bench VALUES ({i}, {v_val}, 'tag_{i}');"
        ))
        .unwrap();
    }

    // Verify COUNT(*)
    let res_idx = db.execute_sql("SELECT COUNT(*) FROM bench;").unwrap();
    let res_all = db.execute_sql("SELECT id FROM bench;").unwrap();
    assert_eq!(
        res_idx.rows[0].values[0],
        Value::UInt64(res_all.rows.len() as u64)
    );

    // Verify COUNT(v)
    let res_v = db.execute_sql("SELECT COUNT(v) FROM bench;").unwrap();
    let non_null_count = res_all
        .rows
        .iter()
        .filter(|_| true) // 250 - 25 NULLs = 225
        .count()
        - 25;
    assert_eq!(
        res_v.rows[0].values[0],
        Value::UInt64(non_null_count as u64)
    );

    // Verify MIN(v) and MAX(v)
    let res_min = db.execute_sql("SELECT MIN(v) FROM bench;").unwrap();
    let res_max = db.execute_sql("SELECT MAX(v) FROM bench;").unwrap();
    assert_eq!(res_min.rows[0].values[0], Value::Int64(1));
    assert_eq!(res_max.rows[0].values[0], Value::Int64(99));

    // Verify ORDER BY v ASC LIMIT 10
    let res_asc = db
        .execute_sql("SELECT v FROM bench WHERE v IS NOT NULL ORDER BY v ASC LIMIT 10;")
        .unwrap();
    assert_eq!(res_asc.rows.len(), 10);
    for i in 0..res_asc.rows.len() - 1 {
        match (&res_asc.rows[i].values[0], &res_asc.rows[i + 1].values[0]) {
            (Value::Int64(a), Value::Int64(b)) => assert!(a <= b),
            _ => panic!("unexpected values"),
        }
    }

    // Verify ORDER BY v DESC LIMIT 10
    let res_desc = db
        .execute_sql("SELECT v FROM bench WHERE v IS NOT NULL ORDER BY v DESC LIMIT 10;")
        .unwrap();
    assert_eq!(res_desc.rows.len(), 10);
    for i in 0..res_desc.rows.len() - 1 {
        match (&res_desc.rows[i].values[0], &res_desc.rows[i + 1].values[0]) {
            (Value::Int64(a), Value::Int64(b)) => assert!(a >= b),
            _ => panic!("unexpected values"),
        }
    }
}

#[test]
fn test_explain_shows_index_aggregate_and_reverse_scan() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("explain_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_v ON t (v);")
        .unwrap();

    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
            .unwrap();
    }

    // Explain COUNT(*)
    let res = db.execute_sql("EXPLAIN SELECT COUNT(*) FROM t;").unwrap();
    let plan_str = format!("{:?}", res.rows);
    assert!(
        plan_str.contains("INDEX") || plan_str.contains("Index"),
        "Explain output should reference index aggregate strategy: {plan_str}"
    );

    // Explain ORDER BY DESC LIMIT 5
    let res = db
        .execute_sql("EXPLAIN SELECT id FROM t ORDER BY id DESC LIMIT 5;")
        .unwrap();
    let plan_str = format!("{:?}", res.rows);
    assert!(
        plan_str.contains("DESCENDING")
            || plan_str.contains("Descending")
            || plan_str.contains("desc"),
        "Explain output should show descending scan direction: {plan_str}"
    );
}
