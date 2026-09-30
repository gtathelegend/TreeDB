use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use treedb::engine::types::Value;
use treedb::engine::{MultiIndexConfig, StatementResult, TreeDb};
use treedb::error::Error;
use treedb::index::RecordId;
use treedb::query::external_multi_index::{RecordIdRunReader, RecordIdRunWriter, SpillOpType};

/// Helper to create and cleanly remove temporary test database environments.
struct Ctx {
    db_path: PathBuf,
    temp_dir: PathBuf,
}

impl Ctx {
    fn new(name: &str) -> Self {
        let base_temp = std::env::temp_dir();
        let unique = format!(
            "treedb_spill_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let db_path = base_temp.join(format!("{unique}.db"));
        let temp_dir = base_temp.join(format!("{unique}_tmp"));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{}.wal", db_path.display()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);

        Self { db_path, temp_dir }
    }

    fn open(&self) -> TreeDb {
        if self.db_path.exists() {
            TreeDb::open(&self.db_path, 100).unwrap()
        } else {
            TreeDb::create(&self.db_path, 100).unwrap()
        }
    }

    fn count_temp_files(&self) -> usize {
        if !self.temp_dir.exists() {
            return 0;
        }
        std::fs::read_dir(&self.temp_dir)
            .map(|rd| rd.flatten().count())
            .unwrap_or(0)
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(format!("{}.wal", self.db_path.display()));
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

fn exec_sql(db: &mut TreeDb, sql: &str) -> treedb::error::Result<StatementResult> {
    let mut txn = db.begin_transaction()?;
    let res = txn.execute_statement(sql)?;
    txn.commit()?;
    Ok(res)
}

fn val_i64(v: &Value) -> i64 {
    match v {
        Value::Int64(i) => *i,
        Value::UInt64(u) => *u as i64,
        other => panic!("Expected integer value, got: {other:?}"),
    }
}

fn explain_analyze_text(txn: &mut treedb::engine::Transaction, sql: &str) -> String {
    let res = txn.execute_sql(&format!("EXPLAIN ANALYZE {sql}")).unwrap();
    match &res.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("Expected string from EXPLAIN ANALYZE"),
    }
}

// ---------------------------------------------------------------------------
// R131 – Small intersection stays in memory
// ---------------------------------------------------------------------------
#[test]
fn test_r131_small_intersection_stays_in_memory() {
    let ctx = Ctx::new("r131");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(64 * 1024)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE users (id INT PRIMARY KEY, age INT, status INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_age ON users (age)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_status ON users (status)").unwrap();

    for i in 1..=500 {
        let age = 20 + (i % 25);
        let status = (i / 25) % 20;
        exec_sql(
            &mut db,
            &format!("INSERT INTO users VALUES ({i}, {age}, {status})"),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE users").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM users WHERE age = 30 AND status = 2")
        .unwrap();

    assert_eq!(qr.rows.len(), 1);
    let explain = explain_analyze_text(
        &mut txn,
        "SELECT id FROM users WHERE age = 30 AND status = 2",
    );
    assert!(
        explain.contains("InMemory") || !explain.contains("spill_runs_created"),
        "Small query must not spill: {explain}"
    );
    drop(txn);
    assert_eq!(ctx.count_temp_files(), 0);
}

// ---------------------------------------------------------------------------
// R132 – Large intersection spills
// ---------------------------------------------------------------------------
#[test]
fn test_r132_large_intersection_spills() {
    let ctx = Ctx::new("r132");
    let mut db = ctx.open();
    // Low budget: 128 bytes forces spilling for > 16 candidate RecordIds
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(128)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE items (id INT PRIMARY KEY, category INT, active INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_cat ON items (category)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_act ON items (active)").unwrap();

    for i in 1..=500 {
        let cat = 20 + (i % 25);
        let act = (i / 25) % 20;
        exec_sql(
            &mut db,
            &format!("INSERT INTO items VALUES ({i}, {cat}, {act})"),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE items").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM items WHERE category = 30 AND active = 2")
        .unwrap();

    assert_eq!(qr.rows.len(), 1);
    let explain = explain_analyze_text(
        &mut txn,
        "SELECT id FROM items WHERE category = 30 AND active = 2",
    );
    assert!(
        explain.contains("spill_runs_created") || explain.contains("runs_created"),
        "Large query with tight budget must spill: {explain}"
    );
    txn.commit().unwrap();
    assert_eq!(
        ctx.count_temp_files(),
        0,
        "Temporary files must be deleted after execution"
    );
}

// ---------------------------------------------------------------------------
// R133 – Large union spills
// ---------------------------------------------------------------------------
#[test]
fn test_r133_large_union_spills() {
    let ctx = Ctx::new("r133");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(1024)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE items (id INT PRIMARY KEY, cat INT, tag INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_cat ON items (cat)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_tag ON items (tag)").unwrap();

    for i in 1..=300 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO items VALUES ({i}, {}, {})", i % 5, i % 7),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE items").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM items WHERE cat = 1 OR tag = 1")
        .unwrap();

    // Verification: count items where i % 5 == 1 or i % 7 == 1
    let expected_count = (1..=300).filter(|&i| i % 5 == 1 || i % 7 == 1).count();
    assert_eq!(qr.rows.len(), expected_count);
    txn.commit().unwrap();
    assert_eq!(ctx.count_temp_files(), 0);
}

// ---------------------------------------------------------------------------
// R134 – Three-index intersection with spill
// ---------------------------------------------------------------------------
#[test]
fn test_r134_three_index_intersection_spills() {
    let ctx = Ctx::new("r134");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(1024)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, c INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON t (b)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c ON t (c)").unwrap();

    for i in 1..=300 {
        exec_sql(
            &mut db,
            &format!(
                "INSERT INTO t VALUES ({i}, {}, {}, {})",
                i % 2,
                i % 3,
                i % 5
            ),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM t WHERE a = 0 AND b = 0 AND c = 0")
        .unwrap();

    // Multiples of 2*3*5 = 30 -> 300 / 30 = 10 rows
    assert_eq!(qr.rows.len(), 10);
    txn.commit().unwrap();
    assert_eq!(ctx.count_temp_files(), 0);
}

// ---------------------------------------------------------------------------
// R135 – Three-index union with spill
// ---------------------------------------------------------------------------
#[test]
fn test_r135_three_index_union_spills() {
    let ctx = Ctx::new("r135");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(1024)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT, c INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON t (b)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c ON t (c)").unwrap();

    for i in 1..=300 {
        exec_sql(
            &mut db,
            &format!(
                "INSERT INTO t VALUES ({i}, {}, {}, {})",
                i % 2,
                i % 3,
                i % 5
            ),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM t WHERE a = 0 OR b = 0 OR c = 0")
        .unwrap();

    let expected_count = (1..=300)
        .filter(|&i| i % 2 == 0 || i % 3 == 0 || i % 5 == 0)
        .count();
    assert_eq!(qr.rows.len(), expected_count);
    txn.commit().unwrap();
    assert_eq!(ctx.count_temp_files(), 0);
}

// ---------------------------------------------------------------------------
// R136 – Intersection duplicate behavior
// ---------------------------------------------------------------------------
#[test]
fn test_r136_intersection_duplicate_behavior() {
    let ctx = Ctx::new("r136");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE dup_t (id INT PRIMARY KEY, x INT, y INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_x ON dup_t (x)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_y ON dup_t (y)").unwrap();

    // Insert records with duplicate indexed values
    for i in 1..=50 {
        exec_sql(&mut db, &format!("INSERT INTO dup_t VALUES ({i}, 10, 20)")).unwrap();
    }
    exec_sql(&mut db, "ANALYZE dup_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM dup_t WHERE x = 10 AND y = 20")
        .unwrap();

    assert_eq!(qr.rows.len(), 50);
    // Verify each ID is returned exactly once
    let ids: std::collections::HashSet<i64> =
        qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert_eq!(ids.len(), 50);
}

// ---------------------------------------------------------------------------
// R137 – Union duplicate elimination
// ---------------------------------------------------------------------------
#[test]
fn test_r137_union_duplicate_elimination() {
    let ctx = Ctx::new("r137");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE u_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON u_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON u_t (b)").unwrap();

    // 100 rows where both a=5 and b=5 (total overlap)
    for i in 1..=100 {
        exec_sql(&mut db, &format!("INSERT INTO u_t VALUES ({i}, 5, 5)")).unwrap();
    }
    exec_sql(&mut db, "ANALYZE u_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM u_t WHERE a = 5 OR b = 5")
        .unwrap();

    assert_eq!(qr.rows.len(), 100);
    let ids: std::collections::HashSet<i64> =
        qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert_eq!(ids.len(), 100);
}

// ---------------------------------------------------------------------------
// R138 – Sorted RecordId merge correctness
// ---------------------------------------------------------------------------
#[test]
fn test_r138_sorted_record_id_merge_correctness() {
    let ctx = Ctx::new("r138");
    let temp_dir = ctx.temp_dir.clone();

    // Write two separate sorted run files directly
    let mut writer1 = RecordIdRunWriter::create_in(&temp_dir, SpillOpType::ChildRun).unwrap();
    writer1
        .write_record(RecordId {
            page_id: 1,
            slot_id: 10,
        })
        .unwrap();
    writer1
        .write_record(RecordId {
            page_id: 2,
            slot_id: 5,
        })
        .unwrap();
    writer1
        .write_record(RecordId {
            page_id: 3,
            slot_id: 1,
        })
        .unwrap();
    let (run1, count1, _) = writer1.finish().unwrap();
    assert_eq!(count1, 3);

    let mut writer2 = RecordIdRunWriter::create_in(&temp_dir, SpillOpType::ChildRun).unwrap();
    writer2
        .write_record(RecordId {
            page_id: 1,
            slot_id: 5,
        })
        .unwrap();
    writer2
        .write_record(RecordId {
            page_id: 2,
            slot_id: 5,
        })
        .unwrap();
    writer2
        .write_record(RecordId {
            page_id: 4,
            slot_id: 0,
        })
        .unwrap();
    let (run2, count2, _) = writer2.finish().unwrap();
    assert_eq!(count2, 3);

    // Read and merge
    let reader1 = RecordIdRunReader::open(run1.path()).unwrap();
    let reader2 = RecordIdRunReader::open(run2.path()).unwrap();

    let merger =
        treedb::query::external_multi_index::KWayRecordIdMerger::new(vec![reader1, reader2])
            .unwrap();
    let mut merger = merger;
    let mut merged = Vec::new();
    while let Some(rid) = merger.next_record().unwrap() {
        merged.push(rid);
    }

    assert_eq!(merged.len(), 6);
    // Verify ascending order
    for w in merged.windows(2) {
        assert!(
            w[0] <= w[1],
            "Merged stream must be sorted: {:?} > {:?}",
            w[0],
            w[1]
        );
    }
}

// ---------------------------------------------------------------------------
// R139 – Memory budget enforcement
// ---------------------------------------------------------------------------
#[test]
fn test_r139_memory_budget_enforcement() {
    let ctx = Ctx::new("r139");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(128)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE budget_t (id INT PRIMARY KEY, c1 INT, c2 INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c1 ON budget_t (c1)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c2 ON budget_t (c2)").unwrap();

    for i in 1..=500 {
        let c1 = 20 + (i % 25);
        let c2 = (i / 25) % 20;
        exec_sql(
            &mut db,
            &format!("INSERT INTO budget_t VALUES ({i}, {c1}, {c2})"),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE budget_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM budget_t WHERE c1 = 30 AND c2 = 2")
        .unwrap();

    assert_eq!(qr.rows.len(), 1);
    let explain =
        explain_analyze_text(&mut txn, "SELECT id FROM budget_t WHERE c1 = 30 AND c2 = 2");
    assert!(
        explain.contains("spill_runs_created") || explain.contains("runs_created"),
        "Must enforce spilling with small budget: {explain}"
    );
}

// ---------------------------------------------------------------------------
// R140 – Spill file cleanup on success
// ---------------------------------------------------------------------------
#[test]
fn test_r140_spill_file_cleanup_on_success() {
    let ctx = Ctx::new("r140");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE cleanup_t (id INT PRIMARY KEY, v1 INT, v2 INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_v1 ON cleanup_t (v1)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_v2 ON cleanup_t (v2)").unwrap();

    for i in 1..=200 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO cleanup_t VALUES ({i}, 10, 20)"),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE cleanup_t").unwrap();

    for _ in 0..5 {
        let mut txn = db.begin_transaction().unwrap();
        let qr = txn
            .execute_sql("SELECT id FROM cleanup_t WHERE v1 = 10 AND v2 = 20")
            .unwrap();
        assert_eq!(qr.rows.len(), 200);
        txn.commit().unwrap();
        assert_eq!(ctx.count_temp_files(), 0);
    }
}

// ---------------------------------------------------------------------------
// R141 – Spill file cleanup on query error
// ---------------------------------------------------------------------------
#[test]
fn test_r141_spill_file_cleanup_on_query_error() {
    let ctx = Ctx::new("r141");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE err_t (id INT PRIMARY KEY, v1 INT, v2 INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_v1 ON err_t (v1)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_v2 ON err_t (v2)").unwrap();

    for i in 1..=100 {
        exec_sql(&mut db, &format!("INSERT INTO err_t VALUES ({i}, 1, 2)")).unwrap();
    }

    let mut txn = db.begin_transaction().unwrap();
    // Query with invalid non-existent column in select list
    let res = txn.execute_sql("SELECT non_existent FROM err_t WHERE v1 = 1 AND v2 = 2");
    assert!(res.is_err());
    drop(txn);

    assert_eq!(ctx.count_temp_files(), 0);
}

// ---------------------------------------------------------------------------
// R142 – Spill file cleanup on early LIMIT
// ---------------------------------------------------------------------------
#[test]
fn test_r142_spill_file_cleanup_on_early_limit() {
    let ctx = Ctx::new("r142");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE lim_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON lim_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON lim_t (b)").unwrap();

    for i in 1..=300 {
        exec_sql(&mut db, &format!("INSERT INTO lim_t VALUES ({i}, 1, 1)")).unwrap();
    }
    exec_sql(&mut db, "ANALYZE lim_t").unwrap();

    for limit in [0, 1, 5] {
        let mut txn = db.begin_transaction().unwrap();
        let qr = txn
            .execute_sql(&format!(
                "SELECT id FROM lim_t WHERE a = 1 AND b = 1 LIMIT {limit}"
            ))
            .unwrap();
        assert_eq!(qr.rows.len(), limit);
        txn.commit().unwrap();
        assert_eq!(
            ctx.count_temp_files(),
            0,
            "No temp files leaked after LIMIT {limit}"
        );
    }
}

// ---------------------------------------------------------------------------
// R143 – Spill corruption detection
// ---------------------------------------------------------------------------
#[test]
fn test_r143_spill_corruption_detection() {
    let ctx = Ctx::new("r143");
    let temp_dir = ctx.temp_dir.clone();

    // 1. Create a valid run file
    let mut writer = RecordIdRunWriter::create_in(&temp_dir, SpillOpType::ChildRun).unwrap();
    writer
        .write_record(RecordId {
            page_id: 1,
            slot_id: 10,
        })
        .unwrap();
    writer
        .write_record(RecordId {
            page_id: 2,
            slot_id: 20,
        })
        .unwrap();
    let (run_file, _, _) = writer.finish().unwrap();

    // 2. Corrupt a payload byte
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(run_file.path())
            .unwrap();
        // Seek into payload (header is 19 bytes)
        file.seek(SeekFrom::Start(20)).unwrap();
        file.write_all(&[0xFF]).unwrap();
    }

    // 3. Opening corrupted file should return Error::ExternalMultiIndexRunCorrupted
    let res = RecordIdRunReader::open(run_file.path());
    match res {
        Err(Error::ExternalMultiIndexRunCorrupted(msg)) => {
            assert!(
                msg.contains("CRC32 mismatch"),
                "Expected CRC32 error, got: {msg}"
            );
        }
        Err(other) => panic!("Expected ExternalMultiIndexRunCorrupted, got: {other:?}"),
        Ok(_) => panic!("Expected error, got Ok"),
    }
}

// ---------------------------------------------------------------------------
// R144 – Spill version validation
// ---------------------------------------------------------------------------
#[test]
fn test_r144_spill_version_validation() {
    let ctx = Ctx::new("r144");
    let temp_dir = ctx.temp_dir.clone();

    // 1. Create run file
    let mut writer = RecordIdRunWriter::create_in(&temp_dir, SpillOpType::ChildRun).unwrap();
    writer
        .write_record(RecordId {
            page_id: 1,
            slot_id: 10,
        })
        .unwrap();
    let (run_file, _, _) = writer.finish().unwrap();

    // 2. Corrupt version field (offset 4..6)
    {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(run_file.path())
            .unwrap();
        file.seek(SeekFrom::Start(4)).unwrap();
        file.write_all(&999u16.to_le_bytes()).unwrap();
    }

    let res = RecordIdRunReader::open(run_file.path());
    match res {
        Err(Error::ExternalMultiIndexRunCorrupted(msg)) => {
            assert!(
                msg.contains("unsupported"),
                "Expected unsupported version message, got: {msg}"
            );
        }
        Err(other) => panic!("Expected ExternalMultiIndexRunCorrupted, got: {other:?}"),
        Ok(_) => panic!("Expected error, got Ok"),
    }
}

// ---------------------------------------------------------------------------
// R145 – ORDER BY after external multi-index processing
// ---------------------------------------------------------------------------
#[test]
fn test_r145_order_by_after_external_multi_index() {
    let ctx = Ctx::new("r145");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE ord_t (id INT PRIMARY KEY, cat INT, val INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_cat ON ord_t (cat)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_val ON ord_t (val)").unwrap();

    // Insert in shuffled order
    for i in 1..=200 {
        let val = (i * 37) % 200;
        exec_sql(
            &mut db,
            &format!("INSERT INTO ord_t VALUES ({i}, {}, {val})", i % 2),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE ord_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id, val FROM ord_t WHERE cat = 0 AND val > 50 ORDER BY val ASC")
        .unwrap();

    assert!(!qr.rows.is_empty());
    let mut prev_val = -1;
    for row in &qr.rows {
        let v = val_i64(&row.values[1]);
        assert!(
            v >= prev_val,
            "ORDER BY val must be ascending: {v} < {prev_val}"
        );
        prev_val = v;
    }
}

// ---------------------------------------------------------------------------
// R146 – LIMIT/OFFSET after external processing
// ---------------------------------------------------------------------------
#[test]
fn test_r146_limit_offset_after_external_processing() {
    let ctx = Ctx::new("r146");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE page_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON page_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON page_t (b)").unwrap();

    for i in 1..=200 {
        exec_sql(&mut db, &format!("INSERT INTO page_t VALUES ({i}, 1, 1)")).unwrap();
    }
    exec_sql(&mut db, "ANALYZE page_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql(
            "SELECT id FROM page_t WHERE a = 1 AND b = 1 ORDER BY id ASC LIMIT 10 OFFSET 20",
        )
        .unwrap();

    assert_eq!(qr.rows.len(), 10);
    for (idx, row) in qr.rows.iter().enumerate() {
        assert_eq!(val_i64(&row.values[0]), (21 + idx) as i64);
    }
}

// ---------------------------------------------------------------------------
// R147 – Prepared statement + external multi-index
// ---------------------------------------------------------------------------
#[test]
fn test_r147_prepared_statement_external_multi_index() {
    let ctx = Ctx::new("r147");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE prep_t (id INT PRIMARY KEY, k1 INT, k2 INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_k1 ON prep_t (k1)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_k2 ON prep_t (k2)").unwrap();

    for i in 1..=200 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO prep_t VALUES ({i}, {}, {})", i % 2, i % 3),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE prep_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let stmt = txn
        .prepare_sql("SELECT id FROM prep_t WHERE k1 = ? AND k2 = ?")
        .unwrap();

    let qr = txn
        .execute_prepared_query(&stmt, &[Value::Int64(0), Value::Int64(0)])
        .unwrap();
    assert_eq!(qr.rows.len(), 33); // Multiples of 6 up to 200 = 33 rows
}

// ---------------------------------------------------------------------------
// R148 – Plan-cache reuse
// ---------------------------------------------------------------------------
#[test]
fn test_r148_plan_cache_reuse() {
    let ctx = Ctx::new("r148");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE pc_t (id INT PRIMARY KEY, x INT, y INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_x ON pc_t (x)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_y ON pc_t (y)").unwrap();

    for i in 1..=100 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO pc_t VALUES ({i}, {}, {})", i % 20, i % 20),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE pc_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let stmt = txn
        .prepare_sql("SELECT id FROM pc_t WHERE x = ? AND y = ?")
        .unwrap();

    let stats_before = txn.plan_cache_stats();
    let (qr1, m1) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(5), Value::Int64(5)])
        .unwrap();
    assert!(!m1.plan_cache_hit);
    assert!(!qr1.rows.is_empty());

    let (qr2, m2) = txn
        .execute_prepared_with_metrics(&stmt, &[Value::Int64(5), Value::Int64(5)])
        .unwrap();
    assert!(m2.plan_cache_hit, "Second execution should hit plan cache");
    assert!(!qr2.rows.is_empty());

    let stats_after = txn.plan_cache_stats();
    assert!(
        stats_after.hits > stats_before.hits,
        "Plan cache hits must increment"
    );
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R149 – CREATE/DROP INDEX invalidation
// ---------------------------------------------------------------------------
#[test]
fn test_r149_create_drop_index_invalidation() {
    let ctx = Ctx::new("r149");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE inv_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON inv_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON inv_t (b)").unwrap();

    for i in 1..=50 {
        exec_sql(&mut db, &format!("INSERT INTO inv_t VALUES ({i}, 1, 2)")).unwrap();
    }
    exec_sql(&mut db, "ANALYZE inv_t").unwrap();

    let sql = "SELECT id FROM inv_t WHERE a = 1 AND b = 2";

    // Populate plan cache
    let mut txn1 = db.begin_transaction().unwrap();
    let _ = txn1.execute_sql(sql).unwrap();
    txn1.commit().unwrap();

    // DROP index should invalidate cached plan
    exec_sql(&mut db, "DROP INDEX idx_b").unwrap();

    let mut txn2 = db.begin_transaction().unwrap();
    let qr = txn2.execute_sql(sql).unwrap();
    assert_eq!(qr.rows.len(), 50);
}

// ---------------------------------------------------------------------------
// R150 – ANALYZE statistics invalidation
// ---------------------------------------------------------------------------
#[test]
fn test_r150_analyze_statistics_invalidation() {
    let ctx = Ctx::new("r150");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE st_t (id INT PRIMARY KEY, c1 INT, c2 INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c1 ON st_t (c1)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c2 ON st_t (c2)").unwrap();

    for i in 1..=50 {
        exec_sql(&mut db, &format!("INSERT INTO st_t VALUES ({i}, 1, 2)")).unwrap();
    }
    exec_sql(&mut db, "ANALYZE st_t").unwrap();

    let stat_gen_before = db.statistics_generation();
    exec_sql(&mut db, "ANALYZE st_t").unwrap();
    assert!(
        db.statistics_generation() > stat_gen_before,
        "ANALYZE must increment statistics generation counter"
    );
}

// ---------------------------------------------------------------------------
// R151 – UPDATE using external multi-index candidates
// ---------------------------------------------------------------------------
#[test]
fn test_r151_update_using_external_multi_index() {
    let ctx = Ctx::new("r151");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE upd_t (id INT PRIMARY KEY, a INT, b INT, val INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON upd_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON upd_t (b)").unwrap();

    for i in 1..=200 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO upd_t VALUES ({i}, {}, {}, 0)", i % 2, i % 3),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE upd_t").unwrap();

    // UPDATE matching rows
    let res = exec_sql(&mut db, "UPDATE upd_t SET val = 999 WHERE a = 0 AND b = 0").unwrap();
    match res {
        StatementResult::Updated { rows } => assert_eq!(rows, 33),
        other => panic!("Expected Updated, got: {other:?}"),
    }

    // Verify
    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM upd_t WHERE val = 999")
        .unwrap();
    assert_eq!(qr.rows.len(), 33);
}

// ---------------------------------------------------------------------------
// R152 – DELETE using external multi-index candidates
// ---------------------------------------------------------------------------
#[test]
fn test_r152_delete_using_external_multi_index() {
    let ctx = Ctx::new("r152");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE del_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON del_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON del_t (b)").unwrap();

    for i in 1..=200 {
        exec_sql(
            &mut db,
            &format!("INSERT INTO del_t VALUES ({i}, {}, {})", i % 2, i % 3),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE del_t").unwrap();

    // DELETE matching rows
    let res = exec_sql(&mut db, "DELETE FROM del_t WHERE a = 0 AND b = 0").unwrap();
    match res {
        StatementResult::Deleted { rows } => assert_eq!(rows, 33),
        other => panic!("Expected Deleted, got: {other:?}"),
    }

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn.execute_sql("SELECT id FROM del_t").unwrap();
    assert_eq!(qr.rows.len(), 200 - 33);
}

// ---------------------------------------------------------------------------
// R153 – Transaction rollback
// ---------------------------------------------------------------------------
#[test]
fn test_r153_transaction_rollback() {
    let ctx = Ctx::new("r153");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE rb_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON rb_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON rb_t (b)").unwrap();

    for i in 1..=100 {
        exec_sql(&mut db, &format!("INSERT INTO rb_t VALUES ({i}, 10, 20)")).unwrap();
    }

    let mut txn = db.begin_transaction().unwrap();
    txn.execute_statement("DELETE FROM rb_t WHERE a = 10 AND b = 20")
        .unwrap();
    txn.abort().unwrap();

    let mut txn2 = db.begin_transaction().unwrap();
    let qr = txn2.execute_sql("SELECT id FROM rb_t").unwrap();
    assert_eq!(qr.rows.len(), 100);
}

// ---------------------------------------------------------------------------
// R154 – Checkpoint/recovery
// ---------------------------------------------------------------------------
#[test]
fn test_r154_checkpoint_recovery() {
    let ctx = Ctx::new("r154");
    {
        let mut db = ctx.open();
        db.set_multi_index_config(
            MultiIndexConfig::default()
                .with_memory_budget_bytes(512)
                .with_temp_dir(ctx.temp_dir.clone()),
        );

        exec_sql(
            &mut db,
            "CREATE TABLE rec_t (id INT PRIMARY KEY, a INT, b INT)",
        )
        .unwrap();
        exec_sql(&mut db, "CREATE INDEX idx_a ON rec_t (a)").unwrap();
        exec_sql(&mut db, "CREATE INDEX idx_b ON rec_t (b)").unwrap();

        for i in 1..=200 {
            exec_sql(
                &mut db,
                &format!("INSERT INTO rec_t VALUES ({i}, {}, {})", i % 2, i % 3),
            )
            .unwrap();
        }
        db.checkpoint().unwrap();
    }

    // Reopen and query
    let mut db2 = ctx.open();
    let mut txn = db2.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM rec_t WHERE a = 0 AND b = 0")
        .unwrap();
    assert_eq!(qr.rows.len(), 33);
}

// ---------------------------------------------------------------------------
// R155 – NULL/3VL differential test
// ---------------------------------------------------------------------------
#[test]
fn test_r155_null_3vl_differential() {
    let ctx = Ctx::new("r155");
    let mut db = ctx.open();
    db.set_multi_index_config(
        MultiIndexConfig::default()
            .with_memory_budget_bytes(512)
            .with_temp_dir(ctx.temp_dir.clone()),
    );

    exec_sql(
        &mut db,
        "CREATE TABLE null_t (id INT PRIMARY KEY, a INT, b INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_a ON null_t (a)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_b ON null_t (b)").unwrap();

    for i in 1..=100 {
        let a_val = if i % 3 == 0 {
            "NULL".to_string()
        } else {
            (i % 5).to_string()
        };
        let b_val = if i % 4 == 0 {
            "NULL".to_string()
        } else {
            (i % 7).to_string()
        };
        exec_sql(
            &mut db,
            &format!("INSERT INTO null_t VALUES ({i}, {a_val}, {b_val})"),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE null_t").unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let qr = txn
        .execute_sql("SELECT id FROM null_t WHERE a = 1 AND b IS NOT NULL")
        .unwrap();

    // Compare with manual scan
    let qr_all = txn.execute_sql("SELECT id, a, b FROM null_t").unwrap();
    let expected_ids: Vec<i64> = qr_all
        .rows
        .iter()
        .filter(|r| r.values[1] == Value::Int64(1) && !r.values[2].is_null())
        .map(|r| val_i64(&r.values[0]))
        .collect();

    let actual_ids: Vec<i64> = qr.rows.iter().map(|r| val_i64(&r.values[0])).collect();
    assert_eq!(actual_ids, expected_ids);
}

// ---------------------------------------------------------------------------
// R156 – Randomized external-vs-in-memory differential test
// ---------------------------------------------------------------------------
#[test]
fn test_r156_randomized_external_vs_in_memory_differential() {
    let ctx = Ctx::new("r156");
    let mut db = ctx.open();

    exec_sql(
        &mut db,
        "CREATE TABLE rand_t (id INT PRIMARY KEY, c1 INT, c2 INT, c3 INT)",
    )
    .unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c1 ON rand_t (c1)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c2 ON rand_t (c2)").unwrap();
    exec_sql(&mut db, "CREATE INDEX idx_c3 ON rand_t (c3)").unwrap();

    // Pseudo-random pseudo-deterministic population
    let mut state: u64 = 0xDEADBEEF;
    for i in 1..=300 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let c1 = ((state >> 32) % 5) as i64;
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let c2 = ((state >> 32) % 4) as i64;
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let c3 = ((state >> 32) % 3) as i64;

        exec_sql(
            &mut db,
            &format!("INSERT INTO rand_t VALUES ({i}, {c1}, {c2}, {c3})"),
        )
        .unwrap();
    }
    exec_sql(&mut db, "ANALYZE rand_t").unwrap();

    let queries = [
        "SELECT id FROM rand_t WHERE c1 = 1 AND c2 = 2",
        "SELECT id FROM rand_t WHERE c1 = 0 OR c2 = 1",
        "SELECT id FROM rand_t WHERE c1 = 2 AND c2 = 1 AND c3 = 0",
        "SELECT id FROM rand_t WHERE c1 = 3 OR c2 = 2 OR c3 = 1",
        "SELECT id FROM rand_t WHERE c1 = 4 AND (c2 = 0 OR c3 = 2)",
    ];

    for query in &queries {
        // 1. External spilled execution (tight budget)
        db.set_multi_index_config(
            MultiIndexConfig::default()
                .with_memory_budget_bytes(256)
                .with_temp_dir(ctx.temp_dir.clone()),
        );
        let mut txn_ext = db.begin_transaction().unwrap();
        let qr_ext = txn_ext.execute_sql(query).unwrap();
        txn_ext.commit().unwrap();

        // 2. In-memory execution (large budget)
        db.set_multi_index_config(
            MultiIndexConfig::default()
                .with_memory_budget_bytes(1024 * 1024)
                .with_temp_dir(ctx.temp_dir.clone()),
        );
        let mut txn_mem = db.begin_transaction().unwrap();
        let qr_mem = txn_mem.execute_sql(query).unwrap();
        txn_mem.commit().unwrap();

        let mut ext_ids: Vec<i64> = qr_ext.rows.iter().map(|r| val_i64(&r.values[0])).collect();
        let mut mem_ids: Vec<i64> = qr_mem.rows.iter().map(|r| val_i64(&r.values[0])).collect();
        ext_ids.sort_unstable();
        mem_ids.sort_unstable();

        assert_eq!(
            ext_ids, mem_ids,
            "External spilled execution must exactly match in-memory execution for: {query}"
        );
    }
}
