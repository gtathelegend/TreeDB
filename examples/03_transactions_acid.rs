//! Example 03: ACID Transactions, Rollback via CLRs, and Read-Your-Writes
//!
//! Demonstrates explicit multi-statement transaction management, atomic commits,
//! Compensation Log Record (CLR) rollbacks via `abort()`, RAII drop-safety, and
//! Multi-Version Concurrency Control (MVCC) read-your-writes visibility.
//!
//! Run with:
//! ```bash
//! cargo run --example 03_transactions_acid
//! ```

use tempfile::tempdir;
use treedb::{Result, TreeDb};

fn main() -> Result<()> {
    println!("=== TreeDB Example 03: ACID Transactions & Rollback ===");

    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("txn_demo.treedb");

    let mut db = TreeDb::create(&db_path, 128)?;

    // 1. Setup Accounts Table
    println!("\n--- 1. Initializing Bank Accounts Schema ---");
    db.execute_statement(
        "CREATE TABLE accounts (
            id INT PRIMARY KEY,
            holder TEXT,
            balance INT
        );",
    )?;

    db.execute_statement(
        "INSERT INTO accounts VALUES
            (1, 'Alice', 500),
            (2, 'Bob',   300);",
    )?;
    println!("Initial State: Alice=$500, Bob=$300");

    // 2. Multi-Statement Atomic Commit (Funds Transfer)
    println!("\n--- 2. Executing Atomic Funds Transfer ($100 from Alice to Bob) ---");
    {
        let mut txn = db.begin()?;

        // Debit Alice
        txn.execute_statement("UPDATE accounts SET balance = balance - 100 WHERE id = 1;")?;

        // Credit Bob
        txn.execute_statement("UPDATE accounts SET balance = balance + 100 WHERE id = 2;")?;

        // In-transaction Read-Your-Writes Verification
        let res = txn.execute_sql("SELECT holder, balance FROM accounts ORDER BY id ASC;")?;
        println!("Inside Transaction (Read-Your-Writes):");
        for row in res.rows() {
            println!("  {}: ${}", row.values[0], row.values[1]);
        }

        // Commit: Persists changes and writes durable WAL commit record
        txn.commit()?;
        println!("Transaction committed successfully to WAL.");
    }

    // Verify committed state
    let post_commit = db.execute_sql("SELECT holder, balance FROM accounts ORDER BY id ASC;")?;
    println!("\nPost-Commit State in Database:");
    for row in post_commit.rows() {
        println!("  {}: ${}", row.values[0], row.values[1]);
    }

    // 3. Transaction Rollback (Simulated Business Logic Failure)
    println!("\n--- 3. Simulating Failed Transfer with Explicit Abort ---");
    {
        let mut txn = db.begin()?;

        // Attempt to withdraw $1,000 from Alice (exceeding balance)
        txn.execute_statement("UPDATE accounts SET balance = balance - 1000 WHERE id = 1;")?;

        // Inspect intermediate balance inside transaction
        let intermediate = txn.execute_sql("SELECT balance FROM accounts WHERE id = 1;")?;
        let new_bal = match &intermediate.rows()[0].values[0] {
            treedb::Value::Int64(v) => *v,
            _ => 0,
        };
        println!("Attempted new balance inside uncommitted transaction: ${new_bal}");

        if new_bal < 0 {
            println!("Validation failed: Overdraft disallowed! Rolling back transaction...");
            // Abort: Uses Compensation Log Records (CLRs) to restore pre-transaction state
            txn.abort()?;
            println!("Transaction aborted successfully.");
        } else {
            txn.commit()?;
        }
    }

    // Verify post-abort state: Alice's balance must still be $400
    let post_abort = db.execute_sql("SELECT holder, balance FROM accounts WHERE id = 1;")?;
    println!(
        "Post-Abort State for Alice: ${}",
        post_abort.rows()[0].values[1]
    );
    assert_eq!(
        post_abort.rows()[0].values[1],
        treedb::Value::Int64(400),
        "Alice's balance should remain $400 after abort!"
    );

    // 4. RAII Auto-Abort on Drop
    println!("\n--- 4. Demonstrating RAII Drop-Safety ---");
    {
        let mut txn = db.begin()?;
        txn.execute_statement("INSERT INTO accounts VALUES (3, 'Mallory', 9999);")?;
        println!("Inserted Mallory within transaction; dropping transaction without commit...");
        // txn is dropped here at the end of block without calling commit()
    }

    let mallory_check = db.execute_sql("SELECT * FROM accounts WHERE id = 3;")?;
    println!(
        "Mallory exists in database after drop: {}",
        !mallory_check.is_empty()
    );
    assert!(
        mallory_check.is_empty(),
        "Uncommitted inserted record should be discarded on drop!"
    );

    db.sync()?;
    println!("\nExample 03 completed successfully!");
    Ok(())
}
