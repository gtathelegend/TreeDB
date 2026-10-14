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

#[test]
fn test_secondary_index_point_lookup_count() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("point_count.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, score BIGINT, cat VARCHAR);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON t (score);")
        .unwrap();

    for i in 1..=50 {
        let score = if i <= 10 { 100 } else { i * 10 };
        db.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, {score}, 'cat_{}');",
            i % 5
        ))
        .unwrap();
    }

    // Point lookup count on secondary index
    let res = db
        .execute_sql("SELECT COUNT(*) FROM t WHERE score = 100;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(10));

    let res_col = db
        .execute_sql("SELECT COUNT(score) FROM t WHERE score = 100;")
        .unwrap();
    assert_eq!(res_col.rows[0].values[0], Value::UInt64(10));

    // Zero-match point lookup
    let res_none = db
        .execute_sql("SELECT COUNT(*) FROM t WHERE score = 9999;")
        .unwrap();
    assert_eq!(res_none.rows[0].values[0], Value::UInt64(0));
}

#[test]
fn test_min_max_with_unrelated_filter_fallback() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("min_max_unrelated.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, cat VARCHAR, score BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_cat ON t (cat);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON t (score);")
        .unwrap();

    // Insert rows: cat_A has scores 50, 80, 20. cat_B has scores 10, 90.
    db.execute_statement("INSERT INTO t VALUES (1, 'cat_A', 50);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (2, 'cat_A', 80);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (3, 'cat_A', 20);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (4, 'cat_B', 10);")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (5, 'cat_B', 90);")
        .unwrap();

    // Query filters on cat, aggregates on score. Must NOT blindly take first of idx_cat!
    let res = db
        .execute_sql("SELECT MIN(score), MAX(score) FROM t WHERE cat = 'cat_A';")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(20));
    assert_eq!(res.rows[0].values[1], Value::Int64(80));

    let res_b = db
        .execute_sql("SELECT MIN(score), MAX(score) FROM t WHERE cat = 'cat_B';")
        .unwrap();
    assert_eq!(res_b.rows[0].values[0], Value::Int64(10));
    assert_eq!(res_b.rows[0].values[1], Value::Int64(90));
}

#[test]
fn test_min_max_filtered_on_same_column() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("min_max_same_col.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, score BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON t (score);")
        .unwrap();

    for i in 1..=50 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", i * 10))
            .unwrap();
    }

    // Bounded range on score
    let res = db
        .execute_sql("SELECT MIN(score), MAX(score) FROM t WHERE score >= 150 AND score <= 350;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::Int64(150));
    assert_eq!(res.rows[0].values[1], Value::Int64(350));

    // Primary key bounds
    let res_pk = db
        .execute_sql("SELECT MIN(id), MAX(id) FROM t WHERE id >= 10 AND id <= 25;")
        .unwrap();
    assert_eq!(res_pk.rows[0].values[0], Value::Int64(10));
    assert_eq!(res_pk.rows[0].values[1], Value::Int64(25));
}

#[test]
fn test_early_stop_with_heavy_residual_filter() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("residual_early_stop.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, flag BIGINT);")
        .unwrap();

    // 200 rows, only 20 have flag = 1
    for i in 1..=200 {
        let flag = if i % 10 == 0 { 1 } else { 0 };
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {flag});"))
            .unwrap();
    }

    // Ascending with residual filter
    let res_asc = db
        .execute_sql("SELECT id FROM t WHERE flag = 1 ORDER BY id ASC LIMIT 5;")
        .unwrap();
    assert_eq!(res_asc.rows.len(), 5);
    assert_eq!(res_asc.rows[0].values[0], Value::Int64(10));
    assert_eq!(res_asc.rows[1].values[0], Value::Int64(20));
    assert_eq!(res_asc.rows[2].values[0], Value::Int64(30));
    assert_eq!(res_asc.rows[3].values[0], Value::Int64(40));
    assert_eq!(res_asc.rows[4].values[0], Value::Int64(50));

    // Descending with residual filter
    let res_desc = db
        .execute_sql("SELECT id FROM t WHERE flag = 1 ORDER BY id DESC LIMIT 5;")
        .unwrap();
    assert_eq!(res_desc.rows.len(), 5);
    assert_eq!(res_desc.rows[0].values[0], Value::Int64(200));
    assert_eq!(res_desc.rows[1].values[0], Value::Int64(190));
    assert_eq!(res_desc.rows[2].values[0], Value::Int64(180));
    assert_eq!(res_desc.rows[3].values[0], Value::Int64(170));
    assert_eq!(res_desc.rows[4].values[0], Value::Int64(160));
}

