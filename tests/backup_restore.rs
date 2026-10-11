//! TreeDB Phase 75: Backup, Restore & Database Image Integrity Integration Tests.
//!
//! Validates:
//! 1. Backup creation across database states (empty, populated, indexed, overflow, MVCC, vacuum).
//! 2. Bounded image integrity checks (manifest CRC, versions, page alignment, checksums, WAL, extra artifacts).
//! 3. Safe two-phase restore (new destination, overwrite allowed/disallowed, rollback protection, path collisions).
//! 4. CommitSeq monotonicity and MVCC snapshot visibility preservation.
//! 5. Complete deterministic failure injection across all 9 backup and restore boundaries.
//! 6. Metamorphic differential testing with independent expected-state reference model across fixed seeds.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use tempfile::tempdir;

use treedb::engine::isolation::{CommitSeq, Snapshot, SnapshotId};
use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::engine::vacuum::VacuumConfig;
use treedb::{
    BackupConfig, BackupError, RestoreConfig, StorageFailurePoint, TreeDb, CURRENT_BACKUP_VERSION,
    MANIFEST_FILE_NAME,
};

// =============================================================================
// Helper Fixtures and Independent Expected-State Reference Model
// =============================================================================

fn make_test_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("account_num", DataType::String, false),
            Column::new("balance", DataType::Int64, false),
            Column::new("category", DataType::String, false),
            Column::new("payload", DataType::Bytes, false),
        ],
        0,
    )
    .unwrap()
}

fn make_test_row(id: i64, acc: &str, bal: i64, cat: &str, payload: Vec<u8>) -> Row {
    Row::new(vec![
        Value::Int64(id),
        Value::String(acc.to_string()),
        Value::Int64(bal),
        Value::String(cat.to_string()),
        Value::Bytes(payload),
    ])
}

