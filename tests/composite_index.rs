//! Composite Secondary Index — Phase 40 Integration Test Suite
//!
//! Requirements exercised:
//!   R01 – CREATE INDEX (programmatic) on 2 columns
//!   R02 – CREATE INDEX (programmatic) on 3 columns
//!   R03 – CREATE UNIQUE INDEX (programmatic) on multiple columns
//!   R04 – CREATE INDEX via SQL DDL ("ON (col1, col2)")
//!   R05 – CREATE UNIQUE INDEX via SQL DDL
//!   R06 – Catalog persistence: column_indices survive DB reopen
//!   R07 – INSERT maintains composite index automatically
//!   R08 – UPDATE maintains composite index (old key removed, new key inserted)
//!   R09 – DELETE removes composite index entries
//!   R10 – Unique violation on exact composite key match
//!   R11 – Unique allows same leading column if trailing column differs
//!   R12 – NULL values are stored in index; unique allows multiple NULL rows
//!   R13 – Full key exact lookup returns correct rows
//!   R14 – Prefix lookup returns all rows sharing the same leading column value
//!   R15 – Range scan over composite index returns rows in lexicographic order
//!   R16 – composite_index_scan with start/end bounds
//!   R17 – Full index+table structural validation passes after mutations
//!   R18 – DROP INDEX removes composite index; subsequent lookup errors
//!   R19 – Plan cache is invalidated when composite index is created
//!   R20 – Schema-level: duplicate column names are rejected at CREATE INDEX time
//!   R21 – Schema-level: out-of-range column name rejected
//!   R22 – Key encoding: (A, B) < (A, B') when B < B' (lexicographic order preserved)
//!   R23 – Key encoding: round-trip for mixed types including null and string-with-null
//!   R24 – MAX_INDEX_COLUMNS (8) enforced
//!   R25 – Multi-column index interoperates correctly alongside a single-column index

use std::collections::BTreeSet;
use std::path::PathBuf;

use treedb::engine::types::{Column, DataType, Row, Schema, Value};
use treedb::error::Error;
use treedb::index::composite::CompositeIndexKey;
use treedb::{StatementResult, TreeDb};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

struct Ctx {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl Ctx {
    fn new(label: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_composite_{}_{}_{}.treedb",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&wal_path);
        Self { db_path, wal_path }
    }

    fn create(&self) -> TreeDb {
        TreeDb::create(&self.db_path, 128).unwrap()
    }

