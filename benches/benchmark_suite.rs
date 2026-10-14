mod bplus_tree_bench;
mod buffer_pool_bench;
mod framework;
mod mvcc_vacuum_bench;
mod record_store_bench;
mod scale_bench;
mod sql_query_bench;
mod wal_transaction_bench;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use bplus_tree_bench::run_bplus_tree_benchmarks;
use buffer_pool_bench::run_buffer_pool_benchmarks;
use framework::BenchmarkRunner;
use mvcc_vacuum_bench::run_mvcc_vacuum_benchmarks;
use record_store_bench::run_record_store_benchmarks;
use scale_bench::run_scale_benchmarks;
use sql_query_bench::run_sql_query_benchmarks;
use wal_transaction_bench::run_wal_transaction_benchmarks;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut selected_category: Option<String> = None;
    let mut selected_scale: Option<String> = None;
    let mut csv_path = PathBuf::from("target/benchmark_results.csv");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--category" => {
                if i + 1 < args.len() {
                    selected_category = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--scale" => {
                if i + 1 < args.len() {
                    selected_scale = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--csv" => {
                if i + 1 < args.len() {
                    csv_path = PathBuf::from(&args[i + 1]);
                    i += 1;
                }
            }
            "--help" | "-h" => {
                println!("TreeDB Benchmark Suite");
                println!();
                println!("Usage: cargo bench --bench benchmark_suite [-- [OPTIONS]]");
                println!();
                println!("Options:");
                println!("  --category <NAME>   Run only benchmarks for category (e.g. 'scale', 'bplus_tree', 'sql', 'wal', 'record', 'buffer', 'mvcc')");
                println!("  --scale <N>         Target dataset scale for scale benchmarks ('2000', '20000', '100000', or 'all')");
                println!("  --csv <PATH>        Export results to CSV (default: target/benchmark_results.csv)");
                println!("  --help, -h          Show this help message");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    println!("================================================================================");
    println!(" TreeDB Performance Baseline Benchmark Suite");
    println!("================================================================================");
    println!("Toolchain: Rust {}", env!("CARGO_PKG_VERSION"));
    println!("Profile:   Release / Bench");
    println!("CSV Output: {}", csv_path.display());
    println!("================================================================================");

    let suite_start = Instant::now();
    let mut runner = BenchmarkRunner::new();

    let cat = selected_category.as_deref().unwrap_or("all").to_lowercase();

    if cat == "all" || cat.contains("btree") || cat.contains("bplus") {
        run_bplus_tree_benchmarks(&mut runner);
    }

    if cat == "all" || cat.contains("record") {
        run_record_store_benchmarks(&mut runner);
    }

    if cat == "all" || cat.contains("buffer") || cat.contains("pool") {
        run_buffer_pool_benchmarks(&mut runner);
    }

    if cat == "all" || cat.contains("wal") || cat.contains("txn") || cat.contains("transaction") {
        run_wal_transaction_benchmarks(&mut runner);
    }

    if cat == "all" || cat.contains("sql") || cat.contains("query") {
        run_sql_query_benchmarks(&mut runner);
    }

    if cat == "all" || cat.contains("mvcc") || cat.contains("vacuum") {
        run_mvcc_vacuum_benchmarks(&mut runner);
    }

    let selected_scales = match selected_scale.as_deref() {
        Some("2000") | Some("2k") => vec![2_000],
        Some("20000") | Some("20k") => vec![20_000],
        Some("100000") | Some("100k") => vec![100_000],
        _ => vec![2_000, 20_000, 100_000],
    };

    if cat == "all" || cat.contains("scale") {
        run_scale_benchmarks(&mut runner, &selected_scales);
    }

    let suite_duration = suite_start.elapsed();
    println!("\n================================================================================");
    println!(" Benchmark Suite Completed in {:.2?}", suite_duration);
    println!(" Total Workloads Measured: {}", runner.results().len());
    println!("================================================================================");

    // Ensure target directory exists for CSV output
    if let Some(parent) = csv_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Err(e) = runner.export_csv(&csv_path) {
        eprintln!(
            "Warning: Failed to export CSV results to {}: {}",
            csv_path.display(),
            e
        );
    } else {
        println!("Exported benchmark results to {}", csv_path.display());
    }
}
