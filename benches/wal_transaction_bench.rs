use std::time::Instant;
use treedb::{LogManager, PageManager, RecoveryEngine, TreeDb};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner};

pub fn run_wal_transaction_benchmarks(runner: &mut BenchmarkRunner) {
    println!("\n=== Running WAL & Transaction Benchmarks ===");

    // 1. Read-only transaction
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wal_ro.treedb");
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();
        db.execute_statement("INSERT INTO kv VALUES (1, 100);")
            .unwrap();
        db.sync().unwrap();

        let iters = 1_000;
        let mut samples = Vec::with_capacity(iters);

        for _ in 0..iters {
            let start = Instant::now();
            let mut txn = db.begin().unwrap();
            let res = txn.get(1).unwrap();
            consume(res);
            txn.commit().unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "WAL & Txn",
            "read_only_transaction",
            1,
            iters,
            samples,
            "begin + get + commit (read-only, no WAL write)",
        ));
    }

    // 2. Small write transaction (single statement with commit)
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wal_small.treedb");
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();
        db.sync().unwrap();

        let iters = 200;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let sql = format!("INSERT INTO kv VALUES ({}, {});", i, i * 10);
            let start = Instant::now();
            db.execute_statement(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "WAL & Txn",
            "small_write_transaction",
            iters,
            iters,
            samples,
            "Atomic single-row INSERT transaction with WAL log entry",
        ));
    }

    // 3. Multi-record write transaction (50 records per txn)
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wal_multi.treedb");
        let mut db = TreeDb::create(&db_path, 128).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();
        db.sync().unwrap();

        let batch_count = 20;
        let records_per_batch = 50;
        let mut samples = Vec::with_capacity(batch_count);

        for b in 0..batch_count {
            let start = Instant::now();
            let mut txn = db.begin().unwrap();
            for r in 0..records_per_batch {
                let k = (b * records_per_batch + r) as u64;
                let v = (k * 2).to_le_bytes();
                txn.put(k, &v).unwrap();
            }
            txn.commit().unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "WAL & Txn",
            "multi_record_write_transaction",
            records_per_batch,
            batch_count,
            samples,
            format!("Batch txn of {} records per commit", records_per_batch),
        ));
    }

    // 4. Transaction Abort / Rollback latency
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wal_abort.treedb");
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.put(1, b"initial").unwrap();
        db.sync().unwrap();

        let iters = 200;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let mut txn = db.begin().unwrap();
            txn.put(i as u64 + 10, &[1, 2, 3, 4]).unwrap();

            let start = Instant::now();
            txn.abort().unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "WAL & Txn",
            "transaction_abort_rollback",
            iters,
            iters,
            samples,
            "CLR compensation log & memory write-set abort",
        ));
    }

    // 5. Durability Flush / Fsync cost (`db.sync()`)
    {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wal_sync.treedb");
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        db.execute_statement("CREATE TABLE kv (k INT PRIMARY KEY, v INT);")
            .unwrap();

        let sync_iters = 50;
        let mut samples = Vec::with_capacity(sync_iters);

        for i in 0..sync_iters {
            db.execute_statement(&format!("INSERT INTO kv VALUES ({}, {});", i, i))
                .unwrap();
            let start = Instant::now();
            db.sync().unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "WAL & Txn",
            "durable_sync_fsync",
            sync_iters,
            sync_iters,
            samples,
            "FlushFileBuffers / fsync barrier to storage",
        ));
    }

    // 6. WAL Recovery / Reopen time
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal_rec.treedb");
        let wal_path = std::path::PathBuf::from(format!("{}.wal", path.display()));

        // Populate a WAL with 1,000 log records
        let records_count = 1_000;
        {
            let _pm = PageManager::create(&path).unwrap();
            let mut lm = LogManager::create(&wal_path).unwrap();
            let txn_id = lm.begin_txn().unwrap();

            for i in 0..records_count {
                let page_id = 2 + (i as u32 % 20);
                lm.append_page_delta(txn_id, page_id, 0, vec![0; 16], vec![1; 16])
                    .unwrap();
            }
            let commit_lsn = lm.commit_txn(txn_id).unwrap();
            lm.flush(commit_lsn).unwrap();
        }

        // Measure ARIES recovery execution time
        let start = Instant::now();
        let mut pm = PageManager::open(&path).unwrap();
        let mut lm = LogManager::open(&wal_path).unwrap();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        let elapsed_ns = start.elapsed().as_nanos() as u64;

        runner.record(BenchmarkResult::from_samples(
            "WAL & Txn",
            "aries_crash_recovery",
            records_count,
            1,
            vec![elapsed_ns],
            format!(
                "3-phase ARIES recovery of {} log records (redo: {}, undo: {})",
                records_count, stats.redo_count, stats.undo_count
            ),
        ));
    }

    // 7. Checkpointed vs Uncheckpointed WAL Recovery Benchmark
    // Workloads are logically identical: same keys, values, and operation counts.
    // Database setup and WAL generation are strictly separated from timed recovery across 10 repetitions.
    {
        let repetitions = 10;

        // 7a. Small WAL Workload (500 operations)
        let small_records = 500;
        run_recovery_benchmark_pair(
            runner,
            "small_500",
            small_records,
            repetitions,
            250, // Checkpoint every 250 operations
        );

        // 7b. Large WAL Workload (5,000 operations)
        let large_records = 5_000;
        run_recovery_benchmark_pair(
            runner,
            "large_5k",
            large_records,
            repetitions,
            1_000, // Checkpoint every 1,000 operations
        );
    }
}