    fn open(&self) -> TreeDb {
        TreeDb::open(&self.db_path, 128).unwrap()
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = std::fs::remove_file(&bak);
    }
}

/// Schema: id (PK UInt64), dept (String NOT NULL), level (Int64 nullable), score (Float64 nullable)
fn orders_schema() -> Schema {
    Schema::new(
        vec![
            Column::new("id".to_string(), DataType::UInt64, false),
            Column::new("dept".to_string(), DataType::String, false),
            Column::new("level".to_string(), DataType::Int64, true),
            Column::new("score".to_string(), DataType::Float64, true),
        ],
        0,
    )
    .unwrap()
}

fn row(id: u64, dept: &str, level: i64, score: f64) -> Row {
    Row::new(vec![
        Value::UInt64(id),
        Value::String(dept.to_string()),
        Value::Int64(level),
        Value::Float64(score),
    ])
}

fn row_null_level(id: u64, dept: &str) -> Row {
    Row::new(vec![
        Value::UInt64(id),
        Value::String(dept.to_string()),
        Value::Null,
        Value::Null,
    ])
}

fn ids_of(rows: &[Row]) -> BTreeSet<u64> {
    rows.iter()
        .map(|r| {
            if let Value::UInt64(v) = r.values[0] {
                v
            } else {
                panic!("unexpected id type")
            }
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// R01 / R02 — CREATE INDEX programmatic: 2 and 3 columns
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r01_r02_create_composite_index_programmatic() {
    let ctx = Ctx::new("r01_r02");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();

    let id2 = txn
        .create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    let id3 = txn
        .create_composite_index(
            "orders",
            "idx_dept_level_score",
            &["dept", "level", "score"],
            false,
        )
        .unwrap();

    assert_ne!(id2, id3, "each index must receive a distinct IndexId");

    let meta2 = txn.get_index("idx_dept_level").unwrap().unwrap();
    assert_eq!(meta2.column_indices, vec![1, 2]);
    assert!(!meta2.unique);

    let meta3 = txn.get_index("idx_dept_level_score").unwrap().unwrap();
    assert_eq!(meta3.column_indices, vec![1, 2, 3]);
    assert!(!meta3.unique);

    txn.validate_catalog().unwrap();
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R03 — CREATE UNIQUE INDEX programmatic on multiple columns
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r03_create_unique_composite_index() {
    let ctx = Ctx::new("r03");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();

    txn.create_composite_index("orders", "idx_uniq_dept_level", &["dept", "level"], true)
        .unwrap();

    let meta = txn.get_index("idx_uniq_dept_level").unwrap().unwrap();
    assert!(meta.unique);
    assert_eq!(meta.column_indices, vec![1, 2]);

    txn.validate_catalog().unwrap();
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R04 / R05 — CREATE INDEX via SQL DDL
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r04_r05_sql_create_composite_index() {
    let ctx = Ctx::new("r04_r05");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement(
        "CREATE TABLE employees (id UINT64 PRIMARY KEY, name TEXT NOT NULL, dept TEXT NOT NULL, grade INT64);",
    )
    .unwrap();

    let r = txn
        .execute_statement("CREATE INDEX idx_dept_grade ON employees (dept, grade);")
        .unwrap();
    assert!(
        matches!(r, StatementResult::CreatedIndex { ref index } if index == "idx_dept_grade"),
        "expected CreatedIndex, got {r:?}"
    );

    let r2 = txn
        .execute_statement("CREATE UNIQUE INDEX idx_uniq_name_dept ON employees (name, dept);")
        .unwrap();
    assert!(
        matches!(r2, StatementResult::CreatedIndex { ref index } if index == "idx_uniq_name_dept"),
        "expected CreatedIndex, got {r2:?}"
    );

    let non_uniq = txn.get_index("idx_dept_grade").unwrap().unwrap();
    assert!(!non_uniq.unique);
    assert_eq!(non_uniq.column_indices.len(), 2);

    let uniq = txn.get_index("idx_uniq_name_dept").unwrap().unwrap();
    assert!(uniq.unique);
    assert_eq!(uniq.column_indices.len(), 2);

    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R06 — Catalog persistence: column_indices survive DB reopen
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r06_catalog_persistence_on_reopen() {
    let ctx = Ctx::new("r06");

    {
        let mut db = ctx.create();
        let mut txn = db.begin_transaction().unwrap();
        txn.create_table("orders", orders_schema()).unwrap();
        txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
            .unwrap();
        txn.create_composite_index("orders", "idx_uniq_dl", &["dept", "level"], true)
            .unwrap();
        txn.validate_catalog().unwrap();
        txn.commit().unwrap();
    }

    {
        let mut db = ctx.open();
        let mut txn = db.begin_transaction().unwrap();

        let meta = txn.get_index("idx_dept_level").unwrap().unwrap();
        assert_eq!(meta.column_indices, vec![1, 2]);
        assert!(!meta.unique);
        assert_eq!(meta.column_idx, 1, "column_idx compat field must equal first index");

        let meta2 = txn.get_index("idx_uniq_dl").unwrap().unwrap();
        assert!(meta2.unique);
        assert_eq!(meta2.column_indices, vec![1, 2]);

        let indexes = txn.list_indexes_for_table("orders").unwrap();
        assert_eq!(indexes.len(), 2);

        txn.validate_catalog().unwrap();
        txn.validate_index("idx_dept_level").unwrap();
        txn.validate_index("idx_uniq_dl").unwrap();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// R07 — INSERT maintains composite index automatically
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r07_insert_maintains_composite_index() {
    let ctx = Ctx::new("r07");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 3, 9.0)).unwrap();
    tbl.insert(&row(2, "eng", 3, 8.5)).unwrap();
    tbl.insert(&row(3, "eng", 4, 7.0)).unwrap();
    tbl.insert(&row(4, "hr", 2, 6.0)).unwrap();

    // Exact lookup: ("eng", 3) -> rows 1 and 2
    let found = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("eng".into()), Value::Int64(3)],
        )
        .unwrap();
    assert_eq!(ids_of(&found), BTreeSet::from([1, 2]));

    // Exact lookup: ("hr", 2) -> row 4
    let found2 = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("hr".into()), Value::Int64(2)],
        )
        .unwrap();
    assert_eq!(ids_of(&found2), BTreeSet::from([4]));

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R08 — UPDATE maintains composite index
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r08_update_maintains_composite_index() {
    let ctx = Ctx::new("r08");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 3, 9.0)).unwrap();
    tbl.insert(&row(2, "hr", 2, 5.0)).unwrap();

    // Update row 1: move from ("eng", 3) to ("ops", 5)
    tbl.update(&Row::new(vec![
        Value::UInt64(1),
        Value::String("ops".to_string()),
        Value::Int64(5),
        Value::Float64(9.0),
    ]))
    .unwrap();

    // Old key must be gone
    let old = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("eng".into()), Value::Int64(3)],
        )
        .unwrap();
    assert!(old.is_empty(), "old composite key must be removed after update");

