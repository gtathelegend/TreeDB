use std::fs::OpenOptions;
use std::io::Write;
use tempfile::tempdir;
use treedb::{
    DbHeader, Error, Page, PageManager, PageType, CURRENT_FORMAT_VERSION, DEFAULT_METADATA_PAGE_ID,
    HEADER_PAGE_ID, MAGIC_BYTES, PAGE_PAYLOAD_SIZE, PAGE_SIZE,
};

#[test]
fn test_database_creation() {
    let dir = tempdir().expect("failed to create temp dir");
    let db_path = dir.path().join("test.treedb");

    assert!(!db_path.exists());
    let pm = PageManager::create(&db_path).expect("failed to create database");
    assert!(db_path.exists());

    assert_eq!(pm.page_count(), 2);
    let header = pm.header();
    assert_eq!(&header.magic, MAGIC_BYTES);
    assert_eq!(header.format_version, CURRENT_FORMAT_VERSION);
    assert_eq!(header.page_size, PAGE_SIZE as u32);
    assert_eq!(header.page_count, 2);
    assert_eq!(header.root_page_id, 0);
    assert_eq!(header.metadata_page_id, DEFAULT_METADATA_PAGE_ID);
}

#[test]
fn test_page_allocation() {
    let dir = tempdir().expect("failed to create temp dir");
    let db_path = dir.path().join("test_alloc.treedb");

    let mut pm = PageManager::create(&db_path).expect("failed to create db");
    assert_eq!(pm.page_count(), 2);

    // Allocate first page (page 2)
    let p2 = pm.allocate_page().expect("failed to allocate page 2");
    assert_eq!(p2, 2);
    assert_eq!(pm.page_count(), 3);

    // Allocate subsequent pages
    let p3 = pm.allocate_page().expect("failed to allocate page 3");
    let p4 = pm.allocate_page().expect("failed to allocate page 4");
    assert_eq!(p3, 3);
    assert_eq!(p4, 4);
    assert_eq!(pm.page_count(), 5);

    // Read back allocated pages and check initial Free status
    let page2 = pm.read_page(2).expect("should read page 2");
    assert_eq!(page2.id(), 2);
    assert_eq!(page2.page_type(), PageType::Free);
}

#[test]
fn test_page_write_and_read() {
    let dir = tempdir().expect("failed to create temp dir");
    let db_path = dir.path().join("test_rw.treedb");

    let mut pm = PageManager::create(&db_path).expect("failed to create db");
    let page_id = pm.allocate_page().expect("failed to allocate");

    let mut page = Page::new(page_id, PageType::Leaf);
    page.set_flags(0xAB);
    page.payload_mut()[..12].copy_from_slice(b"hello treedb");

    pm.write_page(&page).expect("failed to write page");

    let read_back = pm.read_page(page_id).expect("failed to read page");
    assert_eq!(read_back.id(), page_id);
    assert_eq!(read_back.page_type(), PageType::Leaf);
    assert_eq!(read_back.flags(), 0xAB);
    assert_eq!(&read_back.payload()[..12], b"hello treedb");
    assert_eq!(read_back.as_bytes(), page.as_bytes());
}

#[test]
fn test_multiple_pages_no_cross_corruption() {
    let dir = tempdir().expect("failed to create temp dir");
    let db_path = dir.path().join("test_multi.treedb");

    let mut pm = PageManager::create(&db_path).expect("failed to create db");

    let id_a = pm.allocate_page().expect("alloc a");
    let id_b = pm.allocate_page().expect("alloc b");
    let id_c = pm.allocate_page().expect("alloc c");

    let mut page_a = Page::new(id_a, PageType::Internal);
    let mut page_b = Page::new(id_b, PageType::Leaf);
    let mut page_c = Page::new(id_c, PageType::Record);

    page_a.payload_mut().fill(0xAA);
    page_b.payload_mut().fill(0xBB);
    page_c.payload_mut().fill(0xCC);

    pm.write_page(&page_a).expect("write a");
    pm.write_page(&page_b).expect("write b");
    pm.write_page(&page_c).expect("write c");

    let read_a = pm.read_page(id_a).expect("read a");
    let read_b = pm.read_page(id_b).expect("read b");
    let read_c = pm.read_page(id_c).expect("read c");

    assert_eq!(read_a.page_type(), PageType::Internal);
    assert_eq!(read_a.payload(), &[0xAA; PAGE_PAYLOAD_SIZE]);

    assert_eq!(read_b.page_type(), PageType::Leaf);
    assert_eq!(read_b.payload(), &[0xBB; PAGE_PAYLOAD_SIZE]);

    assert_eq!(read_c.page_type(), PageType::Record);
    assert_eq!(read_c.payload(), &[0xCC; PAGE_PAYLOAD_SIZE]);
}

