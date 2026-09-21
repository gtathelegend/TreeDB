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
    // Keys: [50, 100], Children: [10, 20, 30]
    // key < 50 => child 10
    // 50 <= key < 100 => child 20
    // key >= 100 => child 30
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
    // Unsorted keys
    let err_unsorted =
        InternalNode::with_keys_and_children(vec![50, 20], vec![1, 2, 3]).unwrap_err();
    assert!(matches!(err_unsorted, Error::TreeCorruption(_)));

    // Duplicate keys
    let err_dup = InternalNode::with_keys_and_children(vec![50, 50], vec![1, 2, 3]).unwrap_err();
    assert!(matches!(err_dup, Error::TreeCorruption(_)));

    // Child count != key count + 1
    let err_mismatch = InternalNode::with_keys_and_children(vec![50, 100], vec![1, 2]).unwrap_err();
    assert!(matches!(err_mismatch, Error::TreeCorruption(_)));

    // Invalid child page ID 0
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

    // Allocate page 2 as a Leaf root
    let root_id = pool.allocate_page(PageType::Leaf).expect("alloc root");
    assert_eq!(root_id, 2);

    let mut leaf = LeafNode::new();
    leaf.entries.push((100, RecordId::new(10, 1)));
    leaf.entries.push((200, RecordId::new(10, 2)));

    {
        let page_mut = pool.fetch_page_mut(root_id).expect("fetch root");
        leaf.serialize(page_mut).expect("serialize leaf");
    }
    pool.unpin_page(root_id, false).expect("unpin"); // unpin allocate_page
    pool.unpin_page(root_id, true).expect("unpin mut"); // unpin fetch_page_mut

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

#[test]
fn test_two_level_tree_traversal() {
    let (mut pool, _dir, _path) = create_test_pool(5);

    // Layout:
    // Page 2: Internal Root [50] -> Left: Page 3 (Leaf A), Right: Page 4 (Leaf B)
    // Page 3 (Leaf A): [10, 20]
    // Page 4 (Leaf B): [50, 70, 90]

    let root_id = pool.allocate_page(PageType::Internal).expect("alloc root"); // page 2
    let leaf_a_id = pool.allocate_page(PageType::Leaf).expect("alloc leaf a"); // page 3
    let leaf_b_id = pool.allocate_page(PageType::Leaf).expect("alloc leaf b"); // page 4

    // Setup Leaf A
    let mut leaf_a = LeafNode::with_next_leaf(leaf_b_id);
    leaf_a.entries.push((10, RecordId::new(100, 1)));
    leaf_a.entries.push((20, RecordId::new(100, 2)));
    {
        let page = pool.fetch_page_mut(leaf_a_id).expect("fetch a");
        leaf_a.serialize(page).expect("serialize a");
    }
    pool.unpin_page(leaf_a_id, false).expect("unpin a1");
    pool.unpin_page(leaf_a_id, true).expect("unpin a2");

    // Setup Leaf B
    let mut leaf_b = LeafNode::new();
    leaf_b.entries.push((50, RecordId::new(200, 1)));
    leaf_b.entries.push((70, RecordId::new(200, 2)));
    leaf_b.entries.push((90, RecordId::new(200, 3)));
    {
        let page = pool.fetch_page_mut(leaf_b_id).expect("fetch b");
        leaf_b.serialize(page).expect("serialize b");
    }
    pool.unpin_page(leaf_b_id, false).expect("unpin b1");
    pool.unpin_page(leaf_b_id, true).expect("unpin b2");

    // Setup Internal Root
    let root_node =
        InternalNode::with_keys_and_children(vec![50], vec![leaf_a_id, leaf_b_id]).expect("root");
    {
        let page = pool.fetch_page_mut(root_id).expect("fetch root");
        root_node.serialize(page).expect("serialize root");
    }
    pool.unpin_page(root_id, false).expect("unpin root1");
    pool.unpin_page(root_id, true).expect("unpin root2");

    let tree = BPlusTree::new(root_id);

    // Hits
    assert_eq!(
        tree.get(&mut pool, 10).unwrap(),
        Some(RecordId::new(100, 1))
    );
    assert_eq!(
        tree.get(&mut pool, 20).unwrap(),
        Some(RecordId::new(100, 2))
    );
    assert_eq!(
        tree.get(&mut pool, 50).unwrap(),
        Some(RecordId::new(200, 1))
    ); // Exact separator key
    assert_eq!(
        tree.get(&mut pool, 70).unwrap(),
        Some(RecordId::new(200, 2))
    );
    assert_eq!(
        tree.get(&mut pool, 90).unwrap(),
        Some(RecordId::new(200, 3))
    );

    // Misses
    assert_eq!(tree.get(&mut pool, 5).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 15).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 25).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 55).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 80).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 100).unwrap(), None);
}