fn setup_test_db(name: &str) -> (TreeDb, tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.treedb"));
    let mut db = TreeDb::create(&db_path, 64).unwrap();
    db.create_table("accounts", make_test_schema()).unwrap();
    db.create_index("accounts", "idx_acc", "account_num", true)
        .unwrap();
    db.execute_statement(
        "CREATE INDEX idx_cov ON accounts (account_num) INCLUDE (balance, category);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_cat_bal ON accounts (category, balance);")
        .unwrap();
    (db, dir, db_path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpectedAccount {
    account_num: String,
    balance: i64,
    category: String,
    payload: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
struct LogicalModel {
    committed_rows: BTreeMap<i64, ExpectedAccount>,
}

impl LogicalModel {
    fn insert(&mut self, id: i64, acc: &str, bal: i64, cat: &str, payload: Vec<u8>) {
        self.committed_rows.insert(
            id,
            ExpectedAccount {
                account_num: acc.to_string(),
                balance: bal,
                category: cat.to_string(),
                payload,
            },
        );
    }

    fn update(&mut self, id: i64, acc: &str, bal: i64, cat: &str, payload: Vec<u8>) {
        if self.committed_rows.contains_key(&id) {
            self.committed_rows.insert(
                id,
                ExpectedAccount {
                    account_num: acc.to_string(),
                    balance: bal,
                    category: cat.to_string(),
                    payload,
                },
            );
        }
    }

    fn delete(&mut self, id: i64) {
        self.committed_rows.remove(&id);
    }

    fn assert_matches_database(&self, db: &mut TreeDb, context: &str) {
        // 1. Validate Table Scan
        let mut tbl = db.open_table("accounts").unwrap();
        let scanned_rows = tbl.scan(None, None).unwrap();
        assert_eq!(
            scanned_rows.len(),
            self.committed_rows.len(),
            "{context}: Scanned row count mismatch"
        );

        for row in scanned_rows {
            let id = match row.values[0] {
                Value::Int64(v) => v,
                _ => panic!("Expected Int64 id"),
            };
            let exp = self
                .committed_rows
                .get(&id)
                .unwrap_or_else(|| panic!("{context}: Unexpected live row id {id}"));
            assert_eq!(
                row.values[1],
                Value::String(exp.account_num.clone()),
                "{context}: acc mismatch for id {id}"
            );
            assert_eq!(
                row.values[2],
                Value::Int64(exp.balance),
                "{context}: balance mismatch for id {id}"
            );
            assert_eq!(
                row.values[3],
                Value::String(exp.category.clone()),
                "{context}: category mismatch for id {id}"
            );
            assert_eq!(
                row.values[4],
                Value::Bytes(exp.payload.clone()),
                "{context}: payload mismatch for id {id}"
            );
        }

        // 2. Validate Point Lookups via Primary Key
        for (id, exp) in &self.committed_rows {
            let actual = tbl.get(&Value::Int64(*id)).unwrap();
            assert!(
                actual.is_some(),
                "{context}: Expected row id {id} to exist via PK get"
            );
            let row = actual.unwrap();
            assert_eq!(row.values[1], Value::String(exp.account_num.clone()));
            assert_eq!(row.values[2], Value::Int64(exp.balance));
            assert_eq!(row.values[3], Value::String(exp.category.clone()));
            assert_eq!(row.values[4], Value::Bytes(exp.payload.clone()));
        }

        // 3. Validate SQL Query & Count
        drop(tbl);
        let res = db
            .execute_sql("SELECT count(*) FROM accounts")
            .expect("SELECT count(*) must execute cleanly");
        let count_val = match res.rows[0].values[0] {
            Value::UInt64(c) => c as usize,
            Value::Int64(c) => c as usize,
            ref other => panic!("Expected count as UInt64 or Int64, got {other:?}"),
        };
        assert_eq!(
            count_val,
            self.committed_rows.len(),
            "{context}: SQL count(*) mismatch"
        );
    }
}

// =============================================================================
// Stage 10 Test Matrix: A. Backup Creation
// =============================================================================

#[test]
fn test_backup_empty_database() {
    let (mut db, _dir, path) = setup_test_db("empty_backup");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let report = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();
    assert_eq!(report.manifest.backup_version, CURRENT_BACKUP_VERSION);
    assert!(report.manifest.db_file_size > 0);
    assert!(report.manifest.page_count >= 2);

    let val = TreeDb::validate_backup(&bkp_path, &BackupConfig::default()).unwrap();
    assert!(val.is_valid);

    // Restore to new destination
    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    let rest_report =
        TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();
    assert_eq!(rest_report.restored_page_count, report.manifest.page_count);

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    let model = LogicalModel::default();
    model.assert_matches_database(&mut restored_db, "empty_restored");
    let consistency = restored_db.verify_consistency().unwrap();
    assert!(consistency.is_healthy());
    drop(db);
    let _ = path;
}

#[test]
fn test_backup_populated_database() {
    let (mut db, _dir, _path) = setup_test_db("pop_backup");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        for i in 1..=10 {
            let row = make_test_row(
                i,
                &format!("ACC-{i:03}"),
                i * 100,
                if i % 2 == 0 { "EVEN" } else { "ODD" },
                vec![i as u8; 32],
            );
            tbl.insert(&row).unwrap();
            model.insert(
                i,
                &format!("ACC-{i:03}"),
                i * 100,
                if i % 2 == 0 { "EVEN" } else { "ODD" },
                vec![i as u8; 32],
            );
        }
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let report = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();
    assert!(report.db_bytes_written > 0);

    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, "populated_restored");
}

#[test]
fn test_backup_secondary_composite_and_covering_indexes() {
    let (mut db, _dir, _path) = setup_test_db("idx_backup");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        for i in 1..=5 {
            tbl.insert(&make_test_row(
                i,
                &format!("ACC-{i}"),
                i * 50,
                "INVESTMENT",
                vec![i as u8],
            ))
            .unwrap();
            model.insert(i, &format!("ACC-{i}"), i * 50, "INVESTMENT", vec![i as u8]);
        }
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, "indexed_restored");

    // Test secondary index lookup
    let res = restored_db
        .execute_sql("SELECT balance, category FROM accounts WHERE account_num = 'ACC-3'")
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(150));
    assert_eq!(res.rows[0].values[1], Value::String("INVESTMENT".into()));
}

