use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::error::Error;
use treedb::index::secondary::{IndexKey, SecondaryInternalNode, SecondaryLeafNode};
use treedb::storage::page::{Page, PageType};
use treedb::TreeDb;

#[test]
fn test_secondary_index_point_lookups_and_missing_keys() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("point_lookup.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement(
        "CREATE TABLE users (id BIGINT PRIMARY KEY, email VARCHAR, active BOOLEAN);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_email ON users(email);")
        .unwrap();

    db.execute_statement("INSERT INTO users VALUES (1, 'alice@example.com', true);")
        .unwrap();
    db.execute_statement("INSERT INTO users VALUES (2, 'bob@example.com', false);")
        .unwrap();
    db.execute_statement("INSERT INTO users VALUES (3, NULL, true);")
        .unwrap();

    // Point lookup hit
    let res = db
        .execute_sql("SELECT id, email FROM users WHERE email = 'alice@example.com';")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));

    // Point lookup miss
    let res = db
        .execute_sql("SELECT id FROM users WHERE email = 'charlie@example.com';")
        .unwrap();
    assert_eq!(res.rows.len(), 0);

    // NULL key handling
    let res = db
        .execute_sql("SELECT id FROM users WHERE email IS NULL;")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(3));
}

#[test]
fn test_secondary_index_duplicate_keys_spanning_leaf_splits() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("dup_keys.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement(
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, status VARCHAR, amount BIGINT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_status ON orders(status);")
        .unwrap();

    // Insert 600 orders: 300 'PENDING', 200 'COMPLETED', 100 'CANCELLED'
    // Leaf capacity is ~200 entries, so 300 identical keys will span across at least 2 leaves.
    for i in 1..=300 {
        db.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, 'PENDING', {i}0);"
        ))
        .unwrap();
    }
    for i in 301..=500 {
        db.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, 'COMPLETED', {i}0);"
        ))
        .unwrap();
    }
    for i in 501..=600 {
        db.execute_statement(&format!(
            "INSERT INTO orders VALUES ({i}, 'CANCELLED', {i}0);"
        ))
        .unwrap();
    }

    // Lookup spanning multi-leaf duplicate keys
    let res = db
        .execute_sql("SELECT id, status FROM orders WHERE status = 'PENDING';")
        .unwrap();
    assert_eq!(res.rows.len(), 300);

    let res_comp = db
        .execute_sql("SELECT id, status FROM orders WHERE status = 'COMPLETED';")
        .unwrap();
    assert_eq!(res_comp.rows.len(), 200);

    let res_canc = db
        .execute_sql("SELECT id, status FROM orders WHERE status = 'CANCELLED';")
        .unwrap();
    assert_eq!(res_canc.rows.len(), 100);

    // Oracle verification against full table scan
    let mut oracle_pending = 0;
    let all_orders = db.execute_sql("SELECT status FROM orders;").unwrap();
    for row in all_orders.rows {
        if row.values[0] == Value::String("PENDING".to_string()) {
            oracle_pending += 1;
        }
    }
    assert_eq!(oracle_pending, 300);
}