#[test]
fn test_three_level_deep_tree_traversal() {
    let (mut pool, _dir, _path) = create_test_pool(10);

    // Level 1: Root Internal (Page 2) [100] -> [Page 3, Page 4]
    // Level 2: Child Internal A (Page 3) [50] -> [Leaf A1: Page 5, Leaf A2: Page 6]
    // Level 2: Child Internal B (Page 4) [150] -> [Leaf B1: Page 7, Leaf B2: Page 8]
    // Level 3: Leaf Nodes
    //   Leaf A1 (Page 5): [10, 20]
    //   Leaf A2 (Page 6): [50, 60]
    //   Leaf B1 (Page 7): [100, 120]
    //   Leaf B2 (Page 8): [150, 180]

    let p2_root = pool.allocate_page(PageType::Internal).unwrap();
    let p3_int_a = pool.allocate_page(PageType::Internal).unwrap();
    let p4_int_b = pool.allocate_page(PageType::Internal).unwrap();
    let p5_leaf_a1 = pool.allocate_page(PageType::Leaf).unwrap();
    let p6_leaf_a2 = pool.allocate_page(PageType::Leaf).unwrap();
    let p7_leaf_b1 = pool.allocate_page(PageType::Leaf).unwrap();
    let p8_leaf_b2 = pool.allocate_page(PageType::Leaf).unwrap();

    // Leaf A1
    let mut leaf_a1 = LeafNode::with_next_leaf(p6_leaf_a2);
    leaf_a1.entries.push((10, RecordId::new(5, 1)));
    leaf_a1.entries.push((20, RecordId::new(5, 2)));
    {
        let p = pool.fetch_page_mut(p5_leaf_a1).unwrap();
        leaf_a1.serialize(p).unwrap();
    }
    pool.unpin_page(p5_leaf_a1, false).unwrap();
    pool.unpin_page(p5_leaf_a1, true).unwrap();

    // Leaf A2
    let mut leaf_a2 = LeafNode::with_next_leaf(p7_leaf_b1);
    leaf_a2.entries.push((50, RecordId::new(6, 1)));
    leaf_a2.entries.push((60, RecordId::new(6, 2)));
    {
        let p = pool.fetch_page_mut(p6_leaf_a2).unwrap();
        leaf_a2.serialize(p).unwrap();
    }
    pool.unpin_page(p6_leaf_a2, false).unwrap();
    pool.unpin_page(p6_leaf_a2, true).unwrap();

    // Leaf B1
    let mut leaf_b1 = LeafNode::with_next_leaf(p8_leaf_b2);
    leaf_b1.entries.push((100, RecordId::new(7, 1)));
    leaf_b1.entries.push((120, RecordId::new(7, 2)));
    {
        let p = pool.fetch_page_mut(p7_leaf_b1).unwrap();
        leaf_b1.serialize(p).unwrap();
    }
    pool.unpin_page(p7_leaf_b1, false).unwrap();
    pool.unpin_page(p7_leaf_b1, true).unwrap();

    // Leaf B2
    let mut leaf_b2 = LeafNode::new();
    leaf_b2.entries.push((150, RecordId::new(8, 1)));
    leaf_b2.entries.push((180, RecordId::new(8, 2)));
    {
        let p = pool.fetch_page_mut(p8_leaf_b2).unwrap();
        leaf_b2.serialize(p).unwrap();
    }
    pool.unpin_page(p8_leaf_b2, false).unwrap();
    pool.unpin_page(p8_leaf_b2, true).unwrap();

    // Internal A
    let int_a =
        InternalNode::with_keys_and_children(vec![50], vec![p5_leaf_a1, p6_leaf_a2]).unwrap();
    {
        let p = pool.fetch_page_mut(p3_int_a).unwrap();
        int_a.serialize(p).unwrap();
    }
    pool.unpin_page(p3_int_a, false).unwrap();
    pool.unpin_page(p3_int_a, true).unwrap();

    // Internal B
    let int_b =
        InternalNode::with_keys_and_children(vec![150], vec![p7_leaf_b1, p8_leaf_b2]).unwrap();
    {
        let p = pool.fetch_page_mut(p4_int_b).unwrap();
        int_b.serialize(p).unwrap();
    }
    pool.unpin_page(p4_int_b, false).unwrap();
    pool.unpin_page(p4_int_b, true).unwrap();

    // Root
    let root = InternalNode::with_keys_and_children(vec![100], vec![p3_int_a, p4_int_b]).unwrap();
    {
        let p = pool.fetch_page_mut(p2_root).unwrap();
        root.serialize(p).unwrap();
    }
    pool.unpin_page(p2_root, false).unwrap();
    pool.unpin_page(p2_root, true).unwrap();

    let tree = BPlusTree::new(p2_root);

    assert_eq!(tree.get(&mut pool, 10).unwrap(), Some(RecordId::new(5, 1)));
    assert_eq!(tree.get(&mut pool, 20).unwrap(), Some(RecordId::new(5, 2)));
    assert_eq!(tree.get(&mut pool, 50).unwrap(), Some(RecordId::new(6, 1)));
    assert_eq!(tree.get(&mut pool, 60).unwrap(), Some(RecordId::new(6, 2)));
    assert_eq!(tree.get(&mut pool, 100).unwrap(), Some(RecordId::new(7, 1)));
    assert_eq!(tree.get(&mut pool, 120).unwrap(), Some(RecordId::new(7, 2)));
    assert_eq!(tree.get(&mut pool, 150).unwrap(), Some(RecordId::new(8, 1)));
    assert_eq!(tree.get(&mut pool, 180).unwrap(), Some(RecordId::new(8, 2)));

    assert_eq!(tree.get(&mut pool, 0).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 35).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 80).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 130).unwrap(), None);
    assert_eq!(tree.get(&mut pool, 200).unwrap(), None);
}

