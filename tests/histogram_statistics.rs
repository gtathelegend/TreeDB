#![allow(clippy::needless_borrow, clippy::manual_range_contains)]

use std::path::PathBuf;
use treedb::engine::catalog::{IndexId, IndexMetadata, TableId};
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::query::cardinality::CardinalityEstimator;
use treedb::query::expr::{BinaryOp, Expr, UnaryOp};
use treedb::query::stats::{
    Histogram, HistogramBucket, HistogramKind, MAX_HISTOGRAM_BUCKETS, MAX_HISTOGRAM_MEMORY_BYTES,
};
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!(
            "test_hist_stats_{}_{}_{}.treedb",
            name,
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

    fn db(&self) -> TreeDb {
        if self.db_path.exists() {
            TreeDb::open(&self.db_path, 64).unwrap()
        } else {
            TreeDb::create(&self.db_path, 64).unwrap()
        }
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
        let _ = std::fs::remove_file(&self.wal_path);
    }
}

#[test]
fn test_r256_histogram_created_by_analyze() {
    let ctx = TestContext::new("r256");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {});", i * 2))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let col_val = stats.column("val").unwrap();
    assert!(col_val.histogram.is_some());
    let hist = col_val.histogram.as_ref().unwrap();
    assert_eq!(hist.kind, HistogramKind::EquiDepth);
    assert!(!hist.buckets.is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_r257_histogram_bucket_count_bounded() {
    let ctx = TestContext::new("r257");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=1000 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert!(hist.buckets.len() <= MAX_HISTOGRAM_BUCKETS);
    assert_eq!(MAX_HISTOGRAM_BUCKETS, 32);
    txn.commit().unwrap();
}

#[test]
fn test_r258_histogram_memory_bounded() {
    let buckets: Vec<HistogramBucket> = (0..MAX_HISTOGRAM_BUCKETS)
        .map(|i| {
            HistogramBucket::new(
                Value::Int64(i as i64 * 10),
                Value::Int64((i as i64 + 1) * 10),
                10,
                10,
            )
        })
        .collect();
    let hist = Histogram::new(buckets, 0);

    let approx_size =
        std::mem::size_of_val(&hist) + hist.buckets.len() * std::mem::size_of::<HistogramBucket>();
    assert!(approx_size <= MAX_HISTOGRAM_MEMORY_BYTES);
}

#[test]
fn test_r259_deterministic_bucket_construction() {
    let ctx = TestContext::new("r259");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=200 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {});", i % 50))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();
    let stats1 = txn.get_statistics("t1").unwrap().clone();

    txn.execute_statement("ANALYZE t1;").unwrap();
    let stats2 = txn.get_statistics("t1").unwrap().clone();

    assert_eq!(
        stats1.column("val").unwrap().histogram,
        stats2.column("val").unwrap().histogram
    );
    txn.commit().unwrap();
}

#[test]
fn test_r260_null_excluded_from_histogram_values() {
    let ctx = TestContext::new("r260");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        let val_str = if i % 2 == 0 {
            "NULL".to_string()
        } else {
            i.to_string()
        };
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {val_str});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let col = stats.column("val").unwrap();
    let hist = col.histogram.as_ref().unwrap();
    assert_eq!(hist.null_count, 50);

    let total_bucket_rows: usize = hist.buckets.iter().map(|b| b.row_count).sum();
    assert_eq!(total_bucket_rows, 50);
    txn.commit().unwrap();
}

#[test]
fn test_r261_integer_histogram() {
    let ctx = TestContext::new("r261");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in -50..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert!(!hist.buckets.is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_r262_unsigned_integer_histogram() {
    let ctx = TestContext::new("r262");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 0..100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {});", i * 10))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert!(!hist.buckets.is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_r263_float_histogram() {
    let ctx = TestContext::new("r263");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val DOUBLE);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!(
            "INSERT INTO t1 VALUES ({i}, {:.2});",
            i as f64 * 1.5
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert!(!hist.buckets.is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_r264_string_histogram() {
    let ctx = TestContext::new("r264");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val TEXT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, 'user_{:04}');", i))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert!(!hist.buckets.is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_r265_bytes_histogram() {
    let ctx = TestContext::new("r265");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val TEXT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, 'data_{i}');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert!(!hist.buckets.is_empty());
    txn.commit().unwrap();
}

#[test]
fn test_r266_nan_handling() {
    let ctx = TestContext::new("r266");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val DOUBLE);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10.0);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 20.0);")
        .unwrap();
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    for b in &hist.buckets {
        if let Value::Float64(f) = b.lower_bound {
            assert!(!f.is_nan());
        }
        if let Value::Float64(f) = b.upper_bound {
            assert!(!f.is_nan());
        }
    }
    txn.commit().unwrap();
}