#[test]
fn test_backup_with_overflow_records() {
    let (mut db, _dir, _path) = setup_test_db("overflow_backup");
    let mut model = LogicalModel::default();

    let large_payload_1 = vec![b'X'; 6000]; // Multi-page overflow payload
    let large_payload_2 = vec![b'Y'; 12000]; // 3-page overflow payload

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(
            1,
            "ACC-OVERFLOW-1",
            1000,
            "LARGE",
            large_payload_1.clone(),
        ))
        .unwrap();
        model.insert(1, "ACC-OVERFLOW-1", 1000, "LARGE", large_payload_1.clone());

        tbl.insert(&make_test_row(
            2,
            "ACC-OVERFLOW-2",
            2000,
            "LARGE",
            large_payload_2.clone(),
        ))
        .unwrap();
        model.insert(2, "ACC-OVERFLOW-2", 2000, "LARGE", large_payload_2.clone());
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, "overflow_restored");
}

#[test]
fn test_backup_with_historical_mvcc_versions() {
    let (mut db, _dir, _path) = setup_test_db("mvcc_backup");

    // V1
    let mut t1 = db.begin().unwrap();
    {
        let mut tbl = t1.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "V1", vec![1]))
            .unwrap();
    }
    t1.commit().unwrap();
    let s1_seq = db.buffer_pool().durable_max_commit_seq();

    // V2
    let mut t2 = db.begin().unwrap();
    {
        let mut tbl = t2.open_table("accounts").unwrap();
        tbl.update(&make_test_row(1, "ACC-1", 200, "V2", vec![2]))
            .unwrap();
    }
    t2.commit().unwrap();
    let s2_seq = db.buffer_pool().durable_max_commit_seq();

    // V3
    let mut t3 = db.begin().unwrap();
    {
        let mut tbl = t3.open_table("accounts").unwrap();
        tbl.update(&make_test_row(1, "ACC-1", 300, "V3", vec![3]))
            .unwrap();
    }
    t3.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();

    // Verify snapshot visibility as of S1
    let mut txn_s1 = restored_db.begin().unwrap();
    txn_s1.set_snapshot(Snapshot::new(SnapshotId(1), CommitSeq(s1_seq)));
    {
        let mut tbl = txn_s1.open_table("accounts").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::Int64(100));
        assert_eq!(row.values[3], Value::String("V1".into()));
    }
    txn_s1.commit().unwrap();

    // Verify snapshot visibility as of S2
    let mut txn_s2 = restored_db.begin().unwrap();
    txn_s2.set_snapshot(Snapshot::new(SnapshotId(2), CommitSeq(s2_seq)));
    {
        let mut tbl = txn_s2.open_table("accounts").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::Int64(200));
        assert_eq!(row.values[3], Value::String("V2".into()));
    }
    txn_s2.commit().unwrap();

    // Verify current visibility V3
    let mut txn_cur = restored_db.begin().unwrap();
    {
        let mut tbl = txn_cur.open_table("accounts").unwrap();
        let row = tbl.get(&Value::Int64(1)).unwrap().unwrap();
        assert_eq!(row.values[2], Value::Int64(300));
        assert_eq!(row.values[3], Value::String("V3".into()));
    }
    txn_cur.commit().unwrap();
}

#[test]
fn test_backup_after_vacuum() {
    let (mut db, _dir, _path) = setup_test_db("vacuum_backup");
    let mut model = LogicalModel::default();

    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        for i in 1..=5 {
            tbl.insert(&make_test_row(
                i,
                &format!("ACC-{i}"),
                i * 100,
                "OLD",
                vec![i as u8],
            ))
            .unwrap();
            model.insert(i, &format!("ACC-{i}"), i * 100, "OLD", vec![i as u8]);
        }
    }
    t0.commit().unwrap();

    // Generate dead versions
    for round in 1..=3 {
        let mut t = db.begin().unwrap();
        {
            let mut tbl = t.open_table("accounts").unwrap();
            for i in 1..=5 {
                tbl.update(&make_test_row(
                    i,
                    &format!("ACC-{i}"),
                    i * 100 + round,
                    "UPDATED",
                    vec![round as u8],
                ))
                .unwrap();
                model.update(
                    i,
                    &format!("ACC-{i}"),
                    i * 100 + round,
                    "UPDATED",
                    vec![round as u8],
                );
            }
        }
        t.commit().unwrap();
    }

    let vac = db.vacuum(&VacuumConfig::default()).unwrap();
    assert!(vac.versions_reclaimed > 0);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, "vacuum_restored");
}

