use std::collections::BTreeMap;
use treedb::engine::catalog::TableId;
use treedb::error::Error;
use treedb::{Column, DataType, Row, Schema, TreeDb, Value, TUPLE_FORMAT_VERSION};

// ============================================================================
// Helper Utilities
// ============================================================================

fn temp_db_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("table_test.treedb");
    (dir, path)
}

fn standard_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("name", DataType::String, false),
            Column::new("age", DataType::Int64, true),
            Column::new("active", DataType::Boolean, false),
            Column::new("score", DataType::Float64, true),
            Column::new("data", DataType::Bytes, true),
        ],
        0,
    )
    .unwrap()
}

fn signed_pk_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("label", DataType::String, false),
        ],
        0,
    )
    .unwrap()
}

// ============================================================================
// 1. Types, Canonical Primary Keys & Schema Unit Tests
// ============================================================================

#[test]
fn test_types_data_type_discriminants_and_roundtrip() {
    let types = [
        (DataType::Null, 0u8),
        (DataType::Boolean, 1u8),
        (DataType::Int64, 2u8),
        (DataType::UInt64, 3u8),
        (DataType::Float64, 4u8),
        (DataType::String, 5u8),
        (DataType::Bytes, 6u8),
    ];

    for (dt, tag) in types {
        assert_eq!(dt.to_u8(), tag);
        assert_eq!(DataType::from_u8(tag).unwrap(), dt);
    }

    assert!(DataType::from_u8(99).is_err());
}

#[test]
fn test_types_value_data_type_and_is_null() {
    assert_eq!(Value::Null.data_type(), DataType::Null);
    assert!(Value::Null.is_null());

    let b = Value::Boolean(true);
    assert_eq!(b.data_type(), DataType::Boolean);
    assert!(!b.is_null());

    let i = Value::Int64(-42);
    assert_eq!(i.data_type(), DataType::Int64);
    assert!(!i.is_null());

    let u = Value::UInt64(100);
    assert_eq!(u.data_type(), DataType::UInt64);
    assert!(!u.is_null());

    let f = Value::Float64(12.3456);
    assert_eq!(f.data_type(), DataType::Float64);
    assert!(!f.is_null());

    let s = Value::String("hello".to_string());
    assert_eq!(s.data_type(), DataType::String);
    assert!(!s.is_null());

    let by = Value::Bytes(vec![1, 2, 3]);
    assert_eq!(by.data_type(), DataType::Bytes);
    assert!(!by.is_null());
}

#[test]
fn test_types_canonical_primary_key_uint64() {
    let test_keys = [0u64, 1u64, 42u64, 1000u64, u64::MAX];
    for &k in &test_keys {
        let val = Value::UInt64(k);
        let canonical = val.encode_to_canonical_key().unwrap();
        assert_eq!(canonical, k);
        let decoded = Value::decode_from_canonical_key(canonical, DataType::UInt64).unwrap();
        assert_eq!(decoded, val);
    }
}

#[test]
fn test_types_canonical_primary_key_int64_ordering() {
    let signed_keys = [
        i64::MIN,
        -1_000_000,
        -100,
        -1,
        0,
        1,
        100,
        1_000_000,
        i64::MAX,
    ];

    let mut canonical_keys = Vec::new();
    for &k in &signed_keys {
        let val = Value::Int64(k);
        let canonical = val.encode_to_canonical_key().unwrap();
        let decoded = Value::decode_from_canonical_key(canonical, DataType::Int64).unwrap();
        assert_eq!(decoded, val);
        canonical_keys.push(canonical);
    }

    // Monotonic ordering: canonical keys must be strictly monotonically increasing
    for i in 0..canonical_keys.len() - 1 {
        assert!(
            canonical_keys[i] < canonical_keys[i + 1],
            "signed ordering violated: {} not < {}",
            canonical_keys[i],
            canonical_keys[i + 1]
        );
    }
}

