use tempfile::TempDir;
use treedb::engine::types::Value;
use treedb::storage::page::{Page, PageType, PAGE_PAYLOAD_SIZE};
use treedb::storage::record_page::RecordPage;
use treedb::TreeDb;

#[test]
fn test_scan_empty_and_single_row() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("empty_single.treedb");
    let mut db = TreeDb::create(&db_path, 64).unwrap();

    db.execute_statement("CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR, val BIGINT);")
        .unwrap();

    // 1. Empty table scan
    let res = db.execute_sql("SELECT * FROM items;").unwrap();
    assert_eq!(res.rows.len(), 0);

    // 2. Single-row table scan
    db.execute_statement("INSERT INTO items VALUES (1, 'single', 42);")
        .unwrap();
    let res = db.execute_sql("SELECT * FROM items;").unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[0].values[1], Value::String("single".to_string()));
    assert_eq!(res.rows[0].values[2], Value::Int64(42));
}

#[test]
fn test_scan_large_multi_page_and_types() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("large_multipage.treedb");
    let mut db = TreeDb::create(&db_path, 256).unwrap();

    db.execute_statement(
        "CREATE TABLE complex_records (
            id BIGINT PRIMARY KEY,
            flag BOOLEAN,
            score FLOAT,
            notes VARCHAR,
            opt_num BIGINT
        );",
    )
    .unwrap();

    let count = 2_500;
    for i in 1..=count {
        let flag = if i % 2 == 0 { "true" } else { "false" };
        let score = (i as f64) * 1.5;
        let notes = format!("note_{:05}", i);
        let opt_num = if i % 5 == 0 { "NULL" } else { "100" };

        db.execute_statement(&format!(
            "INSERT INTO complex_records VALUES ({i}, {flag}, {score}, '{notes}', {opt_num});"
        ))
        .unwrap();
    }
    db.sync().unwrap();

    // Full table scan
    let res = db.execute_sql("SELECT * FROM complex_records;").unwrap();
    assert_eq!(res.rows.len(), count as usize);

    for (idx, row) in res.rows.iter().enumerate() {
        let i = (idx + 1) as i64;
        assert_eq!(row.values[0], Value::Int64(i));
        assert_eq!(row.values[1], Value::Boolean(i % 2 == 0));
        assert_eq!(row.values[2], Value::Float64((i as f64) * 1.5));
        assert_eq!(row.values[3], Value::String(format!("note_{:05}", i)));
        if i % 5 == 0 {
            assert_eq!(row.values[4], Value::Null);
        } else {
            assert_eq!(row.values[4], Value::Int64(100));
        }
    }

    // Filter scan
    let res_filtered = db
        .execute_sql("SELECT id, notes FROM complex_records WHERE flag = true AND id <= 10;")
        .unwrap();
    assert_eq!(res_filtered.rows.len(), 5);
    for (idx, row) in res_filtered.rows.iter().enumerate() {
        let expected_id = ((idx + 1) * 2) as i64;
        assert_eq!(row.values[0], Value::Int64(expected_id));
        assert_eq!(
            row.values[1],
            Value::String(format!("note_{:05}", expected_id))
        );
    }
}

#[test]
fn test_scan_inline_and_overflow_records() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("inline_overflow.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement(
        "CREATE TABLE docs (
            id BIGINT PRIMARY KEY,
            content VARCHAR
        );",
    )
    .unwrap();

    let short_str = "short inline record";
    let long_str_1 = "X".repeat(6_000);
    let long_str_2 = "Y".repeat(12_000);

    db.execute_statement(&format!("INSERT INTO docs VALUES (1, '{short_str}');"))
        .unwrap();
    db.execute_statement(&format!("INSERT INTO docs VALUES (2, '{long_str_1}');"))
        .unwrap();
    db.execute_statement(&format!("INSERT INTO docs VALUES (3, '{short_str}');"))
        .unwrap();
    db.execute_statement(&format!("INSERT INTO docs VALUES (4, '{long_str_2}');"))
        .unwrap();

    db.sync().unwrap();

    let res = db.execute_sql("SELECT * FROM docs;").unwrap();
    assert_eq!(res.rows.len(), 4);
    assert_eq!(res.rows[0].values[1], Value::String(short_str.to_string()));
    assert_eq!(res.rows[1].values[1], Value::String(long_str_1));
    assert_eq!(res.rows[2].values[1], Value::String(short_str.to_string()));
    assert_eq!(res.rows[3].values[1], Value::String(long_str_2));
}

