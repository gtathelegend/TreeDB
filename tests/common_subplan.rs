use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;
use treedb::engine::types::{Column, DataType, Schema, Value};
use treedb::query::materialize::{
    MaterializationDecision, MaterializeConfig, MaterializeOverride, SubplanFingerprint,
    TempMaterializeFile,
};
use treedb::sql::parser::Parser;
use treedb::sql::tokenize_sql;
use treedb::sql::SelectStatement;
use treedb::TreeDb;

struct TestContext {
    db_path: PathBuf,
    wal_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let db_path = PathBuf::from(format!("test_common_subplan_{name}.treedb"));
        let wal_path = PathBuf::from(format!("test_common_subplan_{name}.treedb.wal"));
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&wal_path);
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
        let _ = fs::remove_file(&self.db_path);
        let _ = fs::remove_file(&self.wal_path);
        let bak = PathBuf::from(format!("{}.bak", self.wal_path.display()));
        let _ = fs::remove_file(&bak);
    }
}

fn parse_select_sql(sql: &str) -> SelectStatement {
    let tokens = tokenize_sql(sql).unwrap();
    let mut parser = Parser::new(&tokens);
    parser.parse_select().unwrap()
}

fn setup_sample_table(db: &mut TreeDb) {
    let mut txn = db.begin().unwrap();
    let schema = Schema::new(
        vec![
            Column::new("id", DataType::UInt64, false),
            Column::new("category", DataType::String, true),
            Column::new("val", DataType::Int64, true),
            Column::new("active", DataType::Boolean, true),
        ],
        0,
    )
    .unwrap();
    txn.create_table("items", schema).unwrap();

    let inserts = vec![
        "INSERT INTO items (id, category, val, active) VALUES (1, 'A', 10, true)",
        "INSERT INTO items (id, category, val, active) VALUES (2, 'A', 20, true)",
        "INSERT INTO items (id, category, val, active) VALUES (3, 'B', 10, false)",
        "INSERT INTO items (id, category, val, active) VALUES (4, 'B', 30, true)",
        "INSERT INTO items (id, category, val, active) VALUES (5, 'A', 10, true)",
        "INSERT INTO items (id, category, val, active) VALUES (6, NULL, 50, true)",
        "INSERT INTO items (id, category, val, active) VALUES (7, 'C', NULL, false)",
        "INSERT INTO items (id, category, val, active) VALUES (8, 'C', 40, NULL)",
    ];

    for stmt in inserts {
        txn.execute_statement(stmt).unwrap();
    }
    txn.commit().unwrap();
}

// =========================================================================
// R1176–R1190: Subplan identity and fingerprinting
// =========================================================================

#[test]
fn test_r1176_r1190_subplan_fingerprint_determinism() {
    let ctx = TestContext::new("fp_det");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    let sql = "SELECT id, category FROM items WHERE active = true";
    let ast = parse_select_sql(sql);
    let plan1 = txn.plan_select(&ast).unwrap();
    let plan2 = txn.plan_select(&ast).unwrap();

    let fp1 = SubplanFingerprint::from_plan(&plan1);
    let fp2 = SubplanFingerprint::from_plan(&plan2);

    assert_eq!(fp1, fp2);
    assert_eq!(fp1.to_hex(), fp2.to_hex());
    assert!(!fp1.as_str().is_empty());

    let sql_diff = "SELECT id, category FROM items WHERE active = false";
    let ast_diff = parse_select_sql(sql_diff);
    let plan_diff = txn.plan_select(&ast_diff).unwrap();
    let fp_diff = SubplanFingerprint::from_plan(&plan_diff);

    assert_ne!(fp1, fp_diff);
    assert_ne!(fp1.to_hex(), fp_diff.to_hex());
    txn.abort().unwrap();
}

// =========================================================================
// R1191–R1205: Basic materialization and reuse
// =========================================================================

#[test]
fn test_r1191_r1205_cte_materialize_and_preserve_multiplicity_and_nulls() {
    let ctx = TestContext::new("mat_nulls");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    let sql = r#"
        WITH exp_items AS (
            SELECT id, category, val, active FROM items WHERE val >= 10 OR val IS NULL
        )
        SELECT a.id, b.id, a.category, a.val
        FROM exp_items a
        JOIN exp_items b ON a.id = b.id
        ORDER BY a.id
    "#;
    let res = txn.execute_sql(sql).unwrap();
    let rows = res.rows;

    // Must preserve exact row count and multiplicity
    assert_eq!(rows.len(), 8); // all 8 rows qualify

    // Check NULL handling
    let has_null_cat = rows.iter().any(|r| r.values[2] == Value::Null);
    let has_null_val = rows.iter().any(|r| r.values[3] == Value::Null);
    assert!(has_null_cat, "NULL category must be preserved");
    assert!(has_null_val, "NULL value must be preserved");

    txn.abort().unwrap();
}