#[test]
fn test_types_canonical_primary_key_invalid_types_rejected() {
    assert!(Value::Null.encode_to_canonical_key().is_err());
    assert!(Value::Boolean(true).encode_to_canonical_key().is_err());
    assert!(Value::Float64(1.0).encode_to_canonical_key().is_err());
    assert!(Value::String("str".to_string())
        .encode_to_canonical_key()
        .is_err());
    assert!(Value::Bytes(vec![0]).encode_to_canonical_key().is_err());
}

#[test]
fn test_schema_validation_success_and_failures() {
    // Valid schema
    assert!(standard_schema().validate().is_ok());

    // Empty columns
    assert!(Schema::new(vec![], 0).is_err());

    // Primary key out of bounds
    let cols = vec![Column::new("id", DataType::UInt64, false)];
    assert!(Schema::new(cols, 1).is_err());

    // Duplicate column names
    let dup_cols = vec![
        Column::new("id", DataType::UInt64, false),
        Column::new("id", DataType::String, false),
    ];
    assert!(Schema::new(dup_cols, 0).is_err());

    // Nullable primary key
    let nullable_pk = vec![Column::new("id", DataType::UInt64, true)];
    assert!(Schema::new(nullable_pk, 0).is_err());

    // Unsupported PK type (e.g. String)
    let string_pk = vec![Column::new("id", DataType::String, false)];
    assert!(Schema::new(string_pk, 0).is_err());
}

// ============================================================================
// 2. Binary Tuple Serialization Tests
// ============================================================================

#[test]
fn test_tuple_serialization_all_types_roundtrip() {
    let schema = standard_schema();
    let row = Row::new(vec![
        Value::UInt64(101),
        Value::String("Alice Wonderland".to_string()),
        Value::Int64(30),
        Value::Boolean(true),
        Value::Float64(98.5),
        Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
    ]);

    let encoded = schema.encode_tuple(&row).expect("encode tuple");
    assert_eq!(encoded[0], TUPLE_FORMAT_VERSION);

    let decoded = schema.decode_tuple(&encoded).expect("decode tuple");
    assert_eq!(decoded, row);
}

