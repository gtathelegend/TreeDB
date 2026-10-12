use std::fs::File;
use std::hint::black_box;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// A fast, zero-dependency, deterministic pseudo-random number generator.
#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Creates a new generator seeded with a non-zero state.
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x1337_2026_cafe_babe
            } else {
                seed
            },
        }
    }

    /// Generates the next pseudo-random 64-bit integer.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Generates a random integer within `0..max`.
    pub fn next_range(&mut self, max: u64) -> u64 {
        if max == 0 {
            0
        } else {
            self.next_u64() % max
        }
    }

    /// Fills a buffer with random bytes.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    pub fn fill_bytes(&mut self, buf: &mut [u8]) {
        let mut chunks = buf.chunks_exact_mut(8);
        for chunk in &mut chunks {
            let val = self.next_u64();
            chunk.copy_from_slice(&val.to_le_bytes());
        }
        let remainder = chunks.into_remainder();
        if !remainder.is_empty() {
            let val = self.next_u64();
            let bytes = val.to_le_bytes();
            remainder.copy_from_slice(&bytes[..remainder.len()]);
        }
    }
}

/// Structured summary of a single benchmark workload execution.
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub category: &'static str,
    pub name: &'static str,
    pub dataset_size: usize,
    pub iterations: usize,
    #[allow(dead_code)]
    pub total_duration: Duration,
    pub throughput_ops_sec: f64,
    pub avg_latency_ns: f64,
    pub min_latency_ns: u64,
    pub max_latency_ns: u64,
    pub p50_latency_ns: u64,
    pub p95_latency_ns: u64,
    pub p99_latency_ns: u64,
    pub notes: String,
}

impl BenchmarkResult {
    /// Computes summary statistics from a slice of sample latencies (in nanoseconds).
    /// Computes summary statistics from a slice of sample latencies (in nanoseconds).
    pub fn from_samples(
        category: &'static str,
        name: &'static str,
        dataset_size: usize,
        iterations: usize,
        mut samples_ns: Vec<u64>,
        notes: impl Into<String>,
    ) -> Self {
        assert!(!samples_ns.is_empty(), "benchmark produced zero samples");
        samples_ns.sort_unstable();

        let count = samples_ns.len();
        let total_ns: u64 = samples_ns.iter().sum();
        let total_duration = Duration::from_nanos(total_ns);

        let min_latency_ns = samples_ns[0];
        let max_latency_ns = samples_ns[count - 1];
        let avg_latency_ns = (total_ns as f64) / (count as f64);

        let p50_latency_ns = samples_ns[count * 50 / 100];
        let p95_latency_ns = samples_ns[(count * 95 / 100).min(count - 1)];
        let p99_latency_ns = samples_ns[(count * 99 / 100).min(count - 1)];

        let total_secs = total_duration.as_secs_f64();
        let throughput_ops_sec = if total_secs > 0.0 {
            (count as f64) / total_secs
        } else {
            0.0
        };

        Self {
            category,
            name,
            dataset_size,
            iterations,
            total_duration,
            throughput_ops_sec,
            avg_latency_ns,
            min_latency_ns,
            max_latency_ns,
            p50_latency_ns,
            p95_latency_ns,
            p99_latency_ns,
            notes: notes.into(),
        }
    }

    /// Computes summary statistics from an un-sampled batch duration.
    pub fn from_batch(
        category: &'static str,
        name: &'static str,
        dataset_size: usize,
        total_ops: usize,
        total_duration: Duration,
        notes: impl Into<String>,
    ) -> Self {
        let total_ns = total_duration.as_nanos() as u64;
        let avg_latency_ns = (total_ns as f64) / (total_ops.max(1) as f64);
        let throughput_ops_sec = if total_duration.as_secs_f64() > 0.0 {
            (total_ops as f64) / total_duration.as_secs_f64()
        } else {
            0.0
        };

        Self {
            category,
            name,
            dataset_size,
            iterations: total_ops,
            total_duration,
            throughput_ops_sec,
            avg_latency_ns,
            min_latency_ns: avg_latency_ns.round() as u64,
            max_latency_ns: avg_latency_ns.round() as u64,
            p50_latency_ns: avg_latency_ns.round() as u64,
            p95_latency_ns: avg_latency_ns.round() as u64,
            p99_latency_ns: avg_latency_ns.round() as u64,
            notes: notes.into(),
        }
    }
}

/// Harness runner coordinating warmups, measurement, and reporting.
pub struct BenchmarkRunner {
    results: Vec<BenchmarkResult>,
}

impl Default for BenchmarkRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchmarkRunner {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
        }
    }

    /// Records an externally computed benchmark result.
    pub fn record(&mut self, result: BenchmarkResult) {
        self.print_result(&result);
        self.results.push(result);
    }

    fn print_result(&self, r: &BenchmarkResult) {
        let lat_str = format_latency(r.avg_latency_ns);
        let p50_str = format_latency(r.p50_latency_ns as f64);
        let p99_str = format_latency(r.p99_latency_ns as f64);
        let tput_str = format_throughput(r.throughput_ops_sec);

        println!(
            "[{:12}] {:<32} | N={:<6} | avg: {:<9} (p50: {}, p99: {}) | {:>14} | {}",
            r.category, r.name, r.dataset_size, lat_str, p50_str, p99_str, tput_str, r.notes
        );
    }

    pub fn results(&self) -> &[BenchmarkResult] {
        &self.results
    }

    /// Exports all recorded results to a CSV file.
    pub fn export_csv(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let mut file = File::create(path)?;
        writeln!(
            file,
            "category,name,dataset_size,iterations,throughput_ops_sec,avg_latency_ns,min_latency_ns,max_latency_ns,p50_latency_ns,p95_latency_ns,p99_latency_ns,notes"
        )?;
        for r in &self.results {
            writeln!(
                file,
                "{},{},{},{},{:.2},{:.2},{},{},{},{},{},\"{}\"",
                r.category,
                r.name,
                r.dataset_size,
                r.iterations,
                r.throughput_ops_sec,
                r.avg_latency_ns,
                r.min_latency_ns,
                r.max_latency_ns,
                r.p50_latency_ns,
                r.p95_latency_ns,
                r.p99_latency_ns,
                r.notes.replace('"', "\"\"")
            )?;
        }
        Ok(())
    }
}

pub fn format_latency(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{:.1} ns", ns)
    } else if ns < 1_000_000.0 {
        format!("{:.2} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

pub fn format_throughput(ops_sec: f64) -> String {
    if ops_sec >= 1_000_000.0 {
        format!("{:.2} M ops/s", ops_sec / 1_000_000.0)
    } else if ops_sec >= 1_000.0 {
        format!("{:.1} k ops/s", ops_sec / 1_000.0)
    } else {
        format!("{:.1} ops/s", ops_sec)
    }
}

/// Helper to ensure a variable is not optimized away.
pub fn consume<T>(val: T) {
    black_box(val);
}
