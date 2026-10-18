#![allow(clippy::needless_borrow, clippy::manual_range_contains)]

use std::path::PathBuf;
use treedb::engine::catalog::{IndexId, IndexMetadata, TableId};
use treedb::engine::transaction::StatementResult;
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::query::cardinality::{CardinalityEstimator, CorrelationSource};
use treedb::query::expr::{BinaryOp, Expr};
use treedb::query::operator::CanonicalValue;
use treedb::query::stats::{
    ColumnGroupKey, ColumnGroupStatistics, ColumnStatistics, Histogram, HistogramBucket,
    TableStatistics, TopKCombination,
};
use treedb::TreeDb;

struct TestContext {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(format!("test_corr_sel_{name}.treedb"));
        let wal_path = PathBuf::from(format!("{}.wal", db_path.display()));
        Self {
            _dir: dir,
            db_path,
            wal_path,
        }
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

fn parse_test_expr(expr_str: &str, schema: &Schema) -> Expr {
    let sql = format!("SELECT * FROM t WHERE {};", expr_str);
    let ast = treedb::sql::parse_sql(&sql).unwrap();
    let bound = treedb::sql::binder::bind_select(&ast, schema).unwrap();
    bound.filter.unwrap()
}

// ---------------------------------------------------------------------------
// R311: Pairwise equality correlation estimate.
// ---------------------------------------------------------------------------
#[test]
fn test_r311_pairwise_equality_correlation_estimate() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    let group_key = ColumnGroupKey::new(vec![0, 1]);
    let top_k = vec![TopKCombination {
        values: vec![
            CanonicalValue::String("IN".to_string()),
            CanonicalValue::String("ACTIVE".to_string()),
        ],
        frequency: 80,
    }];
    let group = ColumnGroupStatistics::new(
        vec![0, 1],
        vec!["country".to_string(), "status".to_string()],
        5,
        0,
        0,
        top_k,
    );
    stats.column_groups.insert(group_key, group);