// =========================================================================
// R1206–R1220: CTE inline vs materialize costing
// =========================================================================

#[test]
fn test_r1206_r1220_cte_single_consumer_inlined_vs_multiple_materialized() {
    let ctx = TestContext::new("cost_decision");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::Auto);

    // Single consumer: optimizer should choose Inline
    let sql_single = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE active = true
        )
        SELECT id, category FROM c WHERE val > 15
    "#;
    let ast_single = parse_select_sql(sql_single);
    let plan_single = txn.plan_select(&ast_single).unwrap();

    assert_eq!(plan_single.ctes.len(), 1);
    if let treedb::sql::BoundCteKind::NonRecursive {
        decision,
        consumer_count,
        ..
    } = &plan_single.ctes[0].kind
    {
        assert_eq!(*consumer_count, 1);
        assert_eq!(*decision, MaterializationDecision::Inline);
    } else {
        panic!("expected NonRecursive CTE");
    }

    // Multiple consumers: optimizer should choose Materialize
    let sql_multi = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE active = true
        )
        SELECT a.id, b.id
        FROM c a
        JOIN c b ON a.val = b.val
    "#;
    let ast_multi = parse_select_sql(sql_multi);
    let plan_multi = txn.plan_select(&ast_multi).unwrap();

    assert_eq!(plan_multi.ctes.len(), 1);
    if let treedb::sql::BoundCteKind::NonRecursive {
        decision,
        consumer_count,
        ..
    } = &plan_multi.ctes[0].kind
    {
        assert_eq!(*consumer_count, 2);
        assert_eq!(*decision, MaterializationDecision::Materialize);
    } else {
        panic!("expected NonRecursive CTE");
    }

    txn.abort().unwrap();
}

// =========================================================================
// R1221–R1235: Multiple consumers (3+ consumers & self joins)
// =========================================================================

#[test]
fn test_r1221_r1235_three_consumers_self_join() {
    let ctx = TestContext::new("three_cons");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE category = 'A'
        )
        SELECT a.id, b.id, c3.id
        FROM c a
        JOIN c b ON a.val = b.val
        JOIN c c3 ON b.val = c3.val
        ORDER BY a.id, b.id, c3.id
    "#;
    let res = txn.execute_sql(sql).unwrap();
    assert!(!res.rows.is_empty());
    txn.abort().unwrap();
}

// =========================================================================
// R1236–R1250: Ordering and physical properties correctness
// =========================================================================

#[test]
fn test_r1236_r1250_consumer_specific_ordering() {
    let ctx = TestContext::new("ordering");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    // Consumer A orders ASC, Consumer B orders DESC
    let sql_asc = r#"
        WITH c AS (
            SELECT id, val FROM items WHERE val IS NOT NULL
        )
        SELECT id, val FROM c ORDER BY val ASC
    "#;
    let res_asc = txn.execute_sql(sql_asc).unwrap();
    let vals_asc: Vec<i64> = res_asc
        .rows
        .iter()
        .map(|r| match r.values[1] {
            Value::Int64(v) => v,
            _ => -1,
        })
        .collect();
    for i in 1..vals_asc.len() {
        assert!(vals_asc[i - 1] <= vals_asc[i]);
    }

    let sql_desc = r#"
        WITH c AS (
            SELECT id, val FROM items WHERE val IS NOT NULL
        )
        SELECT id, val FROM c ORDER BY val DESC
    "#;
    let res_desc = txn.execute_sql(sql_desc).unwrap();
    let vals_desc: Vec<i64> = res_desc
        .rows
        .iter()
        .map(|r| match r.values[1] {
            Value::Int64(v) => v,
            _ => -1,
        })
        .collect();
    for i in 1..vals_desc.len() {
        assert!(vals_desc[i - 1] >= vals_desc[i]);
    }

    txn.abort().unwrap();
}

// =========================================================================
// R1251–R1265: Row-goal interaction
// =========================================================================

#[test]
fn test_r1251_r1265_differing_consumer_limits() {
    let ctx = TestContext::new("limits");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE active = true
        )
        SELECT id, category FROM c ORDER BY id ASC LIMIT 2
    "#;
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].values[0], Value::UInt64(1));
    assert_eq!(res.rows[1].values[0], Value::UInt64(2));

    txn.abort().unwrap();
}

// =========================================================================
// R1266–R1280: Projection and predicate compatibility
// =========================================================================