#[test]
fn test_range_scans_inclusive_exclusive_bounds_and_empty() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("range_bounds.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement("CREATE TABLE metrics (id BIGINT PRIMARY KEY, tag BIGINT, val BIGINT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_tag ON metrics(tag);")
        .unwrap();

    // Insert 500 rows with tag = i * 2 (even tags: 2, 4, 6, ..., 1000)
    for i in 1..=500 {
        let tag = i * 2;
        db.execute_statement(&format!("INSERT INTO metrics VALUES ({i}, {tag}, {i});"))
            .unwrap();
    }

    // 1. Primary key range scan: inclusive bounds (id >= 100 AND id <= 200) -> 101 rows (100..=200)
    let res = db
        .execute_sql("SELECT id FROM metrics WHERE id >= 100 AND id <= 200;")
        .unwrap();
    assert_eq!(res.rows.len(), 101);
    assert_eq!(res.rows[0].values[0], Value::Int64(100));
    assert_eq!(res.rows[100].values[0], Value::Int64(200));

    // 2. Primary key range scan: exclusive bounds (id > 100 AND id < 200) -> 99 rows (101..=199)
    let res_excl = db
        .execute_sql("SELECT id FROM metrics WHERE id > 100 AND id < 200;")
        .unwrap();
    assert_eq!(res_excl.rows.len(), 99);
    assert_eq!(res_excl.rows[0].values[0], Value::Int64(101));
    assert_eq!(res_excl.rows[98].values[0], Value::Int64(199));

    // 3. Primary key empty range (id >= 200 AND id <= 100) -> 0 rows
    let res_empty = db
        .execute_sql("SELECT id FROM metrics WHERE id >= 200 AND id <= 100;")
        .unwrap();
    assert_eq!(res_empty.rows.len(), 0);

    // 4. Secondary index range scan: tag between 200 and 400 (tags 200, 202, ..., 400 -> 101 rows)
    let res_tag = db
        .execute_sql("SELECT id, tag FROM metrics WHERE tag >= 200 AND tag <= 400;")
        .unwrap();
    assert_eq!(res_tag.rows.len(), 101);
    for row in &res_tag.rows {
        if let Value::Int64(t) = row.values[1] {
            assert!((200..=400).contains(&t));
        } else {
            panic!("unexpected value type");
        }
    }

    // 5. Secondary index range scan: exclusive bounds (tag > 200 AND tag < 220) -> tags 202..=218 (9 rows)
    let res_tag_ex = db
        .execute_sql("SELECT id, tag FROM metrics WHERE tag > 200 AND tag < 220;")
        .unwrap();
    assert_eq!(res_tag_ex.rows.len(), 9);
}

#[test]
fn test_composite_index_point_and_prefix_and_range_scans() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("composite.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement(
        "CREATE TABLE geo (id BIGINT PRIMARY KEY, country VARCHAR, city VARCHAR, pop BIGINT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_geo ON geo(country, city);")
        .unwrap();

    db.execute_statement("INSERT INTO geo VALUES (1, 'US', 'New York', 8000000);")
        .unwrap();
    db.execute_statement("INSERT INTO geo VALUES (2, 'US', 'Los Angeles', 4000000);")
        .unwrap();
    db.execute_statement("INSERT INTO geo VALUES (3, 'US', 'Chicago', 2700000);")
        .unwrap();
    db.execute_statement("INSERT INTO geo VALUES (4, 'CA', 'Toronto', 2900000);")
        .unwrap();
    db.execute_statement("INSERT INTO geo VALUES (5, 'CA', 'Montreal', 1700000);")
        .unwrap();
    db.execute_statement("INSERT INTO geo VALUES (6, 'UK', 'London', 9000000);")
        .unwrap();

    // Composite point lookup: country = 'US' AND city = 'Chicago'
    let res = db
        .execute_sql("SELECT id FROM geo WHERE country = 'US' AND city = 'Chicago';")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(3));

    // Composite prefix scan: country = 'US'
    let res_pref = db
        .execute_sql("SELECT id, city FROM geo WHERE country = 'US';")
        .unwrap();
    assert_eq!(res_pref.rows.len(), 3);

    // Missing key
    let res_miss = db
        .execute_sql("SELECT id FROM geo WHERE country = 'FR' AND city = 'Paris';")
        .unwrap();
    assert_eq!(res_miss.rows.len(), 0);
}

#[test]
fn test_secondary_index_mutations_updates_deletes_vacuum() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("mutations.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement(
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, status VARCHAR, bal BIGINT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_acc_status ON accounts(status);")
        .unwrap();

    for i in 1..=50 {
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({i}, 'ACTIVE', {i}00);"
        ))
        .unwrap();
    }

    // Update 20 accounts from 'ACTIVE' to 'FROZEN'
    for i in 1..=20 {
        db.execute_statement(&format!(
            "UPDATE accounts SET status = 'FROZEN' WHERE id = {i};"
        ))
        .unwrap();
    }

    let active_res = db
        .execute_sql("SELECT id FROM accounts WHERE status = 'ACTIVE';")
        .unwrap();
    assert_eq!(active_res.rows.len(), 30);

    let frozen_res = db
        .execute_sql("SELECT id FROM accounts WHERE status = 'FROZEN';")
        .unwrap();
    assert_eq!(frozen_res.rows.len(), 20);

    // Delete 10 active accounts
    for i in 21..=30 {
        db.execute_statement(&format!("DELETE FROM accounts WHERE id = {i};"))
            .unwrap();
    }

    let active_after_del = db
        .execute_sql("SELECT id FROM accounts WHERE status = 'ACTIVE';")
        .unwrap();
    assert_eq!(active_after_del.rows.len(), 20);

    // Run VACUUM
    db.vacuum(&treedb::engine::vacuum::VacuumConfig::default())
        .unwrap();

    // Verify index lookup remains correct after vacuum
    let active_after_vac = db
        .execute_sql("SELECT id FROM accounts WHERE status = 'ACTIVE';")
        .unwrap();
    assert_eq!(active_after_vac.rows.len(), 20);
    let frozen_after_vac = db
        .execute_sql("SELECT id FROM accounts WHERE status = 'FROZEN';")
        .unwrap();
    assert_eq!(frozen_after_vac.rows.len(), 20);
}

