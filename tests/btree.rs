use tempfile::tempdir;
use treedb::{
    BPlusTree, BufferPool, Error, InternalNode, LeafNode, Page, PageManager, PageType, RecordId,
    INTERNAL_MAX_KEYS, INVALID_PAGE_ID, LEAF_MAX_ENTRIES,
};

fn create_test_pool(capacity: usize) -> (BufferPool, tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().expect("create temp dir");
    let db_path = dir.path().join("test_btree.treedb");
    let pm = PageManager::create(&db_path).expect("create pm");
    let pool = BufferPool::new(capacity, pm).expect("create buffer pool");
    (pool, dir, db_path)
}

// ---------------------------------------------------------------------------
// 1. Leaf Node Unit & Boundary Tests
// ---------------------------------------------------------------------------

#[test]
fn test_leaf_node_empty_roundtrip() {
    let leaf = LeafNode::new();
    assert!(leaf.is_empty());
    assert_eq!(leaf.key_count(), 0);
    assert_eq!(leaf.get(42), None);

    let mut page = Page::new(2, PageType::Leaf);
    leaf.serialize(&mut page).expect("serialize");

    let restored = LeafNode::deserialize(&page).expect("deserialize");
    assert_eq!(restored, leaf);
}

#[test]
fn test_leaf_node_entries_and_lookup() {
    let mut leaf = LeafNode::with_next_leaf(99);
    leaf.entries.push((10, RecordId::new(5, 1)));
    leaf.entries.push((20, RecordId::new(5, 2)));
    leaf.entries.push((30, RecordId::new(6, 1)));

    assert_eq!(leaf.get(10), Some(RecordId::new(5, 1)));
    assert_eq!(leaf.get(20), Some(RecordId::new(5, 2)));
    assert_eq!(leaf.get(30), Some(RecordId::new(6, 1)));
    assert_eq!(leaf.get(5), None);
    assert_eq!(leaf.get(15), None);
    assert_eq!(leaf.get(25), None);
    assert_eq!(leaf.get(35), None);
    assert_eq!(leaf.next_leaf, 99);

    let mut page = Page::new(2, PageType::Leaf);
    leaf.serialize(&mut page).expect("serialize");

    let restored = LeafNode::deserialize(&page).expect("deserialize");
    assert_eq!(restored, leaf);
}

#[test]
fn test_leaf_node_max_capacity_boundary() {
    let mut leaf = LeafNode::new();
    for i in 0..LEAF_MAX_ENTRIES {
        leaf.entries.push((i as u64, RecordId::new(3, i as u32)));
    }
    assert!(leaf.is_full());
    assert_eq!(leaf.key_count(), LEAF_MAX_ENTRIES);

    let mut page = Page::new(2, PageType::Leaf);
    leaf.serialize(&mut page).expect("serialize max leaf");

    let restored = LeafNode::deserialize(&page).expect("deserialize max leaf");
    assert_eq!(restored, leaf);

    // Overflow by 1 entry
    leaf.entries.push((99999, RecordId::new(3, 99999)));
    let err = leaf.serialize(&mut page).unwrap_err();
    assert!(matches!(err, Error::ValueTooLarge { .. }));
}

#[test]
fn test_leaf_node_corruption_and_ordering_validation() {
    let mut leaf = LeafNode::new();
    leaf.entries.push((20, RecordId::new(1, 1)));
    leaf.entries.push((10, RecordId::new(1, 2))); // Unsorted

    let mut page = Page::new(2, PageType::Leaf);
    let err_unsorted = leaf.serialize(&mut page).unwrap_err();
    assert!(matches!(err_unsorted, Error::TreeCorruption(_)));

    // Duplicate keys
    let mut leaf_dup = LeafNode::new();
    leaf_dup.entries.push((10, RecordId::new(1, 1)));
    leaf_dup.entries.push((10, RecordId::new(1, 2)));
    let err_dup = leaf_dup.serialize(&mut page).unwrap_err();
    assert!(matches!(err_dup, Error::TreeCorruption(_)));

    // Wrong page type during deserialization
    let wrong_page = Page::new(2, PageType::Internal);
    let err_wrong_type = LeafNode::deserialize(&wrong_page).unwrap_err();
    assert!(matches!(err_wrong_type, Error::PageCorruption { .. }));
}

