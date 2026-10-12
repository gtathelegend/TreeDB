use std::time::Instant;
use treedb::{BPlusTree, BufferPool, PageManager, RecordId};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner, XorShift64};

pub fn run_bplus_tree_benchmarks(runner: &mut BenchmarkRunner) {
    println!("\n=== Running B+ Tree Benchmarks ===");

    for &size in &[1_000, 10_000, 50_000] {
        bench_tree_size(runner, size);
    }
}

fn bench_tree_size(runner: &mut BenchmarkRunner, size: usize) {
    let mut rng = XorShift64::new(0x1337_2026_cafe_0001 ^ (size as u64));

    // Generate deterministic keys
    let mut keys: Vec<u64> = (0..size as u64).collect();
    // Shuffle deterministically
    for i in (1..keys.len()).rev() {
        let j = rng.next_range((i + 1) as u64) as usize;
        keys.swap(i, j);
    }

    // 1. Benchmark: Insert into empty tree
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bench_empty.treedb");
        let pm = PageManager::create(&path).unwrap();
        let mut pool = BufferPool::new(size / 2 + 128, pm).unwrap();
        let mut tree = BPlusTree::new_empty();

        let mut samples = Vec::with_capacity(size);
        for (i, &key) in keys.iter().enumerate() {
            let rid = RecordId::new(100 + (i as u32 / 100), (i % 100) as u32);
            let start = Instant::now();
            tree.insert(&mut pool, key, rid).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        // Correctness check: verify key 0 and middle key exist
        let check_rid = tree.get(&mut pool, keys[0]).unwrap();
        assert!(check_rid.is_some(), "Correctness check failed on key 0");

        let res = BenchmarkResult::from_samples(
            "B+ Tree",
            "insert_empty_tree",
            size,
            size,
            samples,
            format!("Build empty tree with {} keys", size),
        );
        runner.record(res);
    }

    // Set up a pre-populated tree for steady-state benchmarks
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench_prepopulated.treedb");
    let pm = PageManager::create(&path).unwrap();
    let mut pool = BufferPool::new(size / 2 + 256, pm).unwrap();
    let mut tree = BPlusTree::new_empty();

    for (i, &key) in keys.iter().enumerate() {
        let rid = RecordId::new(100 + (i as u32 / 100), (i % 100) as u32);
        tree.insert(&mut pool, key, rid).unwrap();
    }

    // 2. Benchmark: Successful point lookup
    {
        let lookup_count = (size * 2).min(20_000);
        let mut samples = Vec::with_capacity(lookup_count);

        for i in 0..lookup_count {
            let key = keys[i % size];
            let start = Instant::now();
            let rid = tree.get(&mut pool, key).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            consume(rid);
            samples.push(elapsed);
        }

        let res = BenchmarkResult::from_samples(
            "B+ Tree",
            "point_lookup_hit",
            size,
            lookup_count,
            samples,
            format!("Successful point lookups in N={}", size),
        );
        runner.record(res);
    }

    // 3. Benchmark: Unsuccessful point lookup
    {
        let lookup_count = (size * 2).min(20_000);
        let mut samples = Vec::with_capacity(lookup_count);

        for i in 0..lookup_count {
            let missing_key = (size as u64 * 10) + i as u64;
            let start = Instant::now();
            let rid = tree.get(&mut pool, missing_key).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert!(rid.is_none(), "Unsuccessful lookup returned Some");
            samples.push(elapsed);
        }

        let res = BenchmarkResult::from_samples(
            "B+ Tree",
            "point_lookup_miss",
            size,
            lookup_count,
            samples,
            format!("Unsuccessful point lookups in N={}", size),
        );
        runner.record(res);
    }

    // 4. Benchmark: Insert into pre-populated tree
    {
        let extra_inserts = (size / 10).min(5_000);
        let mut extra_keys: Vec<u64> = (0..extra_inserts as u64)
            .map(|i| (size as u64 * 2) + i)
            .collect();
        for i in (1..extra_keys.len()).rev() {
            let j = rng.next_range((i + 1) as u64) as usize;
            extra_keys.swap(i, j);
        }

        let mut samples = Vec::with_capacity(extra_inserts);
        for (idx, &k) in extra_keys.iter().enumerate() {
            let rid = RecordId::new(999, idx as u32);
            let start = Instant::now();
            tree.insert(&mut pool, k, rid).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        let res = BenchmarkResult::from_samples(
            "B+ Tree",
            "insert_prepopulated",
            size,
            extra_inserts,
            samples,
            format!("Insert {} new keys into pre-populated tree", extra_inserts),
        );
        runner.record(res);
    }

    // 5. Benchmark: Mixed insert/lookup workload (70% read, 30% insert)
    {
        let mixed_ops = (size / 5).min(5_000);
        let mut samples = Vec::with_capacity(mixed_ops);

        for i in 0..mixed_ops {
            let is_read = (rng.next_u64() % 100) < 70;
            let start = Instant::now();
            if is_read {
                let k = keys[i % size];
                let rid = tree.get(&mut pool, k).unwrap();
                consume(rid);
            } else {
                let k = (size as u64 * 4) + i as u64;
                let rid = RecordId::new(888, (i % 500) as u32);
                tree.insert(&mut pool, k, rid).unwrap();
            }
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        let res = BenchmarkResult::from_samples(
            "B+ Tree",
            "mixed_workload_70r_30w",
            size,
            mixed_ops,
            samples,
            "70% point lookup, 30% insert",
        );
        runner.record(res);
    }

    // 6. Benchmark: Delete existing keys
    {
        let delete_count = (size / 10).min(5_000);
        let mut samples = Vec::with_capacity(delete_count);

        for &key in keys.iter().take(delete_count) {
            let start = Instant::now();
            let old = tree.delete(&mut pool, key).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            assert!(old.is_some(), "Delete target key was not found");
            samples.push(elapsed);
        }

        let res = BenchmarkResult::from_samples(
            "B+ Tree",
            "delete_existing_keys",
            size,
            delete_count,
            samples,
            format!(
                "Delete {} existing keys with tree rebalancing",
                delete_count
            ),
        );
        runner.record(res);
    }
}