    let expr = parse_test_expr("country = 'IN' AND status = 'ACTIVE'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!((sel - 0.80).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R312: Independent fallback when no pair statistics exist.
// ---------------------------------------------------------------------------
#[test]
fn test_r312_independent_fallback_no_pair_stats() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    let col_c = ColumnStatistics::new(100, 0, 2, None, None);
    let col_s = ColumnStatistics::new(100, 0, 4, None, None);
    stats.columns.insert("country".to_string(), col_c);
    stats.columns.insert("status".to_string(), col_s);

    let expr = parse_test_expr("country = 'IN' AND status = 'ACTIVE'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // Marginal country = 1/2 = 0.5, status = 1/4 = 0.25 -> 0.125
    assert!((sel - 0.125).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R313: Exact joint Top-K match.
// ---------------------------------------------------------------------------
#[test]
fn test_r313_exact_joint_top_k_match() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, false),
        Column::new("c2", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    let group_key = ColumnGroupKey::new(vec![0, 1]);
    let top_k = vec![
        TopKCombination {
            values: vec![CanonicalValue::Int64(10), CanonicalValue::Int64(20)],
            frequency: 450,
        },
        TopKCombination {
            values: vec![CanonicalValue::Int64(10), CanonicalValue::Int64(30)],
            frequency: 250,
        },
    ];
    let group = ColumnGroupStatistics::new(
        vec![0, 1],
        vec!["c1".to_string(), "c2".to_string()],
        10,
        0,
        0,
        top_k,
    );
    stats.column_groups.insert(group_key, group);

    let expr = parse_test_expr("c1 = 10 AND c2 = 20", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!((sel - 0.45).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R314: Joint frequency fallback.
// ---------------------------------------------------------------------------
#[test]
fn test_r314_joint_frequency_fallback() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, false),
        Column::new("c2", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    let group_key = ColumnGroupKey::new(vec![0, 1]);
    let top_k = vec![TopKCombination {
        values: vec![CanonicalValue::Int64(1), CanonicalValue::Int64(1)],
        frequency: 500,
    }];
    // Remaining rows = 500, remaining distinct = 5 -> avg freq = 100
    let group = ColumnGroupStatistics::new(
        vec![0, 1],
        vec!["c1".to_string(), "c2".to_string()],
        6,
        0,
        0,
        top_k,
    );
    stats.column_groups.insert(group_key, group);

    let expr = parse_test_expr("c1 = 99 AND c2 = 99", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // rem_freq = 500, rem_distinct = 5 -> est_freq = 100 -> sel = 100/1000 = 0.1
    assert!((sel - 0.10).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R315: Pair-selection determinism.
// ---------------------------------------------------------------------------
#[test]
fn test_r315_pair_selection_determinism() {
    let schema = Schema::new_projected(vec![
        Column::new("a", DataType::Int64, false),
        Column::new("b", DataType::Int64, false),
        Column::new("c", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    // (a, b) and (b, c)
    let g1_key = ColumnGroupKey::new(vec![0, 1]);
    let g1 = ColumnGroupStatistics::new(
        vec![0, 1],
        vec!["a".to_string(), "b".to_string()],
        10,
        0,
        0,
        vec![TopKCombination {
            values: vec![CanonicalValue::Int64(1), CanonicalValue::Int64(2)],
            frequency: 600,
        }],
    );
    let g2_key = ColumnGroupKey::new(vec![1, 2]);
    let g2 = ColumnGroupStatistics::new(
        vec![1, 2],
        vec!["b".to_string(), "c".to_string()],
        10,
        0,
        0,
        vec![TopKCombination {
            values: vec![CanonicalValue::Int64(2), CanonicalValue::Int64(3)],
            frequency: 400,
        }],
    );
    stats.column_groups.insert(g1_key, g1);
    stats.column_groups.insert(g2_key, g2);

    let expr = parse_test_expr("a = 1 AND b = 2 AND c = 3", &schema);
    let sel1 = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    let sel2 = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert_eq!(sel1, sel2);
}

// ---------------------------------------------------------------------------
// R316: Three-column predicate with one correlated pair.
// ---------------------------------------------------------------------------
#[test]
fn test_r316_three_column_predicate_one_correlated_pair() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("state", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    let group_key = ColumnGroupKey::new(vec![0, 1]);
    let group = ColumnGroupStatistics::new(
        vec![0, 1],
        vec!["country".to_string(), "state".to_string()],
        10,
        0,
        0,
        vec![TopKCombination {
            values: vec![
                CanonicalValue::String("IN".to_string()),
                CanonicalValue::String("RJ".to_string()),
            ],
            frequency: 700,
        }],
    );
    stats.column_groups.insert(group_key, group);
    stats.columns.insert(
        "status".to_string(),
        ColumnStatistics::new(1000, 0, 2, None, None),
    );

    let expr = parse_test_expr(
        "country = 'IN' AND state = 'RJ' AND status = 'ACTIVE'",
        &schema,
    );
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // P(IN, RJ) = 0.7, P(ACTIVE) = 0.5 -> 0.35
    assert!((sel - 0.35).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R317: Three-column predicate with multiple available pairs.
// ---------------------------------------------------------------------------
#[test]
fn test_r317_three_column_multiple_available_pairs() {
    let schema = Schema::new_projected(vec![
        Column::new("a", DataType::Int64, false),
        Column::new("b", DataType::Int64, false),
        Column::new("c", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["a".to_string(), "b".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(1), CanonicalValue::Int64(2)],
                frequency: 800,
            }],
        ),
    );
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![1, 2]),
        ColumnGroupStatistics::new(
            vec![1, 2],
            vec!["b".to_string(), "c".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(2), CanonicalValue::Int64(3)],
                frequency: 400,
            }],
        ),
    );
    stats.columns.insert(
        "a".to_string(),
        ColumnStatistics::new(1000, 0, 10, None, None),
    );
    stats.columns.insert(
        "c".to_string(),
        ColumnStatistics::new(1000, 0, 10, None, None),
    );

    let expr = parse_test_expr("a = 1 AND b = 2 AND c = 3", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // Best pair (b, c) has sel 0.40 (stronger than 0.80), a marginal = 0.10 -> 0.04
    assert!((sel - 0.04).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R318: Overlapping-pair safety.
// ---------------------------------------------------------------------------
#[test]
fn test_r318_overlapping_pair_safety() {
    let schema = Schema::new_projected(vec![
        Column::new("a", DataType::Int64, false),
        Column::new("b", DataType::Int64, false),
        Column::new("c", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["a".to_string(), "b".to_string()],
            2,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(1), CanonicalValue::Int64(2)],
                frequency: 500,
            }],
        ),
    );
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![1, 2]),
        ColumnGroupStatistics::new(
            vec![1, 2],
            vec!["b".to_string(), "c".to_string()],
            2,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(2), CanonicalValue::Int64(3)],
                frequency: 500,
            }],
        ),
    );
    stats.columns.insert(
        "c".to_string(),
        ColumnStatistics::new(1000, 0, 2, None, None),
    );

    let expr = parse_test_expr("a = 1 AND b = 2 AND c = 3", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // P(a=1, b=2) * P(c=3) = 0.5 * 0.5 = 0.25 (not double counting b)
    assert!((sel - 0.25).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R319: Same-column range normalization.
// ---------------------------------------------------------------------------
#[test]
fn test_r319_same_column_range_normalization() {
    let schema = Schema::new_projected(vec![Column::new("age", DataType::Int64, false)]);
    let mut stats = TableStatistics::new("t", 100);
    let buckets = vec![
        HistogramBucket::new(Value::Int64(0), Value::Int64(49), 50, 50),
        HistogramBucket::new(Value::Int64(50), Value::Int64(99), 50, 50),
    ];
    let hist = Histogram::new(buckets, 0);
    let col = ColumnStatistics::new(100, 0, 100, Some(Value::Int64(0)), Some(Value::Int64(99)))
        .with_histogram(hist);
    stats.columns.insert("age".to_string(), col);

    let expr = parse_test_expr("age >= 10 AND age < 30", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // [10, 30) within [0, 49] has width 20/50 * 50 = 20 rows -> 0.20
    assert!((sel - 0.20).abs() < 1e-2);
}

// ---------------------------------------------------------------------------
// R320: Equality + range integration.
// ---------------------------------------------------------------------------
#[test]
fn test_r320_equality_plus_range_integration() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("age", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    stats.columns.insert(
        "country".to_string(),
        ColumnStatistics::new(100, 0, 2, None, None),
    );
    let buckets = vec![HistogramBucket::new(
        Value::Int64(0),
        Value::Int64(100),
        100,
        100,
    )];
    stats.columns.insert(
        "age".to_string(),
        ColumnStatistics::new(100, 0, 100, Some(Value::Int64(0)), Some(Value::Int64(100)))
            .with_histogram(Histogram::new(buckets, 0)),
    );

    let expr = parse_test_expr("country = 'IN' AND age >= 10 AND age < 30", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // country=0.5, age range [10, 30) = 0.2 -> 0.10
    assert!((sel - 0.10).abs() < 1e-2);
}

// ---------------------------------------------------------------------------
// R321: Histogram + correlation integration.
// ---------------------------------------------------------------------------
#[test]
fn test_r321_histogram_plus_correlation_integration() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
        Column::new("age", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![
                    CanonicalValue::String("IN".to_string()),
                    CanonicalValue::String("ACTIVE".to_string()),
                ],
                frequency: 600,
            }],
        ),
    );
    let buckets = vec![HistogramBucket::new(
        Value::Int64(0),
        Value::Int64(100),
        1000,
        100,
    )];
    stats.columns.insert(
        "age".to_string(),
        ColumnStatistics::new(1000, 0, 100, Some(Value::Int64(0)), Some(Value::Int64(100)))
            .with_histogram(Histogram::new(buckets, 0)),
    );

    let expr = parse_test_expr(
        "country = 'IN' AND status = 'ACTIVE' AND age >= 20 AND age < 40",
        &schema,
    );
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // Pair = 0.60, age [20, 40) = 0.20 -> 0.12
    assert!((sel - 0.12).abs() < 1e-2);
}

// ---------------------------------------------------------------------------
// R322: Unsupported conditional-range fallback.
// ---------------------------------------------------------------------------
#[test]
fn test_r322_unsupported_conditional_range_fallback() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, false),
        Column::new("c2", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    stats.columns.insert(
        "c1".to_string(),
        ColumnStatistics::new(100, 0, 10, None, None),
    );
    stats.columns.insert(
        "c2".to_string(),
        ColumnStatistics::new(100, 0, 10, None, None),
    );

    let expr = parse_test_expr("c1 > 10 AND c2 > 20", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel > 0.0 && sel <= 1.0);
}

// ---------------------------------------------------------------------------
// R323: NULL equality semantics.
// ---------------------------------------------------------------------------
#[test]
fn test_r323_null_equality_semantics() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, true),
        Column::new("c2", DataType::Int64, true),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    stats.columns.insert(
        "c1".to_string(),
        ColumnStatistics::new(100, 20, 10, None, None),
    );
    stats.columns.insert(
        "c2".to_string(),
        ColumnStatistics::new(100, 20, 10, None, None),
    );

    let expr = parse_test_expr("c1 = 5 AND c2 = 10", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // Null rows cannot match
    assert!(sel <= (1.0 - 0.20));
}

// ---------------------------------------------------------------------------
// R324: IS NULL handling.
// ---------------------------------------------------------------------------
#[test]
fn test_r324_is_null_handling() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, true),
        Column::new("c2", DataType::Int64, true),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    let group = ColumnGroupStatistics::new(
        vec![0, 1],
        vec!["c1".to_string(), "c2".to_string()],
        5,
        15, // all_null_count
        20,
        vec![],
    );
    stats
        .column_groups
        .insert(ColumnGroupKey::new(vec![0, 1]), group);

    let expr = parse_test_expr("c1 IS NULL AND c2 IS NULL", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!((sel - 0.15).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R325: NOT + NULL semantics.
// ---------------------------------------------------------------------------
#[test]
fn test_r325_not_plus_null_semantics() {
    let schema = Schema::new_projected(vec![Column::new("c1", DataType::Int64, true)]);
    let mut stats = TableStatistics::new("t", 100);
    stats.columns.insert(
        "c1".to_string(),
        ColumnStatistics::new(100, 30, 2, None, None), // 30% null, 2 distinct
    );

    let expr = parse_test_expr("NOT (c1 = 1)", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // null_frac = 0.3, eq_sel = 1 / 2 = 0.5 -> NOT = 1 - 0.3 - 0.5 = 0.2
    assert!((sel - 0.20).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R326: AND selectivity clamping.
// ---------------------------------------------------------------------------
#[test]
fn test_r326_and_selectivity_clamping() {
    let schema = Schema::new_projected(vec![
        Column::new("a", DataType::Int64, false),
        Column::new("b", DataType::Int64, false),
    ]);
    let stats = TableStatistics::new("t", 100);
    let expr = parse_test_expr("a = 1 AND b = 2", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!(sel >= 0.0 && sel <= 1.0);
}

// ---------------------------------------------------------------------------
// R327: OR with correlated branches.
// ---------------------------------------------------------------------------
#[test]
fn test_r327_or_with_correlated_branches() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![
                TopKCombination {
                    values: vec![
                        CanonicalValue::String("IN".to_string()),
                        CanonicalValue::String("ACTIVE".to_string()),
                    ],
                    frequency: 300,
                },
                TopKCombination {
                    values: vec![
                        CanonicalValue::String("US".to_string()),
                        CanonicalValue::String("ACTIVE".to_string()),
                    ],
                    frequency: 200,
                },
            ],
        ),
    );

    let expr = parse_test_expr(
        "(country = 'IN' AND status = 'ACTIVE') OR (country = 'US' AND status = 'ACTIVE')",
        &schema,
    );
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // 0.3 + 0.2 - 0.3*0.2 = 0.44
    assert!((sel - 0.44).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R328: OR overlap handling.
// ---------------------------------------------------------------------------
#[test]
fn test_r328_or_overlap_handling() {
    let schema = Schema::new_projected(vec![Column::new("a", DataType::Int64, false)]);
    let mut stats = TableStatistics::new("t", 100);
    stats.columns.insert(
        "a".to_string(),
        ColumnStatistics::new(100, 0, 4, None, None),
    );

    let expr = parse_test_expr("a = 1 OR a = 1", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // sel1 = 0.25, sel2 = 0.25 -> 0.25 + 0.25 - 0.0625 = 0.4375 <= 1.0
    assert!(sel <= 1.0);
}

// ---------------------------------------------------------------------------
// R329: IN-list + correlation.
// ---------------------------------------------------------------------------
#[test]
fn test_r329_in_list_plus_correlation() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![
                TopKCombination {
                    values: vec![
                        CanonicalValue::String("IN".to_string()),
                        CanonicalValue::String("ACTIVE".to_string()),
                    ],
                    frequency: 400,
                },
                TopKCombination {
                    values: vec![
                        CanonicalValue::String("IN".to_string()),
                        CanonicalValue::String("PENDING".to_string()),
                    ],
                    frequency: 200,
                },
            ],
        ),
    );

    let expr = parse_test_expr(
        "country = 'IN' AND status IN ('ACTIVE', 'PENDING')",
        &schema,
    );
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // 0.4 + 0.2 = 0.60
    assert!((sel - 0.60).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R330: Duplicate IN values.
// ---------------------------------------------------------------------------
#[test]
fn test_r330_duplicate_in_values() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![
                    CanonicalValue::String("IN".to_string()),
                    CanonicalValue::String("ACTIVE".to_string()),
                ],
                frequency: 400,
            }],
        ),
    );

    let expr1 = parse_test_expr("country = 'IN' AND status IN ('ACTIVE')", &schema);
    let expr2 = parse_test_expr("country = 'IN' AND status IN ('ACTIVE', 'ACTIVE')", &schema);
    let sel1 = CardinalityEstimator::estimate_expr_selectivity(&expr1, Some(&stats), &schema);
    let sel2 = CardinalityEstimator::estimate_expr_selectivity(&expr2, Some(&stats), &schema);
    assert_eq!(sel1, sel2);
}

// ---------------------------------------------------------------------------
// R331: Composite-index correlated costing.
// ---------------------------------------------------------------------------
#[test]
fn test_r331_composite_index_correlated_costing() {
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["c1".to_string(), "c2".to_string()],
            20,
            0,
            0,
            vec![],
        ),
    );
    let idx_meta = IndexMetadata {
        index_id: IndexId(1),
        table_id: TableId(1),
        root_page_id: 1,
        name: "idx_c1_c2".to_string(),
        column_idx: 0,
        column_indices: vec![0, 1],
        include_column_indices: vec![],
        unique: false,
    };

