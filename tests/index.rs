use std::collections::{BTreeMap, BTreeSet};
use tempfile::tempdir;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::TreeDb;

fn make_test_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id".to_string(), DataType::UInt64, false),
            Column::new("name".to_string(), DataType::String, false),
            Column::new("age".to_string(), DataType::Int64, true),
            Column::new("score".to_string(), DataType::Float64, true),
            Column::new("is_active".to_string(), DataType::Boolean, false),
            Column::new("payload".to_string(), DataType::Bytes, true),
        ],
        0, // PK is `id`
    )
    .unwrap()
}

#[test]
fn test_index_creation_and_reopen_persistence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("index_creation.db");

    {
        let mut db = TreeDb::create(&db_path, 100).unwrap();
        let mut txn = db.begin_transaction().unwrap();
        txn.create_table("users", make_test_schema()).unwrap();

        // Create secondary indexes on various types
        let idx_name_id = txn
            .create_index("users", "idx_users_name", "name", true)
            .unwrap();
        assert_eq!(idx_name_id.0, 1);

        let idx_age_id = txn
            .create_index("users", "idx_users_age", "age", false)
            .unwrap();
        assert_eq!(idx_age_id.0, 2);

        txn.validate_catalog().unwrap();
        txn.commit().unwrap();
    }

    // Reopen and verify index metadata persisted and deterministic
    {
        let mut db = TreeDb::open(&db_path, 100).unwrap();
        let mut txn = db.begin_transaction().unwrap();

        let indexes = txn.list_indexes_for_table("users").unwrap();
        assert_eq!(indexes.len(), 2);

        let idx_name = txn.get_index("idx_users_name").unwrap().unwrap();
        assert_eq!(idx_name.index_id.0, 1);
        assert_eq!(idx_name.name, "idx_users_name");
        assert_eq!(idx_name.column_idx, 1);
        assert!(idx_name.unique);

        let idx_age = txn.get_index("idx_users_age").unwrap().unwrap();
        assert_eq!(idx_age.index_id.0, 2);
        assert_eq!(idx_age.name, "idx_users_age");
        assert_eq!(idx_age.column_idx, 2);
        assert!(!idx_age.unique);

        txn.validate_catalog().unwrap();
        txn.validate_index("idx_users_name").unwrap();
        txn.validate_index("idx_users_age").unwrap();
    }
}

#[test]
fn test_index_creation_validation_and_errors() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("index_validation.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();

    // 1. Missing table
    let err = txn
        .create_index("nonexistent", "idx_none", "name", false)
        .unwrap_err();
    assert!(matches!(err, Error::TableNotFound(_)));

    // 2. Missing column
    let err = txn
        .create_index("users", "idx_bad_col", "unknown_col", false)
        .unwrap_err();
    assert!(matches!(err, Error::IndexColumnNotFound(_)));

    // 3. Invalid index name (empty)
    let err = txn.create_index("users", "", "name", false).unwrap_err();
    assert!(matches!(err, Error::InvalidIndexName(_)));

    // 4. Duplicate index name
    txn.create_index("users", "idx_name", "name", false)
        .unwrap();
    let err = txn
        .create_index("users", "idx_name", "age", false)
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateIndexName(_)));

    txn.commit().unwrap();
}

#[test]
fn test_create_index_on_populated_table_with_unique_check() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("populated_index.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();

    {
        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::UInt64(1),
                Value::String("Alice".to_string()),
                Value::Int64(30),
                Value::Float64(95.5),
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();

        table
            .insert(&Row::new(vec![
                Value::UInt64(2),
                Value::String("Bob".to_string()),
                Value::Int64(30), // Duplicate age (30)
                Value::Float64(88.0),
                Value::Boolean(false),
                Value::Null,
            ]))
            .unwrap();
    }

    // Creating non-unique index on 'age' succeeds and populates
    txn.create_index("users", "idx_age", "age", false).unwrap();

    let lookup_age30 = txn
        .index_lookup("users", "idx_age", &Value::Int64(30))
        .unwrap();
    assert_eq!(lookup_age30.len(), 2);

    // Creating unique index on 'age' must fail due to duplicate 30
    let err = txn.create_index("users", "idx_age_unique", "age", true);
    assert!(err.is_err());
    assert!(matches!(err.unwrap_err(), Error::DuplicateIndexKey(_)));

    // Index metadata must not exist after failed creation
    assert!(txn.get_index("idx_age_unique").unwrap().is_none());

    txn.commit().unwrap();
}