#[test]
fn test_early_stop_ties_and_nulls() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("ties_and_nulls.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, score BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON t (score);")
        .unwrap();

    // 5 NULLs, 5 with 10, 5 with 20
    for i in 1..=5 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, NULL);"))
            .unwrap();
    }
    for i in 6..=10 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, 10);"))
            .unwrap();
    }
    for i in 11..=15 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, 20);"))
            .unwrap();
    }

    // Ascending: NULLs first
    let res_asc = db
        .execute_sql("SELECT score FROM t ORDER BY score ASC LIMIT 8;")
        .unwrap();
    assert_eq!(res_asc.rows.len(), 8);
    for i in 0..5 {
        assert_eq!(res_asc.rows[i].values[0], Value::Null);
    }
    for i in 5..8 {
        assert_eq!(res_asc.rows[i].values[0], Value::Int64(10));
    }

    // Descending: NULLs last
    let res_desc = db
        .execute_sql("SELECT score FROM t ORDER BY score DESC LIMIT 8;")
        .unwrap();
    assert_eq!(res_desc.rows.len(), 8);
    for i in 0..5 {
        assert_eq!(res_desc.rows[i].values[0], Value::Int64(20));
    }
    for i in 5..8 {
        assert_eq!(res_desc.rows[i].values[0], Value::Int64(10));
    }
}

#[test]
fn test_early_stop_limit_edge_cases() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("limit_edges.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT);")
        .unwrap();

    // 1. LIMIT on empty table
    let res = db
        .execute_sql("SELECT * FROM t ORDER BY id ASC LIMIT 5;")
        .unwrap();
    assert_eq!(res.rows.len(), 0);

    for i in 1..=10 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
            .unwrap();
    }

    // 2. LIMIT 0
    let res_0 = db
        .execute_sql("SELECT * FROM t ORDER BY id ASC LIMIT 0;")
        .unwrap();
    assert_eq!(res_0.rows.len(), 0);

    // 3. LIMIT 1
    let res_1 = db
        .execute_sql("SELECT id FROM t ORDER BY id ASC LIMIT 1;")
        .unwrap();
    assert_eq!(res_1.rows.len(), 1);
    assert_eq!(res_1.rows[0].values[0], Value::Int64(1));

    // 4. LIMIT greater than row count
    let res_big = db
        .execute_sql("SELECT id FROM t ORDER BY id ASC LIMIT 100;")
        .unwrap();
    assert_eq!(res_big.rows.len(), 10);
}

#[test]
fn test_unsupported_ordering_fallback_sort() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("unsupported_sort.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, score BIGINT, name VARCHAR);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_score ON t (score);")
        .unwrap();

    // Insert rows where score has ties but name differs
    db.execute_statement("INSERT INTO t VALUES (1, 10, 'Zack');")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (2, 10, 'Alice');")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (3, 20, 'Bob');")
        .unwrap();
    db.execute_statement("INSERT INTO t VALUES (4, 10, 'Charlie');")
        .unwrap();

    // ORDER BY score ASC, name ASC (index only satisfies score, NOT name)
    let res = db
        .execute_sql("SELECT name FROM t ORDER BY score ASC, name ASC LIMIT 3;")
        .unwrap();
    assert_eq!(res.rows.len(), 3);
    assert_eq!(res.rows[0].values[0], Value::String("Alice".to_string()));
    assert_eq!(res.rows[1].values[0], Value::String("Charlie".to_string()));
    assert_eq!(res.rows[2].values[0], Value::String("Zack".to_string()));
}