// ---------------------------------------------------------------------------
// 2. Internal Node Unit & Routing Tests
// ---------------------------------------------------------------------------

#[test]
fn test_internal_node_routing_boundaries() {
    let node = InternalNode::with_keys_and_children(vec![50, 100], vec![10, 20, 30])
        .expect("valid internal node");

    assert_eq!(node.find_child(0), 10);
    assert_eq!(node.find_child(49), 10);
    assert_eq!(node.find_child(50), 20);
    assert_eq!(node.find_child(75), 20);
    assert_eq!(node.find_child(99), 20);
    assert_eq!(node.find_child(100), 30);
    assert_eq!(node.find_child(500), 30);

    let mut page = Page::new(3, PageType::Internal);
    node.serialize(&mut page).expect("serialize internal");

    let restored = InternalNode::deserialize(&page).expect("deserialize internal");
    assert_eq!(restored, node);
}

#[test]
fn test_internal_node_max_capacity_boundary() {
    let mut keys = Vec::with_capacity(INTERNAL_MAX_KEYS);
    let mut children = Vec::with_capacity(INTERNAL_MAX_KEYS + 1);

    children.push(10);
    for i in 0..INTERNAL_MAX_KEYS {
        keys.push((i as u64 + 1) * 10);
        children.push((i as u32) + 20);
    }

    let node = InternalNode::with_keys_and_children(keys, children).expect("valid max node");
    assert!(node.is_full());
    assert_eq!(node.key_count(), INTERNAL_MAX_KEYS);
    assert_eq!(node.child_count(), INTERNAL_MAX_KEYS + 1);

    let mut page = Page::new(3, PageType::Internal);
    node.serialize(&mut page).expect("serialize max internal");

    let restored = InternalNode::deserialize(&page).expect("deserialize max internal");
    assert_eq!(restored, node);
}

#[test]
fn test_internal_node_invariants_rejected() {
    let err_unsorted =
        InternalNode::with_keys_and_children(vec![50, 20], vec![1, 2, 3]).unwrap_err();
    assert!(matches!(err_unsorted, Error::TreeCorruption(_)));

    let err_dup = InternalNode::with_keys_and_children(vec![50, 50], vec![1, 2, 3]).unwrap_err();
    assert!(matches!(err_dup, Error::TreeCorruption(_)));

    let err_mismatch = InternalNode::with_keys_and_children(vec![50, 100], vec![1, 2]).unwrap_err();
    assert!(matches!(err_mismatch, Error::TreeCorruption(_)));

    let err_invalid_child = InternalNode::with_keys_and_children(vec![50], vec![0, 2]).unwrap_err();
    assert!(matches!(err_invalid_child, Error::TreeCorruption(_)));
}

// ---------------------------------------------------------------------------
// 3. Tree Traversal and Search Tests
// ---------------------------------------------------------------------------

#[test]
fn test_empty_tree_lookup() {
    let (mut pool, _dir, _path) = create_test_pool(5);
    let tree = BPlusTree::new_empty();

    assert!(tree.is_empty());
    assert_eq!(tree.root_page_id(), INVALID_PAGE_ID);
    assert_eq!(tree.get(&mut pool, 42).unwrap(), None);
}

#[test]
fn test_single_leaf_root_tree() {
    let (mut pool, _dir, _path) = create_test_pool(5);

    let root_id = pool.allocate_page(PageType::Leaf).expect("alloc root");
    assert_eq!(root_id, 2);

    let mut leaf = LeafNode::new();
    leaf.entries.push((100, RecordId::new(10, 1)));
    leaf.entries.push((200, RecordId::new(10, 2)));

    {
        let page_mut = pool.fetch_page_mut(root_id).expect("fetch root");
        leaf.serialize(page_mut).expect("serialize leaf");
    }
    pool.unpin_page(root_id, false).expect("unpin");
    pool.unpin_page(root_id, true).expect("unpin mut");

    let tree = BPlusTree::from_root(root_id);
    assert_eq!(
        tree.get(&mut pool, 100).unwrap(),
        Some(RecordId::new(10, 1))
    );
    assert_eq!(
        tree.get(&mut pool, 200).unwrap(),
        Some(RecordId::new(10, 2))
    );
    assert_eq!(tree.get(&mut pool, 150).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 50).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 300).unwrap(), None);
}