#[test]
fn test_index_maintenance_insert_update_delete() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("index_maintenance.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_name", "name", true).unwrap();
    txn.create_index("users", "idx_age", "age", false).unwrap();

    // 1. Insert rows
    {
        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::UInt64(10),
                Value::String("Charlie".to_string()),
                Value::Int64(25),
                Value::Float64(70.0),
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();

        table
            .insert(&Row::new(vec![
                Value::UInt64(20),
                Value::String("David".to_string()),
                Value::Int64(25),
                Value::Float64(85.0),
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();

        table.validate_indexes().unwrap();
    }

    // Verify lookups
    let charlie_rids = txn
        .index_lookup("users", "idx_name", &Value::String("Charlie".to_string()))
        .unwrap();
    assert_eq!(charlie_rids.len(), 1);

    let age25_rids = txn
        .index_lookup("users", "idx_age", &Value::Int64(25))
        .unwrap();
    assert_eq!(age25_rids.len(), 2);

    // 2. Update indexed value (Charlie -> age 26, name remains Charlie)
    {
        let mut table = txn.open_table("users").unwrap();
        table
            .update(&Row::new(vec![
                Value::UInt64(10),
                Value::String("Charlie".to_string()),
                Value::Int64(26), // age changed from 25 to 26
                Value::Float64(75.0),
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();

        table.validate_indexes().unwrap();
    }

    // Old age (25) now has only 1 entry (David), new age (26) has Charlie
    assert_eq!(
        txn.index_lookup("users", "idx_age", &Value::Int64(25))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        txn.index_lookup("users", "idx_age", &Value::Int64(26))
            .unwrap()
            .len(),
        1
    );

    // 3. Delete row (David)
    {
        let mut table = txn.open_table("users").unwrap();
        table.delete(&Value::UInt64(20)).unwrap();
        table.validate_indexes().unwrap();
    }

    // Age 25 is now empty
    assert_eq!(
        txn.index_lookup("users", "idx_age", &Value::Int64(25))
            .unwrap()
            .len(),
        0
    );
    // Name David is now empty
    assert_eq!(
        txn.index_lookup("users", "idx_name", &Value::String("David".to_string()))
            .unwrap()
            .len(),
        0
    );

    txn.commit().unwrap();
}

#[test]
fn test_unique_constraint_enforcement() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("unique_constraint.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_name_unique", "name", true)
        .unwrap();

    {
        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::UInt64(1),
                Value::String("Alice".to_string()),
                Value::Int64(20),
                Value::Null,
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();

        // Duplicate insert must fail
        let err = table.insert(&Row::new(vec![
            Value::UInt64(2),
            Value::String("Alice".to_string()),
            Value::Int64(22),
            Value::Null,
            Value::Boolean(true),
            Value::Null,
        ]));
        assert!(err.is_err());
        assert!(matches!(err.unwrap_err(), Error::DuplicateIndexKey(_)));

        // Primary key 2 was NOT inserted
        assert!(table.get(&Value::UInt64(2)).unwrap().is_none());

        // Insert distinct name succeeds
        table
            .insert(&Row::new(vec![
                Value::UInt64(2),
                Value::String("Bob".to_string()),
                Value::Int64(22),
                Value::Null,
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();

        // Update Bob's name to Alice must fail
        let err = table.update(&Row::new(vec![
            Value::UInt64(2),
            Value::String("Alice".to_string()),
            Value::Int64(22),
            Value::Null,
            Value::Boolean(true),
            Value::Null,
        ]));
        assert!(err.is_err());
        assert!(matches!(err.unwrap_err(), Error::DuplicateIndexKey(_)));

        // Bob's row is still Bob
        let bob_row = table.get(&Value::UInt64(2)).unwrap().unwrap();
        assert_eq!(bob_row.values[1], Value::String("Bob".to_string()));
    }

    txn.commit().unwrap();
}

#[test]
fn test_index_range_scan_ordering_across_all_types() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("range_scan.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_age", "age", false).unwrap();
    txn.create_index("users", "idx_score", "score", false)
        .unwrap();

    let ages = vec![-100i64, -50, -1, 0, 1, 15, 25, 30, 45, 80, 100];

    {
        let mut table = txn.open_table("users").unwrap();
        for (i, &age) in ages.iter().enumerate() {
            table
                .insert(&Row::new(vec![
                    Value::UInt64((i + 1) as u64),
                    Value::String(format!("User{}", i + 1)),
                    Value::Int64(age),
                    Value::Float64(age as f64 * 1.5),
                    Value::Boolean(true),
                    Value::Null,
                ]))
                .unwrap();
        }
    }

    // Range scan on Int64: [-1, 30)
    let scan_res = txn
        .index_scan(
            "users",
            "idx_age",
            Some(&Value::Int64(-1)),
            Some(&Value::Int64(30)),
        )
        .unwrap();

    let returned_ages: Vec<i64> = scan_res
        .into_iter()
        .map(|(v, _)| match v {
            Value::Int64(a) => a,
            _ => panic!("unexpected type"),
        })
        .collect();

    assert_eq!(returned_ages, vec![-1, 0, 1, 15, 25]);

    // Full scan
    let full_scan = txn.index_scan("users", "idx_age", None, None).unwrap();
    let all_returned: Vec<i64> = full_scan
        .into_iter()
        .map(|(v, _)| match v {
            Value::Int64(a) => a,
            _ => panic!("unexpected type"),
        })
        .collect();
    assert_eq!(all_returned, ages);

    txn.commit().unwrap();
}

#[test]
fn test_index_drop_and_page_reclamation() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("index_drop.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_name", "name", false)
        .unwrap();

    {
        let mut table = txn.open_table("users").unwrap();
        for i in 1..=50 {
            table
                .insert(&Row::new(vec![
                    Value::UInt64(i),
                    Value::String(format!("Name{:04}", i)),
                    Value::Int64(20 + (i as i64 % 50)),
                    Value::Null,
                    Value::Boolean(true),
                    Value::Null,
                ]))
                .unwrap();
        }
    }

    // Drop the index
    txn.drop_index("idx_name").unwrap();

    // Verify index is gone
    assert!(txn.get_index("idx_name").unwrap().is_none());
    assert!(txn.list_indexes().unwrap().is_empty());
    txn.validate_catalog().unwrap();

    // Mutating table after index drop works seamlessly
    {
        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::UInt64(999),
                Value::String("PostDrop".to_string()),
                Value::Int64(99),
                Value::Null,
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();
    }

    txn.commit().unwrap();
}

#[test]
fn test_table_drop_cascades_to_secondary_indexes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("table_drop_cascade.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_name", "name", false)
        .unwrap();
    txn.create_index("users", "idx_age", "age", false).unwrap();

    assert_eq!(txn.list_indexes().unwrap().len(), 2);

    // Dropping table drops and frees all secondary indexes
    txn.drop_table("users").unwrap();

    assert!(txn.list_indexes().unwrap().is_empty());
    assert!(txn.get_index("idx_name").unwrap().is_none());
    assert!(txn.get_index("idx_age").unwrap().is_none());
    txn.validate_catalog().unwrap();

    txn.commit().unwrap();
}

#[test]
fn test_transaction_abort_rollbacks_index_mutations() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("txn_abort_index.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();

    // Transaction 1: Create table and index
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.create_table("users", make_test_schema()).unwrap();
        txn.create_index("users", "idx_name", "name", false)
            .unwrap();
        txn.commit().unwrap();
    }

    // Transaction 2: Insert row and abort
    {
        let mut txn = db.begin_transaction().unwrap();
        {
            let mut table = txn.open_table("users").unwrap();
            table
                .insert(&Row::new(vec![
                    Value::UInt64(1),
                    Value::String("Uncommitted".to_string()),
                    Value::Int64(20),
                    Value::Null,
                    Value::Boolean(true),
                    Value::Null,
                ]))
                .unwrap();
        }
        txn.abort().unwrap();
    }

    // Transaction 3: Verify index contains no uncommitted entries
    {
        let mut txn = db.begin_transaction().unwrap();
        let rids = txn
            .index_lookup(
                "users",
                "idx_name",
                &Value::String("Uncommitted".to_string()),
            )
            .unwrap();
        assert!(rids.is_empty());
    }
}

#[test]
fn test_crash_recovery_with_secondary_indexes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("crash_recovery_index.db");

    // Phase 1: Committed insertions
    {
        let mut db = TreeDb::create(&db_path, 100).unwrap();
        let mut txn = db.begin_transaction().unwrap();
        txn.create_table("users", make_test_schema()).unwrap();
        txn.create_index("users", "idx_name", "name", false)
            .unwrap();
        txn.create_index("users", "idx_age", "age", false).unwrap();

        {
            let mut table = txn.open_table("users").unwrap();
            for i in 1..=20 {
                table
                    .insert(&Row::new(vec![
                        Value::UInt64(i),
                        Value::String(format!("User{:02}", i)),
                        Value::Int64(20 + (i as i64 % 5)),
                        Value::Null,
                        Value::Boolean(true),
                        Value::Null,
                    ]))
                    .unwrap();
            }
        }
        txn.commit().unwrap();
    }

    // Phase 2: Uncommitted mutations followed by simulated crash
    {
        let mut db = TreeDb::open(&db_path, 100).unwrap();
        let mut txn = db.begin_transaction().unwrap();
        {
            let mut table = txn.open_table("users").unwrap();
            table
                .insert(&Row::new(vec![
                    Value::UInt64(99),
                    Value::String("CrashUser".to_string()),
                    Value::Int64(99),
                    Value::Null,
                    Value::Boolean(true),
                    Value::Null,
                ]))
                .unwrap();
        }
        // Crash without commit
    }

    // Phase 3: Recover and verify state
    {
        let mut db = TreeDb::open(&db_path, 100).unwrap();
        let mut txn = db.begin_transaction().unwrap();

        txn.validate_catalog().unwrap();
        txn.validate_index("idx_name").unwrap();
        txn.validate_index("idx_age").unwrap();

        // CrashUser must not exist
        let rids = txn
            .index_lookup("users", "idx_name", &Value::String("CrashUser".to_string()))
            .unwrap();
        assert!(rids.is_empty());

        // Committed users must all exist
        let user1_rids = txn
            .index_lookup("users", "idx_name", &Value::String("User01".to_string()))
            .unwrap();
        assert_eq!(user1_rids.len(), 1);
    }
}

#[test]
fn test_checkpoint_and_wal_truncation_with_indexes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("checkpoint_index.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_age", "age", false).unwrap();

    {
        let mut table = txn.open_table("users").unwrap();
        for i in 1..=30 {
            table
                .insert(&Row::new(vec![
                    Value::UInt64(i),
                    Value::String(format!("User{}", i)),
                    Value::Int64(i as i64),
                    Value::Null,
                    Value::Boolean(true),
                    Value::Null,
                ]))
                .unwrap();
        }
    }
    txn.commit().unwrap();

    // Checkpoint & truncate WAL
    let mut txn = db.begin_transaction().unwrap();
    txn.checkpoint_and_truncate_wal().unwrap();
    txn.commit().unwrap();

    // Reopen and verify exact equivalence
    drop(db);
    let mut db = TreeDb::open(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();

    txn.validate_catalog().unwrap();
    txn.validate_index("idx_age").unwrap();

    let res = txn
        .index_scan(
            "users",
            "idx_age",
            Some(&Value::Int64(10)),
            Some(&Value::Int64(20)),
        )
        .unwrap();
    assert_eq!(res.len(), 10);
}

#[test]
fn test_query_engine_secondary_index_selection_and_equivalence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("query_index.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("users", make_test_schema()).unwrap();
    txn.create_index("users", "idx_users_name", "name", false)
        .unwrap();
    txn.create_index("users", "idx_users_age", "age", false)
        .unwrap();

    {
        let mut table = txn.open_table("users").unwrap();
        for i in 1..=50 {
            table
                .insert(&Row::new(vec![
                    Value::UInt64(i),
                    Value::String(format!("User{:02}", i)),
                    Value::Int64(20 + (i as i64 % 10)),
                    Value::Float64(i as f64),
                    Value::Boolean(i % 2 == 0),
                    Value::Null,
                ]))
                .unwrap();
        }
    }

    // 1. SQL query matching secondary index equality: WHERE name = 'User05'
    let sql_res = txn
        .execute_sql("SELECT * FROM users WHERE name = 'User05'")
        .unwrap();
    assert_eq!(sql_res.rows.len(), 1);
    assert_eq!(sql_res.rows[0].values[0], Value::UInt64(5));

    // 2. SQL query matching secondary index + additional predicate: WHERE name = 'User05' AND age >= 18
    let sql_res2 = txn
        .execute_sql("SELECT id, name, age FROM users WHERE name = 'User05' AND age >= 18")
        .unwrap();
    assert_eq!(sql_res2.rows.len(), 1);
    assert_eq!(sql_res2.rows[0].values[0], Value::UInt64(5));

    // 3. SQL query matching secondary index range: WHERE age >= 25 AND age < 28
    let sql_res3 = txn
        .execute_sql("SELECT id, age FROM users WHERE age >= 25 AND age < 28 ORDER BY age, id")
        .unwrap();

    // Verify against full scan by dropping index temporarily in memory or comparing results
    for row in &sql_res3.rows {
        let age = match row.values[1] {
            Value::Int64(a) => a,
            _ => panic!(),
        };
        assert!((25..28).contains(&age));
    }

    txn.commit().unwrap();
}

#[test]
fn test_randomized_secondary_index_oracle() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("random_index_oracle.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("items", make_test_schema()).unwrap();
    txn.create_index("items", "idx_items_age", "age", false)
        .unwrap();

    // Reference model: BTreeMap<Int64, BTreeSet<u64>> (Age -> PKs)
    let mut reference: BTreeMap<i64, BTreeSet<u64>> = BTreeMap::new();
    let mut pk_to_age: BTreeMap<u64, i64> = BTreeMap::new();

    // Deterministic pseudo-random sequence using LCG
    let mut state: u64 = 42;
    let mut next_rand = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 32) as u32
    };

    let mut current_pk = 1u64;

    for _step in 0..200 {
        let op = next_rand() % 4;
        match op {
            0 => {
                // Insert
                let pk = current_pk;
                current_pk += 1;
                let age = (next_rand() % 30) as i64 - 10; // Range [-10, 19]

                {
                    let mut table = txn.open_table("items").unwrap();
                    table
                        .insert(&Row::new(vec![
                            Value::UInt64(pk),
                            Value::String(format!("Item{}", pk)),
                            Value::Int64(age),
                            Value::Null,
                            Value::Boolean(true),
                            Value::Null,
                        ]))
                        .unwrap();
                }

                reference.entry(age).or_default().insert(pk);
                pk_to_age.insert(pk, age);
            }
            1 => {
                // Update
                if !pk_to_age.is_empty() {
                    let keys: Vec<u64> = pk_to_age.keys().cloned().collect();
                    let target_pk = keys[next_rand() as usize % keys.len()];
                    let old_age = pk_to_age[&target_pk];
                    let new_age = (next_rand() % 30) as i64 - 10;

                    {
                        let mut table = txn.open_table("items").unwrap();
                        table
                            .update(&Row::new(vec![
                                Value::UInt64(target_pk),
                                Value::String(format!("Item{}", target_pk)),
                                Value::Int64(new_age),
                                Value::Null,
                                Value::Boolean(true),
                                Value::Null,
                            ]))
                            .unwrap();
                    }

                    if let Some(set) = reference.get_mut(&old_age) {
                        set.remove(&target_pk);
                        if set.is_empty() {
                            reference.remove(&old_age);
                        }
                    }
                    reference.entry(new_age).or_default().insert(target_pk);
                    pk_to_age.insert(target_pk, new_age);
                }
            }
            2 => {
                // Delete
                if !pk_to_age.is_empty() {
                    let keys: Vec<u64> = pk_to_age.keys().cloned().collect();
                    let target_pk = keys[next_rand() as usize % keys.len()];
                    let old_age = pk_to_age.remove(&target_pk).unwrap();

                    {
                        let mut table = txn.open_table("items").unwrap();
                        table.delete(&Value::UInt64(target_pk)).unwrap();
                    }

                    if let Some(set) = reference.get_mut(&old_age) {
                        set.remove(&target_pk);
                        if set.is_empty() {
                            reference.remove(&old_age);
                        }
                    }
                }
            }
            _ => {
                // Verification: Lookup & Range Scan
                let test_age = (next_rand() % 30) as i64 - 10;
                let expected_pks = reference.get(&test_age).cloned().unwrap_or_default();

                let mut table = txn.open_table("items").unwrap();
                let matching_rows = table
                    .index_lookup("idx_items_age", &Value::Int64(test_age))
                    .unwrap();

                let found_pks: BTreeSet<u64> = matching_rows
                    .iter()
                    .map(|r| match r.values[0] {
                        Value::UInt64(k) => k,
                        _ => panic!(),
                    })
                    .collect();

                assert_eq!(found_pks, expected_pks);
            }
        }
    }

    // Final complete verification of range scan vs reference
    let mut table = txn.open_table("items").unwrap();
    table.validate_indexes().unwrap();

    let all_rows = table.index_scan("idx_items_age", None, None).unwrap();
    let mut expected_all = Vec::new();
    for (&age, pks) in &reference {
        for &pk in pks {
            expected_all.push((age, pk));
        }
    }

    let actual_all: Vec<(i64, u64)> = all_rows
        .iter()
        .map(|r| {
            let pk = match r.values[0] {
                Value::UInt64(k) => k,
                _ => panic!(),
            };
            let age = match r.values[2] {
                Value::Int64(a) => a,
                _ => panic!(),
            };
            (age, pk)
        })
        .collect();

    assert_eq!(actual_all, expected_all);

    txn.commit().unwrap();
}