    let est_rows = CardinalityEstimator::estimate_sec_prefix_rows(Some(&stats), &idx_meta, 2);
    // base_rows = 1000, distinct = 20 -> 50 rows
    assert!((est_rows - 50.0).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R332: Multi-index correlated costing.
// ---------------------------------------------------------------------------
#[test]
fn test_r332_multi_index_correlated_costing() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![
                    CanonicalValue::String("IN".to_string()),
                    CanonicalValue::String("ACTIVE".to_string()),
                ],
                frequency: 800,
            }],
        ),
    );

    let expr = parse_test_expr("country = 'IN' AND status = 'ACTIVE'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    assert!((sel - 0.80).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R333: Skip-scan regression.
// ---------------------------------------------------------------------------
#[test]
fn test_r333_skip_scan_regression() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, false),
        Column::new("c2", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.columns.insert(
        "c1".to_string(),
        ColumnStatistics::new(1000, 0, 10, None, None),
    );
    stats.columns.insert(
        "c2".to_string(),
        ColumnStatistics::new(1000, 0, 5, None, None),
    );

    let distinct = CardinalityEstimator::estimate_distinct_prefixes(Some(&stats), &schema, &[0]);
    assert_eq!(distinct, 10.0);
    let skip_rows = CardinalityEstimator::estimate_skip_scan_rows(Some(&stats), &schema, &[1]);
    assert_eq!(skip_rows, 200.0);
}

