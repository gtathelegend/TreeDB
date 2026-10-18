# TreeDB Release Process & Operational Handoff Guide

## Purpose & Scope

This document specifies the exact, reproducible procedure for cutting, verifying, tagging, and publishing official releases of **TreeDB**.
This document outlines the **instructions for release execution**. Preparation of release artifacts does **not** imply that a release tag has been created or published. All tagging and publication actions require explicit human authorization.

---

## 1. Prerequisites & Release Baseline

Before initiating a release workflow, ensure the following prerequisites are met:

1. **Clean Working Tree**:
   Verify that no unstaged changes, untracked files, or temporary build remnants exist:
   ```bash
   git status
   # Must output: "nothing to commit, working tree clean"
   ```
2. **Target Branch & Commit**:
   Confirm that release preparation is on the primary release branch (`main`) at the verified release candidate commit:
   ```bash
   git rev-parse HEAD
   ```
3. **Rust Toolchain**:
   Verify the stable Rust toolchain:
   ```bash
   rustc --version
   cargo --version
   ```

---

## 2. Pre-Release Quality Gates

Execute the full suite of automated quality gates. Every command must exit with code `0`:

```bash
# 1. Compilation & Syntax Check across all targets (lib, bins, tests, benches)
cargo check --all-targets

# 2. Code Formatting Verification
cargo fmt --all -- --check

# 3. Clippy Strict Linting (zero warnings allowed)
cargo clippy --all-targets --all-features -- -D warnings

# 4. Workspace Documentation Compilation (zero rustdoc warnings allowed)
cargo doc --no-deps

# 5. Full Test Suite Execution (Unit + Integration Tests)
cargo test -j 2
```

### 2.1 Test Count & Binaries Verification
Verify that `cargo test -j 2` executes and passes all test cases with **0 failures, 0 warnings, and 0 ignored tests**:
* **Expected Total**: **2,105 tests** across **91 test-bearing binaries** (48 library unit tests + 2,057 integration tests across 90 integration test suites).
* Reconcile actual test counts against `README.md`, `CHANGELOG.md`, and `docs/RELEASE_READINESS.md`.

---

## 3. Package Verification & Artifact Generation

Package the crate using Cargo's hermetic packaging workflow:

```bash
# 1. Inspect the list of files to be packaged
cargo package --list

# 2. Build and verify the crate archive in isolation
cargo package
```

### 3.1 Crate Archive Audit
Verify that `target/package/treedb-<version>.crate` satisfies repository hygiene:
* Contains only source files (`src/`), tests (`tests/`), benchmarks (`benches/`), licenses (`LICENSE-MIT`, `LICENSE-APACHE`), `README.md`, and `docs/`.
* Excludes development workflows (`.github/`), local paths, temporary files (`*.treedb`, `*.treedb.wal`, `*.restore_journal`), and target directories.
* The package compiles and verifies hermetically within `target/package/`.

### 3.2 Artifact Provenance & SHA-256 Checksum
Generate the cryptographic checksum of the packaged `.crate` artifact:

* **On POSIX (Linux/macOS)**:
  ```bash
  sha256sum target/package/treedb-0.1.0.crate
  ```
* **On Windows (PowerShell)**:
  ```powershell
  Get-FileHash -Algorithm SHA256 target/package/treedb-0.1.0.crate | Format-List
  ```

Record the generated SHA-256 hash in release notes and handoff records. Note that a checksum identifies a specific generated artifact; it does not constitute an assertion of multi-environment reproducible build identity unless independently compiled and compared across clean environments.

---

## 4. Tagging Procedure

Tagging must occur only after all quality gates pass and artifact checksums are verified.

### 4.1 Tag Creation (Annotated or Signed)
* **Annotated Tag**:
  ```bash
  git tag -a v0.1.0 -m "release: TreeDB v0.1.0"
  ```
* **GPG/SSH Signed Tag (Recommended for Production)**:
  ```bash
  git tag -s v0.1.0 -m "release: TreeDB v0.1.0"
  ```

### 4.2 Post-Tag Verification
Verify that the tag correctly references the intended commit:
```bash
git rev-parse v0.1.0
git log -1 v0.1.0
```
Confirm that `git describe --tags` matches `v0.1.0`.

---

## 5. Publication & Distribution

> **IMPORTANT**: Publication to crates.io is permanent and irreversible. Crates cannot be deleted, only yanked. Do not execute publication without explicit stakeholder sign-off.

### 5.1 Crates.io Dry-Run
Execute a dry-run publish to test authentication and registry packaging rules without uploading:
```bash
cargo publish --dry-run
```

### 5.2 Official Publication
When authorized:
```bash
cargo publish
```

### 5.3 Pushing Tags & Commits
Push the verified commit and release tag to the upstream remote repository:
```bash
git push origin main
git push origin v0.1.0
```

---

## 6. Rollback & Post-Release Considerations

In the event that an unforeseen critical defect is discovered immediately following publication:
1. **Yanking on Crates.io**:
   Yank the affected release to prevent new projects from depending on it:
   ```bash
   cargo yank --version 0.1.0
   ```
   *(Note: Yanking does not delete existing cargo locks or break projects that already depend on the version.)*
2. **Hotfix Protocol**:
   * Branch from the tagged commit: `git checkout -b hotfix/0.1.1 v0.1.0`.
   * Apply targeted regression fix and regression test.
   * Bump `version = "0.1.1"` in `Cargo.toml`.
   * Follow the complete quality-gate process before releasing `v0.1.1`.

---

## 7. Operational Boundaries & Disclosures Checklist

Ensure all release communications and documentation retain the following explicit disclosures:
* **Single-Writer Operating Model**: Multi-reader MVCC snapshots are supported; concurrent multi-process writes are unsupported.
* **Single-File WAL**: Uses a single `.wal` file per database; multi-segment log rotation is not implemented in v0.1.0.
* **Drop vs. Sync Semantics**: Dropping `TreeDb` does not flush resident dirty pages; durability relies on replaying committed WAL records on restart. Explicit `db.sync()?` or `db.checkpoint()?` is required for clean non-recovery shutdown.
* **Hardware & Filesystem Assumptions**: Relies on host storage controllers honoring `File::sync_all()`. Does not implement double-write page buffering; catastrophic power loss mid-sector write requires backup restoration.