#[test]
fn test_tuple_null_bitmap_handling() {
    let schema = standard_schema();

    // Row with some null fields
    let row = Row::new(vec![
        Value::UInt64(102),
        Value::String("Bob".to_string()),
        Value::Null,
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    let encoded = schema.encode_tuple(&row).expect("encode tuple");
    let decoded = schema.decode_tuple(&encoded).expect("decode tuple");
    assert_eq!(decoded, row);
    assert_eq!(decoded.get(2), Some(&Value::Null));
    assert_eq!(decoded.get(4), Some(&Value::Null));
    assert_eq!(decoded.get(5), Some(&Value::Null));
}

#[test]
fn test_tuple_validation_type_and_null_mismatch() {
    let schema = standard_schema();

    // Column count mismatch
    let short_row = Row::new(vec![Value::UInt64(1)]);
    assert!(matches!(
        schema.encode_tuple(&short_row).unwrap_err(),
        Error::ColumnCountMismatch { .. }
    ));

    // Null constraint violation on non-nullable "name"
    let null_name_row = Row::new(vec![
        Value::UInt64(1),
        Value::Null,
        Value::Int64(20),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);
    assert!(matches!(
        schema.encode_tuple(&null_name_row).unwrap_err(),
        Error::NullConstraintViolation { .. }
    ));

    // Type mismatch: String in UInt64 column
    let bad_type_row = Row::new(vec![
        Value::String("not_int".to_string()),
        Value::String("Alice".to_string()),
        Value::Int64(20),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);
    assert!(matches!(
        schema.encode_tuple(&bad_type_row).unwrap_err(),
        Error::TypeMismatch { .. }
    ));
}

// ============================================================================
// 3. Relational Table CRUD Operations
// ============================================================================

#[test]
fn test_table_create_and_open() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    let tid = db
        .create_table("users", standard_schema())
        .expect("create table");
    assert_eq!(tid, TableId(1));

    let table = db.open_table("users").expect("open table");
    assert_eq!(table.name(), "users");
    assert_eq!(table.table_id(), TableId(1));
    assert_eq!(*table.schema(), standard_schema());
}

#[test]
fn test_table_insert_and_get_uint64_pk() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("users", standard_schema())
        .expect("create table");

    let row1 = Row::new(vec![
        Value::UInt64(1),
        Value::String("Alice".to_string()),
        Value::Int64(25),
        Value::Boolean(true),
        Value::Float64(99.0),
        Value::Bytes(vec![1, 2, 3]),
    ]);

    let row2 = Row::new(vec![
        Value::UInt64(2),
        Value::String("Bob".to_string()),
        Value::Null,
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    {
        let mut table = db.open_table("users").expect("open table");
        table.insert(&row1).expect("insert row1");
        table.insert(&row2).expect("insert row2");
    }

    {
        let mut table = db.open_table("users").expect("open table");
        let found1 = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(found1, row1);

        let found2 = table.get(&Value::UInt64(2)).unwrap().unwrap();
        assert_eq!(found2, row2);

        let missing = table.get(&Value::UInt64(999)).unwrap();
        assert_eq!(missing, None);
    }
}

#[test]
fn test_table_insert_and_get_int64_pk() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("signed_items", signed_pk_schema())
        .expect("create table");

    let rows = vec![
        Row::new(vec![
            Value::Int64(-500),
            Value::String("negative large".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(-1),
            Value::String("negative one".to_string()),
        ]),
        Row::new(vec![Value::Int64(0), Value::String("zero".to_string())]),
        Row::new(vec![
            Value::Int64(1),
            Value::String("positive one".to_string()),
        ]),
        Row::new(vec![
            Value::Int64(500),
            Value::String("positive large".to_string()),
        ]),
    ];

    {
        let mut table = db.open_table("signed_items").expect("open table");
        for r in &rows {
            table.insert(r).expect("insert row");
        }
    }

    {
        let mut table = db.open_table("signed_items").expect("open table");
        for r in &rows {
            let pk = r.get(0).unwrap();
            let found = table.get(pk).unwrap().unwrap();
            assert_eq!(&found, r);
        }
    }
}

#[test]
fn test_table_duplicate_pk_rejected() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("users", standard_schema())
        .expect("create table");

    let row1 = Row::new(vec![
        Value::UInt64(42),
        Value::String("Alice".to_string()),
        Value::Int64(30),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);

    let row2 = Row::new(vec![
        Value::UInt64(42),
        Value::String("Alice Clone".to_string()),
        Value::Int64(31),
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    let mut table = db.open_table("users").expect("open table");
    table.insert(&row1).expect("insert row1");

    let err = table.insert(&row2).unwrap_err();
    assert!(
        matches!(err, Error::DuplicatePrimaryKey(_)),
        "expected DuplicatePrimaryKey, got {err:?}"
    );
}

#[test]
fn test_table_update_existing_and_missing() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("users", standard_schema())
        .expect("create table");

    let row = Row::new(vec![
        Value::UInt64(1),
        Value::String("Original".to_string()),
        Value::Int64(20),
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    let updated_row = Row::new(vec![
        Value::UInt64(1),
        Value::String("Updated".to_string()),
        Value::Int64(25),
        Value::Boolean(true),
        Value::Float64(100.0),
        Value::Bytes(vec![7, 8, 9]),
    ]);

    let missing_row = Row::new(vec![
        Value::UInt64(999),
        Value::String("Missing".to_string()),
        Value::Int64(99),
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    let mut table = db.open_table("users").expect("open table");
    table.insert(&row).expect("insert");

    // Successful update
    table.update(&updated_row).expect("update");
    let found = table.get(&Value::UInt64(1)).unwrap().unwrap();
    assert_eq!(found, updated_row);

    // Missing row update fails
    let err = table.update(&missing_row).unwrap_err();
    assert!(
        matches!(err, Error::RowNotFound),
        "expected RowNotFound, got {err:?}"
    );
}

#[test]
fn test_table_delete_existing_and_missing() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("users", standard_schema())
        .expect("create table");

    let row = Row::new(vec![
        Value::UInt64(1),
        Value::String("To Delete".to_string()),
        Value::Int64(20),
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    let mut table = db.open_table("users").expect("open table");
    table.insert(&row).expect("insert");

    // Delete existing
    let deleted = table.delete(&Value::UInt64(1)).unwrap().unwrap();
    assert_eq!(deleted, row);
    assert_eq!(table.get(&Value::UInt64(1)).unwrap(), None);

    // Delete missing
    let missing = table.delete(&Value::UInt64(1)).unwrap();
    assert_eq!(missing, None);
}

// ============================================================================
// 4. Ordered Range Scan Tests
// ============================================================================

#[test]
fn test_table_range_scan_uint64() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("users", standard_schema())
        .expect("create table");

    let mut table = db.open_table("users").expect("open table");
    for id in [10, 20, 30, 40, 50] {
        let row = Row::new(vec![
            Value::UInt64(id),
            Value::String(format!("User {id}")),
            Value::Int64(id as i64),
            Value::Boolean(true),
            Value::Null,
            Value::Null,
        ]);
        table.insert(&row).expect("insert");
    }

    // Full scan: [None, None]
    let all = table.scan(None, None).expect("full scan");
    assert_eq!(all.len(), 5);
    assert_eq!(all[0].get(0), Some(&Value::UInt64(10)));
    assert_eq!(all[4].get(0), Some(&Value::UInt64(50)));

    // Bounded scan: [20, 45) -> 20, 30, 40
    let bounded = table
        .scan(Some(&Value::UInt64(20)), Some(&Value::UInt64(45)))
        .expect("bounded scan");
    assert_eq!(bounded.len(), 3);
    assert_eq!(bounded[0].get(0), Some(&Value::UInt64(20)));
    assert_eq!(bounded[1].get(0), Some(&Value::UInt64(30)));
    assert_eq!(bounded[2].get(0), Some(&Value::UInt64(40)));

    // Suffix scan: [30, None) -> 30, 40, 50
    let suffix = table
        .scan(Some(&Value::UInt64(30)), None)
        .expect("suffix scan");
    assert_eq!(suffix.len(), 3);
    assert_eq!(suffix[0].get(0), Some(&Value::UInt64(30)));
    assert_eq!(suffix[2].get(0), Some(&Value::UInt64(50)));

    // Prefix scan: [None, 30) -> 10, 20
    let prefix = table
        .scan(None, Some(&Value::UInt64(30)))
        .expect("prefix scan");
    assert_eq!(prefix.len(), 2);
    assert_eq!(prefix[0].get(0), Some(&Value::UInt64(10)));
    assert_eq!(prefix[1].get(0), Some(&Value::UInt64(20)));
}

#[test]
fn test_table_range_scan_int64_ordering() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("signed_items", signed_pk_schema())
        .expect("create table");

    let keys = [-100i64, -50, -1, 0, 1, 50, 100];
    let mut table = db.open_table("signed_items").expect("open table");

    // Insert in shuffled order
    for &k in &[0i64, 50, -50, 100, -100, 1, -1] {
        let row = Row::new(vec![Value::Int64(k), Value::String(format!("val {k}"))]);
        table.insert(&row).expect("insert");
    }

    // Full scan must be sorted monotonically from -100 to 100
    let all = table.scan(None, None).expect("full scan");
    assert_eq!(all.len(), keys.len());
    for (idx, &expected_k) in keys.iter().enumerate() {
        assert_eq!(all[idx].get(0), Some(&Value::Int64(expected_k)));
    }

    // Range scan [-50, 50) -> -50, -1, 0, 1
    let sub = table
        .scan(Some(&Value::Int64(-50)), Some(&Value::Int64(50)))
        .expect("sub scan");
    assert_eq!(sub.len(), 4);
    assert_eq!(sub[0].get(0), Some(&Value::Int64(-50)));
    assert_eq!(sub[1].get(0), Some(&Value::Int64(-1)));
    assert_eq!(sub[2].get(0), Some(&Value::Int64(0)));
    assert_eq!(sub[3].get(0), Some(&Value::Int64(1)));
}

// ============================================================================
// 5. Multi-Page Overflow Tuples
// ============================================================================

#[test]
fn test_table_multi_page_overflow_tuples() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 50).expect("create database");

    db.create_table("big_docs", standard_schema())
        .expect("create table");

    // 15 KB text string and 20 KB binary payload (both exceed single 4096-byte page)
    let large_text = "A".repeat(15_000);
    let large_bytes = vec![0xAB; 20_000];

    let row = Row::new(vec![
        Value::UInt64(1),
        Value::String(large_text.clone()),
        Value::Int64(42),
        Value::Boolean(true),
        Value::Float64(78.9012),
        Value::Bytes(large_bytes.clone()),
    ]);

    {
        let mut table = db.open_table("big_docs").expect("open table");
        table.insert(&row).expect("insert overflow row");
    }

    // Verify retrieval across overflow chain
    {
        let mut table = db.open_table("big_docs").expect("open table");
        let retrieved = table.get(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(retrieved, row);
    }

    // Verify deletion reclaims all overflow pages
    {
        let mut table = db.open_table("big_docs").expect("open table");
        let deleted = table.delete(&Value::UInt64(1)).unwrap().unwrap();
        assert_eq!(deleted, row);
        assert_eq!(table.get(&Value::UInt64(1)).unwrap(), None);
    }

    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}

// ============================================================================
// 6. Transaction Integration Tests
// ============================================================================

#[test]
fn test_table_transaction_commit_and_abort() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("users", standard_schema())
        .expect("create table");

    let row1 = Row::new(vec![
        Value::UInt64(1),
        Value::String("Alice".to_string()),
        Value::Int64(20),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);

    let row2 = Row::new(vec![
        Value::UInt64(2),
        Value::String("Bob".to_string()),
        Value::Int64(25),
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    // 1. Transaction insert -> Abort
    {
        let mut txn = db.begin().expect("begin txn");
        {
            let mut table = txn.open_table("users").expect("open table");
            table.insert(&row1).expect("insert row1");
            assert_eq!(table.get(&Value::UInt64(1)).unwrap(), Some(row1.clone()));
        }
        txn.abort().expect("abort txn");
    }

    // Verify row1 was rolled back
    {
        let mut table = db.open_table("users").expect("open table");
        assert_eq!(table.get(&Value::UInt64(1)).unwrap(), None);
    }

    // 2. Transaction insert -> Commit
    {
        let mut txn = db.begin().expect("begin txn");
        {
            let mut table = txn.open_table("users").expect("open table");
            table.insert(&row2).expect("insert row2");
        }
        txn.commit().expect("commit txn");
    }

    // Verify row2 is committed and visible
    {
        let mut table = db.open_table("users").expect("open table");
        assert_eq!(table.get(&Value::UInt64(2)).unwrap(), Some(row2));
    }
}

#[test]
fn test_table_multi_table_atomic_transaction() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 10).expect("create database");

    db.create_table("accounts", standard_schema())
        .expect("create accounts");
    db.create_table("transfers", standard_schema())
        .expect("create transfers");

    let acc1 = Row::new(vec![
        Value::UInt64(1),
        Value::String("Checking".to_string()),
        Value::Int64(1000),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);

    let tx1 = Row::new(vec![
        Value::UInt64(101),
        Value::String("Wire $500".to_string()),
        Value::Int64(500),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);

    // Multi-table transaction abort
    {
        let mut txn = db.begin().expect("begin txn");
        {
            let mut acc_tbl = txn.open_table("accounts").expect("open accounts");
            acc_tbl.insert(&acc1).expect("insert acc");
        }
        {
            let mut tx_tbl = txn.open_table("transfers").expect("open transfers");
            tx_tbl.insert(&tx1).expect("insert tx");
        }
        txn.abort().expect("abort multi-table txn");
    }

    {
        let mut acc_tbl = db.open_table("accounts").expect("open accounts");
        assert_eq!(acc_tbl.get(&Value::UInt64(1)).unwrap(), None);

        let mut tx_tbl = db.open_table("transfers").expect("open transfers");
        assert_eq!(tx_tbl.get(&Value::UInt64(101)).unwrap(), None);
    }
}

// ============================================================================
// 7. Crash Recovery & Reopen Tests
// ============================================================================

#[test]
fn test_table_crash_recovery() {
    let (_dir, path) = temp_db_path();

    let committed_row = Row::new(vec![
        Value::UInt64(10),
        Value::String("Durable".to_string()),
        Value::Int64(100),
        Value::Boolean(true),
        Value::Null,
        Value::Null,
    ]);

    let uncommitted_row = Row::new(vec![
        Value::UInt64(20),
        Value::String("Crashed".to_string()),
        Value::Int64(200),
        Value::Boolean(false),
        Value::Null,
        Value::Null,
    ]);

    {
        let mut db = TreeDb::create(&path, 10).expect("create database");
        db.create_table("records", standard_schema())
            .expect("create table");

        // Committed insert
        {
            let mut txn = db.begin().expect("begin txn");
            let mut table = txn.open_table("records").expect("open table");
            table.insert(&committed_row).expect("insert committed");
            txn.commit().expect("commit");
        }

        // Uncommitted insert followed by sudden crash
        let mut txn = db.begin().expect("begin txn");
        let mut table = txn.open_table("records").expect("open table");
        table.insert(&uncommitted_row).expect("insert uncommitted");
        std::mem::forget(txn); // Bypass drop handler to simulate hard crash
    }

    // Reopen database and trigger ARIES recovery
    let mut db = TreeDb::open(&path, 10).expect("reopen database");
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");

    let mut table = db.open_table("records").expect("open table");
    assert_eq!(table.get(&Value::UInt64(10)).unwrap(), Some(committed_row));
    assert_eq!(table.get(&Value::UInt64(20)).unwrap(), None);
}

// ============================================================================
// 8. Randomized Table Oracle Comparison Test
// ============================================================================

#[test]
fn test_table_randomized_oracle_comparison() {
    let (_dir, path) = temp_db_path();
    let mut db = TreeDb::create(&path, 50).expect("create database");

    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("payload", DataType::String, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    db.create_table("oracle_table", schema.clone())
        .expect("create table");

    let mut oracle: BTreeMap<i64, Row> = BTreeMap::new();
    let mut lcg: u64 = 0x7AB1_E08A_C1E0_BEEF;
    let mut next_rand = || {
        lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1);
        lcg
    };

    let op_count = 150;

    for step in 0..op_count {
        let op_type = next_rand() % 5;
        let id = ((next_rand() % 50) as i64) - 25; // [-25, 24] range

        match op_type {
            // Op 0: Insert
            0 => {
                let row = Row::new(vec![
                    Value::Int64(id),
                    Value::String(format!("Payload for {id} at step {step}")),
                    if (next_rand() % 3) == 0 {
                        Value::Null
                    } else {
                        Value::Int64((next_rand() % 1000) as i64)
                    },
                ]);

                let should_succeed = !oracle.contains_key(&id);
                let is_txn = (next_rand() % 2) == 1;

                if is_txn {
                    let mut txn = db.begin().expect("begin txn");
                    let res = {
                        let mut tbl = txn.open_table("oracle_table").expect("open");
                        tbl.insert(&row)
                    };

                    if should_succeed {
                        res.expect("insert should succeed");
                        let commit = (next_rand() % 10) < 7;
                        if commit {
                            txn.commit().expect("commit");
                            oracle.insert(id, row);
                        } else {
                            txn.abort().expect("abort");
                        }
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::DuplicatePrimaryKey(_)));
                        txn.abort().expect("abort");
                    }
                } else {
                    let mut tbl = db.open_table("oracle_table").expect("open");
                    let res = tbl.insert(&row);
                    if should_succeed {
                        res.expect("insert should succeed");
                        oracle.insert(id, row);
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::DuplicatePrimaryKey(_)));
                    }
                }
            }

            // Op 1: Get
            1 => {
                let mut tbl = db.open_table("oracle_table").expect("open");
                let actual = tbl.get(&Value::Int64(id)).expect("get");
                let expected = oracle.get(&id).cloned();
                assert_eq!(actual, expected, "mismatch on get id={id}");
            }

            // Op 2: Update
            2 => {
                let row = Row::new(vec![
                    Value::Int64(id),
                    Value::String(format!("Updated {id} at step {step}")),
                    Value::Int64(step as i64),
                ]);

                let should_succeed = oracle.contains_key(&id);
                let is_txn = (next_rand() % 2) == 1;

                if is_txn {
                    let mut txn = db.begin().expect("begin txn");
                    let res = {
                        let mut tbl = txn.open_table("oracle_table").expect("open");
                        tbl.update(&row)
                    };

                    if should_succeed {
                        res.expect("update should succeed");
                        let commit = (next_rand() % 10) < 7;
                        if commit {
                            txn.commit().expect("commit");
                            oracle.insert(id, row);
                        } else {
                            txn.abort().expect("abort");
                        }
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::RowNotFound));
                        txn.abort().expect("abort");
                    }
                } else {
                    let mut tbl = db.open_table("oracle_table").expect("open");
                    let res = tbl.update(&row);
                    if should_succeed {
                        res.expect("update should succeed");
                        oracle.insert(id, row);
                    } else {
                        assert!(matches!(res.unwrap_err(), Error::RowNotFound));
                    }
                }
            }

            // Op 3: Delete
            3 => {
                let should_succeed = oracle.contains_key(&id);
                let is_txn = (next_rand() % 2) == 1;

                if is_txn {
                    let mut txn = db.begin().expect("begin txn");
                    let res = {
                        let mut tbl = txn.open_table("oracle_table").expect("open");
                        tbl.delete(&Value::Int64(id))
                    };

                    if should_succeed {
                        let deleted = res.expect("delete should succeed");
                        assert!(deleted.is_some());
                        let commit = (next_rand() % 10) < 7;
                        if commit {
                            txn.commit().expect("commit");
                            oracle.remove(&id);
                        } else {
                            txn.abort().expect("abort");
                        }
                    } else {
                        let deleted = res.expect("delete missing should return None");
                        assert_eq!(deleted, None);
                        txn.abort().expect("abort");
                    }
                } else {
                    let mut tbl = db.open_table("oracle_table").expect("open");
                    let res = tbl.delete(&Value::Int64(id)).expect("delete");
                    if should_succeed {
                        assert!(res.is_some());
                        oracle.remove(&id);
                    } else {
                        assert_eq!(res, None);
                    }
                }
            }

            // Op 4: Range scan
            _ => {
                let mut tbl = db.open_table("oracle_table").expect("open");
                let all_rows = tbl.scan(None, None).expect("scan all");

                let expected_rows: Vec<Row> = oracle.values().cloned().collect();
                assert_eq!(
                    all_rows.len(),
                    expected_rows.len(),
                    "scan count mismatch at step {step}"
                );
                for (a, e) in all_rows.iter().zip(expected_rows.iter()) {
                    assert_eq!(a, e, "row mismatch during full scan");
                }
            }
        }

        // Periodic checkpoint
        if step % 30 == 0 {
            db.checkpoint_and_truncate_wal().expect("checkpoint");
        }
    }

    // Final scan verification
    let mut tbl = db.open_table("oracle_table").expect("open");
    let final_rows = tbl.scan(None, None).expect("final scan");
    let expected_rows: Vec<Row> = oracle.values().cloned().collect();
    assert_eq!(final_rows, expected_rows);

    tbl.validate_tree().expect("validate tree");
    db.validate_catalog().expect("validate catalog");
    db.buffer_pool_mut()
        .validate_freelist()
        .expect("validate freelist");
}