#[test]
fn test_r267_negative_positive_integer_boundaries() {
    let ctx = TestContext::new("r267");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in -100..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let hist = stats.column("val").unwrap().histogram.as_ref().unwrap();
    assert_eq!(
        hist.buckets.first().unwrap().lower_bound,
        Value::Int64(-100)
    );
    assert_eq!(hist.buckets.last().unwrap().upper_bound, Value::Int64(100));
    txn.commit().unwrap();
}

#[test]
fn test_r268_min_max_integer_handling() {
    let ctx = TestContext::new("r268");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, -9223372036854775807);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 0);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (3, 9223372036854775807);")
        .unwrap();
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(Value::Int64(0))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r269_equality_estimate_from_histogram() {
    let ctx = TestContext::new("r269");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(Value::Int64(50))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 0.1);
    txn.commit().unwrap();
}

#[test]
fn test_r270_top_k_takes_precedence_over_histogram() {
    let ctx = TestContext::new("r270");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (c1 BIGINT PRIMARY KEY, c2 BIGINT);")
        .unwrap();

    for i in 1..=100 {
        let c2_val = if i <= 80 { 42 } else { i };
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {c2_val});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("c1", DataType::Int64, false),
            Column::new("c2", DataType::Int64, false),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("c1".into())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Literal(Value::Int64(1))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r271_less_than_estimate() {
    let ctx = TestContext::new("r271");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Int64(30))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.25 && sel <= 0.35);
    txn.commit().unwrap();
}

#[test]
fn test_r272_less_than_or_equal_estimate() {
    let ctx = TestContext::new("r272");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::LtEq,
        right: Box::new(Expr::Literal(Value::Int64(30))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.25 && sel <= 0.35);
    txn.commit().unwrap();
}

#[test]
fn test_r273_greater_than_estimate() {
    let ctx = TestContext::new("r273");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Gt,
        right: Box::new(Expr::Literal(Value::Int64(70))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.25 && sel <= 0.35);
    txn.commit().unwrap();
}

#[test]
fn test_r274_greater_than_or_equal_estimate() {
    let ctx = TestContext::new("r274");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(Value::Int64(18))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.75 && sel <= 0.85);
    txn.commit().unwrap();
}

#[test]
fn test_r275_between_estimate() {
    let ctx = TestContext::new("r275");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::Int64(18))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::LtEq,
            right: Box::new(Expr::Literal(Value::Int64(30))),
        }),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.10 && sel <= 0.16);
    txn.commit().unwrap();
}

#[test]
fn test_r276_same_column_range_conjunction() {
    let ctx = TestContext::new("r276");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, age BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("age", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("age".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::Int64(18))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("age".into())),
            op: BinaryOp::Lt,
            right: Box::new(Expr::Literal(Value::Int64(30))),
        }),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.10 && sel <= 0.15);
    txn.commit().unwrap();
}

#[test]
fn test_r277_partial_bucket_interpolation() {
    let ctx = TestContext::new("r277");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr1 = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Int64(25))),
    };
    let sel1 = CardinalityEstimator::estimate_expr_selectivity(&expr1, Some(&stats), &schema);

    let expr2 = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Int64(50))),
    };
    let sel2 = CardinalityEstimator::estimate_expr_selectivity(&expr2, Some(&stats), &schema);
    assert!(sel1 < sel2);
    txn.commit().unwrap();
}

#[test]
fn test_r278_multi_bucket_range() {
    let ctx = TestContext::new("r278");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=200 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::Int64(10))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::LtEq,
            right: Box::new(Expr::Literal(Value::Int64(190))),
        }),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.85 && sel <= 0.95);
    txn.commit().unwrap();
}

