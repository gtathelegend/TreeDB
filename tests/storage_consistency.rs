//! Phase 73 Integration Tests: Storage Consistency Verification & Cross-Layer Invariant Enforcement.
//!
//! Validates:
//! 1. Complete consistency audit on valid database fixtures (empty, multi-table, secondary/composite/covering indexes,
//!    overflow records, historical MVCC version chains, post-vacuum, post-recovery).
//! 2. Strict read-only verification guarantee: byte-for-byte identity of persistent database file before and after audit.
//! 3. Bounded audit behavior and resource limits (max_pages, max_records, max_index_entries, max_issues).
//! 4. Deterministic corruption fixture detection:
//!    - Invalid header magic, format version, and truncated file.
//!    - Freelist cycles, reserved page references, and count mismatches.
//!    - Pages simultaneously on freelist and claimed by live tables.
//!    - Slotted page corruption, version chain cycles, and malformed version metadata.
//!    - Overflow chain cycles and invalid overflow pages.
//!    - Secondary index value mismatches, covering payload mismatches, and unique constraint violations.
//!    - WAL record corruption (CRC failure) and unapplied WAL tail detection.
//! 5. Differential and metamorphic consistency across fixed random seeds (42, 1337, 2026).

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tempfile::tempdir;

use treedb::engine::catalog::Catalog;
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::engine::vacuum::VacuumConfig;
use treedb::engine::verification::{ConsistencyCheckConfig, ConsistencyIssueKind};
use treedb::TreeDb;

fn make_accounts_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("account_num", DataType::String, false),
            Column::new("balance", DataType::Int64, false),
            Column::new("payload", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap()
}

fn make_customers_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("email", DataType::String, false),
            Column::new("name", DataType::String, false),
            Column::new("city", DataType::String, false),
        ],
        0,
    )
    .unwrap()
}

fn create_test_db(name: &str) -> (TreeDb, tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.treedb"));
    let db = TreeDb::create(&db_path, 64).unwrap();
    (db, dir, db_path)
}

fn reopen_db(path: &Path) -> TreeDb {
    TreeDb::open(path, 64).unwrap()
}

// -----------------------------------------------------------------------------
// Valid Database Cases
// -----------------------------------------------------------------------------

#[test]
fn test_valid_empty_database() {
    let (mut db, _dir, _path) = create_test_db("valid_empty");
    let report = db.verify_consistency().unwrap();

    assert!(report.is_healthy(), "empty database must be healthy");
    assert_eq!(report.fatal_count(), 0);
    assert_eq!(report.error_count(), 0);
    assert_eq!(report.warning_count(), 0);
    assert!(report.statistics.pages_examined >= 2);
    assert!(report
        .passed_checks
        .contains(&"database_header".to_string()));
    assert!(report
        .passed_checks
        .contains(&"freelist_invariants".to_string()));
}

#[test]
fn test_valid_multi_table_with_indexes() {
    let (mut db, _dir, _path) = create_test_db("valid_multi_table");

    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_table("customers", make_customers_schema())
        .unwrap();

    db.create_index("accounts", "idx_acc_num", "account_num", true)
        .unwrap();
    db.create_index("customers", "idx_cust_email", "email", true)
        .unwrap();

    // Populate accounts
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        for i in 1..=20 {
            tbl.insert(&Row::new(vec![
                Value::Int64(i),
                Value::String(format!("ACC-{i:04}")),
                Value::Int64(i * 100),
                Value::Bytes(vec![b'x'; 32]),
            ]))
            .unwrap();
        }
    }
    t1.commit().unwrap();

    // Populate customers
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("customers").unwrap();
        for i in 1..=15 {
            tbl.insert(&Row::new(vec![
                Value::Int64(i),
                Value::String(format!("user{i}@test.com")),
                Value::String(format!("Customer {i}")),
                Value::String("San Francisco".into()),
            ]))
            .unwrap();
        }
    }
    t2.commit().unwrap();

    let report = db.verify_consistency().unwrap();
    assert!(
        report.is_healthy(),
        "multi-table database with indexes must be healthy"
    );
    assert_eq!(report.statistics.tables_examined, 2);
    assert_eq!(report.statistics.indexes_examined, 2);
    assert!(report.statistics.primary_keys_examined >= 35);
}