#[test]
fn test_differential_mvcc_concurrent_snapshots() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("mvcc_snapshots.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
            .unwrap();
    }

    // 1. Transaction 1: uncommitted insert and update
    let mut tx1 = db.begin_transaction().unwrap();
    for i in 21..=30 {
        tx1.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
            .unwrap();
    }
    tx1.execute_statement("UPDATE t SET val = val + 100 WHERE id = 1;")
        .unwrap();
    // In tx1: sees 30 rows and MAX = 101
    let res1_count = tx1.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
    assert_eq!(res1_count.rows[0].values[0], Value::UInt64(30));
    let res1_max = tx1.execute_sql("SELECT MAX(val) FROM t;").unwrap();
    assert_eq!(res1_max.rows[0].values[0], Value::Int64(101));
    tx1.abort().unwrap();

    // 2. Transaction 2: after abort, sees pre-mutation state
    let mut tx2 = db.begin_transaction().unwrap();
    let res2_count = tx2.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
    assert_eq!(res2_count.rows[0].values[0], Value::UInt64(20));
    let res2_max = tx2.execute_sql("SELECT MAX(val) FROM t;").unwrap();
    assert_eq!(res2_max.rows[0].values[0], Value::Int64(20));

    // Commit a mutation in tx2
    for i in 21..=25 {
        tx2.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
            .unwrap();
    }
    tx2.commit().unwrap();

    // 3. Transaction 3: sees committed changes
    let mut tx3 = db.begin_transaction().unwrap();
    let res3_count = tx3.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
    assert_eq!(res3_count.rows[0].values[0], Value::UInt64(25));
    let res3_max = tx3.execute_sql("SELECT MAX(val) FROM t;").unwrap();
    assert_eq!(res3_max.rows[0].values[0], Value::Int64(25));
    tx3.commit().unwrap();
}

#[test]
fn test_metrics_fast_path_assertion() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("metrics_assertion.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT);")
        .unwrap();
    for i in 1..=50 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i});"))
            .unwrap();
    }

    let mut txn = db.begin_transaction().unwrap();
    let prep = txn.prepare_sql("SELECT COUNT(*) FROM t;").unwrap();
    let (res, metrics) = txn.execute_prepared_with_metrics(&prep, &[]).unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(50));

    // Fast-path index aggregate operator was selected in the plan tree
    let root = metrics.root_operator.expect("root operator exists");
    let has_index_agg = root.kind == treedb::query::OperatorKind::IndexAggregate
        || root
            .children
            .iter()
            .any(|c| c.kind == treedb::query::OperatorKind::IndexAggregate);
    assert!(
        has_index_agg,
        "Operator tree must contain IndexAggregate: {root:?}"
    );
    assert_eq!(metrics.total_tuples_fetched, 0);
    txn.commit().unwrap();
}

#[test]
fn test_count_column_edge_cases_all_null_empty_all_non_null() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("count_edge_cases.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_v ON t (v);")
        .unwrap();

    // 1. Empty table
    let res = db.execute_sql("SELECT COUNT(v) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(0));

    // 2. All NULL rows
    for i in 1..=20 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, NULL);"))
            .unwrap();
    }
    let res = db.execute_sql("SELECT COUNT(v) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(0));
    let res_star = db.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
    assert_eq!(res_star.rows[0].values[0], Value::UInt64(20));

    // 3. All non-NULL rows added
    for i in 21..=40 {
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {});", i * 10))
            .unwrap();
    }
    let res = db.execute_sql("SELECT COUNT(v) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(20));
    let res_star = db.execute_sql("SELECT COUNT(*) FROM t;").unwrap();
    assert_eq!(res_star.rows[0].values[0], Value::UInt64(40));

    // 4. Differential check against reference engine
    let oracle = db.execute_sql_reference("SELECT COUNT(v) FROM t;").unwrap();
    assert_eq!(res.rows[0].values[0], oracle.rows[0].values[0]);
}

#[test]
fn test_secondary_count_filtered_and_composite_prefix() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("sec_count_filtered.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, tenant_id BIGINT, status VARCHAR, amount BIGINT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_tenant_status ON orders (tenant_id, status);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_amount ON orders (amount);")
        .unwrap();

    let mut id = 1;
    for tenant in [100, 200] {
        for i in 1..=100 {
            let status = match i % 4 {
                0 => "NULL",
                1 => "'PENDING'",
                2 => "'SHIPPED'",
                _ => "'DELIVERED'",
            };
            let amt = if i % 5 == 0 { "NULL" } else { "50" };
            db.execute_statement(&format!(
                "INSERT INTO orders VALUES ({id}, {tenant}, {status}, {amt});"
            ))
            .unwrap();
            id += 1;
        }
    }

    // 1. Prefix scan COUNT(*) on tenant 100
    let res = db
        .execute_sql("SELECT COUNT(*) FROM orders WHERE tenant_id = 100;")
        .unwrap();
    let ref_res = db
        .execute_sql_reference("SELECT COUNT(*) FROM orders WHERE tenant_id = 100;")
        .unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(100));
    assert_eq!(res.rows[0].values[0], ref_res.rows[0].values[0]);

    // 2. Secondary index range count on amount
    let res_amt = db
        .execute_sql("SELECT COUNT(amount) FROM orders WHERE amount >= 40 AND amount <= 60;")
        .unwrap();
    let ref_amt = db
        .execute_sql_reference(
            "SELECT COUNT(amount) FROM orders WHERE amount >= 40 AND amount <= 60;",
        )
        .unwrap();
    assert_eq!(res_amt.rows[0].values[0], ref_amt.rows[0].values[0]);
    // 200 total rows, 20% are NULL -> 160 non-NULL rows
    assert_eq!(res_amt.rows[0].values[0], Value::UInt64(160));
}

