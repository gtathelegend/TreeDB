# TreeDB Release Process & Operational Handoff Guide

## Purpose & Scope

This document specifies the exact, reproducible procedures for:
1. **Workflow A**: Staging and verifying a **Release Candidate** (`v0.1.0-rc2`) without publishing to crates.io.
2. **Workflow B**: Publishing an authorized **Stable General Availability Release** to crates.io.

> **CRITICAL RULE ON CARGO VERSIONING**:
> **Git tag names do NOT determine the package version published by Cargo.** Cargo determines the published package name and version solely from `Cargo.toml` (`[package] version = "..."`). Because `Cargo.toml` currently declares `version = "0.1.0"`, running `cargo publish` at any time will publish the permanent, immutable stable release `0.1.0` to crates.io, regardless of whether a Git tag is named `v0.1.0-rc2`. Therefore, the Release Candidate workflow strictly forbids invoking `cargo publish`.

---

## 1. Prerequisites & Release Baseline

Before initiating any release workflow, ensure the following prerequisites are met:

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
* **Expected Total**: **2,105 tests** across **91 test-bearing binaries** (48 library unit tests in `src/lib.rs` + 2,057 integration tests across 90 integration test suites in `tests/`).
* Reconcile actual test counts against `README.md`, `CHANGELOG.md`, and `docs/RELEASE_READINESS.md`.

---

## 3. Package Verification & Artifact Provenance

Package the crate using Cargo's hermetic packaging workflow:

```bash
# 1. Inspect the list of files to be packaged
cargo package --list

# 2. Build and verify the crate archive in isolation
cargo package
```

### 3.1 Crate Archive Audit
Verify that `target/package/treedb-<version>.crate` satisfies repository hygiene:
* Contains only source files (`src/`), tests (`tests/`), benchmarks (`benches/`), licenses (`LICENSE-MIT`, `LICENSE-APACHE`), `README.md`, `CHANGELOG.md`, and `docs/`.
* Excludes development workflows (`.github/`), local paths, temporary files (`*.treedb`, `*.treedb.wal`, `*.restore_journal`), and target directories.
* The package compiles and verifies hermetically within `target/package/`.

### 3.2 Fresh Artifact Provenance & SHA-256 Checksum
The cryptographic checksum **must always be recalculated freshly** for the exact `.crate` artifact being evaluated. Never assume or reuse a previously recorded checksum after modifying source files, editing documentation, or rebuilding:

* **On POSIX (Linux/macOS)**:
  ```bash
  sha256sum target/package/treedb-0.1.0.crate
  ```
* **On Windows (PowerShell)**:
  ```powershell
  Get-FileHash -Algorithm SHA256 target/package/treedb-0.1.0.crate | Format-List
  ```

Record the freshly computed SHA-256 hash in release records. Note that a checksum identifies a specific generated artifact; it does not constitute an assertion of multi-environment reproducible build identity unless independently compiled and compared across clean environments.

---

## 4. Workflow A: Release Candidate Staging (`v0.1.0-rc2`)

The Release Candidate workflow validates stability and creates an annotated Git candidate tag without publishing to crates.io.

### Step A.1: Ensure Quality Gates & Artifact Verification Pass
Confirm that Sections 1, 2, and 3 have been executed successfully on the target commit.

### Step A.2: Create Annotated Candidate Tag
Create a distinct pre-release candidate tag pointing to the verified commit:
```bash
git tag -a v0.1.0-rc2 -m "release candidate: TreeDB v0.1.0-rc2"
```

### Step A.3: Verify Candidate Tag Target (Commit Peeling)
Annotated tags create their own tag objects. Verify that the tag resolves to the exact underlying commit:
```bash
test "$(git rev-parse "v0.1.0-rc2^{commit}")" = "$(git rev-parse HEAD)" && echo "TAG TARGET VERIFIED" || echo "TAG TARGET MISMATCH"
```

### Step A.4: Push Candidate Tag to Upstream
Push the candidate branch and **only the specific candidate tag**:
```bash
git push origin main
git push origin v0.1.0-rc2
```
Verify the remote tag target:
```bash
git ls-remote --tags origin v0.1.0-rc2
```

> **DO NOT PUBLISH TO CRATES.IO**:
> Candidate staging stops here. Do **not** run `cargo publish`. The generated `.crate` artifact remains available locally or for internal test distributions.

---

## 5. Workflow B: Stable General Availability Release Publication

The Stable Publication workflow is executed only when project stakeholders have formally authorized publishing a permanent, immutable release to crates.io.

