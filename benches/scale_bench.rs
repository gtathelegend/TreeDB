use std::time::Instant;
use treedb::{TreeDb, VacuumConfig, Value};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner, XorShift64};

pub fn run_scale_benchmarks(runner: &mut BenchmarkRunner, target_scales: &[usize]) {
    println!("\n================================================================================");
    println!(" Running Scale Benchmarks & Storage Engine Performance Audit");
    println!(" Target Scales: {:?}", target_scales);
    println!("================================================================================");

    for &size in target_scales {
        bench_scale_size(runner, size);
    }
}

fn bench_scale_size(runner: &mut BenchmarkRunner, size: usize) {
    println!("\n>>> Benchmarking Scale: N = {} rows <<<", size);

    std::fs::create_dir_all("target/tmp").unwrap();
    let dir = tempfile::Builder::new()
        .prefix(&format!("scale_bench_{}_", size))
        .tempdir_in("target/tmp")
        .unwrap();
    let db_path = dir.path().join(format!("scale_{}.treedb", size));
    let pool_capacity = if size <= 2_000 {
        512
    } else if size <= 20_000 {
        1024
    } else {
        4096
    };
    let mut db = TreeDb::create(&db_path, pool_capacity).unwrap();

    // -------------------------------------------------------------------------
    // Schema setup
    // -------------------------------------------------------------------------
    db.execute_statement(
        "CREATE TABLE scale_items (
            id INT PRIMARY KEY,
            category INT,
            rating INT,
            price INT,
            status VARCHAR,
            payload TEXT
        );",
    )
    .unwrap();

    db.execute_statement(
        "CREATE TABLE scale_orders (
            order_id INT PRIMARY KEY,
            item_id INT,
            amount INT
        );",
    )
    .unwrap();

    db.execute_statement("CREATE INDEX idx_scale_cat ON scale_items (category);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_scale_rating ON scale_items (rating);")
        .unwrap();
    db.execute_statement(
        "CREATE INDEX idx_scale_cat_cov ON scale_items (category) INCLUDE (price);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_scale_orders_item ON scale_orders (item_id);")
        .unwrap();

    // -------------------------------------------------------------------------
    // Workload 12a: Batch Insert Scale
    // -------------------------------------------------------------------------
    let insert_start = Instant::now();
    let batch_size = 500;
    let commit_batch = 2_000;
    {
        let mut tx = db.begin_transaction().unwrap();
        let mut rows_in_tx = 0;
        for chunk_start in (1..=size).step_by(batch_size) {
            let chunk_end = (chunk_start + batch_size - 1).min(size);
            let mut sql = String::with_capacity(batch_size * 120);
            sql.push_str("INSERT INTO scale_items VALUES ");
            for i in chunk_start..=chunk_end {
                if i > chunk_start {
                    sql.push_str(", ");
                }
                let cat = (i % 20) + 1; // 20 categories (5% each)
                let rating_str = if i % 5 == 0 {
                    "NULL".to_string()
                } else {
                    ((i * 13) % 100 + 1).to_string()
                };
                let price = (i * 37) % 10_000 + 1;
                let status = match i % 3 {
                    0 => "'ACTIVE'",
                    1 => "'PENDING'",
                    _ => "'ARCHIVED'",
                };
                let payload = format!(
                    "'payload_seq_{:06}_abcdefghijklmnopqrstuvwxyz_0123456789'",
                    i
                );
                sql.push_str(&format!(
                    "({}, {}, {}, {}, {}, {})",
                    i, cat, rating_str, price, status, payload
                ));
            }
            sql.push(';');
            tx.execute_statement(&sql).unwrap();
            rows_in_tx += chunk_end - chunk_start + 1;
            if rows_in_tx >= commit_batch && chunk_end < size {
                tx.commit().unwrap();
                if chunk_end % 10_000 == 0 {
                    db.checkpoint_and_truncate_wal().unwrap();
                }
                tx = db.begin_transaction().unwrap();
                rows_in_tx = 0;
            }
        }
        tx.commit().unwrap();
        db.checkpoint_and_truncate_wal().unwrap();
    }
    let insert_duration = insert_start.elapsed();

    runner.record(BenchmarkResult::from_batch(
        "Scale Audit",
        "insert_batch_table",
        size,
        size,
        insert_duration,
        format!(
            "Batch inserted {} rows with 3 indexes in transactions",
            size
        ),
    ));

    // Populate scale_orders (same row count as items)
    {
        let mut tx = db.begin_transaction().unwrap();
        let mut rows_in_tx = 0;
        for chunk_start in (1..=size).step_by(batch_size) {
            let chunk_end = (chunk_start + batch_size - 1).min(size);
            let mut sql = String::with_capacity(batch_size * 50);
            sql.push_str("INSERT INTO scale_orders VALUES ");
            for j in chunk_start..=chunk_end {
                if j > chunk_start {
                    sql.push_str(", ");
                }
                let item_id = ((j * 17) % size) + 1;
                let amount = (j * 7) % 500 + 1;
                sql.push_str(&format!("({}, {}, {})", j, item_id, amount));
            }
            sql.push(';');
            tx.execute_statement(&sql).unwrap();
            rows_in_tx += chunk_end - chunk_start + 1;
            if rows_in_tx >= commit_batch && chunk_end < size {
                tx.commit().unwrap();
                if chunk_end % 10_000 == 0 {
                    db.checkpoint_and_truncate_wal().unwrap();
                }
                tx = db.begin_transaction().unwrap();
                rows_in_tx = 0;
            }
        }
        tx.commit().unwrap();
        db.checkpoint_and_truncate_wal().unwrap();
    }

    db.sync().unwrap();
    db.execute_statement("ANALYZE scale_items;").unwrap();
    db.execute_statement("ANALYZE scale_orders;").unwrap();

    let mut rng = XorShift64::new(0x2026_0920_1337_0001 ^ (size as u64));

    // -------------------------------------------------------------------------
    // Workload 1: Primary-Key Point Lookup
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            500
        } else if size <= 20_000 {
            300
        } else {
            200
        };
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let id = (rng.next_range(size as u64) + 1) as usize;
            let sql = format!("SELECT * FROM scale_items WHERE id = {};", id);
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "pk_point_lookup",
            size,
            iters,
            samples,
            format!("Primary key seek across {} rows", size),
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 2: Primary-Key Range Scan (Fixed 100 rows slice)
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            100
        } else if size <= 20_000 {
            50
        } else {
            30
        };
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start_id = (rng.next_range((size - 100).max(1) as u64) + 1) as usize;
            let end_id = start_id + 99;
            let sql = format!(
                "SELECT * FROM scale_items WHERE id >= {} AND id <= {};",
                start_id, end_id
            );
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 100);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "pk_range_scan_100_rows",
            size,
            iters,
            samples,
            "Primary key range scan fetching fixed 100 rows",
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 3: Secondary-Index Point Lookup with Duplicate Keys (5% of table)
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            100
        } else if size <= 20_000 {
            30
        } else {
            10
        };
        let expected_rows = size / 20;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let cat = (i % 20) + 1;
            let sql = format!(
                "SELECT id, category, price FROM scale_items WHERE category = {};",
                cat
            );
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), expected_rows);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "secondary_index_lookup_duplicates",
            size,
            iters,
            samples,
            format!(
                "Secondary index point seek returning {} duplicate rows (5% selectivity)",
                expected_rows
            ),
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 4: Secondary-Index Range Scan (15% of table: category 5..=7)
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            50
        } else if size <= 20_000 {
            20
        } else {
            5
        };
        let expected_rows = (size * 3) / 20;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT id, category, price FROM scale_items WHERE category >= 5 AND category <= 7;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), expected_rows);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "secondary_index_range_scan",
            size,
            iters,
            samples,
            format!(
                "Secondary index range scan returning {} rows (15% selectivity)",
                expected_rows
            ),
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 5: Full Table Scan (Narrow Projection vs Wide Projection)
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            30
        } else if size <= 20_000 {
            10
        } else {
            3
        };
        // Narrow projection: id, price
        let mut samples_narrow = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT id, price FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), size);
            samples_narrow.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "full_table_scan_narrow",
            size,
            iters,
            samples_narrow,
            format!(
                "Full table scan projecting 2 integer columns across {} rows",
                size
            ),
        ));

        // Wide projection: SELECT *
        let mut samples_wide = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), size);
            samples_wide.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "full_table_scan_wide",
            size,
            iters,
            samples_wide,
            format!(
                "Full table scan projecting all columns including TEXT payload across {} rows",
                size
            ),
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 6: COUNT(*) - Baseline vs Optimized
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            50
        } else if size <= 20_000 {
            20
        } else {
            10
        };
        // Baseline (full table scan)
        let mut samples_base = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT COUNT(price + 0) FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows()[0].values[0], Value::UInt64(size as u64));
            samples_base.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "count_star_baseline",
            size,
            iters,
            samples_base,
            "Baseline COUNT(*) via full table scan",
        ));

        // Optimized (index-level count)
        let mut samples_opt = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT COUNT(*) FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows()[0].values[0], Value::UInt64(size as u64));
            samples_opt.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "count_star_optimized",
            size,
            iters,
            samples_opt,
            "Optimized index-level COUNT(*) with zero base record fetches",
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 7: COUNT(column) with 20% NULLs - Baseline vs Optimized
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            50
        } else if size <= 20_000 {
            20
        } else {
            10
        };
        let expected_count = (size * 4) / 5; // 80% non-NULL
                                             // Baseline
        let mut samples_base = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT COUNT(rating + 0) FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(
                res.rows()[0].values[0],
                Value::UInt64(expected_count as u64)
            );
            samples_base.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "count_col_nulls_baseline",
            size,
            iters,
            samples_base,
            "Baseline COUNT(col) with 20% NULLs via full scan",
        ));

        // Optimized (Phase 91 NULL-skipping index count)
        let mut samples_opt = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT COUNT(rating) FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(
                res.rows()[0].values[0],
                Value::UInt64(expected_count as u64)
            );
            samples_opt.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "count_col_nulls_optimized",
            size,
            iters,
            samples_opt,
            "Optimized secondary index COUNT(rating) with B+ tree root NULL-skipping",
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 8: MIN and MAX - Baseline vs Optimized
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            50
        } else if size <= 20_000 {
            30
        } else {
            20
        };
        // Baseline
        let mut samples_base = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT MIN(id + 0), MAX(id + 0) FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows()[0].values[0], Value::Int64(1));
            assert_eq!(res.rows()[0].values[1], Value::Int64(size as i64));
            samples_base.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "min_max_baseline",
            size,
            iters,
            samples_base,
            "Baseline MIN/MAX via full table scan",
        ));

        // Optimized (Phase 89 extrema seek)
        let mut samples_opt = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT MIN(id), MAX(id) FROM scale_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows()[0].values[0], Value::Int64(1));
            assert_eq!(res.rows()[0].values[1], Value::Int64(size as i64));
            samples_opt.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "min_max_optimized",
            size,
            iters,
            samples_opt,
            "Optimized O(log N) MIN/MAX via index leftmost/rightmost leaf seeks",
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 9: ORDER BY ... LIMIT 10 (ASC and DESC)
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            50
        } else if size <= 20_000 {
            30
        } else {
            20
        };
        // ASC Baseline
        let mut samples_asc_base = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items ORDER BY (id + 0) ASC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples_asc_base.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "order_by_limit_asc_baseline",
            size,
            iters,
            samples_asc_base,
            "Baseline ORDER BY id ASC LIMIT 10 with full scan and sort",
        ));

        // ASC Optimized (early-stop forward scan)
        let mut samples_asc_opt = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items ORDER BY id ASC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples_asc_opt.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "order_by_limit_asc_optimized",
            size,
            iters,
            samples_asc_opt,
            "Optimized early-termination forward index scan ORDER BY ASC LIMIT 10",
        ));

        // DESC Baseline
        let mut samples_desc_base = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items ORDER BY (id + 0) DESC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples_desc_base.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "order_by_limit_desc_baseline",
            size,
            iters,
            samples_desc_base,
            "Baseline ORDER BY id DESC LIMIT 10 with full scan and sort",
        ));

        // DESC Optimized (early-stop reverse scan)
        let mut samples_desc_opt = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items ORDER BY id DESC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples_desc_opt.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "order_by_limit_desc_optimized",
            size,
            iters,
            samples_desc_opt,
            "Optimized early-termination reverse index scan ORDER BY DESC LIMIT 10",
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 10: Selective Residual Filter Queries (~1% selectivity)
    // -------------------------------------------------------------------------
    {
        let iters = if size <= 2_000 {
            30
        } else if size <= 20_000 {
            15
        } else {
            10
        };
        // Baseline: full filter
        let mut samples_base = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items WHERE price > 9900;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            consume(res);
            samples_base.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "filter_selective_residual_full",
            size,
            iters,
            samples_base,
            "Residual filter evaluating price > 9900 (~1% match) across full table",
        ));

        // Optimized: early stop on residual filter
        let mut samples_opt = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT * FROM scale_items WHERE price > 9900 LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples_opt.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "filter_selective_residual_limit",
            size,
            iters,
            samples_opt,
            "Early-stop pushdown: stops scan after finding 10 matches",
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 11: Sort and Hash Join
    // -------------------------------------------------------------------------
    {
        // 11a: In-memory Sort
        let iters_sort = if size <= 2_000 {
            20
        } else if size <= 20_000 {
            8
        } else {
            3
        };
        let mut samples_sort = Vec::with_capacity(iters_sort);
        for _ in 0..iters_sort {
            let sql = "SELECT id, price FROM scale_items ORDER BY price ASC;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), size);
            samples_sort.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "order_by_sort_operator",
            size,
            iters_sort,
            samples_sort,
            format!("Sort operator sorting {} tuples by unindexed column", size),
        ));

        // 11b: Two-table Hash Join (scale_items ⋈ scale_orders)
        let iters_join = if size <= 2_000 {
            15
        } else if size <= 20_000 {
            5
        } else {
            2
        };
        let mut samples_join = Vec::with_capacity(iters_join);
        for _ in 0..iters_join {
            let sql = "SELECT scale_items.id, scale_items.price, scale_orders.amount \
                       FROM scale_items JOIN scale_orders \
                       ON scale_items.id = scale_orders.item_id;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), size);
            samples_join.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "inner_equi_hash_join",
            size,
            iters_join,
            samples_join,
            format!(
                "Equi-join between scale_items ({}) and scale_orders ({})",
                size, size
            ),
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 12: Insert, Update, Delete, and Vacuum Operations
    // -------------------------------------------------------------------------
    {
        let ops_count = if size <= 20_000 { 100 } else { 50 };

        // 12b: Point updates (creating MVCC version chains)
        let mut samples_update = Vec::with_capacity(ops_count);
        for i in 1..=ops_count {
            let id = i * (size / ops_count);
            let sql = format!(
                "UPDATE scale_items SET price = price + 10 WHERE id = {};",
                id
            );
            let start = Instant::now();
            db.execute_statement(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples_update.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "point_update_mvcc",
            size,
            ops_count,
            samples_update,
            format!(
                "Point updates generating MVCC historical versions on {} rows",
                size
            ),
        ));

        // 12c: Point deletes (tombstoning/reclaiming slots)
        let mut samples_delete = Vec::with_capacity(ops_count);
        for i in 1..=ops_count {
            let id = i * (size / ops_count);
            let sql = format!("DELETE FROM scale_items WHERE id = {};", id);
            let start = Instant::now();
            db.execute_statement(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples_delete.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "point_delete_tombstone",
            size,
            ops_count,
            samples_delete,
            format!("Point deletes tombstoning tuples on table of {} rows", size),
        ));

        // 12d: Vacuum execution
        let vac_start = Instant::now();
        let vac_stats = db.vacuum(&VacuumConfig::default()).unwrap();
        let vac_duration = vac_start.elapsed();
        consume(vac_stats);

        runner.record(BenchmarkResult::from_batch(
            "Scale Audit",
            "vacuum_reclaim_lifecycle",
            size,
            1,
            vac_duration,
            format!(
                "Online vacuum cycle reclaiming historical versions on {} rows",
                size
            ),
        ));
    }

    // -------------------------------------------------------------------------
    // Workload 13: Reopen/Recovery and Cold-Start Behavior
    // -------------------------------------------------------------------------
    {
        let cp_start = Instant::now();
        db.checkpoint_and_truncate_wal().unwrap();
        let cp_duration = cp_start.elapsed();

        runner.record(BenchmarkResult::from_batch(
            "Scale Audit",
            "checkpoint_flush_duration",
            size,
            1,
            cp_duration,
            format!(
                "Checkpoint all dirty pages and persist DbHeader for {} rows",
                size
            ),
        ));

        drop(db);

        // Measure reopen latency after clean checkpoint
        let reopen_start = Instant::now();
        let mut reopened_db = TreeDb::open(&db_path, pool_capacity).unwrap();
        let reopen_duration = reopen_start.elapsed();

        runner.record(BenchmarkResult::from_batch(
            "Scale Audit",
            "database_reopen_and_recovery",
            size,
            1,
            reopen_duration,
            format!(
                "Open existing database, initialize page manager & buffer pool for {} rows",
                size
            ),
        ));

        // Cold point read vs subsequent warm reads
        let cold_id = (size / 2) + 1;
        let cold_sql = format!("SELECT * FROM scale_items WHERE id = {};", cold_id);
        let cold_start = Instant::now();
        let res_cold = reopened_db.execute_sql(&cold_sql).unwrap();
        let cold_elapsed = cold_start.elapsed().as_nanos() as u64;
        assert_eq!(res_cold.rows().len(), 1);

        runner.record(BenchmarkResult::from_samples(
            "Scale Audit",
            "cold_cache_point_read",
            size,
            1,
            vec![cold_elapsed],
            "First point lookup on cold reopened database (disk read + page cache fill)",
        ));
    }

    println!(">>> Completed Scale: N = {} rows <<<", size);
}