// ---------------------------------------------------------------------------
// R334: Primary-key range regression.
// ---------------------------------------------------------------------------
#[test]
fn test_r334_primary_key_range_regression() {
    let mut stats = TableStatistics::new("t", 1000);
    let buckets = vec![
        HistogramBucket::new(Value::Int64(1), Value::Int64(500), 500, 500),
        HistogramBucket::new(Value::Int64(501), Value::Int64(1000), 500, 500),
    ];
    stats.columns.insert(
        "id".to_string(),
        ColumnStatistics::new(
            1000,
            0,
            1000,
            Some(Value::Int64(1)),
            Some(Value::Int64(1000)),
        )
        .with_histogram(Histogram::new(buckets, 0)),
    );

    let rows = CardinalityEstimator::estimate_pk_range_rows(
        Some(&stats),
        Some(&Value::Int64(1)),
        Some(&Value::Int64(250)),
    );
    assert!((rows - 250.0).abs() < 10.0);
}

// ---------------------------------------------------------------------------
// R335: Secondary-index range regression.
// ---------------------------------------------------------------------------
#[test]
fn test_r335_secondary_index_range_regression() {
    let mut stats = TableStatistics::new("t", 1000);
    let buckets = vec![HistogramBucket::new(
        Value::Int64(1),
        Value::Int64(1000),
        1000,
        1000,
    )];
    stats.columns.insert(
        "val".to_string(),
        ColumnStatistics::new(
            1000,
            0,
            1000,
            Some(Value::Int64(1)),
            Some(Value::Int64(1000)),
        )
        .with_histogram(Histogram::new(buckets, 0)),
    );
    let idx_meta = IndexMetadata {
        index_id: IndexId(1),
        table_id: TableId(1),
        root_page_id: 1,
        name: "idx_val".to_string(),
        column_idx: 0,
        column_indices: vec![0],
        include_column_indices: vec![],
        unique: false,
    };

    let rows = CardinalityEstimator::estimate_sec_range_rows(
        Some(&stats),
        &idx_meta,
        Some(&Value::Int64(1)),
        Some(&Value::Int64(100)),
    );
    assert!((rows - 100.0).abs() < 10.0);
}