#[test]
fn test_randomized_table_index_consistency() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("random_table_index_consistency.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("people", make_test_schema()).unwrap();
    txn.create_index("people", "idx_name", "name", false)
        .unwrap();
    txn.create_index("people", "idx_age", "age", false).unwrap();
    txn.create_index("people", "idx_active", "is_active", false)
        .unwrap();

    let mut shadow_table: BTreeMap<u64, Row> = BTreeMap::new();

    let mut state: u64 = 12345;
    let mut next_rand = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 32) as u32
    };

    let mut current_id = 1u64;

    for _step in 0..150 {
        let op = next_rand() % 3;
        match op {
            0 => {
                // Insert
                let id = current_id;
                current_id += 1;
                let name = format!("Person{:03}", next_rand() % 10);
                let age = if next_rand() % 5 == 0 {
                    Value::Null
                } else {
                    Value::Int64((next_rand() % 60) as i64)
                };
                let score = Value::Float64((next_rand() % 1000) as f64 / 10.0);
                let is_active = Value::Boolean(next_rand() % 2 == 0);

                let row = Row::new(vec![
                    Value::UInt64(id),
                    Value::String(name),
                    age,
                    score,
                    is_active,
                    Value::Null,
                ]);

                {
                    let mut table = txn.open_table("people").unwrap();
                    table.insert(&row).unwrap();
                }
                shadow_table.insert(id, row);
            }
            1 => {
                // Update
                if !shadow_table.is_empty() {
                    let ids: Vec<u64> = shadow_table.keys().cloned().collect();
                    let target_id = ids[next_rand() as usize % ids.len()];

                    let name = format!("Person{:03}", next_rand() % 10);
                    let age = if next_rand() % 5 == 0 {
                        Value::Null
                    } else {
                        Value::Int64((next_rand() % 60) as i64)
                    };
                    let score = Value::Float64((next_rand() % 1000) as f64 / 10.0);
                    let is_active = Value::Boolean(next_rand() % 2 == 0);

                    let updated_row = Row::new(vec![
                        Value::UInt64(target_id),
                        Value::String(name),
                        age,
                        score,
                        is_active,
                        Value::Null,
                    ]);

                    {
                        let mut table = txn.open_table("people").unwrap();
                        table.update(&updated_row).unwrap();
                    }
                    shadow_table.insert(target_id, updated_row);
                }
            }
            _ => {
                // Delete
                if !shadow_table.is_empty() {
                    let ids: Vec<u64> = shadow_table.keys().cloned().collect();
                    let target_id = ids[next_rand() as usize % ids.len()];

                    {
                        let mut table = txn.open_table("people").unwrap();
                        table.delete(&Value::UInt64(target_id)).unwrap();
                    }
                    shadow_table.remove(&target_id);
                }
            }
        }

        // Periodically verify consistency between table rows and index reachable rows
        if _step % 25 == 0 {
            let mut table = txn.open_table("people").unwrap();
            table.validate_indexes().unwrap();

            let table_rows = table.scan(None, None).unwrap();
            assert_eq!(table_rows.len(), shadow_table.len());

            // Check name index
            let name_index_rows = table.index_scan("idx_name", None, None).unwrap();
            assert_eq!(name_index_rows.len(), shadow_table.len());

            // Check age index
            let age_index_rows = table.index_scan("idx_age", None, None).unwrap();
            assert_eq!(age_index_rows.len(), shadow_table.len());

            // Check active index
            let active_index_rows = table.index_scan("idx_active", None, None).unwrap();
            assert_eq!(active_index_rows.len(), shadow_table.len());
        }
    }

    txn.commit().unwrap();
}

