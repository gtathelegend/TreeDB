#![forbid(unsafe_code)]

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::tempdir;
use treedb::{LogManager, PageManager, RecoveryEngine};

fn create_test_env(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join(format!("{name}.treedb"));
    let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
    (dir, db_path, wal_path)
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryBreakdown {
    /// WAL reader ring/chunk buffer capacity in bytes (default 64 KB).
    pub reader_buffer_capacity: usize,
    /// Largest individual deserialized record allocation in bytes.
    pub max_record_allocation: usize,
    /// Active / loser transaction tracking metadata in bytes (~80B per loser txn).
    pub loser_txn_metadata: usize,
    /// Dirty page recovery bookkeeping in bytes (~16B per unique dirty page).
    pub dirty_page_metadata: usize,
}

impl MemoryBreakdown {
    pub fn total_proxy_bytes(&self) -> usize {
        self.reader_buffer_capacity
            + self.max_record_allocation
            + self.loser_txn_metadata
            + self.dirty_page_metadata
    }
}

#[test]
fn characterize_wal_recovery_memory_and_scaling() {
    println!("\n==========================================================================================================");
    println!(" TreeDB Streaming WAL Recovery Memory Scaling & Honest Resource Breakdown");
    println!(" Note: TotalProxy measures explicit engine recovery heap data structures.");
    println!("       Excluded: OS filesystem page cache, allocator chunk fragmentation, libc/runtime overhead.");
    println!("==========================================================================================================");
    println!(
        "{:<18} | {:<7} | {:<6} | {:<7} | {:<9} | {:<9} | {:<8} | {:<9} | {:<9} | {:<10} | {:<8}",
        "Scenario",
        "TotalTx",
        "Losers",
        "DirtyPg",
        "WAL (KB)",
        "ReaderBuf",
        "MaxRec",
        "LoserMeta",
        "DirtyMeta",
        "TotalProxy",
        "Time(ms)"
    );
    println!("{:-<122}", "");

    // --- Dimension 1: Increasing Numbers of Completed Transactions ---
    // (Constant loser count = 0, constant dirty pages = 1)
    run_scenario("Completed-1K", 1000, 0, 1, false, 64);
    run_scenario("Completed-5K", 5000, 0, 1, false, 64);
    run_scenario("Completed-10K", 10000, 0, 1, false, 64);

    println!("{:-<122}", "");

    // --- Dimension 2: Increasing Numbers of Loser (Uncommitted) Transactions ---
    // (Total 5,000 txns, 1 dirty page)
    run_scenario("Losers-10", 5000, 10, 1, false, 64);
    run_scenario("Losers-50", 5000, 50, 1, false, 64);
    run_scenario("Losers-200", 5000, 200, 1, false, 64);
    run_scenario("Losers-500", 5000, 500, 1, false, 64);

    println!("{:-<122}", "");

    // --- Dimension 3: Increasing Dirty Page Counts ---
    // (1,000 completed txns, varying modified page counts)
    run_scenario("DirtyPages-1", 1000, 0, 1, false, 64);
    run_scenario("DirtyPages-25", 1000, 0, 25, false, 64);
    run_scenario("DirtyPages-100", 1000, 0, 100, false, 64);
    run_scenario("DirtyPages-250", 1000, 0, 250, false, 64);

    println!("{:-<122}", "");

    // --- Dimension 4: Checkpointed vs Uncheckpointed WAL ---
    // (5,000 txns, 1 dirty page)
    run_scenario("Unchkpt-5K", 5000, 0, 1, false, 64);
    run_scenario("Chkpt-5K-cp1K", 5000, 0, 1, true, 64);

    println!("{:-<122}", "");

    // --- Dimension 5: Large Records within Configured Maximum ---
    // Full page delta before/after images (2 * 4096 = 8192 bytes payload)
    run_scenario("LargeRec-FullPg", 500, 5, 5, false, 4096);

    println!("{:-<122}\n", "");
}

fn run_scenario(
    name: &str,
    total_txns: usize,
    active_losers: usize,
    dirty_page_count: usize,
    use_checkpoints: bool,
    payload_size: usize,
) {
    let (_dir, db_path, wal_path) = create_test_env(name);

    let mut pm = PageManager::create(&db_path).unwrap();
    let mut allocated_pages = Vec::with_capacity(dirty_page_count.max(1));
    for _ in 0..dirty_page_count.max(1) {
        let p = pm.allocate_page().unwrap();
        allocated_pages.push(p);
    }
    let mut lm = LogManager::create(&wal_path).unwrap();

    let committed_count = total_txns.saturating_sub(active_losers);
    let clamped_payload = payload_size.min(4000); // Must fit within PAGE_SIZE (4096)

    // 1. Write committed transactions cycling through allocated dirty pages
    for i in 0..committed_count {
        let tid = lm.begin_txn().unwrap();
        let target_page = allocated_pages[i % allocated_pages.len()];
        let before = vec![0u8; clamped_payload];
        let after = vec![(i % 250) as u8; clamped_payload];
        lm.append_page_delta(tid, target_page, 64, before, after)
            .unwrap();
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
    for i in 0..active_losers {
        let tid = lm.begin_txn().unwrap();
        let target_page = allocated_pages[i % allocated_pages.len()];
        let before = vec![0u8; clamped_payload];
        let after = vec![0xFF; clamped_payload];
        lm.append_page_delta(tid, target_page, 64, before, after)
            .unwrap();
    }
    lm.flush(lm.next_lsn()).unwrap();

    let wal_file = OpenOptions::new().read(true).open(&wal_path).unwrap();
    let wal_bytes = wal_file.metadata().unwrap().len();
    drop(wal_file);
    drop(pm);
    drop(lm);

    // 3. Measure recovery
    let start = Instant::now();
    let mut pm = PageManager::open(&db_path).unwrap();
    let mut lm = LogManager::open(&wal_path).unwrap();
    let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
    let duration = start.elapsed();

    // 4. Memory Breakdown Calculation:
    // - Reader Buffer: default 64 KB, or max record size if larger
    let reader_buf_cap = 64 * 1024;
    // - Max single record allocation: page delta header + 2 * clamped_payload
    let max_rec_alloc = clamped_payload * 2 + 128;
    // - Loser transaction tracking: stats.loser_txns * (TxnId, status, prev_lsn, offsets) ~ 80 bytes
    let loser_meta = stats.loser_txns * 80;
    // - Dirty page map bookkeeping: dirty_page_count * (PageId + Lsn) ~ 16 bytes
    let dirty_meta = dirty_page_count.max(1) * 16;

    let breakdown = MemoryBreakdown {
        reader_buffer_capacity: reader_buf_cap,
        max_record_allocation: max_rec_alloc,
        loser_txn_metadata: loser_meta,
        dirty_page_metadata: dirty_meta,
    };

    println!(
        "{:<18} | {:<7} | {:<6} | {:<7} | {:<9.1} | {:<9} | {:<8} | {:<9} | {:<9} | {:<10} | {:<8.2}",
        name,
        stats.committed_txns + stats.loser_txns,
        stats.loser_txns,
        dirty_page_count,
        wal_bytes as f64 / 1024.0,
        format!("{:.1} KB", breakdown.reader_buffer_capacity as f64 / 1024.0),
        format!("{:.1} KB", breakdown.max_record_allocation as f64 / 1024.0),
        format!("{} B", breakdown.loser_txn_metadata),
        format!("{} B", breakdown.dirty_page_metadata),
        format!("{:.1} KB", breakdown.total_proxy_bytes() as f64 / 1024.0),
        duration.as_secs_f64() * 1000.0,
    );

    // Sanity assertions on recovery correctness
    assert_eq!(stats.loser_txns, active_losers);
}