#[test]
fn test_repeated_backup_same_logical_state() {
    let (mut db, _dir, path) = setup_test_db("repeated_backup");
    let mut model = LogicalModel::default();
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
        model.insert(1, "ACC-1", 100, "A", vec![1]);
    }
    t0.commit().unwrap();
    drop(db);

    let dir1 = tempdir().unwrap();
    let bkp1 = dir1.path().join("bkp1");
    let rep1 = TreeDb::backup_database(&path, &bkp1, &BackupConfig::default()).unwrap();

    let dir2 = tempdir().unwrap();
    let bkp2 = dir2.path().join("bkp2");
    let rep2 = TreeDb::backup_database(&path, &bkp2, &BackupConfig::default()).unwrap();

    // Checksums and metadata must be bit-for-bit identical for repeated backups of the same offline state
    assert_eq!(
        rep1.manifest.db_file_checksum,
        rep2.manifest.db_file_checksum
    );
    assert_eq!(rep1.manifest.max_commit_seq, rep2.manifest.max_commit_seq);
    assert_eq!(rep1.manifest.page_count, rep2.manifest.page_count);

    // Both backups must restore identical logical state
    let rest1_dir = tempdir().unwrap();
    let rest1_path = rest1_dir.path().join("r1.treedb");
    TreeDb::restore_backup(&bkp1, &rest1_path, &RestoreConfig::default()).unwrap();
    let mut r1_db = TreeDb::open(&rest1_path, 64).unwrap();
    model.assert_matches_database(&mut r1_db, "repeated_backup_r1");

    let rest2_dir = tempdir().unwrap();
    let rest2_path = rest2_dir.path().join("r2.treedb");
    TreeDb::restore_backup(&bkp2, &rest2_path, &RestoreConfig::default()).unwrap();
    let mut r2_db = TreeDb::open(&rest2_path, 64).unwrap();
    model.assert_matches_database(&mut r2_db, "repeated_backup_r2");
}

#[test]
fn test_offline_backup_directly_from_file_path() {
    let (mut db, _dir, path) = setup_test_db("offline_direct_backup");
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(10, "ACC-10", 999, "OFFLINE", vec![10]))
            .unwrap();
    }
    t0.commit().unwrap();
    drop(db); // Entire database is completely offline

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let report = TreeDb::backup_database(&path, &bkp_path, &BackupConfig::default()).unwrap();
    assert!(report.db_bytes_written > 0);

    let val = TreeDb::validate_backup(&bkp_path, &BackupConfig::default()).unwrap();
    assert!(val.is_valid);
}

// =============================================================================
// Stage 10 Test Matrix: B. Image Integrity and Corruption Rejection
// =============================================================================

#[test]
fn test_integrity_missing_manifest() {
    let (mut db, _dir, _path) = setup_test_db("corrupt_missing_manifest");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Delete treedb.manifest
    fs::remove_file(bkp_path.join(MANIFEST_FILE_NAME)).unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(val_res, Err(BackupError::MissingFile(f)) if f == MANIFEST_FILE_NAME));
}

#[test]
fn test_integrity_invalid_manifest_encoding() {
    let (mut db, _dir, _path) = setup_test_db("corrupt_manifest_encoding");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Overwrite manifest with invalid garbage
    fs::write(bkp_path.join(MANIFEST_FILE_NAME), b"INVALID_GARBAGE_BYTES").unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(
        val_res,
        Err(BackupError::ChecksumMismatch { .. }) | Err(BackupError::InvalidManifest(_))
    ));
}

#[test]
fn test_integrity_unsupported_backup_format_version() {
    let (mut db, _dir, _path) = setup_test_db("unsupported_backup_version");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Modify manifest with unsupported backup version 999
    let mut manifest = TreeDb::inspect_backup_manifest(&bkp_path).unwrap();
    manifest.backup_version = 999;
    let bytes = manifest.serialize();
    fs::write(bkp_path.join(MANIFEST_FILE_NAME), bytes).unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(
        val_res,
        Err(BackupError::UnsupportedBackupVersion { found: 999, .. })
    ));
}

#[test]
fn test_integrity_unsupported_database_format_version() {
    let (mut db, _dir, _path) = setup_test_db("unsupported_db_version");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Modify manifest with unsupported db format version 999
    let mut manifest = TreeDb::inspect_backup_manifest(&bkp_path).unwrap();
    manifest.db_format_version = 999;
    let bytes = manifest.serialize();
    fs::write(bkp_path.join(MANIFEST_FILE_NAME), bytes).unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(
        val_res,
        Err(BackupError::UnsupportedDatabaseVersion { found: 999, .. })
    ));
}

