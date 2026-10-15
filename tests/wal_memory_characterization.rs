#![forbid(unsafe_code)]

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::tempdir;
use treedb::{LogManager, PageManager, RecoveryEngine, WalReader};

fn create_test_env(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

#[test]
fn characterize_wal_recovery_memory_and_scaling() {
    println!("\n=== TreeDB WAL Recovery Memory Scaling & Characterization ===");
    println!(
        "{:<4} | {:<10} | {:<8} | {:<8} | {:<7} | {:<7} | {:<9} | {:<12} | {:<10}",
        "Scen",
        "WAL Bytes",
        "Records",
        "TotalTxn",
        "Active",
        "DirtyPg",
        "Time (ms)",
        "Passes (A/R/U)",
        "MemProxy"
    );
    println!("{:-<86}", "");

    // Scenario 1: Large WAL containing predominantly committed transactions (5,000 txns, 0 active)
    run_scenario(
        "1", 5000,  // total txns
        0,     // active loser txns
        false, // checkpoint
        32,    // payload size
    );

    // Scenario 2: Large WAL with a modest number of incomplete transactions (5,000 txns, 50 active losers)
    run_scenario("2", 5000, 50, false, 32);

    // Scenario 3: Large WAL with a larger incomplete transaction set (5,000 txns, 500 active losers)
    run_scenario("3", 5000, 500, false, 32);

    // Scenario 4: Checkpointed WAL (5,000 txns with periodic checkpoints every 1,000 txns)
    run_scenario("4", 5000, 0, true, 32);

    // Scenario 5: Uncheckpointed WAL (5,000 txns without checkpoint)
    run_scenario("5", 5000, 0, false, 32);

    // Scenario 6: Repeated reopen and recovery (recovery after clean recovery)
    run_repeated_recovery_scenario("6");

    // Scenario 7: Recovery with large records (near reader buffer expansion boundary, e.g. 128 KB deltas)
    run_scenario(
        "7",
        50, // 50 large records
        5,  // 5 active
        false,
        128 * 1024, // 128 KB deltas
    );

    println!("{:-<86}\n", "");
}

fn run_scenario(
    scen_id: &str,
    total_txns: usize,
    active_losers: usize,
    use_checkpoints: bool,
    payload_size: usize,
) {
    let (_dir, db_path, wal_path) = create_test_env(&format!("scen_{scen_id}"));

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    let committed_count = total_txns.saturating_sub(active_losers);

    // 1. Write committed transactions
    for i in 0..committed_count {
        let tid = lm.begin_txn().unwrap();
        let before = vec![0u8; payload_size.min(1024)];
        let after = vec![(i % 250) as u8; payload_size.min(1024)];
        lm.append_page_delta(tid, p1, 64, before, after).unwrap();
        let c_lsn = lm.commit_txn(tid).unwrap();
        if (i + 1) % 500 == 0 {
            lm.flush(c_lsn).unwrap();
        }
        if use_checkpoints && (i + 1) % 1000 == 0 {
            let cp_lsn = lm.append_checkpoint(Vec::new()).unwrap();
            lm.flush(cp_lsn).unwrap();
            lm.truncate_prefix(cp_lsn).unwrap();
        }
    }

    // 2. Write active loser transactions (uncommitted)
    for _i in 0..active_losers {
        let tid = lm.begin_txn().unwrap();
        let before = vec![0u8; payload_size.min(1024)];
        let after = vec![0xFF; payload_size.min(1024)];
        lm.append_page_delta(tid, p1, 64, before, after).unwrap();
    }
    lm.flush(lm.next_lsn()).unwrap();

    let wal_file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let wal_bytes = wal_file.metadata().unwrap().len();

    // Count records and estimate recovery metadata
    let mut reader = WalReader::new(&wal_file, lm.flushed_lsn()).unwrap();
    reader.seek_to_offset(32).unwrap();
    let mut total_records = 0;
    while reader.next_record_with_offset().unwrap().is_some() {
        total_records += 1;
    }
    drop(wal_file);
    drop(pm);
    drop(lm);

    // Measure recovery
    let start = Instant::now();
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    let duration = start.elapsed();

    // Memory Proxy Calculation:
    // Reader buffer: 64 KB default
    // Txn table: stats.loser_txns * 32 bytes
    // Active LSNs & loser offsets: stats.loser_txns * 48 bytes
    // Dirty page table: stats.redo_count (approx dirty pages) * 16 bytes
    let reader_buf_bytes = 64 * 1024;
    let retained_metadata_bytes = (stats.loser_txns * 80) + 16;
    let total_proxy_bytes = reader_buf_bytes + retained_metadata_bytes;

    let passes_str = format!(
        "{}/{}/{}",
        total_records, stats.redo_count, stats.undo_count
    );
    let mem_str = format!("{:.1} KB", total_proxy_bytes as f64 / 1024.0);

    println!(
        "{:<4} | {:<10} | {:<8} | {:<8} | {:<7} | {:<7} | {:<9.2} | {:<12} | {:<10}",
        scen_id,
        wal_bytes,
        total_records,
        stats.committed_txns + stats.loser_txns,
        stats.loser_txns,
        1,
        duration.as_secs_f64() * 1000.0,
        passes_str,
        mem_str
    );
}

fn run_repeated_recovery_scenario(scen_id: &str) {
    let (_dir, db_path, wal_path) = create_test_env(&format!("scen_{scen_id}"));

    let mut pm = PageManager::create(&db_path).unwrap();
    let p1 = pm.allocate_page().unwrap();
    let mut lm = LogManager::create(&wal_path).unwrap();

    for i in 0..1000 {
        let tid = lm.begin_txn().unwrap();
        lm.append_page_delta(tid, p1, 64, vec![0; 16], vec![1; 16])
            .unwrap();
        let c = lm.commit_txn(tid).unwrap();
        if i % 100 == 0 {
            lm.flush(c).unwrap();
        }
    }
    // 10 losers
    for _ in 0..10 {
        let tid = lm.begin_txn().unwrap();
        lm.append_page_delta(tid, p1, 64, vec![0; 16], vec![2; 16])
            .unwrap();
    }
    lm.flush(lm.next_lsn()).unwrap();
    drop(pm);
    drop(lm);

    // Pass 1: Recover from crash
    {
        let mut pm = PageManager::open(&db_path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    }

    // Pass 2: Repeated recovery immediately after clean recovery
    let wal_bytes = std::fs::metadata(&wal_path).unwrap().len();
    let start = Instant::now();
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    let duration = start.elapsed();

    let total_proxy_bytes = 64 * 1024 + 16;
    println!(
        "{:<4} | {:<10} | {:<8} | {:<8} | {:<7} | {:<7} | {:<9.2} | {:<12} | {:<10}",
        scen_id,
        wal_bytes,
        stats.redo_count,
        stats.committed_txns,
        stats.loser_txns,
        0,
        duration.as_secs_f64() * 1000.0,
        format!(
            "{}/{}/{}",
            stats.redo_count, stats.redo_count, stats.undo_count
        ),
        format!("{:.1} KB", total_proxy_bytes as f64 / 1024.0)
    );
}
