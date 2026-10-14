use std::time::Instant;
use treedb::{TreeDb, Value};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner};

pub fn run_sql_query_benchmarks(runner: &mut BenchmarkRunner) {
    println!("\n=== Running SQL & Query Execution Benchmarks ===");

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("sql_bench.treedb");
    let mut db = TreeDb::create(&db_path, 256).unwrap();

    // 1. Schema setup
    db.execute_statement(
        "CREATE TABLE accounts (id INT PRIMARY KEY, owner TEXT, balance INT, category INT);",
    )
    .unwrap();
    db.execute_statement("CREATE TABLE logs (log_id INT PRIMARY KEY, account_id INT, amount INT);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_cat ON accounts (category);")
        .unwrap();

    // Populate data: 2,000 accounts, 4,000 logs
    let account_count = 2_000;
    for i in 1..=account_count {
        let cat = (i % 20) + 1; // 20 categories, 5% each
        let balance = (i * 17) % 10_000;
        let owner = format!("user_{:04}", i);
        db.execute_statement(&format!(
            "INSERT INTO accounts VALUES ({}, '{}', {}, {});",
            i, owner, balance, cat
        ))
        .unwrap();
    }

    for j in 1..=4_000 {
        let acc_id = (j % account_count) + 1;
        let amount = (j * 7) % 500;
        db.execute_statement(&format!(
            "INSERT INTO logs VALUES ({}, {}, {});",
            j, acc_id, amount
        ))
        .unwrap();
    }
    db.sync().unwrap();

    // Collect statistics for cost-based optimizer
    db.execute_statement("ANALYZE accounts;").unwrap();
    db.execute_statement("ANALYZE logs;").unwrap();

    // Workload 1: Primary key point lookup
    {
        let iters = 1_000;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let id = (i % account_count) + 1;
            let sql = format!("SELECT * FROM accounts WHERE id = {};", id);
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "pk_point_lookup",
            account_count,
            iters,
            samples,
            "B+ Tree primary index point seek",
        ));
    }

    // Workload 2: Secondary index lookup (selective: ~5% selectivity)
    {
        let iters = 200;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let cat = (i % 20) + 1;
            let sql = format!("SELECT * FROM accounts WHERE category = {};", cat);
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 100);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "secondary_index_lookup",
            account_count,
            iters,
            samples,
            "Secondary index scan (100 rows / 5% selectivity)",
        ));
    }

    // Workload 3: Range scan over primary key (100 rows)
    {
        let iters = 200;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let start_id = (i % 1_800) + 1;
            let end_id = start_id + 99;
            let sql = format!(
                "SELECT * FROM accounts WHERE id >= {} AND id <= {};",
                start_id, end_id
            );
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 100);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "pk_range_scan",
            account_count,
            iters,
            samples,
            "Primary key range scan (100 rows)",
        ));
    }

    // Workload 4: Full table scan
    {
        let iters = 50;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            let res = db.execute_sql("SELECT * FROM accounts;").unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), account_count);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "full_table_scan",
            account_count,
            iters,
            samples,
            "Full table scan of all 2,000 rows",
        ));
    }

    // Workload 5: Selective filter (scan with ~2% matches)
    {
        let iters = 50;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            let res = db
                .execute_sql("SELECT * FROM accounts WHERE balance > 9800;")
                .unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            consume(res);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "filter_selective",
            account_count,
            iters,
            samples,
            "Predicate evaluation with ~2% row match rate",
        ));
    }

    // Workload 6: Aggregation (COUNT, SUM, AVG)
    {
        let iters = 50;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            let res = db
                .execute_sql("SELECT COUNT(*), SUM(balance), AVG(balance) FROM accounts;")
                .unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "aggregation_global",
            account_count,
            iters,
            samples,
            "Aggregate COUNT(*), SUM, AVG over table",
        ));
    }

    // Workload 7: In-memory Sort
    {
        let iters = 30;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            let res = db
                .execute_sql("SELECT * FROM accounts ORDER BY balance ASC;")
                .unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), account_count);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "order_by_sort",
            account_count,
            iters,
            samples,
            "In-memory SortOperator over 2,000 tuples",
        ));
    }

    // Workload 8: Two-table Hash Join
    {
        let iters = 20;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let start = Instant::now();
            let res = db
                .execute_sql("SELECT accounts.id, accounts.owner, logs.amount FROM accounts JOIN logs ON accounts.id = logs.account_id;")
                .unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 4_000);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "inner_equi_join",
            4_000,
            iters,
            samples,
            "Hash join between accounts (2,000) and logs (4,000)",
        ));
    }

    // Workload 9: Prepared Statement vs Ad-hoc parse/bind/plan
    {
        let iters = 500;
        let stmt = db
            .prepare_sql("SELECT * FROM accounts WHERE id = ?;")
            .unwrap();

        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let id = ((i % account_count) + 1) as i64;
            let params = vec![Value::Int64(id)];
            let start = Instant::now();
            let res = db.execute_prepared(&stmt, &params).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            match res {
                treedb::StatementResult::Query(qres) => assert_eq!(qres.rows().len(), 1),
                _ => panic!("Expected query result"),
            }
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "prepared_statement_execution",
            account_count,
            iters,
            samples,
            "Pre-parsed & bound statement with parameter substitution",
        ));
    }

    // Workload 10: Repeated execution exercising the Plan Cache
    {
        let iters = 500;
        let mut samples = Vec::with_capacity(iters);
        let mut txn = db.begin_transaction().unwrap();
        let prep = txn
            .prepare_sql("SELECT id, owner, balance FROM accounts WHERE id = ?;")
            .unwrap();
        let params = [Value::Int64(42)];

        // Prime the plan cache on the initial execution
        let (_res, m1) = txn.execute_prepared_with_metrics(&prep, &params).unwrap();
        assert!(!m1.plan_cache_hit, "First execution should be a cache miss");

        for _ in 0..iters {
            let start = Instant::now();
            let (res, metrics) = txn.execute_prepared_with_metrics(&prep, &params).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            consume(res);
            assert!(
                metrics.plan_cache_hit,
                "Subsequent execution should hit plan cache"
            );
            samples.push(elapsed);
        }
        txn.commit().unwrap();

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "plan_cache_hit_execution",
            account_count,
            iters,
            samples,
            "Plan cache reuse without re-invoking cost optimizer",
        ));
    }

    // -------------------------------------------------------------------------
    // Phase 87 Focused Workloads: Covering Index & Index-Only Execution
    // -------------------------------------------------------------------------
    db.execute_statement("CREATE INDEX idx_cat_balance ON accounts (category) INCLUDE (balance);")
        .unwrap();
    db.execute_statement("ANALYZE accounts;").unwrap();

    // Workload 11: Covering index point lookup (100 rows, 0 base record fetches)
    {
        let iters = 200;
        let mut samples = Vec::with_capacity(iters);
        for i in 0..iters {
            let cat = (i % 20) + 1;
            let sql = format!(
                "SELECT category, balance FROM accounts WHERE category = {};",
                cat
            );
            let start = Instant::now();
            let res = db.execute_sql(&sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 100);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "covering_index_point_lookup",
            account_count,
            iters,
            samples,
            "Covering index-only lookup (100 rows / zero base-record fetches)",
        ));
    }

    // Workload 12: Covering index range scan (600 rows, 0 base record fetches)
    {
        let iters = 100;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql =
                "SELECT category, balance FROM accounts WHERE category >= 5 AND category <= 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 600);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "covering_index_range_scan",
            account_count,
            iters,
            samples,
            "Covering index-only range scan (600 rows / zero base-record fetches)",
        ));
    }

    // Workload 13: Ineligible secondary index range scan (non-covered column 'owner', 600 base fetches)
    {
        let iters = 100;
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let sql = "SELECT category, balance, owner FROM accounts WHERE category >= 5 AND category <= 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 600);
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "SQL & Query",
            "non_covering_index_range_scan",
            account_count,
            iters,
            samples,
            "Ineligible index range scan (600 rows / base records fetched for unindexed column)",
        ));
    }

    // -------------------------------------------------------------------------
    // Phase 89 & 90 Focused Workloads: Index Aggregation & Early Termination
    // -------------------------------------------------------------------------
    db.execute_statement(
        "CREATE TABLE bench_items (id INT PRIMARY KEY, category VARCHAR, rating INT, price INT);",
    )
    .unwrap();
    db.execute_statement("CREATE INDEX idx_rating ON bench_items (rating);")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_category ON bench_items (category);")
        .unwrap();

    let bench_count = 2_000;
    for i in 1..=bench_count {
        let rating_str = if i % 5 == 0 {
            "NULL".to_string()
        } else {
            ((i * 13) % 100 + 1).to_string()
        };
        let price = (i * 37) % 10_000 + 1;
        let cat = format!("'cat_{}'", (i % 20) + 1);
        db.execute_statement(&format!(
            "INSERT INTO bench_items VALUES ({}, {}, {}, {});",
            i, cat, rating_str, price
        ))
        .unwrap();
    }
    db.sync().unwrap();
    db.execute_statement("ANALYZE bench_items;").unwrap();

    let bench_iters = 200;

    // 1. COUNT(*) - Baseline (Standard aggregation via non-index expression)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT COUNT(price + 0) FROM bench_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "count_star_baseline",
            bench_count,
            bench_iters,
            samples,
            "Baseline: COUNT(*) via full table scan & streaming aggregate (2,000 rows fetched)",
        ));
    }

    // 2. COUNT(*) - Optimized (Index-level aggregation)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT COUNT(*) FROM bench_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "count_star_optimized",
            bench_count,
            bench_iters,
            samples,
            "Optimized: Index-level COUNT(*) (2,000 rows / zero heap record fetches)",
        ));
    }

    // 3. COUNT(col) with NULLs - Baseline
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT COUNT(rating + 0) FROM bench_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "count_col_nulls_baseline",
            bench_count,
            bench_iters,
            samples,
            "Baseline: COUNT(col) with 20% NULLs via full table scan (2,000 rows fetched)",
        ));
    }

    // 4. COUNT(col) with NULLs - Optimized (Secondary index scan skipping NULL tag)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT COUNT(rating) FROM bench_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "count_col_nulls_optimized",
            bench_count,
            bench_iters,
            samples,
            "Optimized: Index-level COUNT(col) with 20% NULLs (1,600 counted / zero heap fetches)",
        ));
    }

    // 5. MIN and MAX - Baseline
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT MIN(rating + 0), MAX(rating + 0) FROM bench_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "min_max_baseline",
            bench_count,
            bench_iters,
            samples,
            "Baseline: MIN/MAX via full table scan & row-by-row comparisons (2,000 rows fetched)",
        ));
    }

    // 6. MIN and MAX - Optimized (Index extrema seek)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT MIN(rating), MAX(rating) FROM bench_items;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "min_max_optimized",
            bench_count,
            bench_iters,
            samples,
            "Optimized: Index MIN/MAX via leftmost and rightmost index leaf seek",
        ));
    }

    // 7. ORDER BY ASC LIMIT 10 - Baseline (Full table scan + full sort)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT id, category, price FROM bench_items ORDER BY id + 0 ASC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "order_by_limit_asc_baseline",
            bench_count,
            bench_iters,
            samples,
            "Baseline: ORDER BY ASC LIMIT 10 via full 2,000-row table scan & sort",
        ));
    }

    // 8. ORDER BY ASC LIMIT 10 - Optimized (Early-termination index scan)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT id, category, price FROM bench_items ORDER BY id ASC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "order_by_limit_asc_optimized",
            bench_count,
            bench_iters,
            samples,
            "Optimized: Early-termination primary index scan ORDER BY ASC LIMIT 10 (10 entries examined)",
        ));
    }

    // 9. ORDER BY DESC LIMIT 10 - Baseline (Full table scan + full sort)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT id, category, price FROM bench_items ORDER BY id + 0 DESC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "order_by_limit_desc_baseline",
            bench_count,
            bench_iters,
            samples,
            "Baseline: ORDER BY DESC LIMIT 10 via full 2,000-row table scan & sort",
        ));
    }

    // 10. ORDER BY DESC LIMIT 10 - Optimized (Reverse early-termination index scan)
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT id, category, price FROM bench_items ORDER BY id DESC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "order_by_limit_desc_optimized",
            bench_count,
            bench_iters,
            samples,
            "Optimized: Early-termination reverse index scan ORDER BY DESC LIMIT 10 (10 entries examined)",
        ));
    }

    // 11. Selective residual filter LIMIT 10 - Baseline
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT id, category, price FROM bench_items WHERE price > 9000 ORDER BY id + 0 ASC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "residual_filter_limit_baseline",
            bench_count,
            bench_iters,
            samples,
            "Baseline: Residual filter (>90% rejected) with full sort before LIMIT 10",
        ));
    }

    // 12. Selective residual filter LIMIT 10 - Optimized
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT id, category, price FROM bench_items WHERE price > 9000 ORDER BY id ASC LIMIT 10;";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 10);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "residual_filter_limit_optimized",
            bench_count,
            bench_iters,
            samples,
            "Optimized: Early-termination index scan stops after 10 visible matching rows found",
        ));
    }

    // 13. Fallback query - Ineligible aggregate correctly falls back to normal path
    {
        let mut samples = Vec::with_capacity(bench_iters);
        for _ in 0..bench_iters {
            let sql = "SELECT MIN(price) FROM bench_items WHERE category = 'cat_1';";
            let start = Instant::now();
            let res = db.execute_sql(sql).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert_eq!(res.rows().len(), 1);
            samples.push(elapsed);
        }
        runner.record(BenchmarkResult::from_samples(
            "Phase 89/90",
            "fallback_unsupported_aggregate",
            bench_count,
            bench_iters,
            samples,
            "Audited Fallback: MIN on unindexed column with filter falls back to normal aggregation",
        ));
    }
}