#[test]
fn test_integrity_truncated_database_image() {
    let (mut db, _dir, _path) = setup_test_db("truncated_image");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Truncate the DB file by 100 bytes
    let db_file_path = bkp_path.join(&rep.manifest.db_file_name);
    let original_len = fs::metadata(&db_file_path).unwrap().len();
    let f = OpenOptions::new().write(true).open(&db_file_path).unwrap();
    f.set_len(original_len - 100).unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(
        val_res,
        Err(BackupError::SizeMismatch { .. }) | Err(BackupError::ImageNotPageAligned { .. })
    ));
}

#[test]
fn test_integrity_modified_image_byte() {
    let (mut db, _dir, _path) = setup_test_db("modified_byte");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Flip a byte in the database file
    let db_file_path = bkp_path.join(&rep.manifest.db_file_name);
    let mut bytes = fs::read(&db_file_path).unwrap();
    let target_idx = bytes.len() - 10;
    bytes[target_idx] ^= 0xFF;
    fs::write(&db_file_path, bytes).unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(val_res, Err(BackupError::ChecksumMismatch { .. })));
}

#[test]
fn test_integrity_missing_required_wal() {
    let (mut db, _dir, _path) = setup_test_db("missing_wal");

    // Produce durable WAL
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    // Backup without WAL truncation to ensure WAL exists in backup
    let bkp_cfg = BackupConfig {
        fault_point: None,
        checkpoint_before_backup: false,
        verify_image: true,
    };
    let rep = db.create_backup(&bkp_path, &bkp_cfg).unwrap();

    if rep.manifest.has_wal {
        let wal_name = rep.manifest.wal_file_name.as_ref().unwrap();
        fs::remove_file(bkp_path.join(wal_name)).unwrap();

        let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
        assert!(matches!(val_res, Err(BackupError::MissingFile(_))));
    }
}

#[test]
fn test_integrity_unexpected_extra_artifact() {
    let (mut db, _dir, _path) = setup_test_db("extra_artifact");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Plant unexpected unauthorized file in backup directory
    fs::write(bkp_path.join("unauthorized_script.sh"), b"echo malicious").unwrap();

    let val_res = TreeDb::validate_backup(&bkp_path, &BackupConfig::default());
    assert!(matches!(
        val_res,
        Err(BackupError::UnexpectedArtifact(f)) if f == "unauthorized_script.sh"
    ));
}

// =============================================================================
// Stage 10 Test Matrix: C. Safe Two-Phase Restore
// =============================================================================

#[test]
fn test_restore_over_existing_destination_allowed_and_disallowed() {
    let (mut db, _dir, _path) = setup_test_db("restore_overwrite");
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "ORIGINAL", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("dest.treedb");

    // 1. Initial restore to non-existing destination
    TreeDb::restore_backup(&bkp_path, &dest_db_path, &RestoreConfig::default()).unwrap();

    // 2. Second restore with allow_overwrite = false must be rejected
    let no_overwrite_cfg = RestoreConfig {
        allow_overwrite: false,
        ..Default::default()
    };
    let err = TreeDb::restore_backup(&bkp_path, &dest_db_path, &no_overwrite_cfg).unwrap_err();
    assert!(matches!(err, BackupError::DestinationExists(_)));

    // 3. Third restore with allow_overwrite = true must succeed cleanly
    let overwrite_cfg = RestoreConfig {
        allow_overwrite: true,
        ..Default::default()
    };
    let ok = TreeDb::restore_backup(&bkp_path, &dest_db_path, &overwrite_cfg);
    assert!(ok.is_ok());
}

#[test]
fn test_restore_source_and_destination_path_collision() {
    let (mut db, _dir, _path) = setup_test_db("path_collision");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Restoring directly into the backup database file itself
    let collision_target = bkp_path.join(&rep.manifest.db_file_name);
    let cfg = RestoreConfig {
        allow_overwrite: true,
        ..Default::default()
    };

    let err = TreeDb::restore_backup(&bkp_path, &collision_target, &cfg).unwrap_err();
    assert!(matches!(err, BackupError::PathCollision { .. }));
}

