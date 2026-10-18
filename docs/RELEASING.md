# TreeDB Release Process & Operational Handoff Guide

## Purpose & Scope

This document specifies the exact, reproducible procedure for verifying, tagging, packaging, and publishing official releases of **TreeDB**.
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
* **Expected Total**: **2,105 tests** across **91 test-bearing binaries** (48 library unit tests in `src/lib.rs` + 2,057 integration tests across 90 integration test suites in `tests/`).
* Reconcile actual test counts against `README.md`, `CHANGELOG.md`, and `docs/RELEASE_READINESS.md`.

---

## 3. Authoritative Registry Verification & Immutability

Package versions on crates.io are **permanently immutable**. Once a version is published, it can never be overwritten, modified, or re-uploaded with different contents—even if yanked.

> **CRITICAL**: Inspecting registry state is strictly a read-only verification step. It does not constitute or imply authorization to publish.

Do not rely solely on `cargo search treedb` to establish whether a specific version exists; `cargo search` is a fuzzy, cached keyword index that does not guarantee real-time per-version precision.

### 3.1 Authoritative Registry Lookup
Before building release artifacts or creating release tags, check the exact version on crates.io:

* **Authoritative API Query for Target Version**:
  ```bash
  # Returns HTTP 200 with metadata if version 0.1.0 exists; HTTP 404 if not yet published
  curl -s -o /dev/null -w "%{http_code}\n" https://crates.io/api/v1/crates/treedb/0.1.0
  ```
* **Listing All Published Versions**:
  ```bash
  curl -s https://crates.io/api/v1/crates/treedb | jq '.versions[].num'
  ```
* **Authoritative Web Inspection**:
  Visit `https://crates.io/crates/treedb` in a browser.

### 3.2 Registry Disposition Rules
* **If the target version (e.g. `0.1.0`) is already present on crates.io**:
  Cargo will reject any attempt to re-upload under this version. If the published artifact differs from the current commit, the version in `Cargo.toml` must be incremented (e.g., to `0.1.1` or `0.2.0`) before proceeding.
* **If the target version is not yet published**:
  Proceed to packaging and artifact verification.

---

## 4. Package Verification & Artifact Provenance

Package the crate using Cargo's hermetic packaging workflow:

```bash
# 1. Inspect the list of files to be packaged
cargo package --list

# 2. Build and verify the crate archive in isolation
cargo package
```

### 4.1 Crate Archive Audit
Verify that `target/package/treedb-<version>.crate` satisfies repository hygiene:
* Contains only source files (`src/`), tests (`tests/`), benchmarks (`benches/`), licenses (`LICENSE-MIT`, `LICENSE-APACHE`), `README.md`, `CHANGELOG.md`, and `docs/`.
* Excludes development workflows (`.github/`), local paths, temporary files (`*.treedb`, `*.treedb.wal`, `*.restore_journal`), and target directories.
* The package compiles and verifies hermetically within `target/package/`.

### 4.2 Fresh Artifact Provenance & SHA-256 Checksum
The cryptographic checksum **must always be recalculated freshly** for the exact `.crate` artifact being released. Never assume or reuse a previously recorded checksum after modifying source files, editing documentation, or rebuilding:

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

## 5. Tag Semantics, Safety & Target Verification

Tagging must occur only after all quality gates pass, registry state is verified, and artifact checksums are recorded.

### 5.1 Historical Tag Status & Tag Preservation
* In this repository, the tag `v0.1.0` historically exists at commit `2f912c2e24b095976a6a5d6340a6b536b2707c9a`.
* **PRESERVE THE HISTORICAL TAG**:
  * Do **not** delete or recreate `v0.1.0` as the normal release procedure.
  * Deleting a local tag (`git tag -d v0.1.0`) **does not remove the tag from any remote repository** or update other developers' clones. Recreating an existing tag locally creates divergent tag histories, causes `git fetch` rejection errors, and violates release immutability.
  * **DO NOT FORCE-MOVE TAGS (`git tag -f`)**. Force-moving release tags is strictly prohibited.
* **Recommended Tagging Policy**:
  * For a release candidate evaluated on the current HEAD, use a distinct pre-release candidate tag:
    ```bash
    git tag -a v0.1.0-rc2 -m "release candidate: TreeDB v0.1.0-rc2"
    ```
  * For an official general availability release, bump the version in `Cargo.toml` to a clean, non-conflicting version (e.g. `0.1.1` or `0.2.0`), or only re-target `v0.1.0` if an authorized release governance decision explicitly resolves the historical tag conflict across all local and remote repositories.

### 5.2 Tag Creation (Annotated or Cryptographically Signed)
* **Annotated Tag**:
  ```bash
  git tag -a <TAG_NAME> -m "release: TreeDB <VERSION>"
  ```
* **GPG/SSH Signed Tag (Recommended for Production)**:
  ```bash
  git tag -s <TAG_NAME> -m "release: TreeDB <VERSION>"
  ```

### 5.3 Post-Tag Target Verification (Peeling to Commit)
Annotated and signed tags in Git create their own unique tag objects with their own distinct object hashes. **Do not compare the raw annotated tag hash directly with HEAD.**

Always peel the tag to its underlying commit object using the `^{commit}` syntax, and verify that it matches `git rev-parse HEAD` exactly:

```bash
# 1. Resolve the underlying commit hash of the tag
git rev-parse "<TAG_NAME>^{commit}"

# 2. Resolve the current HEAD commit hash
git rev-parse HEAD

# 3. Assert that both hashes are identical:
test "$(git rev-parse "<TAG_NAME>^{commit}")" = "$(git rev-parse HEAD)" && echo "TAG TARGET VERIFIED" || echo "TAG TARGET MISMATCH"

# 4. Confirm tag message and commit metadata
git log -1 "<TAG_NAME>^{commit}"
git describe --tags --exact-match "<TAG_NAME>^{commit}"
```

---

## 6. Publication & Distribution Sequence

> **CRITICAL**: Publication to crates.io is permanent and irreversible. Do not execute publication without explicit stakeholder authorization.

Execute publication following this strict, sequential pipeline:

### Step 1: Crates.io Dry-Run
Execute a dry-run publish to validate registry packaging and API credentials without uploading:
```bash
cargo publish --dry-run
```

### Step 2: Official Crate Publication
When authorized:
```bash
cargo publish
```

### Step 3: Explicit Single-Tag Remote Push
Push the verified commit and **only the explicitly authorized release tag** to the remote repository:
```bash
# Push the release branch
git push origin main

# Push ONLY the specific release tag (DO NOT USE `git push origin --tags`)
git push origin <TAG_NAME>
```

> **WARNING ON `git push --tags`**:
> **NEVER use `git push origin --tags`**. A blanket tag push uploads all local tags in your repository, which may inadvertently push experimental, obsolete, or unverified local tags to the public upstream remote. Always specify the exact tag name: `git push origin <TAG_NAME>`.

### Step 4: Verify Remote Tag Target
Confirm that the upstream remote repository resolves the tag to the expected commit:
```bash
git ls-remote --tags origin <TAG_NAME>
```

---

## 7. Rollback & Post-Release Considerations

In the event that an unforeseen critical defect is discovered immediately following publication:

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