// ---------------------------------------------------------------------------
// 4. Persistence Across Database Reopen
// ---------------------------------------------------------------------------

#[test]
fn test_btree_persistence_and_reopen() {
    let (mut pool, _dir, db_path) = create_test_pool(5);

    let root_id = pool.allocate_page(PageType::Internal).unwrap();
    let leaf_a_id = pool.allocate_page(PageType::Leaf).unwrap();
    let leaf_b_id = pool.allocate_page(PageType::Leaf).unwrap();

    let mut leaf_a = LeafNode::with_next_leaf(leaf_b_id);
    leaf_a.entries.push((10, RecordId::new(1, 1)));
    {
        let p = pool.fetch_page_mut(leaf_a_id).unwrap();
        leaf_a.serialize(p).unwrap();
    }
    pool.unpin_page(leaf_a_id, false).unwrap();
    pool.unpin_page(leaf_a_id, true).unwrap();

    let mut leaf_b = LeafNode::new();
    leaf_b.entries.push((50, RecordId::new(2, 1)));
    {
        let p = pool.fetch_page_mut(leaf_b_id).unwrap();
        leaf_b.serialize(p).unwrap();
    }
    pool.unpin_page(leaf_b_id, false).unwrap();
    pool.unpin_page(leaf_b_id, true).unwrap();

    let root = InternalNode::with_keys_and_children(vec![50], vec![leaf_a_id, leaf_b_id]).unwrap();
    {
        let p = pool.fetch_page_mut(root_id).unwrap();
        root.serialize(p).unwrap();
    }
    pool.unpin_page(root_id, false).unwrap();
    pool.unpin_page(root_id, true).unwrap();

    // Set root in DbHeader via PageManager
    pool.page_manager_mut().set_root_page_id(root_id).unwrap();

    // Flush and sync to disk
    pool.sync().unwrap();
    drop(pool);

    // Reopen from disk
    let pm = PageManager::open(&db_path).expect("reopen pm");
    let persisted_root_id = pm.header().root_page_id;
    assert_eq!(persisted_root_id, root_id);

    let mut new_pool = BufferPool::new(5, pm).expect("new pool");
    let tree = BPlusTree::from_root(persisted_root_id);

    assert_eq!(
        tree.get(&mut new_pool, 10).unwrap(),
        Some(RecordId::new(1, 1))
    );
    assert_eq!(
        tree.get(&mut new_pool, 50).unwrap(),
        Some(RecordId::new(2, 1))
    );
    assert_eq!(tree.get(&mut new_pool, 999).unwrap(), None);
}

// ---------------------------------------------------------------------------
// 5. Buffer Pool Interaction & Pin Discipline
// ---------------------------------------------------------------------------

#[test]
fn test_pinning_discipline_and_no_pin_leaks() {
    let (mut pool, _dir, _path) = create_test_pool(3);

    let root_id = pool.allocate_page(PageType::Internal).unwrap();
    let leaf_id = pool.allocate_page(PageType::Leaf).unwrap();

    let mut leaf = LeafNode::new();
    leaf.entries.push((42, RecordId::new(9, 9)));
    {
        let p = pool.fetch_page_mut(leaf_id).unwrap();
        leaf.serialize(p).unwrap();
    }
    pool.unpin_page(leaf_id, false).unwrap();
    pool.unpin_page(leaf_id, true).unwrap();

    let root = InternalNode::with_keys_and_children(vec![], vec![leaf_id]).unwrap();
    {
        let p = pool.fetch_page_mut(root_id).unwrap();
        root.serialize(p).unwrap();
    }
    pool.unpin_page(root_id, false).unwrap();
    pool.unpin_page(root_id, true).unwrap();

    let tree = BPlusTree::new(root_id);

    // Verify initial pin state is 0 for all pages
    assert_eq!(pool.pin_count(root_id).unwrap(), 0);
    assert_eq!(pool.pin_count(leaf_id).unwrap(), 0);

    // Perform multiple lookups
    for _ in 0..10 {
        let res = tree.get(&mut pool, 42).unwrap();
        assert_eq!(res, Some(RecordId::new(9, 9)));

        // Verify no pin leaks after get() completes
        assert_eq!(pool.pin_count(root_id).unwrap(), 0);
        assert_eq!(pool.pin_count(leaf_id).unwrap(), 0);
        assert!(!pool.is_pinned(root_id).unwrap());
        assert!(!pool.is_pinned(leaf_id).unwrap());
    }

    // Repeated lookups should hit cache
    assert!(pool.stats().hits > 0);

    // Because pin count is 0, eviction can occur without error
    let p_extra = pool.allocate_page(PageType::Leaf).unwrap();
    pool.unpin_page(p_extra, false).unwrap();
}