#[test]
fn test_restore_corrupted_backup_rejected_preserves_destination() {
    let (mut db, _dir, _path) = setup_test_db("corrupted_preserves_dest");
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(100, "ACC-100", 9999, "VALID", vec![100]))
            .unwrap();
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    // Restore to destination first
    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("preserved_dest.treedb");
    TreeDb::restore_backup(&bkp_path, &dest_db_path, &RestoreConfig::default()).unwrap();

    // Corrupt the backup image
    let db_file_path = bkp_path.join(&rep.manifest.db_file_name);
    let mut bytes = fs::read(&db_file_path).unwrap();
    bytes[1024] ^= 0xAA;
    fs::write(&db_file_path, bytes).unwrap();

    // Attempt restore over existing destination with allow_overwrite = true
    let overwrite_cfg = RestoreConfig {
        allow_overwrite: true,
        ..Default::default()
    };
    let res = TreeDb::restore_backup(&bkp_path, &dest_db_path, &overwrite_cfg);
    assert!(res.is_err());

    // Destination must remain intact and valid!
    let mut dest_db = TreeDb::open(&dest_db_path, 64).unwrap();
    let mut model = LogicalModel::default();
    model.insert(100, "ACC-100", 9999, "VALID", vec![100]);
    model.assert_matches_database(&mut dest_db, "destination_intact_after_corrupt_restore");
}

#[test]
fn test_restore_commit_seq_monotonicity_and_write_resumption() {
    let (mut db, _dir, _path) = setup_test_db("commit_seq_mono");

    for i in 1..=5 {
        let mut t = db.begin().unwrap();
        {
            let mut tbl = t.open_table("accounts").unwrap();
            tbl.insert(&make_test_row(i, &format!("ACC-{i}"), i * 10, "A", vec![1]))
                .unwrap();
        }
        t.commit().unwrap();
    }
    let pre_backup_seq = db.current_commit_seq().0;

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("restored_mono.treedb");
    TreeDb::restore_backup(&bkp_path, &dest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&dest_db_path, 64).unwrap();
    let restored_seq = restored_db.current_commit_seq().0;
    assert!(
        restored_seq >= pre_backup_seq,
        "Restored commit seq ({restored_seq}) must not regress from pre-backup ({pre_backup_seq})"
    );

    // Perform new writes and verify monotonic increment
    let mut t_new = restored_db.begin().unwrap();
    {
        let mut tbl = t_new.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(6, "ACC-6", 60, "A", vec![1]))
            .unwrap();
    }
    t_new.commit().unwrap();
    let post_write_seq = restored_db.current_commit_seq().0;
    assert!(
        post_write_seq > restored_seq,
        "Post-restore write commit seq ({post_write_seq}) must be strictly greater than restored seq ({restored_seq})"
    );
}

// =============================================================================
// Stage 10 Test Matrix: D. Deterministic Failure Injection
// =============================================================================

#[test]
fn test_failure_injection_before_backup_creation() {
    let (mut db, _dir, path) = setup_test_db("fault_before_bkp_creat");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let cfg = BackupConfig {
        fault_point: Some(StorageFailurePoint::BeforeBackupCreation),
        checkpoint_before_backup: true,
        verify_image: true,
    };
    let err = db.create_backup(&bkp_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::BeforeBackupCreation)
    ));
    assert!(!bkp_path.exists());

    // Verify source is intact
    let mut db_reopen = TreeDb::open(&path, 64).unwrap();
    assert!(db_reopen.verify_consistency().unwrap().is_healthy());
}

#[test]
fn test_failure_injection_during_backup_db_copy() {
    let (mut db, _dir, path) = setup_test_db("fault_during_db_copy");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let cfg = BackupConfig {
        fault_point: Some(StorageFailurePoint::DuringBackupDbCopy),
        checkpoint_before_backup: true,
        verify_image: true,
    };
    let err = db.create_backup(&bkp_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringBackupDbCopy)
    ));
    assert!(!bkp_path.exists());

    let mut db_reopen = TreeDb::open(&path, 64).unwrap();
    assert!(db_reopen.verify_consistency().unwrap().is_healthy());
}

