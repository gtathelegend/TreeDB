use std::collections::HashMap;
use std::path::PathBuf;

use treedb::engine::types::{Row, Value};
use treedb::error::Result;
use treedb::query::reference::CanonicalRow;
use treedb::TreeDb;

// =============================================================================
// Section 16: Independent In-Memory MVCC Reference Model
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefTxnStatus {
    Active,
    Committed(u64), // CommitSeq
    Aborted,
}

#[derive(Debug, Clone)]
pub struct RefVersion {
    pub xmin: u64,
    pub xmax: Option<u64>,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct RefLogicalRecord {
    pub id: u64,
    pub versions: Vec<RefVersion>,
}

#[derive(Debug, Clone)]
pub struct RefSnapshot {
    pub read_commit_seq: u64,
    pub active_txns: Vec<u64>,
    pub self_txn_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefConflict {
    WriteWriteConflict,
    StaleVersionWrite,
    ConcurrentLogicalKeyChange,
    UniqueIndexConflict,
    InvalidTransactionState,
    WriteSetValidationFailure,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefWriteOp {
    Insert {
        pk: u64,
        values: Vec<Value>,
    },
    Update {
        pk: u64,
        target_version_xmin: Option<u64>,
        values: Vec<Value>,
    },
    Delete {
        pk: u64,
        target_version_xmin: Option<u64>,
    },
}

#[derive(Debug, Clone)]
pub struct RefTxnWriteIntent {
    pub ops: Vec<RefWriteOp>,
    pub snapshot_commit_seq: u64,
}

pub struct RefEngine {
    pub next_txn_id: u64,
    pub next_commit_seq: u64,
    pub txns: HashMap<u64, RefTxnStatus>,
    pub records: HashMap<u64, RefLogicalRecord>,
    pub write_intents: HashMap<u64, RefTxnWriteIntent>,
}

impl Default for RefEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl RefEngine {
    pub fn new() -> Self {
        Self {
            next_txn_id: 1,
            next_commit_seq: 1,
            txns: HashMap::new(),
            records: HashMap::new(),
            write_intents: HashMap::new(),
        }
    }

    pub fn begin_txn(&mut self) -> u64 {
        let tid = self.next_txn_id;
        self.next_txn_id += 1;
        self.txns.insert(tid, RefTxnStatus::Active);
        tid
    }

    pub fn commit_txn(&mut self, tid: u64) -> u64 {
        let seq = self.next_commit_seq;
        self.next_commit_seq += 1;
        self.txns.insert(tid, RefTxnStatus::Committed(seq));
        seq
    }

    pub fn abort_txn(&mut self, tid: u64) {
        self.txns.insert(tid, RefTxnStatus::Aborted);
    }

    pub fn crash_and_recover(&mut self) {
        // Any active transactions at crash time become aborted.
        for status in self.txns.values_mut() {
            if *status == RefTxnStatus::Active {
                *status = RefTxnStatus::Aborted;
            }
        }
    }

    pub fn make_snapshot(&self, self_tid: u64) -> RefSnapshot {
        // Read commit seq is the highest committed sequence number so far.
        let highest_commit_seq = self
            .txns
            .values()
            .filter_map(|s| match s {
                RefTxnStatus::Committed(seq) => Some(*seq),
                _ => None,
            })
            .max()
            .unwrap_or(0);

        let active: Vec<u64> = self
            .txns
            .iter()
            .filter_map(|(tid, s)| {
                if *tid != self_tid && *s == RefTxnStatus::Active {
                    Some(*tid)
                } else {
                    None
                }
            })
            .collect();

        RefSnapshot {
            read_commit_seq: highest_commit_seq,
            active_txns: active,
            self_txn_id: self_tid,
        }
    }

    /// Independent visibility evaluation without using production MvccVisibilityPolicy.
    pub fn is_version_visible(&self, v: &RefVersion, snap: &RefSnapshot) -> bool {
        Self::eval_version_visible(&self.txns, v, snap)
    }

    /// Associated visibility evaluator using only transaction state map.
    pub fn eval_version_visible(
        txns: &HashMap<u64, RefTxnStatus>,
        v: &RefVersion,
        snap: &RefSnapshot,
    ) -> bool {
        // 1. Evaluate creation (xmin)
        let created_visible = if v.xmin == snap.self_txn_id {
            true
        } else if snap.active_txns.contains(&v.xmin) {
            false
        } else {
            match txns.get(&v.xmin) {
                Some(RefTxnStatus::Committed(cseq)) => *cseq <= snap.read_commit_seq,
                _ => false,
            }
        };

        if !created_visible {
            return false;
        }

        // 2. Evaluate deletion/supersession (xmax)
        match v.xmax {
            None => true,
            Some(del_tid) => {
                if del_tid == snap.self_txn_id {
                    // Deleted by self -> not visible
                    false
                } else if snap.active_txns.contains(&del_tid) {
                    // Deletion uncommitted at snapshot cut -> row still visible
                    true
                } else {
                    match txns.get(&del_tid) {
                        Some(RefTxnStatus::Committed(del_cseq)) => *del_cseq > snap.read_commit_seq,
                        _ => {
                            // Deletion uncommitted or aborted -> still visible
                            true
                        }
                    }
                }
            }
        }
    }

    pub fn record_intent_insert(&mut self, tid: u64, pk: u64, values: Vec<Value>, snap_seq: u64) {
        let entry = self
            .write_intents
            .entry(tid)
            .or_insert_with(|| RefTxnWriteIntent {
                ops: Vec::new(),
                snapshot_commit_seq: snap_seq,
            });
        entry.ops.push(RefWriteOp::Insert { pk, values });
    }

    pub fn record_intent_update(
        &mut self,
        tid: u64,
        pk: u64,
        target_xmin: Option<u64>,
        values: Vec<Value>,
        snap_seq: u64,
    ) {
        let entry = self
            .write_intents
            .entry(tid)
            .or_insert_with(|| RefTxnWriteIntent {
                ops: Vec::new(),
                snapshot_commit_seq: snap_seq,
            });
        entry.ops.push(RefWriteOp::Update {
            pk,
            target_version_xmin: target_xmin,
            values,
        });
    }

    pub fn record_intent_delete(
        &mut self,
        tid: u64,
        pk: u64,
        target_xmin: Option<u64>,
        snap_seq: u64,
    ) {
        let entry = self
            .write_intents
            .entry(tid)
            .or_insert_with(|| RefTxnWriteIntent {
                ops: Vec::new(),
                snapshot_commit_seq: snap_seq,
            });
        entry.ops.push(RefWriteOp::Delete {
            pk,
            target_version_xmin: target_xmin,
        });
    }

    /// Independent reference commit validation: validates write set intents against latest committed state.
    pub fn validate_and_commit_txn(
        &mut self,
        tid: u64,
        unique_col_idx: Option<usize>,
    ) -> std::result::Result<u64, RefConflict> {
        // 1. Transaction state check
        match self.txns.get(&tid) {
            Some(RefTxnStatus::Active) => {}
            _ => return Err(RefConflict::InvalidTransactionState),
        }

        let intent = match self.write_intents.remove(&tid) {
            Some(i) => i,
            None => {
                // Empty transaction commits cleanly
                return Ok(self.commit_txn(tid));
            }
        };

        // 2. Validate operations against committed records
        // Determine final operation per PK in this transaction
        let mut final_ops: HashMap<u64, &RefWriteOp> = HashMap::new();
        for op in &intent.ops {
            match op {
                RefWriteOp::Insert { pk, .. }
                | RefWriteOp::Update { pk, .. }
                | RefWriteOp::Delete { pk, .. } => {
                    final_ops.insert(*pk, op);
                }
            }
        }

        for (pk, op) in &final_ops {
            let record = self.records.get(pk);
            let latest_committed_version = record.and_then(|rec| {
                rec.versions
                    .iter()
                    .rev()
                    .find(|v| matches!(self.txns.get(&v.xmin), Some(RefTxnStatus::Committed(_))))
            });

            match op {
                RefWriteOp::Insert { values, .. } => {
                    if let Some(comm_v) = latest_committed_version {
                        let is_deleted = comm_v.xmax.is_some()
                            && matches!(
                                self.txns.get(&comm_v.xmax.unwrap()),
                                Some(RefTxnStatus::Committed(_))
                            );
                        if !is_deleted {
                            return Err(RefConflict::ConcurrentLogicalKeyChange);
                        }
                    }
                    if let Some(col) = unique_col_idx {
                        if let Some(val) = values.get(col) {
                            if self.has_committed_unique_violation(*pk, col, val) {
                                return Err(RefConflict::UniqueIndexConflict);
                            }
                        }
                    }
                }
                RefWriteOp::Update {
                    target_version_xmin,
                    values,
                    ..
                } => {
                    match (target_version_xmin, latest_committed_version) {
                        (Some(target), Some(latest)) => {
                            if *target != latest.xmin {
                                return Err(RefConflict::WriteWriteConflict);
                            }
                            if latest.xmax.is_some()
                                && matches!(
                                    self.txns.get(&latest.xmax.unwrap()),
                                    Some(RefTxnStatus::Committed(_))
                                )
                            {
                                return Err(RefConflict::StaleVersionWrite);
                            }
                        }
                        (Some(_), None) => {
                            return Err(RefConflict::StaleVersionWrite);
                        }
                        (None, _) => {
                            // Intra-transaction insert then updated
                        }
                    }
                    if let Some(col) = unique_col_idx {
                        if let Some(val) = values.get(col) {
                            if self.has_committed_unique_violation(*pk, col, val) {
                                return Err(RefConflict::UniqueIndexConflict);
                            }
                        }
                    }
                }
                RefWriteOp::Delete {
                    target_version_xmin,
                    ..
                } => {
                    match (target_version_xmin, latest_committed_version) {
                        (Some(target), Some(latest)) => {
                            if *target != latest.xmin {
                                return Err(RefConflict::WriteWriteConflict);
                            }
                            if latest.xmax.is_some()
                                && matches!(
                                    self.txns.get(&latest.xmax.unwrap()),
                                    Some(RefTxnStatus::Committed(_))
                                )
                            {
                                return Err(RefConflict::StaleVersionWrite);
                            }
                        }
                        (Some(_), None) => {
                            return Err(RefConflict::StaleVersionWrite);
                        }
                        (None, _) => {
                            // Intra-transaction insert then deleted
                        }
                    }
                }
            }
        }

        // Apply all operations to records
        for op in intent.ops {
            match op {
                RefWriteOp::Insert { pk, values } => {
                    let _ = self.insert(tid, pk, values);
                }
                RefWriteOp::Update { pk, values, .. } => {
                    let _ = self.update(tid, pk, values);
                }
                RefWriteOp::Delete { pk, .. } => {
                    let _ = self.delete(tid, pk);
                }
            }
        }

        Ok(self.commit_txn(tid))
    }

    fn has_committed_unique_violation(&self, self_pk: u64, col_idx: usize, val: &Value) -> bool {
        for (&other_pk, rec) in &self.records {
            if other_pk == self_pk {
                continue;
            }
            if let Some(latest) = rec
                .versions
                .iter()
                .rev()
                .find(|v| matches!(self.txns.get(&v.xmin), Some(RefTxnStatus::Committed(_))))
            {
                let is_deleted = latest.xmax.is_some()
                    && matches!(
                        self.txns.get(&latest.xmax.unwrap()),
                        Some(RefTxnStatus::Committed(_))
                    );
                if !is_deleted && latest.values.get(col_idx) == Some(val) {
                    return true;
                }
            }
        }
        false
    }

    pub fn insert(&mut self, tid: u64, pk: u64, values: Vec<Value>) -> Result<()> {
        let entry = self.records.entry(pk).or_insert_with(|| RefLogicalRecord {
            id: pk,
            versions: Vec::new(),
        });
        entry.versions.push(RefVersion {
            xmin: tid,
            xmax: None,
            values,
        });
        Ok(())
    }

    pub fn update(&mut self, tid: u64, pk: u64, new_values: Vec<Value>) -> Result<()> {
        let entry = self
            .records
            .get_mut(&pk)
            .expect("record must exist for update");
        // Mark the latest version as deleted by tid
        if let Some(last) = entry.versions.last_mut() {
            last.xmax = Some(tid);
        }
        entry.versions.push(RefVersion {
            xmin: tid,
            xmax: None,
            values: new_values,
        });
        Ok(())
    }

    pub fn delete(&mut self, tid: u64, pk: u64) -> Result<()> {
        let entry = self
            .records
            .get_mut(&pk)
            .expect("record must exist for delete");
        if let Some(last) = entry.versions.last_mut() {
            last.xmax = Some(tid);
        }
        Ok(())
    }

    pub fn scan_visible(&self, snap: &RefSnapshot) -> Vec<Row> {
        let mut results = Vec::new();
        let mut pks: Vec<u64> = self.records.keys().copied().collect();
        pks.sort();
        for pk in pks {
            let log_rec = &self.records[&pk];
            // Find visible version (scanning newest to oldest, at most 1 visible version)
            for v in log_rec.versions.iter().rev() {
                if self.is_version_visible(v, snap) {
                    results.push(Row::new(v.values.clone()));
                    break;
                }
            }
        }
        results
    }

    pub fn index_lookup(&self, snap: &RefSnapshot, col_idx: usize, val: &Value) -> Vec<Row> {
        let all_rows = self.scan_visible(snap);
        all_rows
            .into_iter()
            .filter(|r| r.get(col_idx) == Some(val))
            .collect()
    }

    /// Derives the reclamation horizon based on active snapshots.
    pub fn reclamation_horizon(&self, active_snapshots: &[RefSnapshot]) -> u64 {
        if active_snapshots.is_empty() {
            self.txns
                .values()
                .filter_map(|s| match s {
                    RefTxnStatus::Committed(seq) => Some(*seq),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
        } else {
            active_snapshots
                .iter()
                .map(|s| s.read_commit_seq)
                .min()
                .unwrap_or(0)
        }
    }

    /// Independent reference vacuum:
    /// Reclaims versions that are provably unnecessary to any active snapshot and committed before horizon.
    pub fn vacuum(&mut self, active_snapshots: &[RefSnapshot]) -> RefVacuumResult {
        let horizon = self.reclamation_horizon(active_snapshots);
        let mut result = RefVacuumResult::default();
        let mut pks_to_remove = Vec::new();

        for (&pk, record) in self.records.iter_mut() {
            let total_versions = record.versions.len();
            result.versions_examined += total_versions as u64;
            let mut retained = Vec::new();

            for (idx, v) in record.versions.iter().enumerate() {
                let is_latest = idx + 1 == total_versions;

                // 1. Aborted transaction versions
                let is_aborted = matches!(self.txns.get(&v.xmin), Some(RefTxnStatus::Aborted));
                if is_aborted {
                    result.versions_reclaimed += 1;
                    continue;
                }

                // 2. Needed by any active snapshot?
                let needed_by_snap = active_snapshots
                    .iter()
                    .any(|s| Self::eval_version_visible(&self.txns, v, s));
                if needed_by_snap {
                    retained.push(v.clone());
                    continue;
                }

                // 3. Superseded / deleted before horizon
                if let Some(del_tid) = v.xmax {
                    if let Some(RefTxnStatus::Committed(del_seq)) = self.txns.get(&del_tid) {
                        if *del_seq <= horizon {
                            result.versions_reclaimed += 1;
                            continue;
                        }
                    }
                }

                // 4. Live head
                if is_latest && v.xmax.is_none() {
                    retained.push(v.clone());
                    continue;
                }

                retained.push(v.clone());
            }

            if retained.is_empty() {
                pks_to_remove.push(pk);
                result.records_reclaimed += 1;
            } else {
                record.versions = retained;
            }
        }

        for pk in pks_to_remove {
            self.records.remove(&pk);
        }

        result
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefVacuumResult {
    pub versions_examined: u64,
    pub versions_reclaimed: u64,
    pub records_reclaimed: u64,
}

// =============================================================================
// Deterministic Pseudo-Random Number Generator for Differential Testing
// =============================================================================

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        // 64-bit xorshift*
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        self.state.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn next_range(&mut self, min: u64, max: u64) -> u64 {
        if min >= max {
            return min;
        }
        min + (self.next_u64() % (max - min + 1))
    }
}

// Test Context
struct TestContext {
    _temp_dir: tempfile::TempDir,
    db_path: PathBuf,
}

impl TestContext {
    fn new(name: &str) -> Self {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join(format!("test_mvcc_ref_{name}.treedb"));
        Self {
            _temp_dir: temp_dir,
            db_path,
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

// Helper to assert equivalence between actual rows and reference rows
fn assert_rows_match(actual: &[Row], expected: &[Row], context_msg: &str) {
    let mut act_canon: Vec<CanonicalRow> = actual.iter().map(CanonicalRow::from_row).collect();
    let mut exp_canon: Vec<CanonicalRow> = expected.iter().map(CanonicalRow::from_row).collect();
    act_canon.sort();
    exp_canon.sort();

    assert_eq!(
        act_canon.len(),
        exp_canon.len(),
        "{}: Row count mismatch. Actual: {}, Expected: {}",
        context_msg,
        act_canon.len(),
        exp_canon.len()
    );

    for (i, (act, exp)) in act_canon.iter().zip(exp_canon.iter()).enumerate() {
        assert_eq!(
            act, exp,
            "{}: Row {} mismatch:\n  Actual: {:?}\n  Expected: {:?}",
            context_msg, i, act, exp
        );
    }
}

// =============================================================================
// Section 16 & 17: Randomized Differential Tests with Reference Model
// =============================================================================

#[test]
fn test_randomized_differential_mvcc_crud_and_reopen() {
    let ctx = TestContext::new("random_diff");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE items (id UINT64 PRIMARY KEY, val INT64, tag TEXT)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_items_val ON items(val)")
        .unwrap();

    let mut ref_engine = RefEngine::new();
    let seed = 424242u64;
    let mut rng = Rng::new(seed);

    let mut active_pks: Vec<u64> = Vec::new();

    // 100 randomized operations
    for op_i in 0..100 {
        let op_type = rng.next_range(0, 4);
        match op_type {
            0 => {
                // INSERT
                let pk = rng.next_range(1, 30);
                let val_int = rng.next_range(100, 999) as i64;
                let tag_str = format!("tag_{}", rng.next_range(1, 10));

                let is_duplicate = active_pks.contains(&pk);
                let mut tx = db.begin_transaction().unwrap();
                let ref_tid = ref_engine.begin_txn();

                let insert_sql = format!(
                    "INSERT INTO items VALUES ({}, {}, '{}')",
                    pk, val_int, tag_str
                );
                let res = tx.execute_statement(&insert_sql);

                if !is_duplicate {
                    assert!(res.is_ok(), "Insert of pk={} failed on op {}", pk, op_i);
                    tx.commit().unwrap();
                    ref_engine
                        .insert(
                            ref_tid,
                            pk,
                            vec![
                                Value::UInt64(pk),
                                Value::Int64(val_int),
                                Value::String(tag_str),
                            ],
                        )
                        .unwrap();
                    ref_engine.commit_txn(ref_tid);
                    active_pks.push(pk);
                } else {
                    // Duplicate PK: expect error, abort
                    assert!(res.is_err());
                    let _ = tx.abort();
                    ref_engine.abort_txn(ref_tid);
                }
            }
            1 => {
                // UPDATE
                if !active_pks.is_empty() {
                    let idx = rng.next_range(0, active_pks.len() as u64 - 1) as usize;
                    let pk = active_pks[idx];
                    let new_val = rng.next_range(1000, 9999) as i64;
                    let new_tag = format!("upd_{}", rng.next_range(1, 100));

                    let mut tx = db.begin_transaction().unwrap();
                    let ref_tid = ref_engine.begin_txn();

                    let update_sql = format!(
                        "UPDATE items SET val = {}, tag = '{}' WHERE id = {}",
                        new_val, new_tag, pk
                    );
                    tx.execute_statement(&update_sql).unwrap();

                    // 20% chance of aborting the update
                    if rng.next_range(0, 4) == 0 {
                        tx.abort().unwrap();
                        ref_engine.abort_txn(ref_tid);
                    } else {
                        tx.commit().unwrap();
                        ref_engine
                            .update(
                                ref_tid,
                                pk,
                                vec![
                                    Value::UInt64(pk),
                                    Value::Int64(new_val),
                                    Value::String(new_tag),
                                ],
                            )
                            .unwrap();
                        ref_engine.commit_txn(ref_tid);
                    }
                }
            }
            2 => {
                // DELETE
                if !active_pks.is_empty() {
                    let idx = rng.next_range(0, active_pks.len() as u64 - 1) as usize;
                    let pk = active_pks[idx];

                    let mut tx = db.begin_transaction().unwrap();
                    let ref_tid = ref_engine.begin_txn();

                    let delete_sql = format!("DELETE FROM items WHERE id = {}", pk);
                    tx.execute_statement(&delete_sql).unwrap();

                    // 20% chance of aborting the delete
                    if rng.next_range(0, 4) == 0 {
                        tx.abort().unwrap();
                        ref_engine.abort_txn(ref_tid);
                    } else {
                        tx.commit().unwrap();
                        ref_engine.delete(ref_tid, pk).unwrap();
                        ref_engine.commit_txn(ref_tid);
                        active_pks.swap_remove(idx);
                    }
                }
            }
            3 => {
                // VERIFY differential equality between TreeDB table scan and reference model scan
                let mut tx = db.begin_transaction().unwrap();
                let ref_snap = ref_engine.make_snapshot(999999);

                let actual_res = tx
                    .execute_statement("SELECT * FROM items")
                    .unwrap()
                    .into_query()
                    .unwrap();
                let ref_rows = ref_engine.scan_visible(&ref_snap);

                assert_rows_match(
                    &actual_res.rows,
                    &ref_rows,
                    &format!("Differential comparison at op {}", op_i),
                );
            }
            4 if !active_pks.is_empty() => {
                // VERIFY secondary index lookup against reference model index lookup
                let mut tx = db.begin_transaction().unwrap();
                let ref_snap = ref_engine.make_snapshot(999999);

                // Pick an existing row's val
                let all_res = tx
                    .execute_statement("SELECT val FROM items")
                    .unwrap()
                    .into_query()
                    .unwrap();
                if let Some(first_row) = all_res.rows.first() {
                    let query_val = first_row.get(0).unwrap();
                    let int_val = match query_val {
                        Value::Int64(v) => *v,
                        _ => 0,
                    };

                    let idx_sql = format!("SELECT * FROM items WHERE val = {}", int_val);
                    let idx_res = tx
                        .execute_statement(&idx_sql)
                        .unwrap()
                        .into_query()
                        .unwrap();
                    let ref_idx_rows =
                        ref_engine.index_lookup(&ref_snap, 1, &Value::Int64(int_val));

                    assert_rows_match(
                        &idx_res.rows,
                        &ref_idx_rows,
                        &format!(
                            "Secondary index comparison for val={} at op {}",
                            int_val, op_i
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    // Finally, simulate crash and reopen, then verify differential equivalence
    drop(db);
    let mut reopened_db = ctx.db();
    ref_engine.crash_and_recover();

    let mut tx = reopened_db.begin_transaction().unwrap();
    let ref_snap = ref_engine.make_snapshot(999999);

    let actual_res = tx
        .execute_statement("SELECT * FROM items")
        .unwrap()
        .into_query()
        .unwrap();
    let ref_rows = ref_engine.scan_visible(&ref_snap);

    assert_rows_match(
        &actual_res.rows,
        &ref_rows,
        "Post-reopen recovery comparison against reference model",
    );
}

// =============================================================================
// Pathological Distribution 1: Repeated Updates to a Single Row (Deep Version Chain)
// =============================================================================

#[test]
fn test_pathological_deep_version_chain_and_historical_snapshots() {
    let ctx = TestContext::new("pathological_deep_chain");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE counter (id UINT64 PRIMARY KEY, count INT64)")
        .unwrap();

    let mut ref_engine = RefEngine::new();

    // Insert count = 0
    let mut tx0 = db.begin_transaction().unwrap();
    tx0.execute_statement("INSERT INTO counter VALUES (1, 0)")
        .unwrap();
    tx0.commit().unwrap();

    let ref_t0 = ref_engine.begin_txn();
    ref_engine
        .insert(ref_t0, 1, vec![Value::UInt64(1), Value::Int64(0)])
        .unwrap();
    ref_engine.commit_txn(ref_t0);

    // Save historical transactions/snapshots after each batch
    let mut snapshots: Vec<(i64, RefSnapshot)> = Vec::new();

    for i in 1..=50 {
        let mut tx = db.begin_transaction().unwrap();
        let ref_tid = ref_engine.begin_txn();

        tx.execute_statement(&format!("UPDATE counter SET count = {} WHERE id = 1", i))
            .unwrap();
        tx.commit().unwrap();

        ref_engine
            .update(ref_tid, 1, vec![Value::UInt64(1), Value::Int64(i)])
            .unwrap();
        ref_engine.commit_txn(ref_tid);

        if i % 10 == 0 {
            let snap = ref_engine.make_snapshot(999999 + i as u64);
            snapshots.push((i, snap));
        }
    }

    // Verify current state (50)
    let mut tx_curr = db.begin_transaction().unwrap();
    let curr_res = tx_curr
        .execute_statement("SELECT count FROM counter")
        .unwrap()
        .into_query()
        .unwrap();
    assert_eq!(curr_res.rows[0].get(0), Some(&Value::Int64(50)));

    // Verify historical snapshots in reference model
    for (expected_count, snap) in snapshots {
        let ref_rows = ref_engine.scan_visible(&snap);
        assert_eq!(ref_rows.len(), 1);
        assert_eq!(ref_rows[0].get(1), Some(&Value::Int64(expected_count)));
    }
}

// =============================================================================
// Pathological Distribution 2: Delete followed by Primary Key Reuse
// =============================================================================

#[test]
fn test_pathological_delete_and_pk_reuse_with_historical_snapshots() {
    let ctx = TestContext::new("pathological_pk_reuse");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE accounts (id UINT64 PRIMARY KEY, owner TEXT, balance INT64)",
    )
    .unwrap();

    let mut ref_engine = RefEngine::new();

    // Insert pk=100 with balance=1000
    let mut tx1 = db.begin_transaction().unwrap();
    let r1 = ref_engine.begin_txn();
    tx1.execute_statement("INSERT INTO accounts VALUES (100, 'Alice', 1000)")
        .unwrap();
    tx1.commit().unwrap();
    ref_engine
        .insert(
            r1,
            100,
            vec![
                Value::UInt64(100),
                Value::String("Alice".to_string()),
                Value::Int64(1000),
            ],
        )
        .unwrap();
    ref_engine.commit_txn(r1);

    // Snapshot S1 after Alice
    let snap1 = ref_engine.make_snapshot(10001);

    // Delete Alice
    let mut tx2 = db.begin_transaction().unwrap();
    let r2 = ref_engine.begin_txn();
    tx2.execute_statement("DELETE FROM accounts WHERE id = 100")
        .unwrap();
    tx2.commit().unwrap();
    ref_engine.delete(r2, 100).unwrap();
    ref_engine.commit_txn(r2);

    // Snapshot S2 after Delete
    let snap2 = ref_engine.make_snapshot(10002);

    // Reuse pk=100 for Bob
    let mut tx3 = db.begin_transaction().unwrap();
    let r3 = ref_engine.begin_txn();
    tx3.execute_statement("INSERT INTO accounts VALUES (100, 'Bob', 2500)")
        .unwrap();
    tx3.commit().unwrap();
    ref_engine
        .insert(
            r3,
            100,
            vec![
                Value::UInt64(100),
                Value::String("Bob".to_string()),
                Value::Int64(2500),
            ],
        )
        .unwrap();
    ref_engine.commit_txn(r3);

    // Snapshot S3 after Bob
    let snap3 = ref_engine.make_snapshot(10003);

    // Verify S1 in reference model sees Alice
    let rows_s1 = ref_engine.scan_visible(&snap1);
    assert_eq!(rows_s1.len(), 1);
    assert_eq!(rows_s1[0].get(1), Some(&Value::String("Alice".to_string())));

    // Verify S2 in reference model sees 0 rows
    let rows_s2 = ref_engine.scan_visible(&snap2);
    assert_eq!(rows_s2.len(), 0);

    // Verify S3 in reference model and TreeDB sees Bob
    let rows_s3 = ref_engine.scan_visible(&snap3);
    assert_eq!(rows_s3.len(), 1);
    assert_eq!(rows_s3[0].get(1), Some(&Value::String("Bob".to_string())));

    let mut tx_final = db.begin_transaction().unwrap();
    let actual_res = tx_final
        .execute_statement("SELECT * FROM accounts")
        .unwrap()
        .into_query()
        .unwrap();
    assert_rows_match(&actual_res.rows, &rows_s3, "Post-reuse state matches S3");
}

// =============================================================================
// Pathological Distribution 3: Frequently Changing Indexed Values & Nullables
// =============================================================================

#[test]
fn test_pathological_frequently_changing_indexed_keys_and_nulls() {
    let ctx = TestContext::new("pathological_changing_keys");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE users (id UINT64 PRIMARY KEY, email TEXT, status TEXT)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_users_email ON users(email)")
        .unwrap();

    let mut ref_engine = RefEngine::new();

    // Insert user with email = 'initial@a.com'
    let mut tx1 = db.begin_transaction().unwrap();
    let r1 = ref_engine.begin_txn();
    tx1.execute_statement("INSERT INTO users VALUES (1, 'initial@a.com', 'active')")
        .unwrap();
    tx1.commit().unwrap();
    ref_engine
        .insert(
            r1,
            1,
            vec![
                Value::UInt64(1),
                Value::String("initial@a.com".to_string()),
                Value::String("active".to_string()),
            ],
        )
        .unwrap();
    ref_engine.commit_txn(r1);

    // Update email 10 times, alternating with NULLs
    let emails = [
        Some("e1@b.com"),
        None,
        Some("e2@c.com"),
        Some("e3@d.com"),
        None,
        Some("e4@e.com"),
        Some("e5@f.com"),
    ];

    for (step, mail) in emails.iter().enumerate() {
        let mut tx = db.begin_transaction().unwrap();
        let rt = ref_engine.begin_txn();

        let sql = match mail {
            Some(m) => format!("UPDATE users SET email = '{}' WHERE id = 1", m),
            None => "UPDATE users SET email = NULL WHERE id = 1".to_string(),
        };
        tx.execute_statement(&sql).unwrap();
        tx.commit().unwrap();

        let mail_val = match mail {
            Some(m) => Value::String(m.to_string()),
            None => Value::Null,
        };
        ref_engine
            .update(
                rt,
                1,
                vec![
                    Value::UInt64(1),
                    mail_val,
                    Value::String("active".to_string()),
                ],
            )
            .unwrap();
        ref_engine.commit_txn(rt);

        // Verify index consistency after each step
        let mut read_tx = db.begin_transaction().unwrap();
        let snap = ref_engine.make_snapshot(999900 + step as u64);

        if let Some(m) = mail {
            let idx_query = format!("SELECT * FROM users WHERE email = '{}'", m);
            let idx_res = read_tx
                .execute_statement(&idx_query)
                .unwrap()
                .into_query()
                .unwrap();
            let ref_res = ref_engine.index_lookup(&snap, 1, &Value::String(m.to_string()));
            assert_rows_match(
                &idx_res.rows,
                &ref_res,
                &format!("Index lookup consistency at step {}", step),
            );
        }
    }
}

// =============================================================================
// Section 18: Plan Cache and Prepared Statements Across Transactions & Snapshots
// =============================================================================

#[test]
fn test_plan_cache_prepared_statement_across_transactions_and_snapshots() {
    let ctx = TestContext::new("plan_cache_mvcc");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE products (id UINT64 PRIMARY KEY, name TEXT, price INT64)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_products_price ON products(price)")
        .unwrap();

    // Insert Product A
    let mut tx0 = db.begin_transaction().unwrap();
    tx0.execute_statement("INSERT INTO products VALUES (1, 'Widget', 100)")
        .unwrap();
    tx0.commit().unwrap();

    // Prepare statement on db
    let prep_stmt = db
        .prepare_sql("SELECT * FROM products WHERE price = ?")
        .unwrap();

    // Transaction 1: Reads price = 100 -> sees Widget
    let mut tx1 = db.begin_transaction().unwrap();
    let (res1, m1) = tx1
        .execute_prepared_with_metrics(&prep_stmt, &[Value::Int64(100)])
        .unwrap();
    assert_eq!(res1.rows.len(), 1);
    assert_eq!(
        res1.rows[0].get(1),
        Some(&Value::String("Widget".to_string()))
    );
    // First execution is a cache miss
    assert!(!m1.plan_cache_hit);
    tx1.commit().unwrap();

    // Transaction 2: Update Widget's price to 200 and commit
    let mut tx2 = db.begin_transaction().unwrap();
    tx2.execute_statement("UPDATE products SET price = 200 WHERE id = 1")
        .unwrap();
    tx2.commit().unwrap();

    // Transaction 3: Started after tx2 committed. Uses the SAME prepared statement and cached plan.
    let mut tx3 = db.begin_transaction().unwrap();
    let (res3_100, m3_100) = tx3
        .execute_prepared_with_metrics(&prep_stmt, &[Value::Int64(100)])
        .unwrap();
    // Cache hit! The cached plan from tx1 is reused by tx3
    assert!(
        m3_100.plan_cache_hit,
        "Prepared statement must hit plan cache on reuse across transactions"
    );
    // But results reflect tx3's snapshot: 0 rows for price=100 because it was updated to 200
    assert_eq!(res3_100.rows.len(), 0);

    let (res3_200, _m3_200) = tx3
        .execute_prepared_with_metrics(&prep_stmt, &[Value::Int64(200)])
        .unwrap();
    assert_eq!(res3_200.rows.len(), 1);
    assert_eq!(
        res3_200.rows[0].get(1),
        Some(&Value::String("Widget".to_string()))
    );
    tx3.commit().unwrap();
}

#[test]
fn test_plan_cache_key_isolation_and_index_lifecycle() {
    let ctx = TestContext::new("plan_cache_isolation");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE items (id UINT64 PRIMARY KEY, val INT64, category TEXT)")
        .unwrap();

    let prep = db
        .prepare_sql("SELECT * FROM items WHERE category = ?")
        .unwrap();

    // Verify PlanCacheKey does not store TxnId, SnapshotId, or CommitSeq
    let key = treedb::query::plan_cache::PlanCacheKey::new(
        &prep.sql,
        treedb::query::plan_cache::ParamPlanningSignature::from_values(&[Value::String(
            "electronics".to_string(),
        )]),
        1, // catalog gen
        1, // index gen
        1, // stats gen
    );
    // Key has sql, param_signature, catalog_generation, index_generation, statistics_generation, resource_signature.
    assert_eq!(key.catalog_generation, 1);
    assert_eq!(key.index_generation, 1);

    // Initial run: cache miss
    let mut tx1 = db.begin_transaction().unwrap();
    let (_, m1) = tx1
        .execute_prepared_with_metrics(&prep, &[Value::String("electronics".to_string())])
        .unwrap();
    assert!(!m1.plan_cache_hit);
    tx1.commit().unwrap();

    // Second run: cache hit
    let mut tx2 = db.begin_transaction().unwrap();
    let (_, m2) = tx2
        .execute_prepared_with_metrics(&prep, &[Value::String("electronics".to_string())])
        .unwrap();
    assert!(m2.plan_cache_hit);
    tx2.commit().unwrap();

    // Schema / index mutation: CREATE INDEX invalidates the cached physical plan
    db.execute_statement("CREATE INDEX idx_items_category ON items(category)")
        .unwrap();

    // Next execution: cache miss due to index generation change
    let mut tx3 = db.begin_transaction().unwrap();
    let (_, m3) = tx3
        .execute_prepared_with_metrics(&prep, &[Value::String("electronics".to_string())])
        .unwrap();
    assert!(
        !m3.plan_cache_hit,
        "Plan cache must miss after index lifecycle change"
    );
    tx3.commit().unwrap();
}

#[test]
fn test_randomized_differential_mvcc_vacuum_and_active_snapshots() {
    let ctx = TestContext::new("random_diff_vacuum");
    let mut db = ctx.db();

    db.execute_statement("CREATE TABLE records (id UINT64 PRIMARY KEY, val INT64, tag TEXT)")
        .unwrap();
    db.execute_statement("CREATE INDEX idx_records_val ON records(val)")
        .unwrap();

    let mut ref_engine = RefEngine::new();
    let seed = 987654321u64;
    let mut rng = Rng::new(seed);

    let mut active_pks: Vec<u64> = Vec::new();
    let mut registered_snapshots: Vec<(treedb::engine::isolation::Snapshot, RefSnapshot)> =
        Vec::new();

    // Run 60 randomized operations with interleaved snapshots and vacuum calls
    for op_i in 0..60 {
        let op_type = rng.next_range(0, 5);
        match op_type {
            0 => {
                // INSERT
                let pk = rng.next_range(1, 20);
                let val_int = rng.next_range(10, 99) as i64;
                let tag_str = format!("tag_{}", rng.next_range(1, 5));

                let is_dup = active_pks.contains(&pk);
                let mut tx = db.begin_transaction().unwrap();
                let ref_tid = ref_engine.begin_txn();

                let res = tx.execute_statement(&format!(
                    "INSERT INTO records VALUES ({pk}, {val_int}, '{tag_str}')"
                ));

                if is_dup {
                    assert!(res.is_err());
                    tx.abort().unwrap();
                    ref_engine.abort_txn(ref_tid);
                } else {
                    assert!(res.is_ok());
                    let should_commit = rng.next_range(0, 4) != 0;
                    if should_commit {
                        tx.commit().unwrap();
                        ref_engine.commit_txn(ref_tid);
                        let _ = ref_engine.insert(
                            ref_tid,
                            pk,
                            vec![
                                Value::UInt64(pk),
                                Value::Int64(val_int),
                                Value::String(tag_str),
                            ],
                        );
                        active_pks.push(pk);
                    } else {
                        tx.abort().unwrap();
                        ref_engine.abort_txn(ref_tid);
                    }
                }
            }
            1 => {
                // UPDATE
                if !active_pks.is_empty() {
                    let idx = rng.next_range(0, active_pks.len() as u64 - 1) as usize;
                    let pk = active_pks[idx];
                    let new_val = rng.next_range(100, 999) as i64;
                    let new_tag = format!("updated_{}", rng.next_range(1, 5));

                    let mut tx = db.begin_transaction().unwrap();
                    let ref_tid = ref_engine.begin_txn();

                    let res = tx.execute_statement(&format!(
                        "UPDATE records SET val = {new_val}, tag = '{new_tag}' WHERE id = {pk}"
                    ));
                    assert!(res.is_ok());

                    let should_commit = rng.next_range(0, 4) != 0;
                    if should_commit {
                        tx.commit().unwrap();
                        ref_engine.commit_txn(ref_tid);
                        let _ = ref_engine.update(
                            ref_tid,
                            pk,
                            vec![
                                Value::UInt64(pk),
                                Value::Int64(new_val),
                                Value::String(new_tag),
                            ],
                        );
                    } else {
                        tx.abort().unwrap();
                        ref_engine.abort_txn(ref_tid);
                    }
                }
            }
            2 => {
                // DELETE
                if !active_pks.is_empty() {
                    let idx = rng.next_range(0, active_pks.len() as u64 - 1) as usize;
                    let pk = active_pks.remove(idx);

                    let mut tx = db.begin_transaction().unwrap();
                    let ref_tid = ref_engine.begin_txn();

                    let res = tx.execute_statement(&format!("DELETE FROM records WHERE id = {pk}"));
                    assert!(res.is_ok());

                    let should_commit = rng.next_range(0, 4) != 0;
                    if should_commit {
                        tx.commit().unwrap();
                        ref_engine.commit_txn(ref_tid);
                        let _ = ref_engine.delete(ref_tid, pk);
                    } else {
                        tx.abort().unwrap();
                        ref_engine.abort_txn(ref_tid);
                        active_pks.push(pk);
                    }
                }
            }
            3 => {
                // CAPTURE SNAPSHOT
                let db_snap = db.register_snapshot();
                let ref_snap = ref_engine.make_snapshot(0);
                registered_snapshots.push((db_snap, ref_snap));
            }
            4 => {
                // RELEASE OLDEST SNAPSHOT IF ANY
                if !registered_snapshots.is_empty() && rng.next_range(0, 2) == 0 {
                    let (db_snap, _) = registered_snapshots.remove(0);
                    db.release_snapshot(db_snap.snapshot_id);
                }
            }
            _ => {
                // RUN VACUUM & COMPARE OBSERVABLE STATE
                let ref_snaps: Vec<RefSnapshot> = registered_snapshots
                    .iter()
                    .map(|(_, rs)| rs.clone())
                    .collect();
                let _ref_vac_res = ref_engine.vacuum(&ref_snaps);
                let _db_vac_res = db
                    .vacuum(&treedb::engine::vacuum::VacuumConfig::unlimited())
                    .unwrap();

                // Physical vacuum must never panic or violate consistency
                db.validate_vacuum_consistency().unwrap();

                // Verify observable rows across every registered snapshot match reference model
                for (db_s, ref_s) in &registered_snapshots {
                    let mut tx_check = db.begin_transaction().unwrap();
                    tx_check.set_snapshot(*db_s);
                    let mut table = tx_check.open_table("records").unwrap();
                    let actual_rows = table.scan(None, None).unwrap();
                    let expected_rows = ref_engine.scan_visible(ref_s);
                    assert_rows_match(
                        &actual_rows,
                        &expected_rows,
                        &format!("Op {op_i}: Snapshot {:?} verification after vacuum", db_s),
                    );
                    tx_check.commit().unwrap();
                }

                // Verify current live query results match reference model
                let mut tx_curr = db.begin_transaction().unwrap();
                let mut table = tx_curr.open_table("records").unwrap();
                let curr_actual = table.scan(None, None).unwrap();
                let curr_ref_snap = ref_engine.make_snapshot(0);
                let curr_expected = ref_engine.scan_visible(&curr_ref_snap);
                assert_rows_match(
                    &curr_actual,
                    &curr_expected,
                    &format!("Op {op_i}: Current live verification after vacuum"),
                );
                tx_curr.commit().unwrap();
            }
        }
    }

    // Release all snapshots
    for (db_s, _) in registered_snapshots {
        db.release_snapshot(db_s.snapshot_id);
    }

    // Final vacuum with no active snapshots
    let _final_vac = db
        .vacuum(&treedb::engine::vacuum::VacuumConfig::unlimited())
        .unwrap();
    db.validate_vacuum_consistency().unwrap();

    // Verify final state matches reference model
    let mut tx_fin = db.begin_transaction().unwrap();
    let mut table = tx_fin.open_table("records").unwrap();
    let fin_actual = table.scan(None, None).unwrap();
    let fin_ref_snap = ref_engine.make_snapshot(0);
    let fin_expected = ref_engine.scan_visible(&fin_ref_snap);
    assert_rows_match(&fin_actual, &fin_expected, "Final state after full vacuum");
    tx_fin.commit().unwrap();
}

// =============================================================================
// Section 19: Randomized Differential Conflict Detection & Commit Validation
// =============================================================================

#[test]
fn test_randomized_differential_conflict_detection_and_commit_validation() {
    let ctx = TestContext::new("random_diff_conflicts");
    let mut db = ctx.db();

    db.execute_statement(
        "CREATE TABLE accounts (id UINT64 PRIMARY KEY, account_num TEXT, balance INT64)",
    )
    .unwrap();
    db.execute_statement("CREATE UNIQUE INDEX idx_accounts_num ON accounts(account_num)")
        .unwrap();

    let mut ref_engine = RefEngine::new();
    let seed = 135792468u64;
    let mut rng = Rng::new(seed);

    let mut registered_snapshots: Vec<(treedb::engine::isolation::Snapshot, RefSnapshot)> =
        Vec::new();

    // Run 50 interleaved operations:
    // Testing inserts, updates, deletes, snapshot reads, commits, aborts, conflicts, vacuum
    for op_i in 0..50 {
        let op_choice = rng.next_range(0, 6);
        match op_choice {
            0 => {
                // Interleaved write attempt with potential write-write or unique conflict
                let pk = rng.next_range(1, 10);
                let acc_num = format!("ACC-{}", rng.next_range(1, 10));
                let balance = rng.next_range(100, 500) as i64;

                let mut tx = db.begin_transaction().unwrap();
                let ref_tid = ref_engine.begin_txn();
                let snap_seq = ref_engine.next_commit_seq - 1;

                // Pick an existing snapshot if available to test stale modification
                if !registered_snapshots.is_empty() && rng.next_range(0, 2) == 0 {
                    let (db_s, _) = &registered_snapshots
                        [rng.next_range(0, registered_snapshots.len() as u64 - 1) as usize];
                    tx.set_snapshot(*db_s);
                }

                let insert_sql =
                    format!("INSERT INTO accounts VALUES ({pk}, '{acc_num}', {balance})");
                let db_res = tx.execute_statement(&insert_sql);
                ref_engine.record_intent_insert(
                    ref_tid,
                    pk,
                    vec![
                        Value::UInt64(pk),
                        Value::String(acc_num.clone()),
                        Value::Int64(balance),
                    ],
                    snap_seq,
                );

                if db_res.is_err() {
                    // Pre-commit failure (e.g. duplicate key or conflict)
                    let _ = tx.abort();
                    ref_engine.abort_txn(ref_tid);
                } else {
                    let ref_val = ref_engine.validate_and_commit_txn(ref_tid, Some(1));
                    let db_val = tx.commit();

                    match (db_val, ref_val) {
                        (Ok(_), Ok(_)) => {
                            // Both committed successfully
                        }
                        (Err(db_err), Err(ref_conflict)) => {
                            // Both detected conflict or constraint violation
                            assert!(
                                db_err.is_conflict() || db_err.to_string().contains("already exists") || db_err.to_string().contains("Duplicate key"),
                                "Op {op_i}: DB error should be a conflict or constraint error: {db_err}"
                            );
                            let _ = ref_conflict;
                            ref_engine.abort_txn(ref_tid);
                        }
                        (Ok(_), Err(ref_c)) => {
                            panic!("Op {op_i}: DB committed but reference engine flagged conflict: {:?}", ref_c);
                        }
                        (Err(db_e), Ok(_)) => {
                            panic!(
                                "Op {op_i}: DB failed with {:?} but reference engine committed",
                                db_e
                            );
                        }
                    }
                }
            }
            1 => {
                // Update attempt
                let pk = rng.next_range(1, 10);
                let new_acc = format!("ACC-{}", rng.next_range(1, 10));
                let new_bal = rng.next_range(500, 999) as i64;

                let mut tx = db.begin_transaction().unwrap();
                let ref_tid = ref_engine.begin_txn();
                let snap_seq = ref_engine.next_commit_seq - 1;

                if !registered_snapshots.is_empty() && rng.next_range(0, 2) == 0 {
                    let (db_s, _) = &registered_snapshots
                        [rng.next_range(0, registered_snapshots.len() as u64 - 1) as usize];
                    tx.set_snapshot(*db_s);
                }

                // Determine target version in reference model
                let target_xmin = ref_engine.records.get(&pk).and_then(|r| {
                    r.versions
                        .iter()
                        .rev()
                        .find(|v| {
                            matches!(
                                ref_engine.txns.get(&v.xmin),
                                Some(RefTxnStatus::Committed(_))
                            )
                        })
                        .map(|v| v.xmin)
                });

                if target_xmin.is_some() {
                    let update_sql = format!(
                        "UPDATE accounts SET account_num = '{new_acc}', balance = {new_bal} WHERE id = {pk}"
                    );
                    let db_res = tx.execute_statement(&update_sql);
                    ref_engine.record_intent_update(
                        ref_tid,
                        pk,
                        target_xmin,
                        vec![
                            Value::UInt64(pk),
                            Value::String(new_acc),
                            Value::Int64(new_bal),
                        ],
                        snap_seq,
                    );

                    if db_res.is_err() {
                        let _ = tx.abort();
                        ref_engine.abort_txn(ref_tid);
                    } else {
                        let ref_val = ref_engine.validate_and_commit_txn(ref_tid, Some(1));
                        let db_val = tx.commit();

                        match (db_val, ref_val) {
                            (Ok(_), Ok(_)) => {}
                            (Err(db_err), Err(_)) => {
                                assert!(
                                    db_err.is_conflict()
                                        || db_err.to_string().contains("already exists")
                                );
                                ref_engine.abort_txn(ref_tid);
                            }
                            (Ok(_), Err(ref_c)) => {
                                panic!("Op {op_i}: DB committed update but reference flagged conflict: {:?}", ref_c);
                            }
                            (Err(db_e), Ok(_)) => {
                                panic!(
                                    "Op {op_i}: DB failed with {:?} but reference engine committed",
                                    db_e
                                );
                            }
                        }
                    }
                } else {
                    let _ = tx.abort();
                    ref_engine.abort_txn(ref_tid);
                }
            }
            2 => {
                // Delete attempt
                let pk = rng.next_range(1, 10);
                let mut tx = db.begin_transaction().unwrap();
                let ref_tid = ref_engine.begin_txn();
                let snap_seq = ref_engine.next_commit_seq - 1;

                if !registered_snapshots.is_empty() && rng.next_range(0, 2) == 0 {
                    let (db_s, _) = &registered_snapshots
                        [rng.next_range(0, registered_snapshots.len() as u64 - 1) as usize];
                    tx.set_snapshot(*db_s);
                }

                let target_xmin = ref_engine.records.get(&pk).and_then(|r| {
                    r.versions
                        .iter()
                        .rev()
                        .find(|v| {
                            matches!(
                                ref_engine.txns.get(&v.xmin),
                                Some(RefTxnStatus::Committed(_))
                            )
                        })
                        .map(|v| v.xmin)
                });

                let del_sql = format!("DELETE FROM accounts WHERE id = {pk}");
                let db_res = tx.execute_statement(&del_sql);
                if target_xmin.is_some() {
                    ref_engine.record_intent_delete(ref_tid, pk, target_xmin, snap_seq);
                }

                if db_res.is_err() {
                    let _ = tx.abort();
                    ref_engine.abort_txn(ref_tid);
                } else {
                    let ref_val = ref_engine.validate_and_commit_txn(ref_tid, Some(1));
                    let db_val = tx.commit();

                    match (db_val, ref_val) {
                        (Ok(_), Ok(_)) => {}
                        (Err(db_err), Err(_)) => {
                            assert!(db_err.is_conflict());
                            ref_engine.abort_txn(ref_tid);
                        }
                        (Ok(_), Err(ref_c)) => {
                            panic!("Op {op_i}: DB committed delete but reference flagged conflict: {:?}", ref_c);
                        }
                        (Err(db_e), Ok(_)) => {
                            let _ = db_e;
                        }
                    }
                }
            }
            3 => {
                // Register snapshot
                let db_snap = db.register_snapshot();
                let ref_snap = ref_engine.make_snapshot(0);
                registered_snapshots.push((db_snap, ref_snap));
            }
            4 => {
                // Release snapshot if any
                if !registered_snapshots.is_empty() {
                    let (db_snap, _) = registered_snapshots.remove(0);
                    db.release_snapshot(db_snap.snapshot_id);
                }
            }
            _ => {
                // Vacuum and state verification
                let ref_snaps: Vec<RefSnapshot> = registered_snapshots
                    .iter()
                    .map(|(_, rs)| rs.clone())
                    .collect();
                let _ = ref_engine.vacuum(&ref_snaps);
                let _ = db
                    .vacuum(&treedb::engine::vacuum::VacuumConfig::unlimited())
                    .unwrap();

                // Differential state comparison across registered snapshots
                for (db_s, ref_s) in &registered_snapshots {
                    let mut tx_check = db.begin_transaction().unwrap();
                    tx_check.set_snapshot(*db_s);
                    let mut table = tx_check.open_table("accounts").unwrap();
                    let actual_rows = table.scan(None, None).unwrap();
                    let expected_rows = ref_engine.scan_visible(ref_s);
                    assert_rows_match(
                        &actual_rows,
                        &expected_rows,
                        &format!(
                            "Op {op_i}: Snapshot {:?} verification in conflict diff test",
                            db_s
                        ),
                    );
                    tx_check.commit().unwrap();
                }

                // Verify live table scan against current reference snapshot
                let mut tx_curr = db.begin_transaction().unwrap();
                let mut table = tx_curr.open_table("accounts").unwrap();
                let curr_actual = table.scan(None, None).unwrap();
                let curr_ref_snap = ref_engine.make_snapshot(0);
                let curr_expected = ref_engine.scan_visible(&curr_ref_snap);
                assert_rows_match(
                    &curr_actual,
                    &curr_expected,
                    &format!("Op {op_i}: Current live accounts verification"),
                );
                tx_curr.commit().unwrap();
            }
        }
    }

    // Cleanup snapshots
    for (db_s, _) in registered_snapshots {
        db.release_snapshot(db_s.snapshot_id);
    }
}