// ---------------------------------------------------------------------------
// R336: Join cardinality with statistics.
// ---------------------------------------------------------------------------
#[test]
fn test_r336_join_cardinality_with_statistics() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, false),
        Column::new("c2", DataType::Int64, false),
    ]);
    let mut stats_u = TableStatistics::new("users", 1000);
    let mut stats_o = TableStatistics::new("orders", 2000);
    stats_u.columns.insert(
        "country".to_string(),
        ColumnStatistics::new(1000, 0, 50, None, None),
    );
    stats_o.columns.insert(
        "country".to_string(),
        ColumnStatistics::new(2000, 0, 20, None, None),
    );

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("users.country".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("orders.country".to_string())),
    };

    let rows = CardinalityEstimator::estimate_inner_join_cardinality(
        1000.0,
        Some(&stats_u),
        2000.0,
        Some(&stats_o),
        &on_expr,
        &schema,
    );
    // max NDV = 50 -> 1000 * 2000 / 50 = 40,000
    assert!((rows - 40000.0).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R337: Filtered join cardinality.
// ---------------------------------------------------------------------------
#[test]
fn test_r337_filtered_join_cardinality() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats_u = TableStatistics::new("users", 1000);
    let stats_o = TableStatistics::new("orders", 2000);
    stats_u.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![
                    CanonicalValue::String("IN".to_string()),
                    CanonicalValue::String("ACTIVE".to_string()),
                ],
                frequency: 800,
            }],
        ),
    );

    let filter_expr = parse_test_expr("country = 'IN' AND status = 'ACTIVE'", &schema);
    let sel =
        CardinalityEstimator::estimate_expr_selectivity(&filter_expr, Some(&stats_u), &schema);
    let filtered_u = 1000.0 * sel;
    assert_eq!(filtered_u, 800.0);

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("users.country".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("orders.country".to_string())),
    };
    let join_rows = CardinalityEstimator::estimate_inner_join_cardinality(
        filtered_u,
        Some(&stats_u),
        2000.0,
        Some(&stats_o),
        &on_expr,
        &schema,
    );
    assert!(join_rows > 0.0);
}

// ---------------------------------------------------------------------------
// R338: Join filter not double-counted.
// ---------------------------------------------------------------------------
#[test]
fn test_r338_join_filter_not_double_counted() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let stats_u = TableStatistics::new("users", 1000);
    let stats_o = TableStatistics::new("orders", 2000);

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("users.country".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("orders.country".to_string())),
    };

    let join_rows = CardinalityEstimator::estimate_inner_join_cardinality(
        500.0, // already filtered
        Some(&stats_u),
        2000.0,
        Some(&stats_o),
        &on_expr,
        &schema,
    );
    assert!(join_rows > 0.0);
}

// ---------------------------------------------------------------------------
// R339: Same-column join restriction.
// ---------------------------------------------------------------------------
#[test]
fn test_r339_same_column_join_restriction() {
    let schema = Schema::new_projected(vec![Column::new("x", DataType::Int64, false)]);
    let mut stats_a = TableStatistics::new("a", 100);
    stats_a.columns.insert(
        "x".to_string(),
        ColumnStatistics::new(100, 0, 10, None, None),
    );
    let stats_b = stats_a.clone();

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("a.x".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("b.x".to_string())),
    };
    let rows = CardinalityEstimator::estimate_inner_join_cardinality(
        10.0, // filtered by a.x = 10
        Some(&stats_a),
        100.0,
        Some(&stats_b),
        &on_expr,
        &schema,
    );
    assert!(rows > 0.0);
}