#[test]
fn test_index_null_and_boundary_values() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boundary_values.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("boundary_test", make_test_schema())
        .unwrap();
    txn.create_index("boundary_test", "idx_age", "age", false)
        .unwrap();
    txn.create_index("boundary_test", "idx_score", "score", false)
        .unwrap();
    txn.create_index("boundary_test", "idx_payload", "payload", false)
        .unwrap();

    let rows = vec![
        Row::new(vec![
            Value::UInt64(1),
            Value::String("MinInt".to_string()),
            Value::Int64(i64::MIN),
            Value::Float64(f64::NEG_INFINITY),
            Value::Boolean(false),
            Value::Bytes(vec![0x00, 0x01]),
        ]),
        Row::new(vec![
            Value::UInt64(2),
            Value::String("MaxInt".to_string()),
            Value::Int64(i64::MAX),
            Value::Float64(f64::INFINITY),
            Value::Boolean(true),
            Value::Bytes(vec![0xFF, 0xFF]),
        ]),
        Row::new(vec![
            Value::UInt64(3),
            Value::String("Nulls".to_string()),
            Value::Null,
            Value::Null,
            Value::Boolean(true),
            Value::Null,
        ]),
        Row::new(vec![
            Value::UInt64(4),
            Value::String("Zero".to_string()),
            Value::Int64(0),
            Value::Float64(0.0),
            Value::Boolean(false),
            Value::Bytes(vec![]),
        ]),
    ];

    {
        let mut table = txn.open_table("boundary_test").unwrap();
        for row in &rows {
            table.insert(row).unwrap();
        }
        table.validate_indexes().unwrap();
    }

    // Lookup NULL
    let null_rids = txn
        .index_lookup("boundary_test", "idx_age", &Value::Null)
        .unwrap();
    assert_eq!(null_rids.len(), 1);

    // Lookup MinInt
    let min_rids = txn
        .index_lookup("boundary_test", "idx_age", &Value::Int64(i64::MIN))
        .unwrap();
    assert_eq!(min_rids.len(), 1);

    // Lookup MaxInt
    let max_rids = txn
        .index_lookup("boundary_test", "idx_age", &Value::Int64(i64::MAX))
        .unwrap();
    assert_eq!(max_rids.len(), 1);

    // Scan all scores: -inf < 0.0 < +inf (NULL is ordered first)
    let score_scan = txn
        .index_scan("boundary_test", "idx_score", None, None)
        .unwrap();
    assert_eq!(score_scan.len(), 4);
    assert_eq!(score_scan[0].0, Value::Null);
    assert_eq!(score_scan[1].0, Value::Float64(f64::NEG_INFINITY));
    assert_eq!(score_scan[2].0, Value::Float64(0.0));
    assert_eq!(score_scan[3].0, Value::Float64(f64::INFINITY));

    txn.commit().unwrap();
}