#[test]
fn test_persistence_across_reopen() {
    let dir = tempdir().expect("failed to create temp dir");
    let db_path = dir.path().join("test_persist.treedb");

    let test_data = b"persistent data string";
    let p2_id;

    // Scope 1: Create, write, sync, drop
    {
        let mut pm = PageManager::create(&db_path).expect("create");
        p2_id = pm.allocate_page().expect("alloc");
        let mut p2 = Page::new(p2_id, PageType::Leaf);
        p2.payload_mut()[..test_data.len()].copy_from_slice(test_data);
        pm.write_page(&p2).expect("write");
        pm.set_root_page_id(p2_id).expect("set root");
        pm.sync().expect("sync");
    }

    // Scope 2: Reopen and verify
    {
        let mut pm = PageManager::open(&db_path).expect("open existing");
        assert_eq!(pm.page_count(), 3);
        assert_eq!(pm.header().root_page_id, p2_id);

        let p2 = pm.read_page(p2_id).expect("read persisted page");
        assert_eq!(p2.id(), p2_id);
        assert_eq!(p2.page_type(), PageType::Leaf);
        assert_eq!(&p2.payload()[..test_data.len()], test_data);
    }
}

#[test]
fn test_invalid_database_files() {
    let dir = tempdir().expect("temp dir");

    // 1. Empty file
    let empty_path = dir.path().join("empty.treedb");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&empty_path)
        .expect("create empty");

    let err_empty = PageManager::open(&empty_path).unwrap_err();
    assert!(matches!(err_empty, Error::InvalidDatabase(_)));

    // 2. File with corrupt magic
    let bad_magic_path = dir.path().join("bad_magic.treedb");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&bad_magic_path)
            .expect("create bad magic");
        let mut bad_buf = [0u8; PAGE_SIZE * 2];
        bad_buf[0..8].copy_from_slice(b"NOT_TREE");
        f.write_all(&bad_buf).expect("write");
    }
    let err_magic = PageManager::open(&bad_magic_path).unwrap_err();
    assert!(matches!(err_magic, Error::InvalidDatabase(_)));

    // 3. File with unsupported version
    let bad_ver_path = dir.path().join("bad_ver.treedb");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&bad_ver_path)
            .expect("create bad ver");
        let mut header = DbHeader::new();
        header.format_version = 999;
        let mut buf = [0u8; PAGE_SIZE * 2];
        buf[0..PAGE_SIZE].copy_from_slice(&header.serialize());
        f.write_all(&buf).expect("write");
    }
    let err_ver = PageManager::open(&bad_ver_path).unwrap_err();
    assert!(matches!(
        err_ver,
        Error::UnsupportedFormatVersion {
            found: 999,
            supported: CURRENT_FORMAT_VERSION
        }
    ));

    // 4. Truncated database (header claims 5 pages, but file only has 2)
    let trunc_path = dir.path().join("trunc.treedb");
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&trunc_path)
            .expect("create trunc");
        let mut header = DbHeader::new();
        header.page_count = 5;
        let mut buf = [0u8; PAGE_SIZE * 2];
        buf[0..PAGE_SIZE].copy_from_slice(&header.serialize());
        f.write_all(&buf).expect("write");
    }
    let err_trunc = PageManager::open(&trunc_path).unwrap_err();
    assert!(matches!(err_trunc, Error::InvalidDatabase(_)));
}

#[test]
fn test_boundary_cases() {
    let dir = tempdir().expect("temp dir");
    let db_path = dir.path().join("boundary.treedb");

    let mut pm = PageManager::create(&db_path).expect("create");

    // Page 0 (Header Page) read
    let page0 = pm.read_page(HEADER_PAGE_ID).expect("read page 0");
    let header_deserialized = DbHeader::deserialize(page0.as_bytes()).expect("deserialize header");
    assert_eq!(&header_deserialized.magic, MAGIC_BYTES);
    assert_eq!(header_deserialized.format_version, CURRENT_FORMAT_VERSION);

    // Metadata page (Page 1) read
    let page1 = pm.read_page(DEFAULT_METADATA_PAGE_ID).expect("read page 1");
    assert_eq!(page1.id(), DEFAULT_METADATA_PAGE_ID);
    assert_eq!(page1.page_type(), PageType::Metadata);

    // Out of bounds page ID read
    let err_out_of_bounds = pm.read_page(99).unwrap_err();
    assert!(matches!(err_out_of_bounds, Error::InvalidPage(99)));

    // Out of bounds page write
    let invalid_page = Page::new(99, PageType::Leaf);
    let err_write_oob = pm.write_page(&invalid_page).unwrap_err();
    assert!(matches!(err_write_oob, Error::InvalidPage(99)));
}
