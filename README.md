# TreeDB

> **TreeDB — A persistent storage engine built from first principles in Rust.**

TreeDB is a single-node, disk-backed storage engine designed to demonstrate and explore database internals through a clean, modern, and production-disciplined implementation.

It is built for educational, research, and deep-systems exploration. It is not intended to compete with commercial production database engines, but rather to clearly demonstrate how storage engines operate under the hood.

---

## High-Level Architecture

```text
Application / CLI
        │
        ▼
   TreeDB API (Planned)
        │
        ▼
     B+ Tree (Read-only Traversal Implemented, Insertion Next)
        │
        ▼
   Record Store (Planned)
        │
        ▼
   Buffer Pool (Implemented)
        │
        ▼
   Page Manager (Implemented)
        │
        ▼
   Database File (Implemented)
```

---

## Core Capabilities & Status

| Subsystem / Feature | Status | Description |
| :--- | :--- | :--- |
| **Database File & Header** | **Implemented** | 4096-byte pages, format verification, magic bytes `TREEDB01` |
| **Page Manager** | **Implemented** | Page allocation, random-access I/O, file synchronization |
| **Page Abstraction** | **Implemented** | Deterministic binary layout, header tags, structural validation |
| **Buffer Pool Manager** | **Implemented** | LRU eviction, frame table, pin/unpin tracking, dirty flush |
| **B+ Tree Index (Search)** | **Implemented** | Leaf & internal node layouts, derived capacity, point search |
| **B+ Tree Index (Insert)** | *Planned* | Leaf & internal node splitting, root elevation (Phase 4) |
| **Record Store** | *Planned* | Slotted-page tuple format, variable-length records (Phase 6) |
| **Write-Ahead Logging** | *Experimental* | WAL frame serialization, crash recovery (future phase) |

---

## Getting Started

### Prerequisites
* Rust 1.80+ (Stable toolchain)

### Building
```bash
cargo build --release
```

### Running Tests
```bash
cargo test --all-targets
```

### Running the CLI
```bash
# Initialize a new database
cargo run --bin treedb -- init test.treedb

# Inspect database file metadata
cargo run --bin treedb -- info test.treedb
```

---

## Architectural Principles

1. **Correctness before performance:** Invariants are strictly verified through tests and structural validation before applying optimizations.
2. **Explicit invariants:** Types and checks enforce internal limits and prevent corruption.
3. **Strict layer separation:** Higher-level abstractions (B+ Tree, Buffer Pool) interact exclusively through clean lower-level interfaces.
4. **Deterministic persistent format:** On-disk binary representation is explicitly specified with little-endian integer encoding.
5. **Safe Rust ownership:** Avoiding unsafe code wherever possible by leveraging Rust's ownership model for lifecycle management.

---

## Documentation

* [Architecture Overview](docs/ARCHITECTURE.md)
* [B+ Tree Index Engine](docs/BPLUS_TREE.md)
* [Buffer Pool Manager](docs/BUFFER_POOL.md)
* [On-Disk Storage Format](docs/STORAGE_FORMAT.md)
* [Design Decisions](docs/DESIGN_DECISIONS.md)
* [Project Roadmap](docs/ROADMAP.md)

---

## License

Dual-licensed under either of:
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE) or http://opensource.org/licenses/MIT)