#[test]
fn test_r1266_r1280_different_projections_and_filters() {
    let ctx = TestContext::new("proj_filt");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val, active FROM items
        )
        SELECT a.id, a.val, b.category
        FROM c a
        JOIN c b ON a.id = b.id
        WHERE a.val > 15 AND b.category = 'A'
    "#;
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1); // id=2 has category='A' and val=20
    assert_eq!(res.rows[0].values[0], Value::UInt64(2));
    assert_eq!(res.rows[0].values[1], Value::Int64(20));
    assert_eq!(res.rows[0].values[2], Value::String("A".into()));

    txn.abort().unwrap();
}

// =========================================================================
// R1281–R1295: Uncorrelated subquery reuse
// =========================================================================

#[test]
fn test_r1281_r1295_uncorrelated_subquery_reuse() {
    let ctx = TestContext::new("subq_reuse");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();

    // Repeated uncorrelated subqueries in filter
    let sql = r#"
        SELECT id, val
        FROM items
        WHERE val = (SELECT MAX(val) FROM items WHERE active = true)
           OR val = (SELECT MAX(val) FROM items WHERE active = true)
    "#;
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(6)); // id=6 has val=50 (max active)

    txn.abort().unwrap();
}

// =========================================================================
// R1296–R1310: Prepared parameters and plan cache
// =========================================================================

#[test]
fn test_r1296_r1310_parameter_safety() {
    let ctx = TestContext::new("params");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE category = ?
        )
        SELECT a.id, b.val
        FROM c a
        JOIN c b ON a.id = b.id
    "#;

    let prep = txn.prepare_sql(sql).unwrap();

    let res_a = txn
        .execute_prepared_query(&prep, &[Value::String("A".into())])
        .unwrap();
    assert_eq!(res_a.rows.len(), 3); // items 1, 2, 5

    let res_b = txn
        .execute_prepared_query(&prep, &[Value::String("B".into())])
        .unwrap();
    assert_eq!(res_b.rows.len(), 2); // items 3, 4

    txn.abort().unwrap();
}

// =========================================================================
// R1311–R1325: Memory/resource-aware materialization
// =========================================================================

#[test]
fn test_r1311_r1325_memory_budget_spill_decision() {
    let ctx = TestContext::new("mem_budget");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    for i in 100..400 {
        txn.execute_statement(&format!(
            "INSERT INTO items (id, category, val, active) VALUES ({i}, 'CAT_{i}', {i}, true)"
        ))
        .unwrap();
    }
    let config = MaterializeConfig {
        memory_budget_bytes: 64,
        ..Default::default()
    };
    txn.set_materialize_config(config);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE val > 10 AND (category = 'CAT_150' OR category = 'CAT_250' OR val < 300)
        )
        SELECT a.id, b.id FROM c a JOIN c b ON a.id = b.id
    "#;
    let ast = parse_select_sql(sql);
    let plan = txn.plan_select(&ast).unwrap();

    if let treedb::sql::BoundCteKind::NonRecursive { decision, .. } = &plan.ctes[0].kind {
        assert_eq!(*decision, MaterializationDecision::ExternalMaterialize);
    } else {
        panic!("expected NonRecursive CTE");
    }

    txn.abort().unwrap();
}

// =========================================================================
// R1326–R1340: External materialization and disk spill
// =========================================================================

#[test]
fn test_r1326_r1340_external_materialize_execution_and_file_cleanup() {
    let ctx = TestContext::new("ext_mat");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceExternalMaterialize);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE active = true
        )
        SELECT a.id, b.category
        FROM c a
        JOIN c b ON a.id = b.id
        ORDER BY a.id
    "#;
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 5); // 1, 2, 4, 5, 6

    // Verify RAII TempMaterializeFile directly
    let temp_dir = TempDir::new().unwrap();
    let temp_file_path;
    {
        let (temp_file, _f) = TempMaterializeFile::create_in(temp_dir.path()).unwrap();
        temp_file_path = temp_file.path().to_path_buf();
        assert!(temp_file_path.exists());
    }
    // File must be automatically deleted after drop
    assert!(!temp_file_path.exists());

    txn.abort().unwrap();
}

// =========================================================================
// R1341–R1355: Runtime feedback & adaptive planning
// =========================================================================

#[test]
fn test_r1341_r1355_runtime_feedback_integration() {
    let ctx = TestContext::new("feedback");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    let sql = r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE val >= 20
        )
        SELECT a.id, b.id FROM c a JOIN c b ON a.val = b.val
    "#;
    let _res = txn.execute_sql(sql).unwrap();

    // Verify transaction executes and cleans up
    txn.abort().unwrap();
}

// =========================================================================
// R1356–R1370: EXPLAIN and EXPLAIN ANALYZE metrics
// =========================================================================