#[test]
fn test_failure_injection_during_backup_wal_copy() {
    let (mut db, _dir, path) = setup_test_db("fault_during_wal_copy");
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let cfg = BackupConfig {
        fault_point: Some(StorageFailurePoint::DuringBackupWalCopy),
        checkpoint_before_backup: false,
        verify_image: true,
    };
    let err = db.create_backup(&bkp_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringBackupWalCopy)
    ));
    assert!(!bkp_path.exists());

    let mut db_reopen = TreeDb::open(&path, 64).unwrap();
    assert!(db_reopen.verify_consistency().unwrap().is_healthy());
}

#[test]
fn test_failure_injection_during_backup_manifest_write() {
    let (mut db, _dir, path) = setup_test_db("fault_during_manifest_write");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let cfg = BackupConfig {
        fault_point: Some(StorageFailurePoint::DuringBackupManifestWrite),
        checkpoint_before_backup: true,
        verify_image: true,
    };
    let err = db.create_backup(&bkp_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringBackupManifestWrite)
    ));
    assert!(!bkp_path.exists());

    let mut db_reopen = TreeDb::open(&path, 64).unwrap();
    assert!(db_reopen.verify_consistency().unwrap().is_healthy());
}

#[test]
fn test_failure_injection_before_backup_publication() {
    let (mut db, _dir, path) = setup_test_db("fault_before_publication");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");

    let cfg = BackupConfig {
        fault_point: Some(StorageFailurePoint::BeforeBackupPublication),
        checkpoint_before_backup: true,
        verify_image: true,
    };
    let err = db.create_backup(&bkp_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::BeforeBackupPublication)
    ));
    assert!(!bkp_path.exists());

    let mut db_reopen = TreeDb::open(&path, 64).unwrap();
    assert!(db_reopen.verify_consistency().unwrap().is_healthy());
}

#[test]
fn test_failure_injection_before_restore_validation() {
    let (mut db, _dir, _path) = setup_test_db("fault_before_rest_val");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("dest.treedb");

    let cfg = RestoreConfig {
        fault_point: Some(StorageFailurePoint::BeforeRestoreValidation),
        ..Default::default()
    };
    let err = TreeDb::restore_backup(&bkp_path, &dest_db_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::BeforeRestoreValidation)
    ));
    assert!(!dest_db_path.exists());
}

#[test]
fn test_failure_injection_during_restore_db_copy() {
    let (mut db, _dir, _path) = setup_test_db("fault_during_rest_db_copy");
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("dest.treedb");

    let cfg = RestoreConfig {
        fault_point: Some(StorageFailurePoint::DuringRestoreDbCopy),
        ..Default::default()
    };
    let err = TreeDb::restore_backup(&bkp_path, &dest_db_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringRestoreDbCopy)
    ));
    assert!(!dest_db_path.exists());
}

#[test]
fn test_failure_injection_during_restore_wal_copy() {
    let (mut db, _dir, _path) = setup_test_db("fault_during_rest_wal_copy");
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-1", 100, "A", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let bkp_cfg = BackupConfig {
        fault_point: None,
        checkpoint_before_backup: false,
        verify_image: true,
    };
    let rep = db.create_backup(&bkp_path, &bkp_cfg).unwrap();

    if rep.manifest.has_wal {
        let dest_dir = tempdir().unwrap();
        let dest_db_path = dest_dir.path().join("dest.treedb");

        let cfg = RestoreConfig {
            fault_point: Some(StorageFailurePoint::DuringRestoreWalCopy),
            ..Default::default()
        };
        let err = TreeDb::restore_backup(&bkp_path, &dest_db_path, &cfg).unwrap_err();
        assert!(matches!(
            err,
            BackupError::InjectedFault(StorageFailurePoint::DuringRestoreWalCopy)
        ));
        assert!(!dest_db_path.exists());
    }
}

#[test]
fn test_failure_injection_before_restore_publication_preserves_existing_destination() {
    let (mut db, _dir, _path) = setup_test_db("fault_before_rest_pub");
    let mut t0 = db.begin().unwrap();
    {
        let mut tbl = t0.open_table("accounts").unwrap();
        tbl.insert(&make_test_row(1, "ACC-ORIGINAL", 555, "ORIG", vec![1]))
            .unwrap();
    }
    t0.commit().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    db.create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();

    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("dest.treedb");

    // Establish a valid initial destination
    TreeDb::restore_backup(&bkp_path, &dest_db_path, &RestoreConfig::default()).unwrap();

    // Now attempt restore with overwrite allowed but failing right before publication
    let cfg = RestoreConfig {
        allow_overwrite: true,
        fault_point: Some(StorageFailurePoint::BeforeRestorePublication),
        ..Default::default()
    };
    let err = TreeDb::restore_backup(&bkp_path, &dest_db_path, &cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::BeforeRestorePublication)
    ));

    // The existing destination must be preserved and undamaged!
    let mut preserved_db = TreeDb::open(&dest_db_path, 64).unwrap();
    let mut model = LogicalModel::default();
    model.insert(1, "ACC-ORIGINAL", 555, "ORIG", vec![1]);
    model.assert_matches_database(&mut preserved_db, "preserved_destination_intact");
}

