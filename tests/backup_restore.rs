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
    restore_journal_path, BackupAuditStatus, BackupConfig, BackupError, RestoreConfig,
    RestoreJournal, RestoreJournalState, StorageFailurePoint, TreeDb, CURRENT_BACKUP_VERSION,
    MANIFEST_FILE_NAME, RESTORE_JOURNAL_MAGIC,
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

// =============================================================================
// Phase 76: Hardening, Source States, Restore Journal, & Lifecycle Regressions
// =============================================================================

fn insert_rows(db: &mut TreeDb, rows: &[Row]) {
    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        for r in rows {
            tbl.insert(r).unwrap();
        }
    }
    txn.commit().unwrap();
}

#[test]
fn test_stage2_unflushed_committed_changes_survive_backup_restore() {
    let (mut db, _src_dir, db_path) = setup_test_db("unflushed_src");
    let mut model = LogicalModel::default();

    // 1. Initial rows checkpointed cleanly
    let mut init_rows = Vec::new();
    for i in 1..=10 {
        let row = make_test_row(
            i,
            &format!("ACC_{i:04}"),
            i * 100,
            "INIT",
            vec![i as u8; 32],
        );
        init_rows.push(row);
        model.insert(
            i,
            &format!("ACC_{i:04}"),
            i * 100,
            "INIT",
            vec![i as u8; 32],
        );
    }
    insert_rows(&mut db, &init_rows);
    db.checkpoint_and_truncate_wal().unwrap();

    // 2. Additional committed transaction WITHOUT checkpointing:
    // Page deltas and commit record are flushed to WAL, but dirty pages in buffer pool
    // are not flushed to PageManager.
    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        for i in 11..=20 {
            let row = make_test_row(
                i,
                &format!("ACC_{i:04}"),
                i * 100,
                "UNFLUSHED",
                vec![i as u8; 32],
            );
            tbl.insert(&row).unwrap();
            model.insert(
                i,
                &format!("ACC_{i:04}"),
                i * 100,
                "UNFLUSHED",
                vec![i as u8; 32],
            );
        }
    }
    txn.commit().unwrap();

    // Drop database instance without running a checkpoint so dirty in-memory pages are evicted
    drop(db);

    // 3. Create offline backup with checkpoint_before_backup disabled:
    // Packages database file (with old checkpoint) and companion WAL (with redo tail).
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let bkp_config = BackupConfig {
        fault_point: None,
        checkpoint_before_backup: false,
        verify_image: true,
    };
    let bkp_report = TreeDb::backup_database(&db_path, &bkp_path, &bkp_config).unwrap();
    assert!(bkp_report.manifest.has_wal);

    // Bounded validation must identify the unapplied WAL tail as RecoverablePendingRecovery
    let val_report = TreeDb::validate_backup(&bkp_path, &bkp_config).unwrap();
    assert_eq!(
        val_report.status,
        BackupAuditStatus::RecoverablePendingRecovery
    );
    assert!(val_report.is_valid);

    // 4. Restore into fresh independent destination
    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    let rest_config = RestoreConfig {
        allow_overwrite: false,
        fault_point: None,
        validate_before_restore: true,
        verify_after_restore: true,
        buffer_pool_size: 64,
    };
    let rest_rep = TreeDb::restore_backup(&bkp_path, &rest_db_path, &rest_config).unwrap();
    assert!(rest_rep.consistency_checked);

    // 5. Open restored database: RecoveryEngine replays WAL redo records.
    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, "stage2_unflushed_survived");

    let consistency = restored_db.verify_consistency().unwrap();
    assert!(
        consistency.is_healthy(),
        "Restored consistency not healthy: {:?}",
        consistency.issues
    );
}

