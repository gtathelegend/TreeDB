use std::time::Instant;
use treedb::{BufferPool, PageManager, PageType};

use crate::framework::{consume, BenchmarkResult, BenchmarkRunner, XorShift64};

pub fn run_buffer_pool_benchmarks(runner: &mut BenchmarkRunner) {
    println!("\n=== Running Buffer Pool & Page Manager Benchmarks ===");

    let total_pages = 2_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pages.treedb");

    // Setup: Pre-populate database with `total_pages` pages
    let mut page_ids = Vec::with_capacity(total_pages);
    {
        let pm = PageManager::create(&path).unwrap();
        let mut pool = BufferPool::new(total_pages + 100, pm).unwrap();

        for i in 0..total_pages {
            let pid = pool.allocate_page(PageType::Record).unwrap();
            {
                let page = pool.get_page_mut(pid).unwrap();
                let payload = page.payload_mut();
                payload[..8].copy_from_slice(&(i as u64).to_le_bytes());
            }
            pool.unpin_page(pid, true).unwrap();
            page_ids.push(pid);
        }
        pool.flush_all().unwrap();
    }

    // 1. Warm-cache point access (all pages fit in buffer pool)
    {
        let pm = PageManager::open(&path).unwrap();
        let mut pool = BufferPool::new(total_pages + 100, pm).unwrap();

        // Warm up the cache by reading all pages once
        for &pid in &page_ids {
            let _ = pool.fetch_page(pid).unwrap();
            pool.unpin_page(pid, false).unwrap();
        }

        let iters = 20_000;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let pid = page_ids[i % total_pages];
            let start = Instant::now();
            let page = pool.fetch_page(pid).unwrap();
            let byte = page.payload()[0];
            consume(byte);
            pool.unpin_page(pid, false).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        let stats = pool.stats();
        let hit_rate = stats.hit_rate() * 100.0;

        runner.record(BenchmarkResult::from_samples(
            "Buffer Pool",
            "warm_cache_point_access",
            total_pages,
            iters,
            samples,
            format!(
                "100% in-memory buffer pool hits (hit rate: {:.1}%)",
                hit_rate
            ),
        ));
    }

    // 2. Cold-cache point access (fresh buffer pool on each access)
    // Here "cold cache" means a newly initialized BufferPool frame table with 0 cached frames,
    // requiring physical I/O to read the page from the disk file.
    {
        let iters = 100;
        let mut samples = Vec::with_capacity(iters);

        for i in 0..iters {
            let pid = page_ids[i % total_pages];
            let start = Instant::now();
            {
                let pm = PageManager::open(&path).unwrap();
                let mut fresh_pool = BufferPool::new(16, pm).unwrap();
                let page = fresh_pool.fetch_page(pid).unwrap();
                let val = page.payload()[0];
                consume(val);
                fresh_pool.unpin_page(pid, false).unwrap();
            }
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Buffer Pool",
            "cold_cache_point_access",
            total_pages,
            iters,
            samples,
            "Fresh buffer pool requiring disk read per operation",
        ));
    }

    // 3. Cache thrashing / Eviction under memory constraint (pool size << working set)
    {
        let constrained_capacity = 64; // only 64 frames for 2,000 pages
        let pm = PageManager::open(&path).unwrap();
        let mut pool = BufferPool::new(constrained_capacity, pm).unwrap();
        let mut rng = XorShift64::new(0x3030_4040_5050);

        let iters = 5_000;
        let mut samples = Vec::with_capacity(iters);

        for _ in 0..iters {
            let idx = rng.next_range(total_pages as u64) as usize;
            let pid = page_ids[idx];
            let start = Instant::now();
            let page = pool.fetch_page(pid).unwrap();
            let val = page.payload()[0];
            consume(val);
            pool.unpin_page(pid, false).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            samples.push(elapsed);
        }

        let stats = pool.stats();
        runner.record(BenchmarkResult::from_samples(
            "Buffer Pool",
            "cache_constrained_random_access",
            total_pages,
            iters,
            samples,
            format!(
                "Pool size {} frames / {} pages (hit rate: {:.1}%, evictions: {})",
                constrained_capacity,
                total_pages,
                stats.hit_rate() * 100.0,
                stats.evictions
            ),
        ));
    }

    // 4. Dirty page flushing
    {
        let dir2 = tempfile::tempdir().unwrap();
        let path2 = dir2.path().join("dirty.treedb");
        let pm2 = PageManager::create(&path2).unwrap();
        let dirty_count = 500;
        let mut pool = BufferPool::new(dirty_count + 50, pm2).unwrap();

        for i in 0..dirty_count {
            let pid = pool.allocate_page(PageType::Record).unwrap();
            {
                let page = pool.get_page_mut(pid).unwrap();
                page.payload_mut()[0] = (i % 256) as u8;
            }
            pool.unpin_page(pid, true).unwrap();
        }

        let start = Instant::now();
        pool.flush_all().unwrap();
        let elapsed_ns = start.elapsed().as_nanos() as u64;

        runner.record(BenchmarkResult::from_samples(
            "Buffer Pool",
            "flush_dirty_pages",
            dirty_count,
            1,
            vec![elapsed_ns],
            format!(
                "Flush {} dirty pages to disk through PageManager",
                dirty_count
            ),
        ));
    }

    // 5. Page allocation & freelist recycling
    {
        let dir3 = tempfile::tempdir().unwrap();
        let path3 = dir3.path().join("alloc.treedb");
        let pm3 = PageManager::create(&path3).unwrap();
        let mut pool = BufferPool::new(256, pm3).unwrap();
        let count = 1_000;

        // Allocate pages
        let mut allocated = Vec::with_capacity(count);
        let mut alloc_samples = Vec::with_capacity(count);
        for _ in 0..count {
            let start = Instant::now();
            let pid = pool.allocate_page(PageType::Record).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            allocated.push(pid);
            pool.unpin_page(pid, true).unwrap();
            alloc_samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Buffer Pool",
            "page_allocation",
            count,
            count,
            alloc_samples,
            format!("Sequential allocation of {} new pages", count),
        ));

        // Free pages to freelist
        let mut free_samples = Vec::with_capacity(count);
        for &pid in &allocated {
            let start = Instant::now();
            pool.free_page(pid).unwrap();
            let elapsed = start.elapsed().as_nanos() as u64;
            free_samples.push(elapsed);
        }

        runner.record(BenchmarkResult::from_samples(
            "Buffer Pool",
            "page_deallocation_freelist",
            count,
            count,
            free_samples,
            format!("Return {} pages to persistent freelist", count),
        ));
    }
}
