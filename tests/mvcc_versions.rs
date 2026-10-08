//! Phase 68 Integration Tests: MVCC Record Versioning, Version Chains & Metadata Storage.
//!
//! Validates:
//! 1. Version metadata header layout, serialization, and deserialization roundtrips.
//! 2. Version metadata semantic invariant validations (created_at <= deleted_at, xmax requirement).
//! 3. Unsupported version format and buffer bounds enforcement.
//! 4. Physical version chaining via `prev_version` pointers across multiple updates.
//! 5. Version chain cycle detection preventing infinite loops.
//! 6. Version chain maximum depth limit enforcement (`MAX_VERSION_CHAIN_DEPTH`).
//! 7. In-place version metadata updates for both inline and overflow records.

use tempfile::tempdir;
use treedb::engine::isolation::{CommitSeq, Snapshot, SnapshotId};
use treedb::error::Error;
use treedb::index::RecordId;
use treedb::storage::buffer_pool::BufferPool;
use treedb::storage::page_manager::PageManager;
use treedb::storage::record_store::{
    RecordStore, VersionMetadata, MAX_VERSION_CHAIN_DEPTH, MVCC_VERSION_FORMAT, VERSION_HEADER_SIZE,
};
use treedb::wal::log_manager::LogManager;
use treedb::wal::lsn::TxnId;

fn create_buffer_pool(name: &str) -> (BufferPool, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("{name}.db"));
    let wal_path = dir.path().join(format!("{name}.db.wal"));

    let page_manager = PageManager::create(&db_path).unwrap();
    let log_manager = LogManager::create(&wal_path).unwrap();
    let pool = BufferPool::with_wal(64, page_manager, log_manager).unwrap();
    (pool, dir)
}

#[test]
fn test_version_metadata_serialization_roundtrip() {
    // 1. Full metadata with all fields populated
    let meta_full = VersionMetadata {
        format_version: MVCC_VERSION_FORMAT,
        xmin: TxnId(42),
        xmax: Some(TxnId(84)),
        created_at: Some(CommitSeq(100)),
        deleted_at: Some(CommitSeq(200)),
        prev_version: Some(RecordId::new(5, 12)),
    };

    let payload = b"user_record_payload_bytes";
    let mut buf = vec![0u8; VERSION_HEADER_SIZE + payload.len()];
    meta_full.serialize_into(&mut buf[..VERSION_HEADER_SIZE]);
    buf[VERSION_HEADER_SIZE..].copy_from_slice(payload);

    let (deser_meta, deser_payload) = VersionMetadata::deserialize(&buf).unwrap();
    assert_eq!(meta_full, deser_meta);
    assert_eq!(payload, deser_payload);

    // 2. Minimal metadata (uncommitted, no previous version, no deletion)
    let meta_min = VersionMetadata::new(TxnId(1), None);
    assert_eq!(meta_min.format_version, MVCC_VERSION_FORMAT);
    assert_eq!(meta_min.xmin, TxnId(1));
    assert_eq!(meta_min.xmax, None);
    assert_eq!(meta_min.created_at, None);
    assert_eq!(meta_min.deleted_at, None);
    assert_eq!(meta_min.prev_version, None);

    let mut buf_min = vec![0u8; VERSION_HEADER_SIZE];
    meta_min.serialize_into(&mut buf_min);
    let (deser_min, deser_min_payload) = VersionMetadata::deserialize(&buf_min).unwrap();
    assert_eq!(meta_min, deser_min);
    assert!(deser_min_payload.is_empty());
}