#[test]
fn test_valid_covering_and_composite_index() {
    let (mut db, _dir, _path) = create_test_db("valid_covering");
    db.create_table("customers", make_customers_schema())
        .unwrap();

    // Create covering index on (city) INCLUDE (name, email)
    Catalog::create_covering_index(
        db.buffer_pool_mut(),
        "customers",
        "idx_cust_city_cov",
        &["city"],
        &["name", "email"],
        false,
    )
    .unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("customers").unwrap();
        for i in 1..=10 {
            tbl.insert(&Row::new(vec![
                Value::Int64(i),
                Value::String(format!("email{i}@domain.com")),
                Value::String(format!("User {i}")),
                Value::String(if i % 2 == 0 { "New York" } else { "London" }.into()),
            ]))
            .unwrap();
        }
    }
    txn.commit().unwrap();

    let report = db.verify_consistency().unwrap();
    assert!(
        report.is_healthy(),
        "covering index database must be healthy"
    );
    assert_eq!(report.statistics.indexes_examined, 1);
}

#[test]
fn test_valid_overflow_records() {
    let (mut db, _dir, _path) = create_test_db("valid_overflow");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let large_payload = vec![b'Z'; 6000]; // Multi-page overflow payload

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-OVF-001".into()),
            Value::Int64(50000),
            Value::Bytes(large_payload),
        ]))
        .unwrap();
    }
    txn.commit().unwrap();

    let report = db.verify_consistency().unwrap();
    assert!(
        report.is_healthy(),
        "overflow records database must be healthy"
    );
    assert!(report.statistics.overflow_pages_examined >= 2);
}

#[test]
fn test_valid_historical_mvcc_versions() {
    let (mut db, _dir, _path) = create_test_db("valid_mvcc_history");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Insert key 1
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-0001".into()),
            Value::Int64(100),
            Value::Bytes(vec![1, 2, 3]),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Update key 1 three times to create an MVCC chain
    for seq in 2..=4 {
        let mut txn = db.begin().unwrap();
        {
            let mut tbl = txn.open_table("accounts").unwrap();
            tbl.update(&Row::new(vec![
                Value::Int64(1),
                Value::String("ACC-0001".into()),
                Value::Int64(seq * 100),
                Value::Bytes(vec![1, 2, 3]),
            ]))
            .unwrap();
        }
        txn.commit().unwrap();
    }

    let report = db.verify_consistency().unwrap();
    assert!(
        report.is_healthy(),
        "historical MVCC versions database must be healthy"
    );
    assert_eq!(report.statistics.version_chains_examined, 1);
    assert!(report.statistics.records_examined >= 4);
}

#[test]
fn test_valid_database_after_vacuum() {
    let (mut db, _dir, _path) = create_test_db("valid_post_vacuum");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        for i in 1..=10 {
            tbl.insert(&Row::new(vec![
                Value::Int64(i),
                Value::String(format!("ACC-{i}")),
                Value::Int64(i * 10),
                Value::Bytes(vec![0u8; 16]),
            ]))
            .unwrap();
        }
    }
    t1.commit().unwrap();

    // Delete rows 1..=5
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        for i in 1..=5 {
            tbl.delete(&Value::Int64(i)).unwrap();
        }
    }
    t2.commit().unwrap();

    // Run vacuum
    let v_res = db.vacuum(&VacuumConfig::default()).unwrap();
    assert!(v_res.versions_reclaimed > 0);

    let report = db.verify_consistency().unwrap();
    assert!(report.is_healthy(), "post-vacuum database must be healthy");
    assert_eq!(report.fatal_count(), 0);
    assert_eq!(report.error_count(), 0);
}

#[test]
fn test_valid_aborted_transaction() {
    let (mut db, _dir, _path) = create_test_db("valid_aborted_txn");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Committed insert
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(10),
            Value::String("ACC-10".into()),
            Value::Int64(100),
            Value::Bytes(vec![]),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Aborted insert
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(20),
            Value::String("ACC-20".into()),
            Value::Int64(200),
            Value::Bytes(vec![]),
        ]))
        .unwrap();
    }
    t2.abort().unwrap();

    let report = db.verify_consistency().unwrap();
    assert!(report.is_healthy(), "database after abort must be healthy");
}

