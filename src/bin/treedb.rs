use std::env;
use std::process;
use treedb::{PageManager, Result};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        println!("TreeDB - A persistent storage engine written from first principles in Rust.");
        println!();
        println!("Usage: treedb <command> [args]");
        println!();
        println!("Commands:");
        println!("  init <path>         Initialize a new TreeDB database file");
        println!("  info <path>         Display database file metadata");
        println!("  query <path> <sql>  Execute a SQL SELECT query against a database");
        println!("  version             Show TreeDB version information");
        return Ok(());
    }

    match args[1].as_str() {
        "version" => {
            println!("TreeDB v{}", env!("CARGO_PKG_VERSION"));
        }
        "query" => {
            if args.len() < 4 {
                eprintln!("Error: database path and SQL query required. Usage: treedb query <path> \"<SQL>\"");
                process::exit(1);
            }
            let path = &args[2];
            let sql = &args[3];
            let mut db = treedb::TreeDb::open(path, 64).unwrap_or_else(|e| {
                eprintln!("Failed to open database at '{path}': {e}");
                process::exit(1);
            });

            let stmt_result = db.execute_statement(sql).unwrap_or_else(|e| {
                eprintln!("SQL Execution Error: {e}");
                process::exit(1);
            });

            match stmt_result {
                treedb::StatementResult::Query(result) => {
                    let schema = result.schema();
                    let col_names: Vec<String> =
                        schema.columns.iter().map(|c| c.name.clone()).collect();
                    println!("{}", col_names.join(" | "));
                    println!("{}", "-".repeat(col_names.join(" | ").len().max(10)));
                    for row in result.rows() {
                        let row_str: Vec<String> =
                            row.values.iter().map(|v| v.to_string()).collect();
                        println!("{}", row_str.join(" | "));
                    }
                    println!("({} row(s) returned)", result.len());
                }
                treedb::StatementResult::CreatedIndex { index } => {
                    println!("Created index '{index}'");
                }
                treedb::StatementResult::DroppedIndex { index } => {
                    println!("Dropped index '{index}'");
                }
                treedb::StatementResult::CreatedTable { table } => {
                    println!("Created table '{table}'");
                }
                treedb::StatementResult::DroppedTable { table } => {
                    println!("Dropped table '{table}'");
                }
                treedb::StatementResult::Inserted { rows } => {
                    println!("Inserted {rows} row(s)");
                }
                treedb::StatementResult::Updated { rows } => {
                    println!("Updated {rows} row(s)");
                }
                treedb::StatementResult::Deleted { rows } => {
                    println!("Deleted {rows} row(s)");
                }
            }
        }
        "init" => {
            if args.len() < 3 {
                eprintln!("Error: path required. Usage: treedb init <path>");
                process::exit(1);
            }
            let path = &args[2];
            let pm = PageManager::create(path).unwrap_or_else(|e| {
                eprintln!("Failed to create database at '{path}': {e}");
                process::exit(1);
            });
            println!(
                "Initialized TreeDB database at '{}' (allocated pages: {})",
                path,
                pm.page_count()
            );
        }
        "info" => {
            if args.len() < 3 {
                eprintln!("Error: path required. Usage: treedb info <path>");
                process::exit(1);
            }
            let path = &args[2];
            let pm = PageManager::open(path).unwrap_or_else(|e| {
                eprintln!("Failed to open database at '{path}': {e}");
                process::exit(1);
            });
            let header = pm.header();
            println!("TreeDB Database Info:");
            println!("  Path:             {}", pm.path().display());
            println!("  Format Version:   {}", header.format_version);
            println!("  Page Size:        {} bytes", header.page_size);
            println!("  Page Count:       {}", header.page_count);
            println!("  Root Page ID:     {}", header.root_page_id);
            println!("  Metadata Page ID: {}", header.metadata_page_id);
        }
        unknown => {
            eprintln!("Unknown command: '{unknown}'. Run 'treedb' for help.");
            process::exit(1);
        }
    }

    Ok(())
}
