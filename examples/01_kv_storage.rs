//! Example 01: Low-Level Key-Value Storage Engine API
//!
//! Demonstrates initializing a persistent TreeDB instance, performing basic
//! key-value insertions, point lookups, deletions, disk synchronization,
//! and running a non-destructive cross-layer consistency verification.
//!
//! Run with:
//! ```bash
//! cargo run --example 01_kv_storage
//! ```

use tempfile::tempdir;
use treedb::{Result, TreeDb};

fn main() -> Result<()> {
    println!("=== TreeDB Example 01: Key-Value Storage Engine ===");

    // 1. Create a temporary directory for an isolated database file
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("kv_demo.treedb");

    println!("Initializing database at: {}", db_path.display());

    // 2. Create a new TreeDB instance with a 64-page buffer pool (256 KiB cache)
    let mut db = TreeDb::create(&db_path, 64)?;

    // 3. Insert key-value pairs (Key is u64; Value is arbitrary bytes)
    println!("\n--- 1. Inserting Key-Value Pairs ---");
    db.put(1001, b"User: Alice | Role: Admin")?;
    db.put(1002, b"User: Bob   | Role: Developer")?;
    db.put(1003, b"User: Carol | Role: Analyst")?;
    println!("Inserted 3 records successfully.");

    // 4. Point Lookups
    println!("\n--- 2. Point Lookups ---");
    for &key in &[1001, 1002, 1003, 1004] {
        match db.get(key)? {
            Some(val) => {
                let text = String::from_utf8_lossy(&val);
                println!("Key {key} => Found: \"{text}\"");
            }
            None => {
                println!("Key {key} => Not Found (None)");
            }
        }
    }

    // 5. Update an existing key
    println!("\n--- 3. Updating an Existing Key ---");
    db.put(1002, b"User: Bob   | Role: Lead Architect")?;
    if let Some(val) = db.get(1002)? {
        println!("Updated Key 1002 => \"{}\"", String::from_utf8_lossy(&val));
    }

    // 6. Delete a key
    println!("\n--- 4. Deleting a Key ---");
    let deleted = db.delete(1003)?;
    println!("Deleted Key 1003 was present: {}", deleted.is_some());
    println!("Key 1003 post-delete exists: {}", db.get(1003)?.is_some());

    // 7. Synchronize all dirty buffer pool frames and WAL records to disk
    println!("\n--- 5. Durability Sync ---");
    db.sync()?;
    println!("Database synchronized to physical storage via sync_all().");

    // 8. Run non-destructive cross-layer consistency verification
    println!("\n--- 6. Consistency Verification ---");
    let report = db.verify_consistency()?;
    println!(
        "Consistency Report: Healthy={}, Issues={}",
        report.is_healthy(),
        report.issues.len()
    );
    assert!(report.is_healthy(), "Database consistency check failed!");

    println!("\nExample 01 completed successfully!");
    Ok(())
}