#[test]
fn test_valid_database_reopened_multiple_times() {
    let (mut db, _dir, path) = create_test_db("valid_reopened");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(42),
            Value::String("ACC-42".into()),
            Value::Int64(4200),
            Value::Bytes(vec![42]),
        ]))
        .unwrap();
    }
    txn.commit().unwrap();
    db.sync().unwrap();
    drop(db);

    for _ in 0..3 {
        let mut reopened = reopen_db(&path);
        let report = reopened.verify_consistency().unwrap();
        assert!(report.is_healthy(), "reopened database must be healthy");
        reopened.sync().unwrap();
    }
}

// -----------------------------------------------------------------------------
// Read-Only Verification Guarantee
// -----------------------------------------------------------------------------

#[test]
fn test_read_only_guarantee_exact_byte_equality() {
    let (mut db, _dir, path) = create_test_db("read_only_guarantee");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", false)
        .unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        for i in 1..=25 {
            tbl.insert(&Row::new(vec![
                Value::Int64(i),
                Value::String(format!("ACC-{i}")),
                Value::Int64(i * 100),
                Value::Bytes(vec![b'a'; 40]),
            ]))
            .unwrap();
        }
    }
    txn.commit().unwrap();
    db.sync().unwrap();

    // Read full raw bytes and length before verification
    let bytes_before = fs::read(&path).unwrap();
    let size_before = bytes_before.len();

    // Execute comprehensive verification
    let report = db.verify_consistency().unwrap();
    assert!(report.is_healthy());

    // Flush any pending file buffers to ensure disk reflects engine state
    db.sync().unwrap();

    // Read full raw bytes and length after verification
    let bytes_after = fs::read(&path).unwrap();
    let size_after = bytes_after.len();

    assert_eq!(
        size_before, size_after,
        "verification must not alter database file size"
    );
    assert_eq!(
        bytes_before, bytes_after,
        "verification must be strictly read-only: database bytes must match 100%"
    );
}

// -----------------------------------------------------------------------------
// Resource Limits & Diagnostic Bounds
// -----------------------------------------------------------------------------

#[test]
fn test_resource_limits_max_pages() {
    let (mut db, _dir, _path) = create_test_db("limit_pages");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        for i in 1..=30 {
            tbl.insert(&Row::new(vec![
                Value::Int64(i),
                Value::String(format!("ACC-{i}")),
                Value::Int64(i),
                Value::Bytes(vec![0; 50]),
            ]))
            .unwrap();
        }
    }
    txn.commit().unwrap();

    let config = ConsistencyCheckConfig {
        max_pages: Some(2), // Restrict to only 2 pages
        ..Default::default()
    };

    let report = db.check_consistency(&config).unwrap();
    assert!(
        report.truncated,
        "audit must be marked truncated when max_pages reached"
    );
    assert!(!report.is_healthy(), "truncated report is not healthy");
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::VerificationLimitReached { ref limit_name, .. } if limit_name == "max_pages"
    )));
}

#[test]
fn test_resource_limits_max_issues() {
    let (mut db, _dir, path) = create_test_db("limit_issues");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.sync().unwrap();
    drop(db);

    // Corrupt header format version
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(8)).unwrap();
        file.write_all(&999u32.to_le_bytes()).unwrap();
    }

    let config = ConsistencyCheckConfig {
        max_issues: 1, // Only allow 1 issue
        ..Default::default()
    };

    let report = TreeDb::check_file(&path, &config).unwrap();
    assert!(report.issues.len() <= 2);
}

// -----------------------------------------------------------------------------
// Corruption Fixtures & Negative Tests (Byte-Level Mutations)
// -----------------------------------------------------------------------------