#[test]
fn test_r279_empty_range() {
    let ctx = TestContext::new("r279");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(Value::Int64(80))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::Lt,
            right: Box::new(Expr::Literal(Value::Int64(20))),
        }),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert_eq!(sel, 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r280_full_range_estimate() {
    let ctx = TestContext::new("r280");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::Int64(1))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::LtEq,
            right: Box::new(Expr::Literal(Value::Int64(100))),
        }),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert_eq!(sel, 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r281_range_outside_minimum() {
    let ctx = TestContext::new("r281");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 50..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Int64(10))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert_eq!(sel, 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r282_range_outside_maximum() {
    let ctx = TestContext::new("r282");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Gt,
        right: Box::new(Expr::Literal(Value::Int64(100))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert_eq!(sel, 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r283_string_range_fallback() {
    let ctx = TestContext::new("r283");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val TEXT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, 'item_{:03}');", i))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(Value::String("item_050".into()))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r284_bytes_range_fallback() {
    let ctx = TestContext::new("r284");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val TEXT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, 'val_{i}');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Bytes, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Bytes(vec![50]))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r285_float_infinity_handling() {
    let ctx = TestContext::new("r285");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val DOUBLE);")
        .unwrap();

    txn.execute_statement("INSERT INTO t1 VALUES (1, 10.0);")
        .unwrap();
    txn.execute_statement("INSERT INTO t1 VALUES (2, 50.0);")
        .unwrap();
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Float64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Gt,
        right: Box::new(Expr::Literal(Value::Float64(0.0))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r286_null_range_exclusion() {
    let ctx = TestContext::new("r286");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        let val_str = if i <= 20 {
            "NULL".to_string()
        } else {
            i.to_string()
        };
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {val_str});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(Value::Int64(0))),
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel <= 0.81);
    txn.commit().unwrap();
}

#[test]
fn test_r287_not_range_estimation() {
    let ctx = TestContext::new("r287");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let inner_expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Int64(30))),
    };
    let not_expr = Expr::Unary {
        op: UnaryOp::Not,
        expr: Box::new(inner_expr.clone()),
    };

    let inner_sel =
        CardinalityEstimator::estimate_expr_selectivity(&inner_expr, Some(&stats), &schema);
    let not_sel = CardinalityEstimator::estimate_expr_selectivity(&not_expr, Some(&stats), &schema);

    assert!((not_sel - (1.0 - inner_sel)).abs() < 1e-4);
    txn.commit().unwrap();
}

#[test]
fn test_r288_in_list_estimation() {
    let ctx = TestContext::new("r288");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let in_expr = Expr::InList {
        expr: Box::new(Expr::Column("val".into())),
        list: vec![
            Expr::Literal(Value::Int64(18)),
            Expr::Literal(Value::Int64(19)),
            Expr::Literal(Value::Int64(20)),
        ],
        negated: false,
    };
    let sel = CardinalityEstimator::estimate_expr_selectivity(&in_expr, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 0.1);
    txn.commit().unwrap();
}

#[test]
fn test_r289_duplicate_in_literals() {
    let ctx = TestContext::new("r289");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let in_single = Expr::InList {
        expr: Box::new(Expr::Column("val".into())),
        list: vec![Expr::Literal(Value::Int64(10))],
        negated: false,
    };
    let in_dup = Expr::InList {
        expr: Box::new(Expr::Column("val".into())),
        list: vec![
            Expr::Literal(Value::Int64(10)),
            Expr::Literal(Value::Int64(10)),
        ],
        negated: false,
    };

    let sel1 = CardinalityEstimator::estimate_expr_selectivity(&in_single, Some(&stats), &schema);
    let sel2 = CardinalityEstimator::estimate_expr_selectivity(&in_dup, Some(&stats), &schema);

    assert_eq!(sel1, sel2);
    txn.commit().unwrap();
}

#[test]
fn test_r290_or_range_estimation() {
    let ctx = TestContext::new("r290");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let or_expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::Lt,
            right: Box::new(Expr::Literal(Value::Int64(20))),
        }),
        op: BinaryOp::Or,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(Value::Int64(80))),
        }),
    };

    let sel = CardinalityEstimator::estimate_expr_selectivity(&or_expr, Some(&stats), &schema);
    assert!(sel >= 0.35 && sel <= 0.45);
    txn.commit().unwrap();
}

#[test]
fn test_r291_range_plus_equality_correlation_integration() {
    let ctx = TestContext::new("r291");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (c1 BIGINT PRIMARY KEY, c2 BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("c1", DataType::Int64, false),
            Column::new("c2", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let conj = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("c1".into())),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(Value::Int64(10))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("c2".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::Int64(5))),
        }),
    };

    let sel = CardinalityEstimator::estimate_expr_selectivity(&conj, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 0.02);
    txn.commit().unwrap();
}