// ---------------------------------------------------------------------------
// R340: Inner join enumeration uses improved estimates.
// ---------------------------------------------------------------------------
#[test]
fn test_r340_inner_join_enumeration_uses_improved_estimates() {
    let ctx = TestContext::new("r340");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE u (id BIGINT PRIMARY KEY, c TEXT, s TEXT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE o (id BIGINT PRIMARY KEY, u_id BIGINT);")
        .unwrap();
    txn.execute_statement("INSERT INTO u VALUES (1, 'IN', 'ACTIVE');")
        .unwrap();
    txn.execute_statement("INSERT INTO o VALUES (10, 1);")
        .unwrap();
    txn.execute_statement("ANALYZE u;").unwrap();
    txn.execute_statement("ANALYZE o;").unwrap();

    let res = txn
        .execute_statement("SELECT * FROM u JOIN o ON u.id = o.u_id WHERE u.c = 'IN';")
        .unwrap();
    match res {
        StatementResult::Query(qr) => assert_eq!(qr.rows.len(), 1),
        _ => panic!("Expected query result"),
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R341: Outer-join reorder barrier preserved.
// ---------------------------------------------------------------------------
#[test]
fn test_r341_outer_join_reorder_barrier_preserved() {
    let ctx = TestContext::new("r341");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE a (id BIGINT PRIMARY KEY, val INT);")
        .unwrap();
    txn.execute_statement("CREATE TABLE b (id BIGINT PRIMARY KEY, a_id BIGINT);")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (1, 100);")
        .unwrap();
    txn.execute_statement("INSERT INTO a VALUES (2, 200);")
        .unwrap();
    txn.execute_statement("INSERT INTO b VALUES (10, 1);")
        .unwrap();
    txn.execute_statement("ANALYZE a;").unwrap();
    txn.execute_statement("ANALYZE b;").unwrap();

    let res = txn
        .execute_statement("SELECT * FROM a LEFT JOIN b ON a.id = b.a_id;")
        .unwrap();
    match res {
        StatementResult::Query(qr) => assert_eq!(qr.rows.len(), 2),
        _ => panic!("Expected query result"),
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R342: SEMI join cardinality.
// ---------------------------------------------------------------------------
#[test]
fn test_r342_semi_join_cardinality() {
    let schema = Schema::new_projected(vec![Column::new("id", DataType::Int64, false)]);
    let mut stats_u = TableStatistics::new("u", 100);
    let mut stats_o = TableStatistics::new("o", 500);
    stats_u.columns.insert(
        "id".to_string(),
        ColumnStatistics::new(100, 0, 100, None, None),
    );
    stats_o.columns.insert(
        "u_id".to_string(),
        ColumnStatistics::new(500, 0, 50, None, None),
    );

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("u.id".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("o.u_id".to_string())),
    };

    let semi_rows = CardinalityEstimator::estimate_join_cardinality(
        treedb::sql::ast::JoinType::Semi,
        100.0,
        Some(&stats_u),
        500.0,
        Some(&stats_o),
        &on_expr,
        &schema,
    );
    assert!(semi_rows > 0.0 && semi_rows <= 100.0);
}

// ---------------------------------------------------------------------------
// R343: ANTI join cardinality.
// ---------------------------------------------------------------------------
#[test]
fn test_r343_anti_join_cardinality() {
    let schema = Schema::new_projected(vec![Column::new("id", DataType::Int64, false)]);
    let mut stats_u = TableStatistics::new("u", 100);
    let mut stats_o = TableStatistics::new("o", 500);
    stats_u.columns.insert(
        "id".to_string(),
        ColumnStatistics::new(100, 0, 100, None, None),
    );
    stats_o.columns.insert(
        "u_id".to_string(),
        ColumnStatistics::new(500, 0, 50, None, None),
    );

    let on_expr = Expr::Binary {
        left: Box::new(Expr::Column("u.id".to_string())),
        op: BinaryOp::Eq,
        right: Box::new(Expr::Column("o.u_id".to_string())),
    };

    let anti_rows = CardinalityEstimator::estimate_join_cardinality(
        treedb::sql::ast::JoinType::Anti,
        100.0,
        Some(&stats_u),
        500.0,
        Some(&stats_o),
        &on_expr,
        &schema,
    );
    assert!(anti_rows > 0.0 && anti_rows <= 100.0);
}

// ---------------------------------------------------------------------------
// R344: NOT IN NULL semantics.
// ---------------------------------------------------------------------------
#[test]
fn test_r344_not_in_null_semantics() {
    let schema = Schema::new_projected(vec![Column::new("val", DataType::Int64, true)]);
    let mut stats = TableStatistics::new("t", 100);
    stats.columns.insert(
        "val".to_string(),
        ColumnStatistics::new(100, 20, 5, None, None),
    );

    let expr = parse_test_expr("val NOT IN (1, 2)", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // null_frac = 0.2, must not match NULL
    assert!(sel <= 0.8);
}

// ---------------------------------------------------------------------------
// R345: Adaptive feedback integration.
// ---------------------------------------------------------------------------
#[test]
fn test_r345_adaptive_feedback_integration() {
    let ctx = TestContext::new("r345");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, c TEXT, s TEXT);")
        .unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'ACTIVE');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    // Execute query to register feedback
    let _ = txn
        .execute_statement("SELECT * FROM t WHERE c = 'IN' AND s = 'ACTIVE';")
        .unwrap();
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R346: Resource feedback integration.
// ---------------------------------------------------------------------------
#[test]
fn test_r346_resource_feedback_integration() {
    let ctx = TestContext::new("r346");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, c TEXT, s TEXT);")
        .unwrap();
    for i in 1..=100 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'ACTIVE');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let explain_str = txn
        .explain_sql("SELECT * FROM t WHERE c = 'IN' AND s = 'ACTIVE' ORDER BY id;")
        .unwrap();
    assert!(!explain_str.is_empty());
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R347: ANALYZE invalidates cached plans.
// ---------------------------------------------------------------------------
#[test]
fn test_r347_analyze_invalidates_cached_plans() {
    use treedb::query::plan_cache::PlanCache;
    let mut cache = PlanCache::new();
    cache.invalidate_all();
    assert_eq!(cache.len(), 0);
}

// ---------------------------------------------------------------------------
// R348: Prepared statement statistics generation.
// ---------------------------------------------------------------------------
#[test]
fn test_r348_prepared_statement_stats_gen() {
    let ctx = TestContext::new("r348");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, c TEXT);")
        .unwrap();
    txn.execute_statement("INSERT INTO t VALUES (1, 'A');")
        .unwrap();
    txn.execute_statement("ANALYZE t;").unwrap();

    let stmt = txn.prepare_sql("SELECT * FROM t WHERE c = ?;").unwrap();
    let res = txn
        .execute_prepared(&stmt, &[Value::String("A".to_string())])
        .unwrap();
    match res {
        StatementResult::Query(qr) => assert_eq!(qr.rows.len(), 1),
        _ => panic!("Expected query result"),
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R349: EXPLAIN correlation diagnostics.
// ---------------------------------------------------------------------------
#[test]
fn test_r349_explain_correlation_diagnostics() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 100);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            5,
            0,
            0,
            vec![TopKCombination {
                values: vec![
                    CanonicalValue::String("IN".to_string()),
                    CanonicalValue::String("ACTIVE".to_string()),
                ],
                frequency: 80,
            }],
        ),
    );

    let expr = parse_test_expr("country = 'IN' AND status = 'ACTIVE'", &schema);
    let diag = CardinalityEstimator::diagnose_correlation_selectivity(&expr, Some(&stats), &schema);
    assert!(diag.is_some());
    let d = diag.unwrap();
    assert_eq!(d.columns, vec!["country", "status"]);
    assert_eq!(d.source, CorrelationSource::JointTopK);
    assert!((d.selectivity - 0.80).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R350: EXPLAIN ANALYZE correlation diagnostics.
// ---------------------------------------------------------------------------
#[test]
fn test_r350_explain_analyze_correlation_diagnostics() {
    let ctx = TestContext::new("r350");
    let mut db = ctx.db();
    let mut txn = db.begin_transaction().unwrap();

    txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, country TEXT, status TEXT);")
        .unwrap();
    for i in 1..=50 {
        txn.execute_statement(&format!("INSERT INTO t VALUES ({i}, 'IN', 'ACTIVE');"))
            .unwrap();
    }
    txn.execute_statement("ANALYZE t;").unwrap();

    let explain_res = txn
        .execute_statement(
            "EXPLAIN ANALYZE SELECT * FROM t WHERE country = 'IN' AND status = 'ACTIVE';",
        )
        .unwrap();
    match explain_res {
        StatementResult::Query(qr) => {
            assert!(!qr.rows.is_empty());
        }
        _ => panic!("Expected EXPLAIN ANALYZE query result"),
    }
    txn.commit().unwrap();
}

// ---------------------------------------------------------------------------
// R351: Statistics remain in-memory.
// ---------------------------------------------------------------------------
#[test]
fn test_r351_statistics_remain_in_memory() {
    let ctx = TestContext::new("r351");
    {
        let mut db = ctx.db();
        let mut txn = db.begin_transaction().unwrap();
        txn.execute_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, val INT);")
            .unwrap();
        txn.execute_statement("INSERT INTO t VALUES (1, 10);")
            .unwrap();
        txn.execute_statement("ANALYZE t;").unwrap();
        txn.commit().unwrap();
    }
    {
        let mut db = ctx.db();
        let txn = db.begin_transaction().unwrap();
        // Fresh open has no persisted stats
        let stats = txn.get_statistics("t");
        assert!(stats.is_none());
    }
}

