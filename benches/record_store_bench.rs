use std::time::Instant;
use treedb::{BufferPool, PageManager, RecordStore};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner, XorShift64};

pub fn run_record_store_benchmarks(runner: &mut BenchmarkRunner) {
    println!("\n=== Running Record Store Benchmarks ===");

    // 1. Small inline records (64 bytes)
    bench_records_by_size(runner, "small_inline_64B", 64, 5_000);

    // 2. Medium inline records (512 bytes)
    bench_records_by_size(runner, "medium_inline_512B", 512, 2_000);

    // 3. Large overflow records (8,192 bytes = 2+ pages)
    bench_records_by_size(runner, "large_overflow_8KB", 8_192, 500);

    // 4. Very large overflow records (16,384 bytes = 4+ pages)
    bench_records_by_size(runner, "very_large_overflow_16KB", 16_384, 250);
}

fn bench_records_by_size(
    runner: &mut BenchmarkRunner,
    label: &'static str,
    payload_size: usize,
    count: usize,
) {
    let mut rng = XorShift64::new(0x2026_1337_cafe_0002 ^ (payload_size as u64));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.treedb");
    let pm = PageManager::create(&path).unwrap();
    let mut pool = BufferPool::new(512, pm).unwrap();
    let mut store = RecordStore::new();

    let mut payload = vec![0u8; payload_size];
    rng.fill_bytes(&mut payload);

    // Benchmark 1: Insert records
    let mut record_ids = Vec::with_capacity(count);
    let mut insert_samples = Vec::with_capacity(count);

    for i in 0..count {
        // Tag first 8 bytes with record index for correctness verification
        payload[..8].copy_from_slice(&(i as u64).to_le_bytes());

        let start = Instant::now();
        let rid = store.insert(&mut pool, &payload).unwrap();
        let elapsed = start.elapsed().as_nanos() as u64;

        record_ids.push(rid);
        insert_samples.push(elapsed);
    }

    runner.record(BenchmarkResult::from_samples(
        "Record Store",
        label,
        count,
        count,
        insert_samples,
        format!("Sequential inserts ({}B)", payload_size),
    ));

    // Benchmark 2: Sequential Read
    {
        let mut read_samples = Vec::with_capacity(count);
        for (i, &rid) in record_ids.iter().enumerate() {
            let start = Instant::now();
            let data = store.get(&mut pool, rid).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;

            let bytes = data.expect("Record must exist");
            assert_eq!(
                u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                i as u64,
                "Record content mismatch during sequential read"
            );
            read_samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Record Store",
            label,
            count,
            count,
            read_samples,
            format!("Sequential reads ({}B)", payload_size),
        ));
    }

    // Benchmark 3: Random Read
    {
        let mut shuffled_indices: Vec<usize> = (0..count).collect();
        for i in (1..shuffled_indices.len()).rev() {
            let j = rng.next_range((i + 1) as u64) as usize;
            shuffled_indices.swap(i, j);
        }

        let mut rand_samples = Vec::with_capacity(count);
        for &idx in &shuffled_indices {
            let rid = record_ids[idx];
            let start = Instant::now();
            let data = store.get(&mut pool, rid).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;

            consume(data);
            rand_samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Record Store",
            label,
            count,
            count,
            rand_samples,
            format!("Randomized reads ({}B)", payload_size),
        ));
    }

    // Benchmark 4: In-place Update
    {
        let update_count = (count / 2).max(10);
        let mut update_samples = Vec::with_capacity(update_count);

        let mut updated_payload = payload.clone();
        for (i, rid) in record_ids.iter_mut().enumerate().take(update_count) {
            let old_rid = *rid;
            updated_payload[..8].copy_from_slice(&((i as u64) ^ 0xdead_beef).to_le_bytes());

            let start = Instant::now();
            store.delete(&mut pool, old_rid).unwrap();
            let new_rid = store.insert(&mut pool, &updated_payload).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;

            *rid = new_rid;
            update_samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Record Store",
            label,
            count,
            update_count,
            update_samples,
            format!("Updates to existing records ({}B)", payload_size),
        ));
    }

    // Benchmark 5: Record Deletion
    {
        let delete_count = (count / 2).max(10);
        let mut delete_samples = Vec::with_capacity(delete_count);

        for &rid in record_ids.iter().take(delete_count) {
            let start = Instant::now();
            let deleted = store.delete(&mut pool, rid).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;

            assert!(deleted, "Record was not deleted");
            delete_samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Record Store",
            label,
            count,
            delete_count,
            delete_samples,
            format!("Record deletions and slot reclamation ({}B)", payload_size),
        ));
    }
}