#[test]
fn test_version_metadata_semantic_invariants() {
    // Invariant 1: created_at > deleted_at is illegal
    let invalid_seq = VersionMetadata {
        format_version: MVCC_VERSION_FORMAT,
        xmin: TxnId(1),
        xmax: Some(TxnId(2)),
        created_at: Some(CommitSeq(500)),
        deleted_at: Some(CommitSeq(100)),
        prev_version: None,
    };
    let mut buf = vec![0u8; VERSION_HEADER_SIZE];
    invalid_seq.serialize_into(&mut buf);
    match VersionMetadata::deserialize(&buf) {
        Err(Error::InvalidVersionMetadata(msg)) => {
            assert!(msg.contains("exceeds deletion commit sequence"));
        }
        other => panic!("expected InvalidVersionMetadata, got {other:?}"),
    }

    // Invariant 2: deleted_at without xmax is illegal
    let invalid_deleted_no_xmax = VersionMetadata {
        format_version: MVCC_VERSION_FORMAT,
        xmin: TxnId(1),
        xmax: None,
        created_at: Some(CommitSeq(50)),
        deleted_at: Some(CommitSeq(100)),
        prev_version: None,
    };
    invalid_deleted_no_xmax.serialize_into(&mut buf);
    match VersionMetadata::deserialize(&buf) {
        Err(Error::InvalidVersionMetadata(msg)) => {
            assert!(msg.contains("without deleting transaction identity"));
        }
        other => panic!("expected InvalidVersionMetadata, got {other:?}"),
    }

    // Invariant 3: Unsupported format version
    buf[0..2].copy_from_slice(&99u16.to_le_bytes());
    match VersionMetadata::deserialize(&buf) {
        Err(Error::InvalidVersionMetadata(msg)) => {
            assert!(msg.contains("unsupported version format"));
        }
        other => panic!("expected InvalidVersionMetadata with unsupported format, got {other:?}"),
    }

    // Invariant 4: Buffer smaller than VERSION_HEADER_SIZE
    let short_buf = vec![0u8; VERSION_HEADER_SIZE - 1];
    match VersionMetadata::deserialize(&short_buf) {
        Err(Error::InvalidVersionMetadata(msg)) => {
            assert!(msg.contains("smaller than version header size"));
        }
        other => panic!("expected InvalidVersionMetadata with short buffer, got {other:?}"),
    }
}

#[test]
fn test_version_chain_creation_and_traversal() {
    let (mut pool, _dir) = create_buffer_pool("chain_creation");
    let mut store = RecordStore::new();

    // v1: initial insert
    let meta1 = VersionMetadata::new_committed(TxnId(1), CommitSeq(1), None);
    let rid1 = store
        .insert_version(&mut pool, &meta1, b"version_1")
        .unwrap();

    // v2: update v1
    let mut meta1_updated = meta1;
    meta1_updated.xmax = Some(TxnId(2));
    meta1_updated.deleted_at = Some(CommitSeq(2));
    store
        .update_version_metadata(&mut pool, rid1, &meta1_updated)
        .unwrap();

    let meta2 = VersionMetadata::new_committed(TxnId(2), CommitSeq(2), Some(rid1));
    let rid2 = store
        .insert_version(&mut pool, &meta2, b"version_2")
        .unwrap();

    // v3: update v2
    let mut meta2_updated = meta2;
    meta2_updated.xmax = Some(TxnId(3));
    meta2_updated.deleted_at = Some(CommitSeq(3));
    store
        .update_version_metadata(&mut pool, rid2, &meta2_updated)
        .unwrap();

    let meta3 = VersionMetadata::new_committed(TxnId(3), CommitSeq(3), Some(rid2));
    let rid3 = store
        .insert_version(&mut pool, &meta3, b"version_3")
        .unwrap();

    // Verify raw chain links
    let (read_meta3, payload3) = store.read_version(&mut pool, rid3).unwrap().unwrap();
    assert_eq!(read_meta3.prev_version, Some(rid2));
    assert_eq!(payload3, b"version_3");

    let (read_meta2, payload2) = store.read_version(&mut pool, rid2).unwrap().unwrap();
    assert_eq!(read_meta2.prev_version, Some(rid1));
    assert_eq!(payload2, b"version_2");

    let (read_meta1, payload1) = store.read_version(&mut pool, rid1).unwrap().unwrap();
    assert_eq!(read_meta1.prev_version, None);
    assert_eq!(payload1, b"version_1");

    // Traversal as of snapshot horizons:
    // As of CommitSeq(1): sees v1
    let snap1 = Snapshot::new(SnapshotId(1), CommitSeq(1));
    let vis1 = store
        .read_visible_payload(&mut pool, rid3, None, &snap1)
        .unwrap()
        .unwrap();
    assert_eq!(vis1, b"version_1");

    // As of CommitSeq(2): sees v2
    let snap2 = Snapshot::new(SnapshotId(2), CommitSeq(2));
    let vis2 = store
        .read_visible_payload(&mut pool, rid3, None, &snap2)
        .unwrap()
        .unwrap();
    assert_eq!(vis2, b"version_2");

    // As of CommitSeq(3): sees v3
    let snap3 = Snapshot::new(SnapshotId(3), CommitSeq(3));
    let vis3 = store
        .read_visible_payload(&mut pool, rid3, None, &snap3)
        .unwrap()
        .unwrap();
    assert_eq!(vis3, b"version_3");
}