fn run_recovery_benchmark_pair(
    runner: &mut BenchmarkRunner,
    label: &str,
    records_count: usize,
    repetitions: usize,
    checkpoint_interval: usize,
) {
    // 1. Prepare Uncheckpointed Baseline
    let dir_un = tempfile::tempdir().unwrap();
    let path_un = dir_un.path().join("source.treedb");
    let wal_path_un = std::path::PathBuf::from(format!("{}.wal", path_un.display()));
    {
        let mut db = TreeDb::create(&path_un, 64).unwrap();
        for i in 0..records_count {
            let key = (i as u64 % 100) + 1;
            let val = (i as u64).to_le_bytes();
            db.put(key, &val).unwrap();
        }
        db.sync().unwrap();
    }
    let unchk_wal_bytes = std::fs::metadata(&wal_path_un).unwrap().len();

    // Timed repetitions for Uncheckpointed
    let mut un_samples = Vec::with_capacity(repetitions);
    let mut un_stats = None;
    for _ in 0..repetitions {
        let run_dir = tempfile::tempdir().unwrap();
        let run_db = run_dir.path().join("run.treedb");
        let run_wal = std::path::PathBuf::from(format!("{}.wal", run_db.display()));
        std::fs::copy(&path_un, &run_db).unwrap();
        std::fs::copy(&wal_path_un, &run_wal).unwrap();

        let mut pm = PageManager::open(&run_db).unwrap();
        let mut lm = LogManager::open(&run_wal).unwrap();
        let start = Instant::now();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        let elapsed = start.elapsed().as_nanos() as u64;
        un_samples.push(elapsed);
        un_stats = Some(stats);
    }
    let un_final_stats = un_stats.unwrap();

    runner.record(BenchmarkResult::from_samples(
        "WAL & Txn",
        Box::leak(format!("aries_recovery_uncheckpointed_{label}").into_boxed_str()),
        records_count,
        repetitions,
        un_samples,
        format!(
            "Uncheckpointed recovery of {} ops (WAL: {} KB, redo: {}, undo: {}, {} reps)",
            records_count,
            unchk_wal_bytes / 1024,
            un_final_stats.redo_count,
            un_final_stats.undo_count,
            repetitions
        ),
    ));

    // 2. Prepare Checkpointed Baseline (logically identical operations)
    let dir_cp = tempfile::tempdir().unwrap();
    let path_cp = dir_cp.path().join("source_cp.treedb");
    let wal_path_cp = std::path::PathBuf::from(format!("{}.wal", path_cp.display()));
    {
        let mut db = TreeDb::create(&path_cp, 64).unwrap();
        for i in 0..records_count {
            let key = (i as u64 % 100) + 1;
            let val = (i as u64).to_le_bytes();
            db.put(key, &val).unwrap();
            if (i + 1) % checkpoint_interval == 0 {
                db.checkpoint_and_truncate_wal().unwrap();
            }
        }
        db.sync().unwrap();
    }
    let cp_wal_bytes = std::fs::metadata(&wal_path_cp).unwrap().len();

    // Timed repetitions for Checkpointed
    let mut cp_samples = Vec::with_capacity(repetitions);
    let mut cp_stats = None;
    for _ in 0..repetitions {
        let run_dir = tempfile::tempdir().unwrap();
        let run_db = run_dir.path().join("run_cp.treedb");
        let run_wal = std::path::PathBuf::from(format!("{}.wal", run_db.display()));
        std::fs::copy(&path_cp, &run_db).unwrap();
        std::fs::copy(&wal_path_cp, &run_wal).unwrap();

        let mut pm = PageManager::open(&run_db).unwrap();
        let mut lm = LogManager::open(&run_wal).unwrap();
        let start = Instant::now();
        let stats = RecoveryEngine::recover(&mut pm, &mut lm).unwrap();
        let elapsed = start.elapsed().as_nanos() as u64;
        cp_samples.push(elapsed);
        cp_stats = Some(stats);
    }
    let cp_final_stats = cp_stats.unwrap();

    runner.record(BenchmarkResult::from_samples(
        "WAL & Txn",
        Box::leak(format!("aries_recovery_checkpointed_{label}").into_boxed_str()),
        records_count,
        repetitions,
        cp_samples,
        format!(
            "Checkpointed recovery of {} ops (cp every {}, WAL: {} KB, redo: {}, undo: {}, {} reps)",
            records_count,
            checkpoint_interval,
            cp_wal_bytes / 1024,
            cp_final_stats.redo_count,
            cp_final_stats.undo_count,
            repetitions
        ),
    ));
}