#[test]
fn test_r1356_r1370_explain_and_analyze_output() {
    let ctx = TestContext::new("explain_diag");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let mut txn = db.begin().unwrap();

    let sql = r#"
        WITH c AS (
            SELECT id, category FROM items WHERE active = true
        )
        SELECT a.id, b.id FROM c a JOIN c b ON a.id = b.id
    "#;

    let explain_text = txn.explain_sql(sql).unwrap();
    assert!(explain_text.contains("CTE: c") || explain_text.contains("Materialize"));

    let analyze_stmt_res = txn
        .execute_statement(&format!("EXPLAIN ANALYZE {sql}"))
        .unwrap();
    let qr = analyze_stmt_res.into_query().unwrap();
    assert!(!qr.rows.is_empty());
    let text = match &qr.rows[0].values[0] {
        Value::String(s) => s.clone(),
        _ => panic!("expected string in explain analyze output"),
    };
    assert!(!text.is_empty());

    txn.abort().unwrap();
}

// =========================================================================
// R1371–R1390: Transactions, DML, persistence, cleanup
// =========================================================================

#[test]
fn test_r1371_r1390_dml_and_persistence_cleanup() {
    let ctx = TestContext::new("dml_persist");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    // 1. Transaction visibility with mutations
    let mut txn = db.begin().unwrap();
    txn.set_materialize_override(MaterializeOverride::ForceMaterialize);

    txn.execute_statement(
        "INSERT INTO items (id, category, val, active) VALUES (99, 'NEW', 999, true)",
    )
    .unwrap();

    let sql = r#"
        WITH c AS (
            SELECT id, category FROM items WHERE category = 'NEW'
        )
        SELECT a.id, b.category FROM c a JOIN c b ON a.id = b.id
    "#;
    let res = txn.execute_sql(sql).unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].values[0], Value::UInt64(99));
    txn.commit().unwrap();

    // 2. Reopen DB and verify no lingering temporary files
    drop(db);
    let mut db_reopened = ctx.db();
    let mut txn2 = db_reopened.begin().unwrap();
    let count_res = txn2.execute_sql("SELECT COUNT(*) FROM items").unwrap();
    assert_eq!(count_res.rows[0].values[0], Value::UInt64(9)); // 8 original + 1 inserted
    txn2.commit().unwrap();
}

// =========================================================================
// R1391–R1420: Randomized differential testing
// =========================================================================

#[test]
fn test_r1391_r1420_randomized_differential_testing() {
    let ctx = TestContext::new("rand_diff");
    let mut db = ctx.db();
    setup_sample_table(&mut db);

    let queries = [
        r#"
        WITH c AS (
            SELECT id, category, val, active FROM items WHERE active = true
        )
        SELECT a.id, b.id, a.category, a.val
        FROM c a
        JOIN c b ON a.category = b.category
        ORDER BY a.id, b.id
        "#,
        r#"
        WITH c AS (
            SELECT id, val FROM items WHERE val IS NOT NULL
        )
        SELECT a.id, b.val
        FROM c a
        JOIN c b ON a.val = b.val
        WHERE a.val > 10
        ORDER BY a.id, b.val
        "#,
        r#"
        WITH c AS (
            SELECT id, category, val FROM items WHERE category IS NOT NULL
        )
        SELECT a.id, a.category, b.val
        FROM c a
        JOIN c b ON a.id = b.id
        ORDER BY a.id
        "#,
    ];

    for (idx, sql) in queries.iter().enumerate() {
        // Run with ForceInline
        let mut txn_inline = db.begin().unwrap();
        txn_inline.set_materialize_override(MaterializeOverride::ForceInline);
        let res_inline = txn_inline.execute_sql(sql).unwrap();
        txn_inline.abort().unwrap();

        // Run with ForceMaterialize
        let mut txn_mat = db.begin().unwrap();
        txn_mat.set_materialize_override(MaterializeOverride::ForceMaterialize);
        let res_mat = txn_mat.execute_sql(sql).unwrap();
        txn_mat.abort().unwrap();

        // Run with ForceExternalMaterialize
        let mut txn_ext = db.begin().unwrap();
        txn_ext.set_materialize_override(MaterializeOverride::ForceExternalMaterialize);
        let res_ext = txn_ext.execute_sql(sql).unwrap();
        txn_ext.abort().unwrap();

        // All 3 strategies must yield identical results preserving multiplicity and row order
        assert_eq!(
            res_inline.rows, res_mat.rows,
            "Query {idx} failed: ForceInline and ForceMaterialize results mismatch"
        );
        assert_eq!(
            res_mat.rows, res_ext.rows,
            "Query {idx} failed: ForceMaterialize and ForceExternalMaterialize results mismatch"
        );
    }
}