// ---------------------------------------------------------------------------
// 4. B+ Tree Insertion Tests (Phase 4)
// ---------------------------------------------------------------------------

#[test]
fn test_insert_empty_tree() {
    let (mut pool, _dir, _path) = create_test_pool(5);
    let mut tree = BPlusTree::new_empty();

    assert!(tree.is_empty());
    tree.insert(&mut pool, 42, RecordId::new(10, 1))
        .expect("insert first key");

    assert!(!tree.is_empty());
    assert_ne!(tree.root_page_id(), INVALID_PAGE_ID);

    assert_eq!(tree.get(&mut pool, 42).unwrap(), Some(RecordId::new(10, 1)));
    assert_eq!(tree.get(&mut pool, 100).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 0).unwrap(), None);

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_sequential_no_split() {
    let (mut pool, _dir, _path) = create_test_pool(5);
    let mut tree = BPlusTree::new_empty();

    // Insert 100 entries (well within LEAF_MAX_ENTRIES = 255)
    for i in 1..=100 {
        tree.insert(&mut pool, i as u64, RecordId::new(i as u32, 1))
            .expect("insert");
    }

    // Root should still be a leaf
    let root_page = pool.fetch_page(tree.root_page_id()).expect("fetch root");
    assert_eq!(root_page.page_type(), PageType::Leaf);
    pool.unpin_page(tree.root_page_id(), false).expect("unpin");

    for i in 1..=100 {
        assert_eq!(
            tree.get(&mut pool, i as u64).unwrap(),
            Some(RecordId::new(i as u32, 1))
        );
    }
    assert_eq!(tree.get(&mut pool, 0).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 101).unwrap(), None);

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_first_leaf_split_and_root_elevation() {
    let (mut pool, _dir, _path) = create_test_pool(10);
    let mut tree = BPlusTree::new_empty();

    // Insert exactly LEAF_MAX_ENTRIES (255) entries
    for i in 1..=LEAF_MAX_ENTRIES {
        tree.insert(&mut pool, i as u64, RecordId::new(i as u32, 0))
            .expect("insert up to capacity");
    }

    // Root must still be a Leaf
    let root_page = pool.fetch_page(tree.root_page_id()).unwrap();
    assert_eq!(root_page.page_type(), PageType::Leaf);
    pool.unpin_page(tree.root_page_id(), false).unwrap();

    // Insert 256th entry -> Forces first leaf split and root elevation!
    tree.insert(&mut pool, 256, RecordId::new(256, 0))
        .expect("insert causing split");

    // Root must now be an Internal node
    let root_page = pool.fetch_page(tree.root_page_id()).unwrap();
    assert_eq!(root_page.page_type(), PageType::Internal);
    let internal = InternalNode::deserialize(root_page).unwrap();
    pool.unpin_page(tree.root_page_id(), false).unwrap();

    assert_eq!(internal.key_count(), 1);
    assert_eq!(internal.child_count(), 2);

    // Verify all 256 keys
    for i in 1..=256 {
        assert_eq!(
            tree.get(&mut pool, i as u64).unwrap(),
            Some(RecordId::new(i as u32, 0)),
            "failed to find key {i}"
        );
    }

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_duplicate_key_rejected() {
    let (mut pool, _dir, _path) = create_test_pool(5);
    let mut tree = BPlusTree::new_empty();

    tree.insert(&mut pool, 42, RecordId::new(1, 1))
        .expect("first insert");
    let err = tree.insert(&mut pool, 42, RecordId::new(2, 2)).unwrap_err();

    assert!(matches!(err, Error::DuplicateKey(42)));
    // Original value remains intact
    assert_eq!(tree.get(&mut pool, 42).unwrap(), Some(RecordId::new(1, 1)));

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_multiple_leaf_splits() {
    let (mut pool, _dir, _path) = create_test_pool(20);
    let mut tree = BPlusTree::new_empty();

    // Insert 1000 keys
    for i in 1..=1000 {
        tree.insert(&mut pool, (i * 2) as u64, RecordId::new(i as u32, 5))
            .expect("insert");
    }

    for i in 1..=1000 {
        let key = (i * 2) as u64;
        assert_eq!(
            tree.get(&mut pool, key).unwrap(),
            Some(RecordId::new(i as u32, 5)),
            "key {key} missing"
        );
        // Odd numbers are missing
        assert_eq!(tree.get(&mut pool, key - 1).unwrap(), None);
    }

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_random_order() {
    let (mut pool, _dir, _path) = create_test_pool(30);
    let mut tree = BPlusTree::new_empty();

    // Generate deterministic pseudorandom permutation using simple LCG
    let count = 800;
    let mut keys: Vec<u64> = (1..=count).collect();

    // Simple deterministic Fisher-Yates shuffle with fixed seed
    let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
    for i in (1..keys.len()).rev() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let j = (seed >> 32) as usize % (i + 1);
        keys.swap(i, j);
    }

    for &key in &keys {
        tree.insert(&mut pool, key, RecordId::new(key as u32, (key % 10) as u32))
            .expect("insert random");
    }

    for &key in &keys {
        assert_eq!(
            tree.get(&mut pool, key).unwrap(),
            Some(RecordId::new(key as u32, (key % 10) as u32))
        );
    }
    assert_eq!(tree.get(&mut pool, count + 1).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 0).unwrap(), None);

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_deep_tree_recursive_splits() {
    let (mut pool, _dir, _path) = create_test_pool(50);
    let mut tree = BPlusTree::new_empty();

    // Insert 2000 keys to force multiple leaf and internal splits
    for i in 1..=2000 {
        tree.insert(&mut pool, i as u64, RecordId::new(i as u32, 1))
            .expect("insert deep");
    }

    for i in 1..=2000 {
        assert_eq!(
            tree.get(&mut pool, i as u64).unwrap(),
            Some(RecordId::new(i as u32, 1))
        );
    }

    tree.validate_tree(&mut pool).expect("tree valid");
}

#[test]
fn test_insert_persistence_across_reopen() {
    let (mut pool, _dir, db_path) = create_test_pool(20);
    let mut tree = BPlusTree::new_empty();

    // Insert 600 keys (multiple leaf splits)
    for i in 1..=600 {
        tree.insert(&mut pool, (i * 3) as u64, RecordId::new(i as u32, 7))
            .expect("insert persist");
    }

    let root_page_id = tree.root_page_id();
    assert_ne!(root_page_id, INVALID_PAGE_ID);

    // Sync all dirty pages to physical media
    pool.sync().expect("sync pool");
    drop(pool);

    // Reopen database from disk
    let pm = PageManager::open(&db_path).expect("reopen pm");
    let persisted_root = pm.header().root_page_id;
    assert_eq!(persisted_root, root_page_id);

    let mut new_pool = BufferPool::new(20, pm).expect("new pool");
    let reopened_tree = BPlusTree::from_root(persisted_root);

    // Verify all keys
    for i in 1..=600 {
        let key = (i * 3) as u64;
        assert_eq!(
            reopened_tree.get(&mut new_pool, key).unwrap(),
            Some(RecordId::new(i as u32, 7))
        );
        assert_eq!(reopened_tree.get(&mut new_pool, key - 1).unwrap(), None);
    }

    reopened_tree
        .validate_tree(&mut new_pool)
        .expect("reopened tree valid");
}

#[test]
fn test_insert_pin_discipline_and_no_pin_leaks() {
    let (mut pool, _dir, _path) = create_test_pool(10);
    let mut tree = BPlusTree::new_empty();

    for i in 1..=300 {
        tree.insert(&mut pool, i as u64, RecordId::new(i as u32, 0))
            .expect("insert");

        // Verify that after every insert, NO frames remain pinned
        for pid in 0..pool.page_count() {
            if pool.is_resident(pid) {
                assert_eq!(
                    pool.pin_count(pid).unwrap(),
                    0,
                    "page {pid} leaked a pin after inserting key {i}"
                );
            }
        }
    }
}