#[test]
fn test_index_transaction_abort_matrix() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("txn_abort_matrix.db");

    let mut db = TreeDb::create(&db_path, 100).unwrap();

    // Setup base table with 1 row
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.create_table("users", make_test_schema()).unwrap();
        let mut table = txn.open_table("users").unwrap();
        table
            .insert(&Row::new(vec![
                Value::UInt64(1),
                Value::String("Initial".to_string()),
                Value::Int64(20),
                Value::Null,
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();
        txn.commit().unwrap();
    }

    // 1. Create Index + Abort
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.create_index("users", "idx_temp", "name", false)
            .unwrap();
        txn.abort().unwrap();
    }
    {
        let mut txn = db.begin_transaction().unwrap();
        assert!(txn.get_index("idx_temp").unwrap().is_none());
    }

    // 2. Create Index + Commit
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.create_index("users", "idx_name", "name", false)
            .unwrap();
        txn.commit().unwrap();
    }

    // 3. Drop Index + Abort
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.drop_index("idx_name").unwrap();
        txn.abort().unwrap();
    }
    {
        let mut txn = db.begin_transaction().unwrap();
        assert!(txn.get_index("idx_name").unwrap().is_some());
        let rids = txn
            .index_lookup("users", "idx_name", &Value::String("Initial".to_string()))
            .unwrap();
        assert_eq!(rids.len(), 1);
    }

    // 4. Update + Abort
    {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("users").unwrap();
        table
            .update(&Row::new(vec![
                Value::UInt64(1),
                Value::String("Modified".to_string()),
                Value::Int64(25),
                Value::Null,
                Value::Boolean(true),
                Value::Null,
            ]))
            .unwrap();
        txn.abort().unwrap();
    }
    {
        let mut txn = db.begin_transaction().unwrap();
        let old_lookup = txn
            .index_lookup("users", "idx_name", &Value::String("Initial".to_string()))
            .unwrap();
        assert_eq!(old_lookup.len(), 1);
        let new_lookup = txn
            .index_lookup("users", "idx_name", &Value::String("Modified".to_string()))
            .unwrap();
        assert_eq!(new_lookup.len(), 0);
    }

    // 5. Delete + Abort
    {
        let mut txn = db.begin_transaction().unwrap();
        let mut table = txn.open_table("users").unwrap();
        table.delete(&Value::UInt64(1)).unwrap();
        txn.abort().unwrap();
    }
    {
        let mut txn = db.begin_transaction().unwrap();
        let lookup = txn
            .index_lookup("users", "idx_name", &Value::String("Initial".to_string()))
            .unwrap();
        assert_eq!(lookup.len(), 1);
    }
}