#[test]
fn test_version_chain_cycle_detection() {
    let (mut pool, _dir) = create_buffer_pool("chain_cycle");
    let mut store = RecordStore::new();

    // Create v1 pointing to dummy rid
    let meta1 = VersionMetadata::new(TxnId(1), None);
    let rid1 = store.insert_version(&mut pool, &meta1, b"val1").unwrap();

    // Create v2 pointing to v1
    let meta2 = VersionMetadata::new(TxnId(1), Some(rid1));
    let rid2 = store.insert_version(&mut pool, &meta2, b"val2").unwrap();

    // Deliberately introduce a cycle: v1 -> v2 (creating v2 -> v1 -> v2)
    let mut meta1_cycle = meta1;
    meta1_cycle.prev_version = Some(rid2);
    store
        .update_version_metadata(&mut pool, rid1, &meta1_cycle)
        .unwrap();

    let snap = Snapshot::new(SnapshotId(1), CommitSeq(100));
    let res = store.read_visible(&mut pool, rid2, None, &snap);
    match res {
        Err(Error::VersionChainCycle(msg)) => {
            assert!(msg.contains("cycle detected"));
        }
        other => panic!("expected VersionChainCycle error, got {other:?}"),
    }
}

#[test]
fn test_version_chain_max_depth_limit() {
    let (mut pool, _dir) = create_buffer_pool("chain_depth");
    let mut store = RecordStore::new();

    let mut curr_rid = None;
    // Build a chain of depth MAX_VERSION_CHAIN_DEPTH + 1
    for i in 0..=MAX_VERSION_CHAIN_DEPTH {
        let meta = VersionMetadata::new(TxnId(1), curr_rid);
        let rid = store
            .insert_version(&mut pool, &meta, format!("val_{i}").as_bytes())
            .unwrap();
        curr_rid = Some(rid);
    }

    let head_rid = curr_rid.unwrap();
    // Snapshot with reader_txn=None and uncommitted version won't match, so it traverses the full chain
    let snap = Snapshot::new(SnapshotId(1), CommitSeq(100));
    let res = store.read_visible(&mut pool, head_rid, None, &snap);
    match res {
        Err(Error::VersionChainTooDeep { depth, max_depth }) => {
            assert!(depth > max_depth);
            assert_eq!(max_depth, MAX_VERSION_CHAIN_DEPTH);
        }
        other => panic!("expected VersionChainTooDeep error, got {other:?}"),
    }
}

#[test]
fn test_in_place_version_metadata_update_inline_and_overflow() {
    let (mut pool, _dir) = create_buffer_pool("inplace_meta");
    let mut store = RecordStore::new();

    // 1. Inline version
    let meta_inline = VersionMetadata::new(TxnId(1), None);
    let inline_rid = store
        .insert_version(&mut pool, &meta_inline, b"short_inline_value")
        .unwrap();

    let mut update_inline = meta_inline;
    update_inline.xmax = Some(TxnId(99));
    update_inline.deleted_at = Some(CommitSeq(123));
    store
        .update_version_metadata(&mut pool, inline_rid, &update_inline)
        .unwrap();

    let (read_meta, read_val) = store.read_version(&mut pool, inline_rid).unwrap().unwrap();
    assert_eq!(read_meta.xmax, Some(TxnId(99)));
    assert_eq!(read_meta.deleted_at, Some(CommitSeq(123)));
    assert_eq!(read_val, b"short_inline_value");

    // 2. Overflow version (> MAX_RECORD_SIZE)
    let overflow_val = vec![b'X'; 6000];
    let meta_overflow = VersionMetadata::new(TxnId(2), Some(inline_rid));
    let overflow_rid = store
        .insert_version(&mut pool, &meta_overflow, &overflow_val)
        .unwrap();

    let mut update_overflow = meta_overflow;
    update_overflow.xmax = Some(TxnId(101));
    update_overflow.deleted_at = Some(CommitSeq(456));
    store
        .update_version_metadata(&mut pool, overflow_rid, &update_overflow)
        .unwrap();

    let (read_ov_meta, read_ov_val) = store
        .read_version(&mut pool, overflow_rid)
        .unwrap()
        .unwrap();
    assert_eq!(read_ov_meta.xmax, Some(TxnId(101)));
    assert_eq!(read_ov_meta.deleted_at, Some(CommitSeq(456)));
    assert_eq!(read_ov_val, overflow_val);
}