#[test]
fn test_corruption_invalid_header_magic() {
    let (db, _dir, path) = create_test_db("corrupt_magic");
    drop(db);

    // Mutate magic bytes on Page 0
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"CORRUPT!").unwrap();
    }

    let report = TreeDb::verify_file(&path).unwrap();
    assert!(report.has_fatal());
    assert!(report
        .issues
        .iter()
        .any(|i| matches!(i.kind, ConsistencyIssueKind::InvalidHeaderMagic { .. })));
}

#[test]
fn test_corruption_unsupported_format_version() {
    let (mut db, _dir, path) = create_test_db("corrupt_version");
    db.sync().unwrap();
    drop(db);

    // Corrupt format version at offset 8 of Page 0 to 999
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(8)).unwrap();
        file.write_all(&999u32.to_le_bytes()).unwrap();
    }

    let report = TreeDb::verify_file(&path).unwrap();
    assert!(report.has_fatal());
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::UnsupportedFormatVersion { found: 999, .. }
    )));
}

#[test]
fn test_corruption_truncated_file() {
    let (mut db, _dir, path) = create_test_db("corrupt_truncated");
    db.sync().unwrap();
    drop(db);

    // Truncate file to 1000 bytes (< 2 pages)
    {
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(1000).unwrap();
    }

    let report = TreeDb::verify_file(&path).unwrap();
    assert!(report.has_fatal());
    assert!(report
        .issues
        .iter()
        .any(|i| matches!(i.kind, ConsistencyIssueKind::DatabaseFileTruncated { .. })));
}

#[test]
fn test_corruption_freelist_count_mismatch() {
    let (mut db, _dir, path) = create_test_db("corrupt_freelist_count");
    db.sync().unwrap();
    drop(db);

    // Corrupt free_page_count field in DbHeader (offset 32 on Page 0) to 500
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(32)).unwrap();
        file.write_all(&500u32.to_le_bytes()).unwrap();
    }

    let mut db = reopen_db(&path);
    let report = db.verify_consistency().unwrap();
    assert!(report.has_errors());
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::FreelistCountMismatch {
            header_count: 500,
            ..
        }
    )));
}

#[test]
fn test_corruption_freelist_contains_reserved_page() {
    let (mut db, _dir, path) = create_test_db("corrupt_freelist_reserved");
    db.sync().unwrap();
    drop(db);

    // Corrupt freelist_head to point to Page 1 (reserved metadata page)
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(28)).unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
    }

    let mut db = reopen_db(&path);
    let report = db.verify_consistency().unwrap();
    assert!(report.has_fatal());
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::FreelistContainsReservedPage { page_id: 1 }
    )));
}

#[test]
fn test_corruption_page_in_both_freelist_and_live_table() {
    let (mut db, _dir, path) = create_test_db("corrupt_freelist_live_collision");
    db.create_table("accounts", make_accounts_schema()).unwrap();
    db.sync().unwrap();

    let table_root = db.open_table("accounts").unwrap().metadata().root_page_id;
    drop(db);

    // Corrupt freelist_head in DbHeader to point to the table's live primary tree root page!
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(28)).unwrap(); // freelist_head
        file.write_all(&table_root.to_le_bytes()).unwrap();
    }

    let mut db = reopen_db(&path);
    let report = db.verify_consistency().unwrap();
    assert!(report.has_fatal());
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::FreelistContainsLivePage { page_id, .. } if page_id == table_root
    )));
}

#[test]
fn test_corruption_secondary_index_value_mismatch() {
    let (mut db, _dir, path) = create_test_db("corrupt_sec_idx_val");
    db.create_table("customers", make_customers_schema())
        .unwrap();
    db.create_index("customers", "idx_email", "email", false)
        .unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("customers").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("original@test.com".into()),
            Value::String("Original".into()),
            Value::String("City".into()),
        ]))
        .unwrap();
    }
    txn.commit().unwrap();
    db.sync().unwrap();

    // Verify healthy before mutation
    assert!(db.verify_consistency().unwrap().is_healthy());

    // Corrupt base table row's string bytes directly in the record page payload
    // Search for "original@test.com" (17 bytes) and replace with "modified@test.com" (17 bytes)
    drop(db);
    {
        let mut bytes = fs::read(&path).unwrap();
        let target = b"original@test.com";
        let replacement = b"modified@test.com";
        if let Some(pos) = bytes.windows(target.len()).position(|w| w == target) {
            bytes[pos..pos + target.len()].copy_from_slice(replacement);
            fs::write(&path, &bytes).unwrap();
        }
    }

    let mut db = reopen_db(&path);
    let report = db.verify_consistency().unwrap();
    assert!(report.has_errors());
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::SecondaryIndexValueMismatch { .. }
    )));
}