#[test]
fn test_stage4_publication_failure_db_publish_rolls_back_original_destination() {
    let (mut orig_db, _orig_dir, orig_db_path) = setup_test_db("dest_db_publish_fault");
    let mut orig_model = LogicalModel::default();
    let mut orig_rows = Vec::new();
    for i in 1..=10 {
        let row = make_test_row(i, &format!("ORIG_{i:04}"), i * 10, "OLD", vec![i as u8; 16]);
        orig_rows.push(row);
        orig_model.insert(i, &format!("ORIG_{i:04}"), i * 10, "OLD", vec![i as u8; 16]);
    }
    insert_rows(&mut orig_db, &orig_rows);
    orig_db.checkpoint_and_truncate_wal().unwrap();
    drop(orig_db);

    // Create a new backup to restore over original
    let (mut new_db, _new_dir, new_db_path) = setup_test_db("new_backup_src");
    let mut new_rows = Vec::new();
    for i in 100..=120 {
        let row = make_test_row(i, &format!("NEW_{i:04}"), i * 20, "NEW", vec![i as u8; 16]);
        new_rows.push(row);
    }
    insert_rows(&mut new_db, &new_rows);
    new_db.checkpoint_and_truncate_wal().unwrap();
    drop(new_db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    TreeDb::backup_database(&new_db_path, &bkp_path, &BackupConfig::default()).unwrap();

    // Inject failure at DuringRestoreDbPublish
    let rest_config = RestoreConfig {
        allow_overwrite: true,
        fault_point: Some(StorageFailurePoint::DuringRestoreDbPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let err = TreeDb::restore_backup(&bkp_path, &orig_db_path, &rest_config).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringRestoreDbPublish)
    ));

    // Verify on disk: journal was preserved in ExistingBackedUp state
    let journal_path = restore_journal_path(&orig_db_path);
    assert!(journal_path.exists());

    // Recover interrupted publication
    let recovered = TreeDb::recover_interrupted_restore(&orig_db_path).unwrap();
    assert!(recovered);
    assert!(!journal_path.exists());

    // Original destination was rolled back completely
    let mut reopened_db = TreeDb::open(&orig_db_path, 64).unwrap();
    orig_model.assert_matches_database(&mut reopened_db, "orig_after_db_publish_fault");
    let rep = reopened_db.verify_consistency().unwrap();
    assert!(rep.is_healthy());
}

#[test]
fn test_stage4_publication_failure_wal_publish_completes_forward() {
    let (mut orig_db, _orig_dir, orig_db_path) = setup_test_db("dest_wal_publish_fault");
    let mut orig_rows = Vec::new();
    for i in 1..=5 {
        let row = make_test_row(i, &format!("ORIG_{i:04}"), i * 10, "OLD", vec![i as u8; 16]);
        orig_rows.push(row);
    }
    insert_rows(&mut orig_db, &orig_rows);
    orig_db.checkpoint_and_truncate_wal().unwrap();
    drop(orig_db);

    // Create a new backup that HAS a companion WAL
    let (mut new_db, _new_dir, new_db_path) = setup_test_db("new_backup_with_wal");
    let mut new_model = LogicalModel::default();
    let mut batch1 = Vec::new();
    for i in 50..=60 {
        let row = make_test_row(
            i,
            &format!("NEW_{i:04}"),
            i * 50,
            "NEW_WAL",
            vec![i as u8; 16],
        );
        batch1.push(row);
        new_model.insert(
            i,
            &format!("NEW_{i:04}"),
            i * 50,
            "NEW_WAL",
            vec![i as u8; 16],
        );
    }
    insert_rows(&mut new_db, &batch1);
    new_db.checkpoint_and_truncate_wal().unwrap();

    let mut txn = new_db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        for i in 61..=70 {
            let row = make_test_row(
                i,
                &format!("NEW_{i:04}"),
                i * 50,
                "NEW_WAL",
                vec![i as u8; 16],
            );
            tbl.insert(&row).unwrap();
            new_model.insert(
                i,
                &format!("NEW_{i:04}"),
                i * 50,
                "NEW_WAL",
                vec![i as u8; 16],
            );
        }
    }
    txn.commit().unwrap();
    drop(new_db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let bkp_config = BackupConfig {
        fault_point: None,
        checkpoint_before_backup: false,
        verify_image: true,
    };
    let bkp_rep = TreeDb::backup_database(&new_db_path, &bkp_path, &bkp_config).unwrap();
    assert!(bkp_rep.manifest.has_wal);

    // Inject failure at DuringRestoreWalPublish (after DB has been published to dest)
    let rest_config = RestoreConfig {
        allow_overwrite: true,
        fault_point: Some(StorageFailurePoint::DuringRestoreWalPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let err = TreeDb::restore_backup(&bkp_path, &orig_db_path, &rest_config).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringRestoreWalPublish)
    ));

    // Verify on disk: journal exists in DbPublished state
    let journal_path = restore_journal_path(&orig_db_path);
    assert!(journal_path.exists());

    // Recover interrupted publication: moves forward and publishes WAL
    let recovered = TreeDb::recover_interrupted_restore(&orig_db_path).unwrap();
    assert!(recovered);
    assert!(!journal_path.exists());

    // Reopening destination reflects the newly restored data completely
    let mut reopened_db = TreeDb::open(&orig_db_path, 64).unwrap();
    new_model.assert_matches_database(&mut reopened_db, "restored_forward_after_wal_publish_fault");
    let rep = reopened_db.verify_consistency().unwrap();
    assert!(rep.is_healthy());
}