#[test]
fn test_secondary_count_differential_oracle_with_transactions_and_vacuum() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("diff_sec_count.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT, cat VARCHAR);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_v ON t (v);")
        .unwrap();

    // Populate initial rows
    for i in 1..=120 {
        let v = if i % 3 == 0 {
            "NULL".to_string()
        } else {
            (i * 7).to_string()
        };
        db.execute_statement(&format!(
            "INSERT INTO t VALUES ({i}, {v}, 'cat_{}');",
            i % 4
        ))
        .unwrap();
    }

    // Transaction with uncommitted inserts that aborts
    let mut tx = db.begin_transaction().unwrap();
    for i in 121..=140 {
        tx.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, 'cat_0');"))
            .unwrap();
    }
    tx.abort().unwrap();

    // Verify equivalence after abort
    let opt = db.execute_sql("SELECT COUNT(v) FROM t;").unwrap();
    let oracle = db.execute_sql_reference("SELECT COUNT(v) FROM t;").unwrap();
    assert_eq!(opt.rows[0].values[0], oracle.rows[0].values[0]);

    // Transaction with committed deletes
    let mut tx2 = db.begin_transaction().unwrap();
    tx2.execute_statement("DELETE FROM t WHERE id <= 20;")
        .unwrap();
    tx2.commit().unwrap();

    // Run vacuum
    let vac_cfg = treedb::VacuumConfig::default();
    db.vacuum(&vac_cfg).unwrap();

    // Verify equivalence after vacuum
    let opt2 = db.execute_sql("SELECT COUNT(v) FROM t;").unwrap();
    let oracle2 = db.execute_sql_reference("SELECT COUNT(v) FROM t;").unwrap();
    assert_eq!(opt2.rows[0].values[0], oracle2.rows[0].values[0]);
    // 100 rows remaining (21..=120). Divisible by 3 in 21..=120: 34 rows. Non-null: 100 - 34 = 66.
    assert_eq!(opt2.rows[0].values[0], Value::UInt64(66));
}

#[test]
fn test_aggregate_path_selection_fallback_when_ineligible() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("path_selection_fallback.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    // Table with unindexed 'unindexed_col'
    db.execute_statement(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, indexed_col BIGINT, unindexed_col BIGINT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_col ON t (indexed_col);")
        .unwrap();

    for i in 1..=30 {
        let u_val = if i % 2 == 0 { "NULL" } else { "100" };
        db.execute_statement(&format!("INSERT INTO t VALUES ({i}, {i}, {u_val});"))
            .unwrap();
    }

    // 1. COUNT(unindexed_col) must fall back to normal aggregation
    let mut txn = db.begin_transaction().unwrap();
    let prep = txn
        .prepare_sql("SELECT COUNT(unindexed_col) FROM t;")
        .unwrap();
    let (res, metrics) = txn.execute_prepared_with_metrics(&prep, &[]).unwrap();
    assert_eq!(res.rows[0].values[0], Value::UInt64(15));
    // Since unindexed_col has no index, rows were fetched from table scan
    assert!(
        metrics.total_tuples_fetched > 0,
        "Ineligible query must fetch table rows: {}",
        metrics.total_tuples_fetched
    );
    txn.commit().unwrap();

    // 2. COUNT(*) is eligible and fetches 0 base records
    let mut txn2 = db.begin_transaction().unwrap();
    let prep2 = txn2.prepare_sql("SELECT COUNT(*) FROM t;").unwrap();
    let (res2, metrics2) = txn2.execute_prepared_with_metrics(&prep2, &[]).unwrap();
    assert_eq!(res2.rows[0].values[0], Value::UInt64(30));
    assert_eq!(
        metrics2.total_tuples_fetched, 0,
        "Eligible COUNT(*) must avoid base heap fetches"
    );
    txn2.commit().unwrap();
}
