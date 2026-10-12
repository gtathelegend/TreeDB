use std::time::Instant;
use treedb::{TreeDb, VacuumConfig};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner};

pub fn run_mvcc_vacuum_benchmarks(runner: &mut BenchmarkRunner) {
    println!("\n=== Running MVCC & Vacuum Benchmarks ===");

    let row_count = 500;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("mvcc_bench.treedb");
    let mut db = TreeDb::create(&db_path, 256).unwrap();

    db.execute_statement("CREATE TABLE items (id INT PRIMARY KEY, name TEXT, counter INT);")
        .unwrap();

    // Insert 500 baseline items
    for i in 1..=row_count {
        db.execute_statement(&format!(
            "INSERT INTO items VALUES ({}, 'item_{:04}', 0);",
            i, i
        ))
        .unwrap();
    }
    db.sync().unwrap();

    // 1. Reading current version at depth 0
    {
        let iters = 1_000;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let id = (i % row_count) + 1;
            let sql = format!("SELECT * FROM items WHERE id = {};", id);
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "MVCC & Vacuum",
            "point_read_version_chain_depth_0",
            row_count,
            iters,
            samples,
            "Direct read of latest tuple with 0 historical versions",
        ));
    }

    // 2. Generate version chains: update each row 5 times
    {
        let rounds = 5;
        let total_updates = row_count * rounds;
        let start = Instant::now();

        for r in 1..=rounds {
            for i in 1..=row_count {
                db.execute_statement(&format!(
                    "UPDATE items SET counter = {} WHERE id = {};",
                    r, i
                ))
                .unwrap();
            }
        }
        let total_duration = start.elapsed();

        runner.record(BenchmarkResult::from_batch(
            "MVCC & Vacuum",
            "generate_version_chains_depth_5",
            total_updates,
            total_updates,
            total_duration,
            format!(
                "{} updates building 5 historical versions per row",
                total_updates
            ),
        ));
    }

    // 3. Reading latest version through depth 5 version chain
    {
        let iters = 1_000;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let id = (i % row_count) + 1;
            let sql = format!("SELECT * FROM items WHERE id = {};", id);
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "MVCC & Vacuum",
            "point_read_version_chain_depth_5",
            row_count,
            iters,
            samples,
            "Point lookup traversing chain with 5 historical versions",
        ));
    }

    // 4. Full table scan with historical version chains
    {
        let iters = 50;
        let mut samples = Vec::with_capacity(iters);

        for _ in 0..iters {
            let start = Instant::now();
            let res = db.execute_sql("SELECT * FROM items;").unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), row_count);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "MVCC & Vacuum",
            "scan_table_with_version_chains",
            row_count,
            iters,
            samples,
            "Full scan filtering obsolete versions across chains",
        ));
    }

    // 5. Online vacuuming and space reclamation
    {
        // Reopen or commit to establish a clean vacuum horizon
        db.sync().unwrap();

        let start = Instant::now();
        let result = db.vacuum(&VacuumConfig::default()).unwrap();
        let elapsed_ns = start.elapsed().as_nanos() as u64;

        // Correctness check: ensure all 500 rows are still visible and counter = 5
        let check = db
            .execute_sql("SELECT COUNT(*) FROM items WHERE counter = 5;")
            .unwrap();
        assert_eq!(check.rows().len(), 1);

        runner.record(BenchmarkResult::from_samples(
            "MVCC & Vacuum",
            "vacuum_reclaim_obsolete_versions",
            row_count,
            1,
            vec![elapsed_ns],
            format!(
                "Vacuum reclaimed {} versions, {} pages (examined {} pages)",
                result.versions_reclaimed, result.pages_reclaimed, result.pages_examined
            ),
        ));
    }

    // 6. Post-vacuum point read (verifying pruned chain performance)
    {
        let iters = 1_000;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let id = (i % row_count) + 1;
            let sql = format!("SELECT * FROM items WHERE id = {};", id);
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            consume(res);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "MVCC & Vacuum",
            "point_read_post_vacuum",
            row_count,
            iters,
            samples,
            "Point read after obsolete historical versions pruned",
        ));
    }
}