### 5.1 Authorization & Governance Prerequisites
Before publishing:
1. **Explicit Human Sign-Off**: Do not publish based on automated triggers.
2. **Historical Tag Resolution**: In this repository, the tag `v0.1.0` historically exists at commit `2f912c2e24b095976a6a5d6340a6b536b2707c9a`. Deleting a local tag (`git tag -d`) does not remove remote tags or update other clones. If releasing stable `0.1.0`, stakeholders must explicitly agree on whether to resolve the historical tag or release under a clean version (e.g. `0.1.1`).
3. **Commit & Artifact Confirmation**: Confirm that `git rev-parse HEAD` matches the audited commit and that the artifact checksum matches the audited artifact.

### 5.2 Authoritative Registry Verification
Package versions on crates.io are **permanently immutable**. Once published, a version can never be overwritten, modified, or re-uploaded.

Query the crates.io public API directly (do not rely solely on fuzzy `cargo search`):
```bash
# Returns HTTP 200 if 0.1.0 already exists; HTTP 404 if available for publication
curl -s -o /dev/null -w "%{http_code}\n" https://crates.io/api/v1/crates/treedb/0.1.0
```
* **If HTTP 200**: Version `0.1.0` is already published. You cannot overwrite it. Increment `version` in `Cargo.toml` before proceeding.
* **If HTTP 404**: Version `0.1.0` is available for publication.

### 5.3 Step-by-Step Stable Publication Sequence

#### Step B.1: Crates.io Dry-Run
Validate packaging and API token permissions without uploading:
```bash
cargo publish --dry-run
```

#### Step B.2: Publish Crate to Crates.io
Execute publication:
```bash
cargo publish
```

#### Step B.3: Create & Verify Stable Release Tag
Create an annotated or signed release tag for the published version:
```bash
# Example for an authorized release tag:
git tag -a <STABLE_TAG> -m "release: TreeDB <VERSION>"

# Verify the tag peels to the current HEAD commit:
test "$(git rev-parse "<STABLE_TAG>^{commit}")" = "$(git rev-parse HEAD)" && echo "TAG TARGET VERIFIED" || echo "TAG TARGET MISMATCH"
```

#### Step B.4: Push Specific Stable Tag Only
Push the commit and **only the authorized stable release tag**:
```bash
git push origin main
git push origin <STABLE_TAG>

# Verify remote target:
git ls-remote --tags origin <STABLE_TAG>
```

> **WARNING ON `git push --tags`**:
> **NEVER use `git push origin --tags`**. A blanket tag push uploads all local tags, which risks pushing experimental or historical local tags to the public upstream repository. Always specify the exact tag name.

---

## 6. Package Metadata & Prerelease Versions Note

* **Current Status**: `Cargo.toml` declares `version = "0.1.0"`. This version must not be changed arbitrarily to match Git pre-release candidate tags.
* **Prerelease Crate Publishing**: If the project ever decides to publish a public pre-release crate to crates.io (e.g., allowing downstream crates to depend on `treedb = "0.1.0-rc.2"`), `Cargo.toml` must be explicitly edited to `version = "0.1.0-rc.2"` (SemVer pre-release syntax) with stakeholder authorization before running `cargo publish`. Otherwise, candidate staging must remain strictly non-publishing.

---

## 7. Rollback & Post-Release Considerations

In the event that a critical defect is discovered immediately following publication:

1. **Yanking on Crates.io**:
   Yank the affected release to prevent new projects from resolving it as a dependency:
   ```bash
   cargo yank --version <VERSION>
   ```
   *(Note: Yanking does not delete existing cargo locks or break projects that already depend on the version.)*
2. **Hotfix Protocol**:
   * Branch from the tagged release commit: `git checkout -b hotfix/<NEW_VERSION> <TAG_NAME>`.
   * Apply targeted regression fix and verify with a named regression test.
   * Bump `version = "<NEW_VERSION>"` in `Cargo.toml`.
   * Follow the complete quality-gate process before releasing `<NEW_VERSION>`.

---

## 8. Operational Boundaries & Disclosures Checklist

Ensure all release communications retain the following explicit disclosures:
* **Single-Writer Operating Model**: Multi-reader MVCC snapshots are supported; concurrent multi-process writes are unsupported.
* **Single-File WAL**: Uses a single `.wal` file per database; multi-segment log rotation is not implemented in v0.1.0.
* **Drop vs. Sync Semantics**: Dropping `TreeDb` does not flush resident dirty pages; durability relies on replaying committed WAL records on restart. Explicit `db.sync()?` or `db.checkpoint()?` is required for clean non-recovery shutdown.
* **Hardware & Filesystem Assumptions**: Relies on host storage controllers honoring `File::sync_all()`. Does not implement double-write page buffering; catastrophic power loss mid-sector write requires backup restoration.
