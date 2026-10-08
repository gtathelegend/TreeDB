//! Phase 68 Integration Tests: MVCC Crash Recovery, Durable Commit Sequences & Version Preservation.
//!
//! Validates:
//! 1. Logical commit sequences (`max_commit_seq`) durability across crash and recovery.
//! 2. Uncommitted versions from active transactions rolling back and remaining invisible after recovery.
//! 3. Checkpointing preserving durable commit sequences and version chain heads.
//! 4. Multi-version record chain integrity (v1 -> v2 -> v3) preserved and traversable across reopen.
//! 5. Monotonic commit sequence advancement across multiple reopen cycles without collisions or regression.

use tempfile::tempdir;
use treedb::engine::isolation::{CommitSeq, Snapshot, SnapshotId};
use treedb::TreeDb;

#[test]
fn test_durable_commit_sequence_survives_crash_recovery() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("durable_seq.db");

    let saved_seq;
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        let mut t1 = db.begin().unwrap();
        t1.put(1, b"val1").unwrap();
        t1.commit().unwrap();

        let mut t2 = db.begin().unwrap();
        t2.put(2, b"val2").unwrap();
        t2.commit().unwrap();

        saved_seq = db.buffer_pool().durable_max_commit_seq();
        assert!(saved_seq >= 2);
    }

    // Reopen and verify durable sequence
    {
        let db = TreeDb::open(&db_path, 64).unwrap();
        let recovered_seq = db.buffer_pool().durable_max_commit_seq();
        assert_eq!(recovered_seq, saved_seq);
        assert_eq!(db.current_commit_seq(), CommitSeq(saved_seq + 1));
    }
}

#[test]
fn test_uncommitted_versions_invisible_after_crash() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("uncommitted_crash.db");

    // Phase 1: commit key 1, leave key 2 uncommitted, simulate crash
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();
        let mut t1 = db.begin().unwrap();
        t1.put(1, b"committed_val").unwrap();
        t1.commit().unwrap();

        // Active uncommitted transaction
        let mut t2 = db.begin().unwrap();
        t2.put(2, b"uncommitted_val").unwrap();
        t2.put(1, b"uncommitted_overwrite").unwrap();
        // Drop t2 and db without commit to simulate abrupt process termination
        std::mem::forget(t2);
    }

    // Phase 2: reopen and run ARIES recovery
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        let mut txn = db.begin().unwrap();
        // Key 1 must still have its committed value
        assert_eq!(txn.get(1).unwrap(), Some(b"committed_val".to_vec()));
        // Key 2 was never committed -> must be absent
        assert_eq!(txn.get(2).unwrap(), None);
        txn.commit().unwrap();
    }
}

#[test]
fn test_checkpoint_preserves_durable_commit_seq_and_versions() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("checkpoint_mvcc.db");

    let pre_checkpoint_seq;
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();

        let mut t1 = db.begin().unwrap();
        t1.put(10, b"v1_pre_checkpoint").unwrap();
        t1.commit().unwrap();

        // Checkpoint flushes dirty pages and updates header
        db.checkpoint_and_truncate_wal().unwrap();
        pre_checkpoint_seq = db.buffer_pool().durable_max_commit_seq();

        // Post-checkpoint mutation
        let mut t2 = db.begin().unwrap();
        t2.put(10, b"v2_post_checkpoint").unwrap();
        t2.put(20, b"new_post_checkpoint").unwrap();
        t2.commit().unwrap();
    }

    // Reopen and verify
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();
        assert!(db.buffer_pool().durable_max_commit_seq() > pre_checkpoint_seq);

        let mut txn = db.begin().unwrap();
        assert_eq!(txn.get(10).unwrap(), Some(b"v2_post_checkpoint".to_vec()));
        assert_eq!(txn.get(20).unwrap(), Some(b"new_post_checkpoint".to_vec()));
        txn.commit().unwrap();
    }
}

#[test]
fn test_version_chain_integrity_across_crash_recovery() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("chain_integrity.db");

    let s1_seq;
    let s2_seq;
    {
        let mut db = TreeDb::create(&db_path, 64).unwrap();

        let mut t1 = db.begin().unwrap();
        t1.put(100, b"payload_v1").unwrap();
        t1.commit().unwrap();
        s1_seq = db.buffer_pool().durable_max_commit_seq();

        let mut t2 = db.begin().unwrap();
        t2.put(100, b"payload_v2").unwrap();
        t2.commit().unwrap();
        s2_seq = db.buffer_pool().durable_max_commit_seq();

        let mut t3 = db.begin().unwrap();
        t3.put(100, b"payload_v3").unwrap();
        t3.commit().unwrap();
    }

    // Reopen database and verify full version chain is accessible using historical snapshots
    {
        let mut db = TreeDb::open(&db_path, 64).unwrap();

        // As of S1 -> payload_v1
        let mut txn_s1 = db.begin().unwrap();
        txn_s1.set_snapshot(Snapshot::new(SnapshotId(1), CommitSeq(s1_seq)));
        assert_eq!(txn_s1.get(100).unwrap(), Some(b"payload_v1".to_vec()));
        txn_s1.commit().unwrap();

        // As of S2 -> payload_v2
        let mut txn_s2 = db.begin().unwrap();
        txn_s2.set_snapshot(Snapshot::new(SnapshotId(2), CommitSeq(s2_seq)));
        assert_eq!(txn_s2.get(100).unwrap(), Some(b"payload_v2".to_vec()));
        txn_s2.commit().unwrap();

        // Current -> payload_v3
        let mut txn_current = db.begin().unwrap();
        assert_eq!(txn_current.get(100).unwrap(), Some(b"payload_v3".to_vec()));
        txn_current.commit().unwrap();
    }
}

#[test]
fn test_multi_round_reopen_monotonicity() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("multi_reopen.db");

    let mut last_seq = 0u64;

    for round in 1..=5 {
        let mut db = if round == 1 {
            TreeDb::create(&db_path, 64).unwrap()
        } else {
            TreeDb::open(&db_path, 64).unwrap()
        };

        let current_seq = db.buffer_pool().durable_max_commit_seq();
        assert!(
            current_seq >= last_seq,
            "commit sequence regressed across reopen"
        );

        let mut txn = db.begin().unwrap();
        txn.put(round as u64, format!("round_{round}").as_bytes())
            .unwrap();
        txn.commit().unwrap();

        last_seq = db.buffer_pool().durable_max_commit_seq();
        assert!(
            last_seq > current_seq,
            "commit sequence failed to advance on commit"
        );
    }
}