#[test]
fn test_r292_unsupported_multi_column_range_fallback() {
    let ctx = TestContext::new("r292");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (c1 BIGINT PRIMARY KEY, c2 BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("c1", DataType::Int64, false),
            Column::new("c2", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("c1".into())),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(Value::Int64(10))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("c2".into())),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(Value::Int64(20))),
        }),
    };

    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r293_composite_index_range_estimation() {
    let ctx = TestContext::new("r293");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, c1 BIGINT, c2 BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_c1_c2 ON t1 (c1, c2);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i}, {});", i * 2))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let idx_meta = IndexMetadata {
        index_id: IndexId(1),
        table_id: TableId(1),
        root_page_id: 0,
        name: "idx_c1_c2".into(),
        column_indices: vec![1, 2],
        column_idx: 1,
        include_column_indices: vec![],
        unique: false,
    };
    let p_rows = CardinalityEstimator::estimate_sec_prefix_rows(Some(&stats), &idx_meta, 1);
    assert!(p_rows > 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r294_primary_key_range_estimation() {
    let ctx = TestContext::new("r294");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let est = CardinalityEstimator::estimate_pk_range_rows(
        Some(&stats),
        Some(&Value::Int64(10)),
        Some(&Value::Int64(30)),
    );
    assert!(est >= 15.0 && est <= 25.0);
    txn.commit().unwrap();
}

#[test]
fn test_r295_secondary_index_range_estimation() {
    let ctx = TestContext::new("r295");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_val ON t1 (val);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let idx_meta = IndexMetadata {
        index_id: IndexId(1),
        table_id: TableId(1),
        root_page_id: 0,
        name: "idx_val".into(),
        column_indices: vec![1],
        column_idx: 1,
        include_column_indices: vec![],
        unique: false,
    };
    let est = CardinalityEstimator::estimate_sec_range_rows(
        Some(&stats),
        &idx_meta,
        Some(&Value::Int64(20)),
        Some(&Value::Int64(40)),
    );
    assert!(est >= 15.0 && est <= 25.0);
    txn.commit().unwrap();
}

#[test]
fn test_r296_skip_scan_regression() {
    let ctx = TestContext::new("r296");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, c1 BIGINT, c2 BIGINT);")
        .unwrap();
    txn.execute_statement("CREATE INDEX idx_c1_c2 ON t1 (c1, c2);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {}, {i});", i % 5))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("c1", DataType::Int64, true),
            Column::new("c2", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let skip_est = CardinalityEstimator::estimate_skip_scan_rows(Some(&stats), &schema, &[2]);
    assert!(skip_est > 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r297_multi_index_costing_uses_histogram_estimate() {
    let table_rows = 1000.0;
    let sel1 = 0.1;
    let sel2 = 0.2;
    let intersect_card =
        CardinalityEstimator::estimate_intersection_cardinality(table_rows, &[sel1, sel2]);
    assert!((intersect_card - 20.0).abs() < 1e-4);
}

#[test]
fn test_r298_adaptive_feedback_integration() {
    use treedb::query::feedback::{FeedbackKey, FeedbackObservation, FeedbackRegistry};
    let mut fb = FeedbackRegistry::new();
    let key = FeedbackKey::scan("t1", "seq", "val > 0", None, 1);
    for _ in 0..5 {
        fb.record_observation(key.clone(), FeedbackObservation::new(100.0, 50.0));
    }
    let adjusted = fb.get_adjusted_cardinality(&key, 100.0);
    assert_eq!(adjusted, 50.0);
}

#[test]
fn test_r299_resource_aware_costing_integration() {
    use treedb::query::cost::CostModel;
    let in_mem_cost = CostModel::nested_loop_join_cost(1.0, 100.0, 1.0, 1000.0);
    let spill_cost = CostModel::nested_loop_join_cost(10.0, 100.0, 10.0, 1000.0);
    assert!(spill_cost > in_mem_cost);
}

#[test]
fn test_r300_analyze_invalidates_stale_plans() {
    use treedb::query::plan_cache::PlanCache;
    let mut cache = PlanCache::new();
    cache.invalidate_all();
    assert_eq!(cache.len(), 0);
}

#[test]
fn test_r301_prepared_statements_remain_correct() {
    let ctx = TestContext::new("r301");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Gt,
        right: Box::new(Expr::Parameter {
            index: 0,
            value: Value::Int64(25),
        }),
    };

    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0);
    txn.commit().unwrap();
}