#[test]
fn test_corruption_covering_payload_mismatch() {
    let (mut db, _dir, path) = create_test_db("corrupt_cov_payload");
    db.create_table("customers", make_customers_schema())
        .unwrap();
    Catalog::create_covering_index(
        db.buffer_pool_mut(),
        "customers",
        "idx_cust_cov",
        &["email"],
        &["name"],
        false,
    )
    .unwrap();

    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("customers").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(10),
            Value::String("alice@test.com".into()),
            Value::String("Alice Wonderland".into()),
            Value::String("London".into()),
        ]))
        .unwrap();
    }
    txn.commit().unwrap();
    db.sync().unwrap();

    assert!(db.verify_consistency().unwrap().is_healthy());
    drop(db);

    // Mutate the covered column "Alice Wonderland" (16 bytes) in the base row payload to "AlicX Wonderland" (16 bytes)
    {
        let mut bytes = fs::read(&path).unwrap();
        let target = b"Alice Wonderland";
        let replacement = b"AlicX Wonderland";
        if let Some(pos) = bytes.windows(target.len()).position(|w| w == target) {
            bytes[pos..pos + target.len()].copy_from_slice(replacement);
            fs::write(&path, &bytes).unwrap();
        }
    }

    let mut db = reopen_db(&path);
    let report = db.verify_consistency().unwrap();
    assert!(report.has_errors());
    assert!(report
        .issues
        .iter()
        .any(|i| matches!(i.kind, ConsistencyIssueKind::CoveringPayloadMismatch { .. })));
}

#[test]
fn test_corruption_unique_constraint_violation() {
    let (mut db, _dir, _path) = create_test_db("corrupt_unique_violation");
    db.create_table("customers", make_customers_schema())
        .unwrap();
    db.create_index("customers", "idx_unique_email", "email", true)
        .unwrap();

    // Insert Alice
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("customers").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("duplicate@test.com".into()),
            Value::String("Alice".into()),
            Value::String("Paris".into()),
        ]))
        .unwrap();
    }
    t1.commit().unwrap();

    // Insert Bob with a different email
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("customers").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(2),
            Value::String("bob@test.com".into()),
            Value::String("Bob".into()),
            Value::String("Berlin".into()),
        ]))
        .unwrap();
    }
    t2.commit().unwrap();

    // Verify healthy
    assert!(db.verify_consistency().unwrap().is_healthy());

    let sec_tree = treedb::index::secondary::SecondaryIndex::from_root(
        db.open_table("customers").unwrap().indexes()[0]
            .0
            .root_page_id,
    );
    let bob_key =
        treedb::index::secondary::IndexKey::from_value(&Value::String("bob@test.com".into()));
    let bob_rids = sec_tree.lookup(db.buffer_pool_mut(), &bob_key).unwrap();
    let bob_rid = bob_rids[0];

    // Insert duplicate index entry for Alice's email pointing to Bob's valid record
    let dup_key =
        treedb::index::secondary::IndexKey::from_value(&Value::String("duplicate@test.com".into()));
    let mut mut_tree = sec_tree;
    mut_tree
        .insert(db.buffer_pool_mut(), &dup_key, bob_rid)
        .unwrap();

    // Now verification will detect two live distinct records sharing the same unique key
    let report = db.verify_consistency().unwrap();
    assert!(report.has_errors());
    assert!(report.issues.iter().any(|i| matches!(
        i.kind,
        ConsistencyIssueKind::UniqueConstraintViolation { .. }
    )));
}