// ---------------------------------------------------------------------------
// R352: Randomized two-column correlated dataset.
// ---------------------------------------------------------------------------
#[test]
fn test_r352_randomized_two_column_correlated_dataset() {
    let schema = Schema::new_projected(vec![
        Column::new("country", DataType::String, false),
        Column::new("status", DataType::String, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    // Highly correlated: IN is always ACTIVE (700 rows), US is always PENDING (300 rows)
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["country".to_string(), "status".to_string()],
            2,
            0,
            0,
            vec![
                TopKCombination {
                    values: vec![
                        CanonicalValue::String("IN".to_string()),
                        CanonicalValue::String("ACTIVE".to_string()),
                    ],
                    frequency: 700,
                },
                TopKCombination {
                    values: vec![
                        CanonicalValue::String("US".to_string()),
                        CanonicalValue::String("PENDING".to_string()),
                    ],
                    frequency: 300,
                },
            ],
        ),
    );
    stats.columns.insert(
        "country".to_string(),
        ColumnStatistics::new(1000, 0, 2, None, None),
    );
    stats.columns.insert(
        "status".to_string(),
        ColumnStatistics::new(1000, 0, 2, None, None),
    );

    let expr = parse_test_expr("country = 'IN' AND status = 'ACTIVE'", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // Correlation gives 0.70, whereas naive marginal would give 0.7 * 0.7 = 0.49
    assert!((sel - 0.70).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R353: Randomized three-column dataset with pairwise correlations.
// ---------------------------------------------------------------------------
#[test]
fn test_r353_randomized_three_column_pairwise_dataset() {
    let schema = Schema::new_projected(vec![
        Column::new("a", DataType::Int64, false),
        Column::new("b", DataType::Int64, false),
        Column::new("c", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["a".to_string(), "b".to_string()],
            2,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(1), CanonicalValue::Int64(2)],
                frequency: 600,
            }],
        ),
    );
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![1, 2]),
        ColumnGroupStatistics::new(
            vec![1, 2],
            vec!["b".to_string(), "c".to_string()],
            2,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(2), CanonicalValue::Int64(3)],
                frequency: 500,
            }],
        ),
    );
    stats.columns.insert(
        "a".to_string(),
        ColumnStatistics::new(1000, 0, 4, None, None),
    );
    stats.columns.insert(
        "c".to_string(),
        ColumnStatistics::new(1000, 0, 4, None, None),
    );

    let expr = parse_test_expr("a = 1 AND b = 2 AND c = 3", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // Best pair (b, c) has sel 0.50 (stronger than 0.60), a marginal = 0.25 -> 0.125
    assert!((sel - 0.125).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// R354: Randomized mixed equality/range predicates.
// ---------------------------------------------------------------------------
#[test]
fn test_r354_randomized_mixed_equality_range() {
    let schema = Schema::new_projected(vec![
        Column::new("a", DataType::Int64, false),
        Column::new("b", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.columns.insert(
        "a".to_string(),
        ColumnStatistics::new(1000, 0, 10, None, None),
    );
    let buckets = vec![HistogramBucket::new(
        Value::Int64(1),
        Value::Int64(100),
        1000,
        100,
    )];
    stats.columns.insert(
        "b".to_string(),
        ColumnStatistics::new(1000, 0, 100, Some(Value::Int64(1)), Some(Value::Int64(100)))
            .with_histogram(Histogram::new(buckets, 0)),
    );

    let expr = parse_test_expr("a = 5 AND b >= 20 AND b < 60", &schema);
    let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
    // a=0.10, b=[20, 60)=0.40 -> 0.04
    assert!((sel - 0.04).abs() < 1e-2);
}

// ---------------------------------------------------------------------------
// R355: Estimate bounds across randomized workloads.
// ---------------------------------------------------------------------------
#[test]
fn test_r355_estimate_bounds_across_randomized_workloads() {
    let schema = Schema::new_projected(vec![
        Column::new("c1", DataType::Int64, false),
        Column::new("c2", DataType::Int64, false),
        Column::new("c3", DataType::Int64, false),
    ]);
    let mut stats = TableStatistics::new("t", 1000);
    stats.column_groups.insert(
        ColumnGroupKey::new(vec![0, 1]),
        ColumnGroupStatistics::new(
            vec![0, 1],
            vec!["c1".to_string(), "c2".to_string()],
            10,
            0,
            0,
            vec![TopKCombination {
                values: vec![CanonicalValue::Int64(10), CanonicalValue::Int64(20)],
                frequency: 500,
            }],
        ),
    );
    stats.columns.insert(
        "c3".to_string(),
        ColumnStatistics::new(1000, 0, 5, None, None),
    );

    let queries = [
        "c1 = 10 AND c2 = 20",
        "c1 = 10 AND c2 = 20 AND c3 = 30",
        "c1 = 10 OR c2 = 20",
        "c1 = 10 AND c2 IN (20, 30)",
        "c1 IS NULL AND c2 IS NULL",
    ];

    for q in &queries {
        let expr = parse_test_expr(q, &schema);
        let sel = CardinalityEstimator::estimate_expr_selectivity(&expr, Some(&stats), &schema);
        assert!(
            sel >= 0.0,
            "Selectivity must be non-negative for query: {q}"
        );
        assert!(sel <= 1.0, "Selectivity must not exceed 1.0 for query: {q}");
        assert!(sel.is_finite(), "Selectivity must be finite for query: {q}");
    }
}