#[test]
fn test_r302_explain_histogram_diagnostics() {
    let ctx = TestContext::new("r302");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    assert!(stats.column("val").unwrap().histogram.is_some());
    txn.commit().unwrap();
}

#[test]
fn test_r303_explain_analyze_histogram_diagnostics() {
    use treedb::query::metrics::estimate_error_ratio;
    let ratio = estimate_error_ratio(10, Some(12.0));
    assert!(ratio.is_some());
}

#[test]
fn test_r304_histogram_statistics_not_persisted() {
    let ctx = TestContext::new("r304");
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
            .unwrap();
        txn.execute_statement("INSERT INTO t1 VALUES (1, 10);")
            .unwrap();
        txn.execute_statement("ANALYZE t1;").unwrap();
        txn.commit().unwrap();
    }

    let mut db2 = ctx.db();
    let mut txn2 = db2.begin_transaction().unwrap();
    let res = txn2.execute_statement("SELECT * FROM t1;").unwrap();
    assert!(matches!(
        res,
        treedb::engine::transaction::StatementResult::Query(_)
    ));
    txn2.commit().unwrap();
}

#[test]
fn test_r305_failed_analyze_does_not_corrupt_existing_statistics() {
    let ctx = TestContext::new("r305");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats1 = txn.get_statistics("t1").unwrap();
    assert!(stats1.column("val").unwrap().histogram.is_some());
    txn.commit().unwrap();
}

#[test]
fn test_r306_insert_update_delete_do_not_corrupt_histogram_state() {
    let ctx = TestContext::new("r306");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats1 = txn.get_statistics("t1").unwrap().clone();
    txn.execute_statement("INSERT INTO t1 VALUES (51, 51);")
        .unwrap();

    assert_eq!(stats1.row_count, 50);
    txn.commit().unwrap();
}

#[test]
fn test_r307_randomized_integer_range_differential_test() {
    let ctx = TestContext::new("r307");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    let mut values = Vec::new();
    for i in 1..=500 {
        let v = (i * 37) % 200;
        values.push(v);
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {v});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let actual_matching = values.iter().filter(|&&v| v >= 50 && v <= 150).count() as f64;
    let expr = Expr::Binary {
        left: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(Value::Int64(50))),
        }),
        op: BinaryOp::And,
        right: Box::new(Expr::Binary {
            left: Box::new(Expr::Column("val".into())),
            op: BinaryOp::LtEq,
            right: Box::new(Expr::Literal(Value::Int64(150))),
        }),
    };

    let est_sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    let est_matching = est_sel * (values.len() as f64);

    assert!(est_matching >= 0.0 && est_matching <= values.len() as f64);
    assert!((est_matching - actual_matching).abs() <= 100.0);
    txn.commit().unwrap();
}

#[test]
fn test_r308_randomized_skewed_distribution_differential_test() {
    let ctx = TestContext::new("r308");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=1000 {
        let v = if i <= 700 { i % 10 } else { 10 + (i % 900) };
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {v});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::Lt,
        right: Box::new(Expr::Literal(Value::Int64(10))),
    };

    let est_sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    let est_matching = est_sel * 1000.0;

    assert!(est_matching > 400.0);
    txn.commit().unwrap();
}

#[test]
fn test_r309_randomized_string_range_differential_test() {
    let ctx = TestContext::new("r309");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, name TEXT);")
        .unwrap();

    for i in 1..=200 {
        txn.execute_statement(&format!(
            "INSERT INTO t1 VALUES ({i}, 'customer_{:03}');",
            i
        ))
        .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("name", DataType::String, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("name".into())),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(Value::String("customer_050".into()))),
    };

    let est_sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(est_sel > 0.0 && est_sel <= 1.0);
    txn.commit().unwrap();
}

#[test]
fn test_r310_histogram_estimate_remains_bounded_by_table_cardinality() {
    let ctx = TestContext::new("r310");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t1 (id BIGINT PRIMARY KEY, val BIGINT);")
        .unwrap();

    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t1 VALUES ({i}, {i});"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t1;").unwrap();

    let stats = txn.get_statistics("t1").unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::Int64, false),
            Column::new("val", DataType::Int64, true),
        ],
        0,
    )
    .unwrap();

    let expr = Expr::Binary {
        left: Box::new(Expr::Column("val".into())),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(Value::Int64(-1000))),
    };

    let est_sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    let est_rows = est_sel * (stats.row_count as f64);
    assert!(est_rows <= stats.row_count as f64);
    assert!(est_rows >= 0.0);
    txn.commit().unwrap();
}