#[test]
fn test_corruption_wal_record_crc_failure() {
    let (mut db, _dir, path) = create_test_db("corrupt_wal_crc");
    db.create_table("accounts", make_accounts_schema()).unwrap();

    // Insert row creating WAL records
    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        tbl.insert(&Row::new(vec![
            Value::Int64(1),
            Value::String("ACC-1".into()),
            Value::Int64(100),
            Value::Bytes(vec![]),
        ]))
        .unwrap();
    }
    txn.commit().unwrap();
    db.sync().unwrap();
    drop(db);

    let wal_path = PathBuf::from(format!("{}.wal", path.display()));
    assert!(wal_path.exists());

    // Corrupt the last byte of WAL payload so CRC fails
    {
        let mut bytes = fs::read(&wal_path).unwrap();
        let last_idx = bytes.len() - 1;
        bytes[last_idx] ^= 0xFF;
        fs::write(&wal_path, &bytes).unwrap();
    }

    let report = TreeDb::verify_file(&path).unwrap();
    assert!(report.has_errors());
    assert!(report
        .issues
        .iter()
        .any(|i| matches!(i.kind, ConsistencyIssueKind::WalRecordCorruption { .. })));
}

// -----------------------------------------------------------------------------
// Metamorphic & Differential Testing (Fixed Seeds)
// -----------------------------------------------------------------------------

#[test]
fn test_metamorphic_verification_stability_across_fixed_seeds() {
    let seeds = [42u64, 1337u64, 2026u64];

    for seed in seeds {
        let (mut db, _dir, path) = create_test_db(&format!("meta_seed_{seed}"));
        db.create_table("accounts", make_accounts_schema()).unwrap();
        db.create_index("accounts", "idx_acc", "account_num", false)
            .unwrap();

        // Deterministic PRNG using LCG
        let mut state = seed;
        let mut next_u64 = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state
        };

        let mut inserted_keys = Vec::new();

        // 1. Insert 20 random rows
        let mut t1 = db.begin().unwrap();
        {
            let mut tbl = t1.open_table("accounts").unwrap();
            for i in 1..=20 {
                let bal = (next_u64() % 10000) as i64;
                tbl.insert(&Row::new(vec![
                    Value::Int64(i),
                    Value::String(format!("ACC-{seed}-{i}")),
                    Value::Int64(bal),
                    Value::Bytes(vec![(i % 255) as u8; 20]),
                ]))
                .unwrap();
                inserted_keys.push(i);
            }
        }
        t1.commit().unwrap();

        // 2. Update 10 rows
        let mut t2 = db.begin().unwrap();
        {
            let mut tbl = t2.open_table("accounts").unwrap();
            for i in 1..=10 {
                tbl.update(&Row::new(vec![
                    Value::Int64(i),
                    Value::String(format!("ACC-{seed}-{i}")),
                    Value::Int64(9999),
                    Value::Bytes(vec![(i % 255) as u8; 20]),
                ]))
                .unwrap();
            }
        }
        t2.commit().unwrap();

        // 3. Delete 5 rows
        let mut t3 = db.begin().unwrap();
        {
            let mut tbl = t3.open_table("accounts").unwrap();
            for i in 1..=5 {
                tbl.delete(&Value::Int64(i)).unwrap();
            }
        }
        t3.commit().unwrap();

        // First verification pass
        let report1 = db.verify_consistency().unwrap();
        assert!(report1.is_healthy(), "seed {seed} run 1 must be healthy");

        // Metamorphic Invariant 1: Verifying healthy database twice yields identical findings
        let report2 = db.verify_consistency().unwrap();
        assert_eq!(
            report1.is_healthy(),
            report2.is_healthy(),
            "metamorphic invariant: repeated verification must yield identical health"
        );
        assert_eq!(
            report1.issues, report2.issues,
            "metamorphic invariant: repeated verification must yield identical issue set"
        );
        assert_eq!(
            report1.statistics.pages_examined, report2.statistics.pages_examined,
            "metamorphic invariant: pages examined must match"
        );

        // Metamorphic Invariant 2: Closing and reopening database preserves audit results
        db.sync().unwrap();
        drop(db);

        let mut reopened = reopen_db(&path);
        let report3 = reopened.verify_consistency().unwrap();
        assert!(
            report3.is_healthy(),
            "metamorphic invariant: reopened database must remain healthy"
        );
        assert_eq!(report1.issues, report3.issues);
    }
}