// =============================================================================
// Stage 10 Test Matrix: E. Metamorphic and Reference-Model Testing
// =============================================================================

struct LcgRng {
    state: u64,
}

impl LcgRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn next_range(&mut self, min: u64, max: u64) -> u64 {
        min + (self.next_u64() % (max - min + 1))
    }
}

fn run_metamorphic_backup_restore_test(seed: u64) {
    let mut rng = LcgRng::new(seed);
    let (mut db, _dir, _path) = setup_test_db(&format!("meta_seed_{seed}"));
    let mut model = LogicalModel::default();

    let mut op_log = Vec::new();

    for step in 1..=30 {
        let op_type = rng.next_range(1, 10);
        let id = rng.next_range(1, 15) as i64;
        let bal = rng.next_range(10, 1000) as i64;

        let mut t = db.begin().unwrap();
        if op_type <= 6 {
            // Insert or Upsert
            let acc = format!("ACC-{id:03}");
            let cat = if id % 2 == 0 { "CHECKING" } else { "SAVINGS" };
            let payload = vec![(step % 250) as u8; ((id as usize % 5) + 1) * 20];

            let mut tbl = t.open_table("accounts").unwrap();
            if tbl.get(&Value::Int64(id)).unwrap().is_some() {
                tbl.update(&make_test_row(id, &acc, bal, cat, payload.clone()))
                    .unwrap();
                model.update(id, &acc, bal, cat, payload);
                op_log.push(format!("UPDATE id={id} bal={bal}"));
            } else {
                tbl.insert(&make_test_row(id, &acc, bal, cat, payload.clone()))
                    .unwrap();
                model.insert(id, &acc, bal, cat, payload);
                op_log.push(format!("INSERT id={id} bal={bal}"));
            }
        } else if op_type <= 8 {
            // Delete
            let mut tbl = t.open_table("accounts").unwrap();
            let _ = tbl.delete(&Value::Int64(id));
            model.delete(id);
            op_log.push(format!("DELETE id={id}"));
        } else {
            // Read-only verification step
            op_log.push(format!("READ_STEP step={step}"));
        }
        t.commit().unwrap();
    }

    model.assert_matches_database(&mut db, &format!("pre_backup_seed_{seed}"));

    // Create backup
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap_or_else(|e| {
            panic!("Backup creation failed for seed {seed}: {e}. Trace: {op_log:?}");
        });

    let val = TreeDb::validate_backup(&bkp_path, &BackupConfig::default()).unwrap_or_else(|e| {
        panic!("Backup validation failed for seed {seed}: {e}. Trace: {op_log:?}");
    });
    assert!(val.is_valid);

    // Restore to independent location
    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap_or_else(
        |e| {
            panic!("Restore failed for seed {seed}: {e}. Trace: {op_log:?}");
        },
    );

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, &format!("post_restore_seed_{seed}"));

    let consistency = restored_db.verify_consistency().unwrap();
    assert!(
        consistency.is_healthy(),
        "Seed {seed} consistency report not healthy: {:?}. Trace: {op_log:?}",
        consistency.issues
    );
    assert_eq!(rep.manifest.max_commit_seq, val.max_commit_seq);
}

#[test]
fn test_metamorphic_seed_42() {
    run_metamorphic_backup_restore_test(42);
}

#[test]
fn test_metamorphic_seed_1337() {
    run_metamorphic_backup_restore_test(1337);
}

#[test]
fn test_metamorphic_seed_2026() {
    run_metamorphic_backup_restore_test(2026);
}

#[test]
fn test_metamorphic_seed_9999() {
    run_metamorphic_backup_restore_test(9999);
}