#[test]
fn test_scan_mvcc_visibility_and_aborts() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("mvcc_scan.treedb");
    let mut db = TreeDb::create(&db_path, 128).unwrap();

    db.execute_statement("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT);")
        .unwrap();

    db.execute_statement("INSERT INTO accounts VALUES (1, 100);")
        .unwrap();
    db.execute_statement("INSERT INTO accounts VALUES (2, 200);")
        .unwrap();
    db.execute_statement("INSERT INTO accounts VALUES (3, 300);")
        .unwrap();

    // Txn 1 updates row 2 and deletes row 3
    let mut txn1 = db.begin_transaction().unwrap();
    {
        let mut tbl = txn1.open_table("accounts").unwrap();
        tbl.update(&treedb::engine::types::Row::new(vec![
            Value::Int64(2),
            Value::Int64(250),
        ]))
        .unwrap();
        tbl.delete(&Value::Int64(3)).unwrap();
    }
    txn1.commit().unwrap();

    // Txn 2 inserts an uncommitted row and aborts
    let mut txn2 = db.begin_transaction().unwrap();
    {
        let mut tbl = txn2.open_table("accounts").unwrap();
        tbl.insert(&treedb::engine::types::Row::new(vec![
            Value::Int64(4),
            Value::Int64(400),
        ]))
        .unwrap();
    }
    txn2.abort().unwrap();

    // Committed scan: sees 1: 100, 2: 250, row 3 deleted, row 4 not present
    let res = db.execute_sql("SELECT * FROM accounts;").unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::Int64(1));
    assert_eq!(res.rows[0].values[1], Value::Int64(100));
    assert_eq!(res.rows[1].values[0], Value::Int64(2));
    assert_eq!(res.rows[1].values[1], Value::Int64(250));
}

#[test]
fn test_scan_persistence_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("reopen_scan.treedb");

    {
        let mut db = TreeDb::create(&db_path, 128).unwrap();
        db.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR);")
            .unwrap();
        for i in 1..=500 {
            db.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'val_{i}');"))
                .unwrap();
        }
        db.sync().unwrap();
    }

    // Reopen database
    {
        let mut db = TreeDb::open(&db_path, 128).unwrap();
        let res = db.execute_sql("SELECT * FROM t;").unwrap();
        assert_eq!(res.rows.len(), 500);
        assert_eq!(res.rows[0].values[0], Value::Int64(1));
        assert_eq!(res.rows[499].values[0], Value::Int64(500));
    }
}

#[test]
fn test_record_page_raw_slot_validation() {
    let mut page = Page::new(10, PageType::Record);

    // 1. Empty uninitialized page returns Ok(None)
    assert!(RecordPage::get_location_from_raw_page(&page, 0)
        .unwrap()
        .is_none());

    // 2. Valid page with 1 inline slot
    let mut rp = RecordPage::new();
    let slot_id = rp.insert(b"test_record_payload").unwrap();
    rp.serialize(&mut page).unwrap();

    let loc = RecordPage::get_location_from_raw_page(&page, slot_id)
        .unwrap()
        .unwrap();
    match loc {
        treedb::storage::record_page::RecordLocation::Inline(slice) => {
            assert_eq!(slice, b"test_record_payload");
        }
        _ => panic!("expected inline record"),
    }

    // 3. Out-of-bounds slot_id returns Ok(None)
    assert!(RecordPage::get_location_from_raw_page(&page, 99)
        .unwrap()
        .is_none());

    // 4. Deleted slot returns Ok(None)
    rp.delete(slot_id);
    rp.serialize(&mut page).unwrap();
    assert!(RecordPage::get_location_from_raw_page(&page, slot_id)
        .unwrap()
        .is_none());

    // 5. Wrong page type returns Error::PageCorruption
    let page_internal = Page::new(11, PageType::Internal);
    assert!(RecordPage::get_location_from_raw_page(&page_internal, 0).is_err());

    // 6. Corrupted bounds return Error::PageCorruption
    page.set_page_type(PageType::Record);
    let payload = page.payload_mut();
    // slot_count = 1
    payload[0..2].copy_from_slice(&1u16.to_le_bytes());
    // free_start = 10, free_end = 4000
    payload[2..4].copy_from_slice(&10u16.to_le_bytes());
    payload[4..6].copy_from_slice(&4000u16.to_le_bytes());
    // slot 0 offset exceeds PAGE_PAYLOAD_SIZE
    let bad_offset = (PAGE_PAYLOAD_SIZE + 50) as u16;
    payload[6..8].copy_from_slice(&bad_offset.to_le_bytes());
    payload[8..10].copy_from_slice(&20u16.to_le_bytes());

    assert!(RecordPage::get_location_from_raw_page(&page, 0).is_err());
}