#[test]
fn test_stage4_open_automatically_recovers_interrupted_journal() {
    let (mut orig_db, _orig_dir, orig_db_path) = setup_test_db("dest_auto_recover");
    let mut orig_model = LogicalModel::default();
    let mut orig_rows = Vec::new();
    for i in 1..=5 {
        let row = make_test_row(i, &format!("ORIG_{i:04}"), i * 10, "OLD", vec![i as u8; 16]);
        orig_rows.push(row);
        orig_model.insert(i, &format!("ORIG_{i:04}"), i * 10, "OLD", vec![i as u8; 16]);
    }
    insert_rows(&mut orig_db, &orig_rows);
    orig_db.checkpoint_and_truncate_wal().unwrap();
    drop(orig_db);

    let (mut new_db, _new_dir, new_db_path) = setup_test_db("new_auto_src");
    let mut new_rows = Vec::new();
    for i in 1..=5 {
        let row = make_test_row(i, &format!("NEW_{i:04}"), i * 20, "NEW", vec![i as u8; 16]);
        new_rows.push(row);
    }
    insert_rows(&mut new_db, &new_rows);
    new_db.checkpoint_and_truncate_wal().unwrap();
    drop(new_db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    TreeDb::backup_database(&new_db_path, &bkp_path, &BackupConfig::default()).unwrap();

    let rest_config = RestoreConfig {
        allow_overwrite: true,
        fault_point: Some(StorageFailurePoint::DuringRestoreDbPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let _ = TreeDb::restore_backup(&bkp_path, &orig_db_path, &rest_config);
    let journal_path = restore_journal_path(&orig_db_path);
    assert!(journal_path.exists());

    // TreeDb::open should automatically recover the journal before initializing
    let mut reopened = TreeDb::open(&orig_db_path, 64).unwrap();
    assert!(!journal_path.exists());
    orig_model.assert_matches_database(&mut reopened, "auto_recovered_orig");
}

#[test]
fn test_stage5_backup_failure_points_validation_and_sync() {
    let (mut db, _dir, db_path) = setup_test_db("bkp_faults");
    let mut rows = Vec::new();
    for i in 1..=5 {
        let row = make_test_row(i, &format!("ACC_{i:04}"), i * 10, "BKP", vec![i as u8; 16]);
        rows.push(row);
    }
    insert_rows(&mut db, &rows);
    db.checkpoint_and_truncate_wal().unwrap();
    drop(db);

    let bkp_dir = tempdir().unwrap();

    // 1. Failure at DuringBackupValidation
    let bkp_path_1 = bkp_dir.path().join("bkp_val");
    let cfg_val = BackupConfig {
        fault_point: Some(StorageFailurePoint::DuringBackupValidation),
        checkpoint_before_backup: true,
        verify_image: true,
    };
    let err = TreeDb::backup_database(&db_path, &bkp_path_1, &cfg_val).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringBackupValidation)
    ));
    assert!(!bkp_path_1.exists());

    // 2. Failure at DuringBackupSync
    let bkp_path_2 = bkp_dir.path().join("bkp_sync");
    let cfg_sync = BackupConfig {
        fault_point: Some(StorageFailurePoint::DuringBackupSync),
        checkpoint_before_backup: true,
        verify_image: true,
    };
    let err = TreeDb::backup_database(&db_path, &bkp_path_2, &cfg_sync).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringBackupSync)
    ));
    assert!(!bkp_path_2.exists());
}

