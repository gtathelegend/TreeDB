# TreeDB Release Candidate Validation & Clean-Checkout Reproducibility (Phase 79)

## 1. Final Verdict

**Verdict:** `PASS WITH LIMITATIONS`

TreeDB v0.1.0-rc1 is fully reproducible, clean-checkout verified, and certified for release candidate status under its documented single-writer and offline-backup operational contract.

---

## 2. Audited Commit, Branch & Tag Status

* **Audited Release Commit:** `22dfc2ef6647f9b4f778dd5bde91d142e0ecc867`
* **Release Tag:** `v0.1.0-rc1` (annotated tag pointing to `22dfc2ef6647f9b4f778dd5bde91d142e0ecc867`)
* **Tag Message:** `TreeDB Release Candidate 1 (Phase 78 Persistence Freeze)`
* **Branch:** `main`
* **Working Tree:** Clean (verified; 0 uncommitted changes prior to documentation updates)
* **Toolchain Information:**
  * `rustc 1.98.1 (48a229cea 2026-09-01)`
  * `cargo 1.98.1 (797e8a9bc 2026-08-05)`
  * Target: `x86_64-pc-windows-msvc` (Windows, stable Rust)

---

## 3. Clean-Checkout Reproducibility Results

A clean checkout was instantiated in an isolated Git worktree (`.clean-checkout`) directly from tag `v0.1.0-rc1` to verify independent build and test reproducibility without relying on existing development state:

```bash
git worktree add .clean-checkout v0.1.0-rc1
```

All quality gates and test suites were executed inside the clean checkout:

| Command Executed | Clean-Checkout Exit Code | Result | Duration |
|---|---|---|---|
| `cargo fmt --all -- --check` | 0 | 0 formatting discrepancies | < 1s |
| `cargo clippy --all-targets --all-features -- -D warnings` | 0 | 0 compiler or Clippy warnings | 28.97s (cold) |
| `cargo test --all` | 0 | **1,935 passed; 0 failed; 0 ignored** | ~5m (cold build + run) |

Following successful validation, the clean checkout worktree was removed via `git worktree remove --force .clean-checkout`.

---

## 4. Aggregate Test Counts & Binary Distribution

Across all 80 compiled test binaries, exact test results were recorded:

* **Passed:** 1,935
* **Failed:** 0
* **Ignored:** 0
* **Measured:** 0
* **Filtered out:** 0

### Persistence & Storage Test Distribution:
* `tests/backup_restore.rs`: **51 passed** (offline backup, manifest validation, journal publication, crash fault injection)
* `tests/transaction_atomicity.rs`: **34 passed** (commit/abort durability, rollback invisibility, failure injection)
* `tests/storage_consistency.rs`: **22 passed** (cross-layer invariant checks, freelist integrity, corruption detection, byte immutability)
* `tests/recovery_certification.rs`: **20 passed** (ARIES crash recovery, fuzzy checkpoints, dirty page replaying)
* `tests/transaction_isolation.rs`: **20 passed** (isolation levels, snapshot registry, write set tracking)
* `tests/mvcc_conflicts.rs`: **16 passed** (write-write conflict detection, serialization barriers)
* `tests/transaction.rs`: **15 passed** (read-your-writes, transaction lifecycle)
* `tests/mvcc_vacuum.rs`: **12 passed** (version pruning, freelist reclamation, active snapshot protection)
* `tests/mvcc_reference_model.rs`: **8 passed** (metamorphic oracle differential testing)
* `tests/mvcc_indexes.rs`: **7 passed** (secondary index snapshot visibility, unique constraints)
* `tests/mvcc_recovery.rs`: **5 passed** (version chain recovery, commit sequence monotonicity)
* Unit, Query, Optimizer, SQL, and Storage Suites: **1,725 passed**

---

## 5. Minimal End-to-End Workflow Validation

Both the public CLI and Rust library API were validated against clean, temporary databases:

### CLI Workflow Validation:
1. **Initialize Database**: `treedb init test_disposable.treedb` -> Initialized Page 0 Header and Page 1 Metadata (allocated pages: 2).
2. **Metadata Inspection**: `treedb info test_disposable.treedb` -> Format version 2, 4096-byte pages, Page count 2.
3. **Table Creation**: `treedb query test_disposable.treedb "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, balance INT);"` -> Created table 'users'.
4. **Row Insertion**: `treedb query test_disposable.treedb "INSERT INTO users VALUES (1, 'Alice', 100), (2, 'Bob', 250);"` -> Inserted 2 rows.
5. **Selection**: `treedb query test_disposable.treedb "SELECT id, name, balance FROM users ORDER BY id ASC;"` -> Returned Alice (100) and Bob (250).
6. **Update & Delete**: `UPDATE users SET balance = 150 WHERE id = 1;` and `DELETE FROM users WHERE id = 2;` -> Verified updated balance 150 and Bob removed.
7. **Clean Removal**: Verified safe deletion of `test_disposable.treedb` and companion `.wal` files.

### Rust Public API Workflow Validation:
```rust
use treedb::{TreeDb, Result};

let mut db = TreeDb::open("test.treedb", 64)?;
db.execute_statement("CREATE TABLE accounts (id INT PRIMARY KEY, owner TEXT, balance INT);")?;
db.execute_statement("INSERT INTO accounts VALUES (1, 'Alice', 1000);")?;
let res = db.execute_sql("SELECT owner, balance FROM accounts WHERE id = 1;")?;
assert_eq!(res.rows().len(), 1);
db.sync()?;
let report = db.verify_consistency()?;
assert!(report.is_healthy());
```

---

## 6. Backup & Restore Reproducibility

Exercised on disposable data through the public engine API:
1. **Quiescent Backup Creation**: `db.create_backup(backup_dir, &BackupConfig::default())` generated `treedb.manifest`, database image, and companion WAL.
2. **Pre-Publication Audit**: Manifest CRC32 and image byte hashes verified with `validate_backup`.
3. **Staged Publication**: Restored to a fresh destination path using `TreeDb::restore_backup`.
4. **Post-Restore Integrity**: Reopened restored database and verified data row contents match expected state.
5. **Artifact Cleanliness**: Verified zero `.tmp_restore_*`, `.bak_*`, or `.restore_journal` files survive completion.

---

## 7. CI & Developer Setup Findings

* `.github/workflows/ci.yml` was audited and updated to run `cargo test --all`, aligning CI with the documented local developer command.
* Toolchain requirements (stable Rust 1.80+) and lockfile (`Cargo.lock`) are fully specified and checked in.
* The repository requires zero native external dependencies beyond the Rust standard toolchain.

---

## 8. Operational Constraints & Known Limitations

The release candidate is certified for use under the following explicit operational boundaries:

1. **Single-Writer Exclusivity**:
   * TreeDB relies on single-process ownership. Multi-process concurrent write access without external locking is unsupported and will cause corruption.
2. **Offline Quiescent Backups Only**:
   * Backups must be performed when no write transactions are active. Online hot backup during active mutations is unsupported.
3. **No Heuristic Corruption Repair**:
   * Corrupted headers, page checksum failures, or torn pages fail loudly. Heuristic guessing or silent data alteration is intentionally omitted to prevent data loss.
4. **Filesystem Synchronization Assumptions**:
   * Durability guarantees assume the host OS and storage subsystem honor `File::sync_all()` (`FlushFileBuffers` on Windows, `fsync` on POSIX) and provide journaled directory metadata for atomic single-file renames.

---

## 9. Final Release Recommendation

TreeDB v0.1.0-rc1 is fully verified, reproducible from a clean checkout, and ready for release candidate distribution.
