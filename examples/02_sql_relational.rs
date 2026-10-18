//! Example 02: Relational SQL Tables, Secondary Indexes, and Query Execution
//!
//! Demonstrates defining relational schemas via DDL, inserting structured tuples,
//! indexing secondary columns, executing queries with filters, joins, groupings,
//! and aggregations through the Volcano-style query engine.
//!
//! Run with:
//! ```bash
//! cargo run --example 02_sql_relational
//! ```

use tempfile::tempdir;
use treedb::{Result, TreeDb};

fn main() -> Result<()> {
    println!("=== TreeDB Example 02: Relational SQL & Query Execution ===");

    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("sql_demo.treedb");

    let mut db = TreeDb::create(&db_path, 128)?;

    // 1. Create Tables using SQL DDL
    println!("\n--- 1. Creating Relational Tables ---");
    db.execute_statement(
        "CREATE TABLE products (
            id INT PRIMARY KEY,
            name TEXT,
            category TEXT,
            price INT
        );",
    )?;

    db.execute_statement(
        "CREATE TABLE sales (
            sale_id INT PRIMARY KEY,
            product_id INT,
            quantity INT,
            region TEXT
        );",
    )?;
    println!("Created tables 'products' and 'sales'.");

    // 2. Create a Secondary Index
    println!("\n--- 2. Creating Secondary B+ Tree Index ---");
    db.execute_statement("CREATE INDEX idx_products_category ON products (category);")?;
    println!("Created index 'idx_products_category' on products(category).");

    // 3. Insert Relational Tuples via DML
    println!("\n--- 3. Inserting Tuples ---");
    db.execute_statement(
        "INSERT INTO products VALUES
            (1, 'Mechanical Keyboard', 'Hardware', 140),
            (2, 'Ergonomic Mouse', 'Hardware', 80),
            (3, '4K Monitor', 'Hardware', 350),
            (4, 'IDE License', 'Software', 200),
            (5, 'Compiler Toolchain', 'Software', 0);",
    )?;

    db.execute_statement(
        "INSERT INTO sales VALUES
            (101, 1, 5, 'North America'),
            (102, 1, 2, 'Europe'),
            (103, 3, 1, 'North America'),
            (104, 4, 10, 'Asia');",
    )?;
    println!("Inserted initial product and sales records.");

    // 4. Filtered & Sorted Query
    println!("\n--- 4. Query: Filtered & Sorted ---");
    let query_sql =
        "SELECT name, category, price FROM products WHERE price > 50 ORDER BY price DESC;";
    println!("SQL: {query_sql}");
    let res = db.execute_sql(query_sql)?;
    println!("Results ({} rows):", res.len());
    for row in res.rows() {
        println!(
            "  Product: {:<20} | Category: {:<10} | Price: ${}",
            row.values[0], row.values[1], row.values[2]
        );
    }

    // 5. Relational JOIN Query
    println!("\n--- 5. Query: Relational Inner Join ---");
    let join_sql = "SELECT p.name, s.quantity, s.region
                    FROM products p
                    JOIN sales s ON p.id = s.product_id
                    ORDER BY s.quantity DESC;";
    println!("SQL: {join_sql}");
    let join_res = db.execute_sql(join_sql)?;
    println!("Results ({} rows):", join_res.len());
    for row in join_res.rows() {
        println!(
            "  Item: {:<20} | Quantity: {:<3} | Region: {}",
            row.values[0], row.values[1], row.values[2]
        );
    }

    // 6. Aggregation with GROUP BY and HAVING
    println!("\n--- 6. Query: Aggregation with GROUP BY ---");
    let agg_sql = "SELECT category, COUNT(*), AVG(price), MAX(price)
                   FROM products
                   GROUP BY category;";
    println!("SQL: {agg_sql}");
    let agg_res = db.execute_sql(agg_sql)?;
    println!("Results ({} categories):", agg_res.len());
    for row in agg_res.rows() {
        println!(
            "  Category: {:<10} | Count: {} | Avg Price: ${} | Max Price: ${}",
            row.values[0], row.values[1], row.values[2], row.values[3]
        );
    }

    // 7. Flush and ensure clean durability
    db.sync()?;
    println!("\nExample 02 completed successfully!");
    Ok(())
}