#[test]
fn test_stage6_manifest_bounds_and_path_traversal_rejection() {
    let (mut db, _dir, db_path) = setup_test_db("manifest_bounds_db");
    let row = make_test_row(1, "ACC_1", 100, "BOUNDS", vec![1; 16]);
    insert_rows(&mut db, &[row]);
    db.checkpoint_and_truncate_wal().unwrap();
    drop(db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    TreeDb::backup_database(&db_path, &bkp_path, &BackupConfig::default()).unwrap();

    let manifest_path = bkp_path.join(MANIFEST_FILE_NAME);

    // 1. Manifest length > 64KB
    let oversized = vec![0u8; 65537];
    fs::write(&manifest_path, &oversized).unwrap();
    let err = TreeDb::inspect_backup_manifest(&bkp_path).unwrap_err();
    assert!(matches!(err, BackupError::InvalidManifest(_)));

    // 2. Path traversal in db_file_name
    let mut manifest = TreeDb::backup_database(&db_path, &bkp_path, &BackupConfig::default())
        .unwrap()
        .manifest;
    manifest.db_file_name = "../traversal.db".to_string();
    let bytes = manifest.serialize();
    fs::write(&manifest_path, &bytes).unwrap();
    let err = TreeDb::inspect_backup_manifest(&bkp_path).unwrap_err();
    assert!(matches!(err, BackupError::InvalidManifest(_)));

    // Windows backslash traversal
    manifest.db_file_name = "..\\traversal.db".to_string();
    let bytes = manifest.serialize();
    fs::write(&manifest_path, &bytes).unwrap();
    let err = TreeDb::inspect_backup_manifest(&bkp_path).unwrap_err();
    assert!(matches!(err, BackupError::InvalidManifest(_)));
}

#[test]
fn test_stage6_open_read_only_unrecovered_does_not_mutate_disk() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("immutability_test.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();
    db.create_table("accounts", make_test_schema()).unwrap();
    db.checkpoint_and_truncate_wal().unwrap();
    drop(db);

    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    let _ = fs::remove_file(&wal_path);
    assert!(!wal_path.exists());

    // Open read-only unrecovered
    let unrecovered = TreeDb::open_read_only_unrecovered(&db_path, 64).unwrap();
    // Must NOT have created a WAL file on disk
    assert!(!wal_path.exists());
    drop(unrecovered);
    assert!(!wal_path.exists());
}

#[test]
fn test_stage8_persistence_lifecycle_regressions() {
    let (mut db, _src_dir, _db_path) = setup_test_db("lifecycle_db");
    let mut rows = Vec::new();
    for i in 1..=10 {
        let row = make_test_row(i, &format!("ACC_{i:04}"), i * 10, "LIFE", vec![i as u8; 16]);
        rows.push(row);
    }
    insert_rows(&mut db, &rows);
    db.checkpoint_and_truncate_wal().unwrap();

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let bkp_rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();
    assert_eq!(
        bkp_rep.validation.as_ref().unwrap().status,
        BackupAuditStatus::FullyHealthy
    );

    // 1. Restore -> reopen -> new write
    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    let new_row = make_test_row(100, "ACC_0100", 1000, "NEW_WRITE", vec![100; 16]);
    insert_rows(&mut restored_db, &[new_row]);
    let rep = restored_db.verify_consistency().unwrap();
    assert!(rep.is_healthy());

    // 2. Restore -> checkpoint -> reopen
    restored_db.checkpoint_and_truncate_wal().unwrap();
    drop(restored_db);
    let mut reopened_db = TreeDb::open(&rest_db_path, 64).unwrap();
    let rep2 = reopened_db.verify_consistency().unwrap();
    assert!(rep2.is_healthy());

    // 3. Vacuum -> reopen
    let vac_res = reopened_db.vacuum(&VacuumConfig::default()).unwrap();
    assert!(vac_res.pages_examined > 0);
    drop(reopened_db);
    let mut reopened_db2 = TreeDb::open(&rest_db_path, 64).unwrap();
    assert!(reopened_db2.verify_consistency().unwrap().is_healthy());
}

#[test]
fn test_stage2_corrupt_journal_rejected_without_destructive_cleanup() {
    let (mut db, _dir, db_path) = setup_test_db("corrupt_journal_db");
    let mut model = LogicalModel::default();
    let row = make_test_row(1, "ACC_0001", 500, "CORRUPT_TEST", vec![1; 16]);
    insert_rows(&mut db, &[row]);
    model.insert(1, "ACC_0001", 500, "CORRUPT_TEST", vec![1; 16]);
    db.checkpoint_and_truncate_wal().unwrap();
    drop(db);

    // Place a corrupted restore journal at <db_path>.restore_journal
    let journal_path = restore_journal_path(&db_path);
    let mut corrupt_bytes = Vec::new();
    corrupt_bytes.extend_from_slice(RESTORE_JOURNAL_MAGIC);
    corrupt_bytes.extend_from_slice(&[1, 0, 0, 0]); // version 1
    corrupt_bytes.extend_from_slice(&[0xFF; 20]); // garbage payload with invalid CRC
    fs::write(&journal_path, &corrupt_bytes).unwrap();

    // 1. Explicit recover_interrupted_restore must reject corrupt journal
    let err = TreeDb::recover_interrupted_restore(&db_path).unwrap_err();
    assert!(matches!(
        err,
        BackupError::ChecksumMismatch { .. } | BackupError::InvalidManifest(_)
    ));
    // Journal file and database file must NOT be deleted
    assert!(journal_path.exists());
    assert!(db_path.exists());

    // 2. TreeDb::open must also propagate error and not silently destroy disk
    let open_err = TreeDb::open(&db_path, 64).err().unwrap();
    let err_str = open_err.to_string();
    assert!(err_str.contains("interrupted restore journal recovery failed"));

    // Remove the corrupt journal manually so we can verify the DB itself was never destroyed
    let _ = fs::remove_file(&journal_path);
    let mut reopened = TreeDb::open(&db_path, 64).unwrap();
    model.assert_matches_database(&mut reopened, "corrupt_journal_safe");
}

#[test]
fn test_stage2_existing_backed_up_fresh_destination_cleans_orphaned_files() {
    let (mut src_db, _src_dir, src_db_path) = setup_test_db("fresh_src_db");
    let row = make_test_row(1, "SRC_001", 100, "FRESH", vec![1; 16]);
    insert_rows(&mut src_db, &[row]);
    src_db.checkpoint_and_truncate_wal().unwrap();
    drop(src_db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    TreeDb::backup_database(&src_db_path, &bkp_path, &BackupConfig::default()).unwrap();

    // Fresh destination that does NOT exist beforehand
    let dest_dir = tempdir().unwrap();
    let dest_db_path = dest_dir.path().join("fresh_dest.treedb");
    assert!(!dest_db_path.exists());

    let rest_cfg = RestoreConfig {
        allow_overwrite: false,
        fault_point: Some(StorageFailurePoint::DuringRestoreDbPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let err = TreeDb::restore_backup(&bkp_path, &dest_db_path, &rest_cfg).unwrap_err();
    assert!(matches!(
        err,
        BackupError::InjectedFault(StorageFailurePoint::DuringRestoreDbPublish)
    ));

    let journal_path = restore_journal_path(&dest_db_path);
    assert!(journal_path.exists());

    // Recovery rolls back: since destination was fresh and had no existing DB,
    // dest_db_path must be removed and not left as an orphaned file!
    let recovered = TreeDb::recover_interrupted_restore(&dest_db_path).unwrap();
    assert!(recovered);
    assert!(!journal_path.exists());
    assert!(!dest_db_path.exists());
}

#[test]
fn test_stage2_db_published_missing_wal_rolls_back_to_valid_destination() {
    let (mut orig_db, _orig_dir, orig_db_path) = setup_test_db("orig_rollback_db");
    let mut orig_model = LogicalModel::default();
    let row = make_test_row(1, "ORIG_1", 200, "ORIG", vec![2; 16]);
    insert_rows(&mut orig_db, &[row]);
    orig_model.insert(1, "ORIG_1", 200, "ORIG", vec![2; 16]);
    orig_db.checkpoint_and_truncate_wal().unwrap();
    drop(orig_db);

    // Create a backup with a companion WAL
    let (mut new_db, _new_dir, new_db_path) = setup_test_db("new_wal_src_db");
    let row2 = make_test_row(2, "NEW_1", 300, "NEW", vec![3; 16]);
    insert_rows(&mut new_db, &[row2]);
    new_db.checkpoint_and_truncate_wal().unwrap();
    // Commit uncheckpointed transaction to force WAL inclusion in backup
    let mut txn = new_db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        let row3 = make_test_row(3, "NEW_2", 400, "NEW", vec![4; 16]);
        tbl.insert(&row3).unwrap();
    }
    txn.commit().unwrap();
    drop(new_db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let bkp_rep = TreeDb::backup_database(
        &new_db_path,
        &bkp_path,
        &BackupConfig {
            fault_point: None,
            checkpoint_before_backup: false,
            verify_image: true,
        },
    )
    .unwrap();
    assert!(bkp_rep.manifest.has_wal);

    // Restore with DuringRestoreWalPublish fault
    let rest_cfg = RestoreConfig {
        allow_overwrite: true,
        fault_point: Some(StorageFailurePoint::DuringRestoreWalPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let _ = TreeDb::restore_backup(&bkp_path, &orig_db_path, &rest_cfg);

    let journal_path = restore_journal_path(&orig_db_path);
    assert!(journal_path.exists());
    let bytes = fs::read(&journal_path).unwrap();
    let journal = RestoreJournal::deserialize(&bytes, journal_path.clone()).unwrap();
    assert_eq!(journal.state, RestoreJournalState::DbPublished);

    // Now delete the staged WAL, simulating physical file loss/corruption during crash
    if let Some(ref staged_wal) = journal.staged_wal_path {
        if staged_wal.exists() {
            fs::remove_file(staged_wal).unwrap();
        }
    }

    // Now recovery cannot roll forward because staged_wal is gone!
    // It must roll back to the previous valid destination (bak_db) rather than
    // destroying bak_db and leaving a broken DB without its companion WAL!
    let rec_res = TreeDb::recover_interrupted_restore(&orig_db_path);
    assert!(rec_res.is_err());
    assert!(!journal_path.exists());

    // Reopen original destination: must be completely intact with original 1 row!
    let mut reopened = TreeDb::open(&orig_db_path, 64).unwrap();
    orig_model.assert_matches_database(&mut reopened, "rolled_back_after_missing_wal");
}

#[test]
fn test_stage2_repeated_journal_recovery_is_idempotent() {
    let (mut db, _dir, db_path) = setup_test_db("idempotent_recovery_db");
    let row = make_test_row(1, "ACC_1", 100, "IDEM", vec![1; 16]);
    insert_rows(&mut db, &[row]);
    db.checkpoint_and_truncate_wal().unwrap();
    drop(db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    TreeDb::backup_database(&db_path, &bkp_path, &BackupConfig::default()).unwrap();

    let rest_dir = tempdir().unwrap();
    let dest_db_path = rest_dir.path().join("dest.treedb");

    let rest_cfg = RestoreConfig {
        allow_overwrite: false,
        fault_point: Some(StorageFailurePoint::DuringRestoreDbPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let _ = TreeDb::restore_backup(&bkp_path, &dest_db_path, &rest_cfg);

    // First recovery pass
    let r1 = TreeDb::recover_interrupted_restore(&dest_db_path).unwrap();
    assert!(r1);

    // Second recovery pass: journal is gone, must safely return Ok(false)
    let r2 = TreeDb::recover_interrupted_restore(&dest_db_path).unwrap();
    assert!(!r2);

    // Third recovery pass
    let r3 = TreeDb::recover_interrupted_restore(&dest_db_path).unwrap();
    assert!(!r3);
}

#[test]
fn test_stage3_end_to_end_lifecycle_with_independent_model() {
    let (mut db, _dir, _db_path) = setup_test_db("e2e_lifecycle_db");
    let mut model = LogicalModel::default();

    // 1. Initial Inserts
    let mut rows = Vec::new();
    for i in 1..=20 {
        let row = make_test_row(
            i,
            &format!("ACC_{i:04}"),
            i * 50,
            "INITIAL",
            vec![i as u8; 32],
        );
        rows.push(row);
        model.insert(
            i,
            &format!("ACC_{i:04}"),
            i * 50,
            "INITIAL",
            vec![i as u8; 32],
        );
    }
    insert_rows(&mut db, &rows);
    model.assert_matches_database(&mut db, "after_initial_inserts");

    // 2. Updates & Deletes
    let mut txn = db.begin().unwrap();
    {
        let mut tbl = txn.open_table("accounts").unwrap();
        // Update first 5 rows
        for i in 1..=5 {
            let updated =
                make_test_row(i, &format!("ACC_{i:04}"), i * 999, "UPDATED", vec![99; 32]);
            tbl.update(&updated).unwrap();
            model.update(i, &format!("ACC_{i:04}"), i * 999, "UPDATED", vec![99; 32]);
        }
        // Delete rows 15..20
        for i in 15..=20 {
            tbl.delete(&Value::Int64(i)).unwrap();
            model.delete(i);
        }
    }
    txn.commit().unwrap();
    model.assert_matches_database(&mut db, "after_updates_and_deletes");

    // 3. Overflow Record (8 KB payload)
    let mut txn2 = db.begin().unwrap();
    {
        let mut tbl = txn2.open_table("accounts").unwrap();
        let overflow_row = make_test_row(25, "ACC_OVERFLOW", 12345, "OVERFLOW", vec![0xAB; 8192]);
        tbl.insert(&overflow_row).unwrap();
        model.insert(25, "ACC_OVERFLOW", 12345, "OVERFLOW", vec![0xAB; 8192]);
    }
    txn2.commit().unwrap();
    model.assert_matches_database(&mut db, "after_overflow_insert");

    // 4. Aborted Transaction
    let mut txn_abort = db.begin().unwrap();
    {
        let mut tbl = txn_abort.open_table("accounts").unwrap();
        let abort_row = make_test_row(99, "ACC_ABORT", 9999, "ABORTED", vec![0xFF; 16]);
        tbl.insert(&abort_row).unwrap();
    }
    txn_abort.abort().unwrap();
    model.assert_matches_database(&mut db, "after_aborted_transaction");

    // 5. Checkpoint
    db.checkpoint_and_truncate_wal().unwrap();
    let rep_chk = db.verify_consistency().unwrap();
    assert!(rep_chk.is_healthy());
    assert!(!rep_chk.truncated);
    assert!(rep_chk.skipped_checks.is_empty());

    // 6. Vacuum
    let vac_rep = db.vacuum(&VacuumConfig::default()).unwrap();
    assert!(vac_rep.pages_examined > 0);
    let rep_vac = db.verify_consistency().unwrap();
    assert!(rep_vac.is_healthy());

    // 7. Backup Creation
    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    let bkp_rep = db
        .create_backup(&bkp_path, &BackupConfig::default())
        .unwrap();
    assert_eq!(
        bkp_rep.validation.as_ref().unwrap().status,
        BackupAuditStatus::FullyHealthy
    );

    // 8. Restore into Fresh Destination
    let rest_dir = tempdir().unwrap();
    let rest_db_path = rest_dir.path().join("restored_e2e.treedb");
    TreeDb::restore_backup(&bkp_path, &rest_db_path, &RestoreConfig::default()).unwrap();

    // 9. Reopen Restored DB & Verify against independent model
    let mut restored_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut restored_db, "restored_database");
    let rep_rest = restored_db.verify_consistency().unwrap();
    assert!(rep_rest.is_healthy());

    // 10. New writes on restored DB
    let mut txn3 = restored_db.begin().unwrap();
    {
        let mut tbl = txn3.open_table("accounts").unwrap();
        for i in 101..=105 {
            let row = make_test_row(
                i,
                &format!("NEW_{i:04}"),
                i * 100,
                "POST_RESTORE",
                vec![i as u8; 16],
            );
            tbl.insert(&row).unwrap();
            model.insert(
                i,
                &format!("NEW_{i:04}"),
                i * 100,
                "POST_RESTORE",
                vec![i as u8; 16],
            );
        }
    }
    txn3.commit().unwrap();
    model.assert_matches_database(&mut restored_db, "post_restore_writes");

    // 11. Final Reopen & Checkpoint
    restored_db.checkpoint_and_truncate_wal().unwrap();
    drop(restored_db);
    let mut final_db = TreeDb::open(&rest_db_path, 64).unwrap();
    model.assert_matches_database(&mut final_db, "final_reopened_state");
    let final_rep = final_db.verify_consistency().unwrap();
    assert!(final_rep.is_healthy());
    assert!(!final_rep.truncated);
    assert!(final_rep.skipped_checks.is_empty());
}

#[test]
fn test_stage7_stale_journal_prevents_unauthorized_overwrite() {
    let (mut orig_db, _dir, orig_db_path) = setup_test_db("stale_journal_overwrite_db");
    let row = make_test_row(1, "ORIG_1", 100, "ORIG", vec![1; 16]);
    insert_rows(&mut orig_db, &[row]);
    orig_db.checkpoint_and_truncate_wal().unwrap();
    drop(orig_db);

    let (mut new_db, _new_dir, new_db_path) = setup_test_db("new_backup_src_db");
    let row2 = make_test_row(2, "NEW_1", 200, "NEW", vec![2; 16]);
    insert_rows(&mut new_db, &[row2]);
    new_db.checkpoint_and_truncate_wal().unwrap();
    drop(new_db);

    let bkp_dir = tempdir().unwrap();
    let bkp_path = bkp_dir.path().join("backup");
    TreeDb::backup_database(&new_db_path, &bkp_path, &BackupConfig::default()).unwrap();

    // Start restore with overwrite: true, but inject fault at DuringRestoreDbPublish
    // This moves orig_db_path to .bak_* and creates a journal in ExistingBackedUp state
    let rest_cfg = RestoreConfig {
        allow_overwrite: true,
        fault_point: Some(StorageFailurePoint::DuringRestoreDbPublish),
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let _ = TreeDb::restore_backup(&bkp_path, &orig_db_path, &rest_cfg);
    let journal_path = restore_journal_path(&orig_db_path);
    assert!(journal_path.exists());

    // Now attempt a subsequent restore with allow_overwrite: false
    // Restore must recover the journal (restoring original DB from .bak) and then REJECT
    // because destination exists and overwrite was not allowed!
    let rest_cfg_disallowed = RestoreConfig {
        allow_overwrite: false,
        fault_point: None,
        validate_before_restore: true,
        verify_after_restore: false,
        buffer_pool_size: 64,
    };
    let err = TreeDb::restore_backup(&bkp_path, &orig_db_path, &rest_cfg_disallowed).unwrap_err();
    assert!(matches!(err, BackupError::DestinationExists(_)));

    // Verify original DB is operational and undamaged
    let mut reopened = TreeDb::open(&orig_db_path, 64).unwrap();
    let rep = reopened.verify_consistency().unwrap();
    assert!(rep.is_healthy());
}

#[test]
fn test_stage7_open_read_only_unrecovered_rejects_pending_journal() {
    let (mut db, _dir, db_path) = setup_test_db("read_only_journal_reject_db");
    let row = make_test_row(1, "RO_1", 100, "RO", vec![1; 16]);
    insert_rows(&mut db, &[row]);
    db.checkpoint_and_truncate_wal().unwrap();
    drop(db);

    // Place a journal file
    let journal_path = restore_journal_path(&db_path);
    let journal = RestoreJournal {
        journal_path: journal_path.clone(),
        state: RestoreJournalState::Staged,
        dest_db_path: db_path.clone(),
        dest_wal_path: None,
        staged_db_path: PathBuf::from("fake_staged.db"),
        staged_wal_path: None,
        bak_db_path: None,
        bak_wal_path: None,
        dest_had_db: true,
        dest_had_wal: false,
    };
    journal.write_to_disk().unwrap();

    // open_read_only_unrecovered must reject because pending restore publication is unresolved
    let err = TreeDb::open_read_only_unrecovered(&db_path, 64)
        .err()
        .unwrap();
    assert!(err
        .to_string()
        .contains("interrupted restore publication is pending"));

    // Remove journal and verify normal open succeeds
    let _ = fs::remove_file(&journal_path);
    let unrecovered = TreeDb::open_read_only_unrecovered(&db_path, 64).unwrap();
    drop(unrecovered);
}