#[test]
fn test_secondary_index_mvcc_snapshot_isolation() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("mvcc_iso.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement(
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, category VARCHAR, val BIGINT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_cat ON docs(category);")
        .unwrap();

    db.execute_statement("INSERT INTO docs VALUES (1, 'TECH', 10);")
        .unwrap();
    db.execute_statement("INSERT INTO docs VALUES (2, 'TECH', 20);")
        .unwrap();

    // Transaction 1 updates row 1 category to 'SCIENCE' but aborts
    let mut txn1 = db.begin_transaction().unwrap();
    txn1.execute_statement("UPDATE docs SET category = 'SCIENCE' WHERE id = 1;")
        .unwrap();

    // From txn1's perspective: 'SCIENCE' has 1 row, 'TECH' has 1 row
    let res_t1_sci = txn1
        .execute_sql("SELECT id FROM docs WHERE category = 'SCIENCE';")
        .unwrap();
    assert_eq!(res_t1_sci.rows.len(), 1);
    let res_t1_tech = txn1
        .execute_sql("SELECT id FROM docs WHERE category = 'TECH';")
        .unwrap();
    assert_eq!(res_t1_tech.rows.len(), 1);

    // Abort transaction 1
    txn1.abort().unwrap();

    // After abort, autocommit still sees 2 'TECH' rows and 0 'SCIENCE' rows
    let res_after_abort = db
        .execute_sql("SELECT id FROM docs WHERE category = 'TECH';")
        .unwrap();
    assert_eq!(res_after_abort.rows.len(), 2);
    let res_after_abort_sci = db
        .execute_sql("SELECT id FROM docs WHERE category = 'SCIENCE';")
        .unwrap();
    assert_eq!(res_after_abort_sci.rows.len(), 0);

    // Now commit a real change
    let mut txn2 = db.begin_transaction().unwrap();
    txn2.execute_statement("UPDATE docs SET category = 'SCIENCE' WHERE id = 1;")
        .unwrap();
    txn2.commit().unwrap();

    // After commit, autocommit sees 'SCIENCE' = 1, 'TECH' = 1
    let res_after_commit_sci = db
        .execute_sql("SELECT id FROM docs WHERE category = 'SCIENCE';")
        .unwrap();
    assert_eq!(res_after_commit_sci.rows.len(), 1);
    let res_after_commit_tech = db
        .execute_sql("SELECT id FROM docs WHERE category = 'TECH';")
        .unwrap();
    assert_eq!(res_after_commit_tech.rows.len(), 1);
}

#[test]
fn test_secondary_index_persistence_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("reopen.treedb");

    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement(
            "CREATE TABLE catalog (id BIGINT PRIMARY KEY, brand VARCHAR, price BIGINT);",
        )
        .unwrap();
        db.execute_statement("CREATE INDEX idx_brand ON catalog(brand);")
            .unwrap();

        for i in 1..=100 {
            let brand = if i % 2 == 0 { "BrandA" } else { "BrandB" };
            db.execute_statement(&format!(
                "INSERT INTO catalog VALUES ({i}, '{brand}', {i}0);"
            ))
            .unwrap();
        }
        db.sync().unwrap();
    }

    // Reopen database from disk
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let res_a = db
            .execute_sql("SELECT id FROM catalog WHERE brand = 'BrandA';")
            .unwrap();
        assert_eq!(res_a.rows.len(), 50);

        let res_b = db
            .execute_sql("SELECT id FROM catalog WHERE brand = 'BrandB';")
            .unwrap();
        assert_eq!(res_b.rows.len(), 50);

        // Verify primary key range scan also intact across restart
        let res_pk = db
            .execute_sql("SELECT id FROM catalog WHERE id >= 10 AND id <= 20;")
            .unwrap();
        assert_eq!(res_pk.rows.len(), 11);
    }
}

#[test]
fn test_corrupt_index_or_page_detection() {
    // Test raw detection routines on invalid page buffers
    let mut corrupt_leaf = Page::new(1, PageType::Leaf);
    // Payload too short (< SEC_LEAF_HEADER_SIZE) or overflow count
    corrupt_leaf.payload_mut()[0..2].copy_from_slice(&9999u16.to_le_bytes()); // entry count 9999 exceeds payload

    let res = SecondaryLeafNode::for_each_entry_raw(&corrupt_leaf, |_k, _pl, _rid| true);
    assert!(res.is_err());
    match res {
        Err(Error::IndexCorruption(_)) => {}
        other => panic!("expected IndexCorruption error, got {other:?}"),
    }

    let mut corrupt_internal = Page::new(2, PageType::Internal);
    corrupt_internal.payload_mut()[0..2].copy_from_slice(&10u16.to_le_bytes());
    corrupt_internal.payload_mut()[2..4].copy_from_slice(&0u16.to_le_bytes()); // 0 children

    let key = IndexKey::from_value(&Value::Int64(42));
    let res_internal = SecondaryInternalNode::find_child_raw(&corrupt_internal, &key, true);
    assert!(res_internal.is_err());
    match res_internal {
        Err(Error::IndexCorruption(_)) => {}
        other => panic!("expected IndexCorruption error, got {other:?}"),
    }
}
