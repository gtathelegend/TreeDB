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
        println!("  init <path>    Initialize a new TreeDB database file");
        println!("  info <path>    Display database file metadata");
        println!("  version        Show TreeDB version information");
        return Ok(());
    }

    match args[1].as_str() {
        "version" => {
            println!("TreeDB v{}", env!("CARGO_PKG_VERSION"));
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