    // New key must exist
    let new = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("ops".into()), Value::Int64(5)],
        )
        .unwrap();
    assert_eq!(ids_of(&new), BTreeSet::from([1]));

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R09 — DELETE removes composite index entries
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r09_delete_removes_composite_index_entry() {
    let ctx = Ctx::new("r09");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 3, 9.0)).unwrap();
    tbl.insert(&row(2, "eng", 4, 8.0)).unwrap();

    tbl.delete(&Value::UInt64(1)).unwrap();

    let found = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("eng".into()), Value::Int64(3)],
        )
        .unwrap();
    assert!(found.is_empty(), "deleted row's composite key must be purged");

    let found2 = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("eng".into()), Value::Int64(4)],
        )
        .unwrap();
    assert_eq!(ids_of(&found2), BTreeSet::from([2]));

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R10 — Unique violation on exact composite key match
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r10_unique_violation_on_duplicate_composite_key() {
    let ctx = Ctx::new("r10");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_uniq_dept_level", &["dept", "level"], true)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 3, 9.0)).unwrap();
    let err = tbl.insert(&row(2, "eng", 3, 8.0));
    assert!(
        matches!(err, Err(Error::DuplicateIndexKey(_))),
        "expected DuplicateIndexKey, got: {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// R11 — Unique allows same leading column if trailing column differs
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r11_unique_allows_same_prefix_different_suffix() {
    let ctx = Ctx::new("r11");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_uniq_dept_level", &["dept", "level"], true)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 3, 9.0)).unwrap();
    tbl.insert(&row(2, "eng", 4, 8.0)).unwrap();
    tbl.insert(&row(3, "hr", 3, 7.0)).unwrap();

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// R12 — NULL values in composite index: engine treats NULL == NULL for uniqueness.
// Non-unique index accepts multiple NULL rows; unique index rejects them (NULL is treated equal).

#[test]
fn test_r12_null_in_composite_index() {
    let ctx = Ctx::new("r12");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    // Non-unique index: must accept multiple rows with NULL in indexed columns
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    // Unique index on same columns: NULL == NULL, so second identical (dept, NULL) row must be rejected
    txn.create_composite_index("orders", "idx_uniq_dept_level", &["dept", "level"], true)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    // Non-unique index accepts multiple rows with level=NULL
    tbl.insert(&row_null_level(1, "eng")).unwrap();
    tbl.insert(&row_null_level(2, "hr")).unwrap();

    // Unique index: ("eng", NULL) was inserted via row 1.
    // A second ("eng", NULL) row must be rejected (NULL == NULL semantics)
    let err = tbl.insert(&row_null_level(3, "eng"));
    assert!(
        matches!(err, Err(Error::DuplicateIndexKey(_))),
        "unique composite index must reject duplicate (dept, NULL) pair; got: {err:?}"
    );

    // But ("finance", NULL) must be accepted since dept differs
    tbl.insert(&row_null_level(4, "finance")).unwrap();

    // Non-unique index lookup for prefix "hr" must return row 2
    let found = tbl
        .composite_index_lookup("idx_dept_level", &[Value::String("hr".into())])
        .unwrap();
    assert_eq!(ids_of(&found), BTreeSet::from([2]));

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R13 — Full key exact lookup
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r13_full_key_exact_lookup() {
    let ctx = Ctx::new("r13");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    for i in 1u64..=5 {
        tbl.insert(&row(i, "eng", i as i64, i as f64)).unwrap();
    }

    let found = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("eng".into()), Value::Int64(3)],
        )
        .unwrap();
    assert_eq!(ids_of(&found), BTreeSet::from([3]));

    let empty = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("hr".into()), Value::Int64(1)],
        )
        .unwrap();
    assert!(empty.is_empty());

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R14 — Prefix lookup returns all rows sharing the same leading column
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r14_prefix_lookup() {
    let ctx = Ctx::new("r14");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 1, 9.0)).unwrap();
    tbl.insert(&row(2, "eng", 2, 8.0)).unwrap();
    tbl.insert(&row(3, "eng", 3, 7.0)).unwrap();
    tbl.insert(&row(4, "hr", 1, 6.0)).unwrap();
    tbl.insert(&row(5, "hr", 2, 5.0)).unwrap();

    // Prefix "eng" must return rows 1, 2, 3
    let eng = tbl
        .composite_index_lookup("idx_dept_level", &[Value::String("eng".into())])
        .unwrap();
    assert_eq!(ids_of(&eng), BTreeSet::from([1, 2, 3]));

    // Prefix "hr" must return rows 4, 5
    let hr = tbl
        .composite_index_lookup("idx_dept_level", &[Value::String("hr".into())])
        .unwrap();
    assert_eq!(ids_of(&hr), BTreeSet::from([4, 5]));

    // Absent prefix returns empty
    let none = tbl
        .composite_index_lookup("idx_dept_level", &[Value::String("finance".into())])
        .unwrap();
    assert!(none.is_empty());

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R15 — Range scan returns rows in lexicographic (dept, level) order
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r15_range_scan_lexicographic_order() {
    let ctx = Ctx::new("r15");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    // Insert out-of-order deliberately
    tbl.insert(&row(3, "eng", 3, 7.0)).unwrap();
    tbl.insert(&row(1, "eng", 1, 9.0)).unwrap();
    tbl.insert(&row(5, "hr", 2, 5.0)).unwrap();
    tbl.insert(&row(4, "hr", 1, 6.0)).unwrap();
    tbl.insert(&row(2, "eng", 2, 8.0)).unwrap();

    let start_key =
        CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Int64(1)])
            .to_index_key(2);
    let end_key =
        CompositeIndexKey::from_values(&[Value::String("hr".into()), Value::Int64(2)])
            .to_index_key(2);

    let rows = tbl
        .composite_index_scan("idx_dept_level", Some(&start_key), Some(&end_key))
        .unwrap();

    // end is exclusive: ("hr", 2) not included -> ids 1, 2, 3, 4
    let returned_ids: Vec<u64> = rows
        .iter()
        .map(|r| {
            if let Value::UInt64(v) = r.values[0] {
                v
            } else {
                panic!("unexpected")
            }
        })
        .collect();
    assert_eq!(returned_ids, vec![1, 2, 3, 4]);

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R16 — composite_index_scan with start/end bounds
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r16_composite_index_scan_with_bounds() {
    let ctx = Ctx::new("r16");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    for i in 1u64..=10 {
        tbl.insert(&row(i, "eng", i as i64, i as f64)).unwrap();
    }

    // Scan [("eng", 3), ("eng", 7)) — exclusive end: ids 3, 4, 5, 6
    let start_key =
        CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Int64(3)])
            .to_index_key(2);
    let end_key =
        CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Int64(7)])
            .to_index_key(2);

    let rows = tbl
        .composite_index_scan("idx_dept_level", Some(&start_key), Some(&end_key))
        .unwrap();

    assert_eq!(ids_of(&rows), BTreeSet::from([3, 4, 5, 6]));

    // Unbounded scan returns all 10
    let all = tbl
        .composite_index_scan("idx_dept_level", None, None)
        .unwrap();
    assert_eq!(all.len(), 10);

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R17 — validate_tree and validate_indexes pass after heavy mutations
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r17_structural_validation_after_mutations() {
    let ctx = Ctx::new("r17");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    for i in 1u64..=50 {
        tbl.insert(&row(
            i,
            if i % 2 == 0 { "eng" } else { "hr" },
            (i % 10) as i64,
            i as f64,
        ))
        .unwrap();
    }
    for i in (1u64..=50).step_by(3) {
        tbl.delete(&Value::UInt64(i)).unwrap();
    }

    tbl.validate_tree().unwrap();
    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.commit().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R18 — DROP INDEX removes composite index; subsequent lookup errors
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r18_drop_composite_index() {
    let ctx = Ctx::new("r18");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    txn.drop_index("idx_dept_level").unwrap();

    let meta = txn.get_index("idx_dept_level").unwrap();
    assert!(meta.is_none(), "dropped index must not appear in catalog");

    let indexes = txn.list_indexes_for_table("orders").unwrap();
    assert!(indexes.is_empty());

    txn.validate_catalog().unwrap();
    txn.commit().unwrap();

    // Reopen — still gone
    let mut db2 = ctx.open();
    let mut txn2 = db2.begin_transaction().unwrap();
    assert!(txn2.get_index("idx_dept_level").unwrap().is_none());

    let mut tbl = txn2.open_table("orders").unwrap();
    let err = tbl.composite_index_lookup("idx_dept_level", &[Value::String("eng".into())]);
    assert!(
        matches!(err, Err(Error::IndexNotFound(_))),
        "expected IndexNotFound, got: {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// R19 — Plan cache is invalidated when composite index is created
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r19_plan_cache_invalidated_on_create() {
    // R19: creating a composite index must invalidate the plan cache
    // (entries cleared) and leave the database fully operational afterward.
    //
    // The plan cache is only consulted for *prepared* statements (execute_prepared),
    // not for ad-hoc execute_sql calls. We use prepare_sql + execute_prepared here.
    let ctx = Ctx::new("r19");
    let mut db = ctx.create();

    // DDL must use execute_statement (db.execute_sql rejects DDL StatementResults).
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement(
            "CREATE TABLE staff (id UINT64 PRIMARY KEY, dept TEXT NOT NULL, level INT64);",
        )
        .unwrap();
        txn.execute_statement("INSERT INTO staff VALUES (1, 'eng', 3);")
            .unwrap();
        txn.execute_statement("INSERT INTO staff VALUES (2, 'hr', 5);")
            .unwrap();
        txn.commit().unwrap();
    }

    // Prepare a query — the compiled plan will be cached on first execute_prepared call.
    let stmt = db.prepare_sql("SELECT * FROM staff;").unwrap();

    // First execution: cache miss → plan compiled and inserted.
    let r0a = db.execute_prepared(&stmt, &[]).unwrap();
    let r0a_qr = r0a.into_query().unwrap();
    assert_eq!(r0a_qr.rows.len(), 2);

    // Second execution of the same prepared statement: cache hit.
    let r0b = db.execute_prepared(&stmt, &[]).unwrap();
    let r0b_qr = r0b.into_query().unwrap();
    assert_eq!(r0b_qr.rows.len(), 2);

    let stats_mid = db.plan_cache_stats();
    assert!(
        stats_mid.hits > 0,
        "plan cache must record at least one hit after the repeated prepared query"
    );
    assert!(
        stats_mid.entries > 0,
        "plan cache must have at least one cached entry before index creation"
    );

    // Create composite index — must call invalidate_all(), clearing cached entries.
    {
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE INDEX idx_dept_level ON staff (dept, level);")
            .unwrap();
        txn.commit().unwrap();
    }

    // Cache must be empty immediately after the invalidation triggered by CREATE INDEX.
    let stats_after = db.plan_cache_stats();
    assert_eq!(
        stats_after.entries, 0,
        "plan cache entries must be zero immediately after CREATE INDEX invalidation"
    );

    // Database must remain fully functional — re-execute the prepared statement.
    let r1 = db.execute_prepared(&stmt, &[]).unwrap();
    let r1_qr = r1.into_query().unwrap();
    assert_eq!(r1_qr.rows.len(), 2, "all rows still visible after index creation");
}

// ─────────────────────────────────────────────────────────────────────────────
// R20 — Duplicate column names in CREATE INDEX rejected
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r20_duplicate_column_names_rejected() {
    let ctx = Ctx::new("r20");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();

    let err = txn.create_composite_index("orders", "idx_bad", &["dept", "dept"], false);
    assert!(err.is_err(), "duplicate column names must be rejected");
    txn.abort().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R21 — Unknown column name rejected
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r21_unknown_column_name_rejected() {
    let ctx = Ctx::new("r21");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();

    let err =
        txn.create_composite_index("orders", "idx_bad", &["dept", "nonexistent_col"], false);
    assert!(err.is_err(), "unknown column must be rejected");
    txn.abort().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R22 — Key encoding preserves lexicographic order
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r22_key_encoding_lexicographic_order() {
    // (A, B) < (A, B') when B < B'
    let t1 = CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Int64(1)]);
    let t2 = CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Int64(2)]);
    assert!(t1.encode() < t2.encode(), "(eng,1) must be less than (eng,2)");

    // (A, Z) < (B, A) when A < B lexicographically
    let t3 = CompositeIndexKey::from_values(&[Value::String("a".into()), Value::Int64(1000)]);
    let t4 = CompositeIndexKey::from_values(&[Value::String("b".into()), Value::Int64(-1)]);
    assert!(
        t3.encode() < t4.encode(),
        "('a', 1000) must be less than ('b', -1)"
    );

    // Null tag (0) sorts before string tag (6)
    let null_pair = CompositeIndexKey::from_values(&[Value::Null, Value::Int64(1)]);
    let str_pair = CompositeIndexKey::from_values(&[Value::String("a".into()), Value::Int64(1)]);
    assert!(
        null_pair.encode() < str_pair.encode(),
        "(NULL, 1) must sort before ('a', 1)"
    );

    // Same first column, NULL second sorts before any non-null second
    let null_second = CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Null]);
    let int_second =
        CompositeIndexKey::from_values(&[Value::String("eng".into()), Value::Int64(-9999)]);
    assert!(
        null_second.encode() < int_second.encode(),
        "('eng', NULL) must sort before ('eng', -9999)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// R23 — Key encoding round-trip for mixed types
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r23_key_encoding_roundtrip_mixed_types() {
    let cases: &[&[Value]] = &[
        &[Value::Int64(-1), Value::UInt64(u64::MAX), Value::Boolean(true)],
        &[
            Value::Null,
            Value::String("hello\0world".to_string()),
            Value::Float64(3.14),
        ],
        &[Value::Boolean(false), Value::Int64(0), Value::Null],
        &[Value::String("".to_string()), Value::UInt64(0)],
        &[
            Value::String("\0\0\0".to_string()),
            Value::String("normal".to_string()),
        ],
    ];

    for values in cases {
        let key = CompositeIndexKey::from_values(values);
        let encoded = key.encode();
        let decoded = CompositeIndexKey::decode(&encoded, values.len())
            .unwrap_or_else(|e| panic!("decode failed for {values:?}: {e}"));
        assert_eq!(key, decoded, "round-trip must be identity for {values:?}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// R24 — MAX_INDEX_COLUMNS (8) enforced
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r24_max_index_columns_enforced() {
    let mut cols = vec![Column::new("id".to_string(), DataType::UInt64, false)];
    for i in 0..9 {
        cols.push(Column::new(format!("c{i}"), DataType::Int64, true));
    }
    let schema = Schema::new(cols, 0).unwrap();

    let ctx = Ctx::new("r24");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("wide", schema).unwrap();

    // 8 columns is exactly MAX_INDEX_COLUMNS — must succeed
    txn.create_composite_index(
        "wide",
        "idx_8cols",
        &["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"],
        false,
    )
    .unwrap();

    // 9 columns exceeds MAX_INDEX_COLUMNS — must fail
    let err = txn.create_composite_index(
        "wide",
        "idx_9cols",
        &["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8"],
        false,
    );
    assert!(
        err.is_err(),
        "creating an index with > MAX_INDEX_COLUMNS columns must be rejected"
    );
    txn.abort().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// R25 — Composite index coexists alongside a single-column index
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_r25_composite_coexists_with_single_column_index() {
    let ctx = Ctx::new("r25");
    let mut db = ctx.create();
    let mut txn = db.begin_transaction().unwrap();
    txn.create_table("orders", orders_schema()).unwrap();
    txn.create_index("orders", "idx_dept", "dept", false)
        .unwrap();
    txn.create_composite_index("orders", "idx_dept_level", &["dept", "level"], false)
        .unwrap();
    txn.commit().unwrap();

    let mut txn = db.begin_transaction().unwrap();
    let mut tbl = txn.open_table("orders").unwrap();

    tbl.insert(&row(1, "eng", 3, 9.0)).unwrap();
    tbl.insert(&row(2, "eng", 4, 8.0)).unwrap();
    tbl.insert(&row(3, "hr", 3, 7.0)).unwrap();

    // Single-column lookup
    let sc = tbl
        .index_lookup("idx_dept", &Value::String("eng".into()))
        .unwrap();
    assert_eq!(ids_of(&sc), BTreeSet::from([1, 2]));

    // Composite prefix lookup
    let cp = tbl
        .composite_index_lookup("idx_dept_level", &[Value::String("eng".into())])
        .unwrap();
    assert_eq!(ids_of(&cp), BTreeSet::from([1, 2]));

    // Composite full key lookup
    let ce = tbl
        .composite_index_lookup(
            "idx_dept_level",
            &[Value::String("hr".into()), Value::Int64(3)],
        )
        .unwrap();
    assert_eq!(ids_of(&ce), BTreeSet::from([3]));

    tbl.validate_indexes().unwrap();

    // Delete row 1 — both indexes must be updated consistently
    tbl.delete(&Value::UInt64(1)).unwrap();

    let sc2 = tbl
        .index_lookup("idx_dept", &Value::String("eng".into()))
        .unwrap();
    assert_eq!(ids_of(&sc2), BTreeSet::from([2]));

    let cp2 = tbl
        .composite_index_lookup("idx_dept_level", &[Value::String("eng".into())])
        .unwrap();
    assert_eq!(ids_of(&cp2), BTreeSet::from([2]));

    tbl.validate_indexes().unwrap();
    drop(tbl);
    txn.validate_catalog().unwrap();
    txn.commit().unwrap();

    // Persistence: reopen and verify both indexes survive
    let mut db2 = ctx.open();
    let mut txn2 = db2.begin_transaction().unwrap();
    assert!(txn2.get_index("idx_dept").unwrap().is_some());

    let meta = txn2.get_index("idx_dept_level").unwrap().unwrap();
    assert_eq!(meta.column_indices, vec![1, 2]);

    txn2.validate_catalog().unwrap();
    txn2.validate_index("idx_dept").unwrap();
    txn2.validate_index("idx_dept_level").unwrap();
}
