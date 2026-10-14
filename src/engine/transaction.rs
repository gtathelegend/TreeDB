use crate::engine::types::{DataType, Row, Schema, Value};
use crate::engine::TreeDb;
use crate::error::{Error, Result};
use crate::storage::page::{Key, PageId};
use crate::wal::lsn::TxnId;

/// Default maximum allowed recursive CTE iterations before aborting with an error.
pub const DEFAULT_MAX_RECURSION_ITERATIONS: usize = 1000;

/// Default maximum allowed recursive CTE total generated rows before aborting with an error.
pub const DEFAULT_MAX_RECURSION_ROWS: usize = 1_000_000;

fn ast_references_table(stmt: &crate::sql::SelectStatement, target_table: &str) -> bool {
    if let Some(with_clause) = &stmt.with {
        for cte in &with_clause.ctes {
            let refs = match &cte.query {
                crate::sql::CteQuery::NonRecursive(select) => {
                    ast_references_table(select, target_table)
                }
                crate::sql::CteQuery::SetOperation(set_q) => {
                    ast_set_query_references_table(set_q, target_table)
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    ast_references_table(anchor, target_table)
                        || ast_references_table(recursive, target_table)
                }
            };
            if refs {
                return true;
            }
        }
    }
    if stmt.from.name() == target_table || stmt.from.alias() == Some(target_table) {
        return true;
    }
    for join in &stmt.joins {
        if join.table.name() == target_table || join.table.alias() == Some(target_table) {
            return true;
        }
        if ast_expr_references_table(&join.on, target_table) {
            return true;
        }
    }
    if let Some(w) = &stmt.selection {
        if ast_expr_references_table(w, target_table) {
            return true;
        }
    }
    if let Some(h) = &stmt.having {
        if ast_expr_references_table(h, target_table) {
            return true;
        }
    }
    for item in &stmt.projection {
        match item {
            crate::sql::SelectItem::Expression { expr, .. } => {
                if ast_expr_references_table(expr, target_table) {
                    return true;
                }
            }
            crate::sql::SelectItem::QualifiedWildcard(tbl) => {
                if tbl == target_table {
                    return true;
                }
            }
            crate::sql::SelectItem::Wildcard => {}
        }
    }
    for item in &stmt.group_by {
        if ast_expr_references_table(item, target_table) {
            return true;
        }
    }
    for item in &stmt.order_by {
        if ast_expr_references_table(&item.expr, target_table) {
            return true;
        }
    }
    false
}

fn ast_set_query_references_table(query: &crate::sql::SetQuery, target_table: &str) -> bool {
    if let Some(with_clause) = &query.with {
        for cte in &with_clause.ctes {
            let refs = match &cte.query {
                crate::sql::CteQuery::NonRecursive(select) => {
                    ast_references_table(select, target_table)
                }
                crate::sql::CteQuery::SetOperation(set_q) => {
                    ast_set_query_references_table(set_q, target_table)
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    ast_references_table(anchor, target_table)
                        || ast_references_table(recursive, target_table)
                }
            };
            if refs {
                return true;
            }
        }
    }
    if ast_set_query_body_references_table(&query.body, target_table) {
        return true;
    }
    for item in &query.order_by {
        if ast_expr_references_table(&item.expr, target_table) {
            return true;
        }
    }
    false
}

fn ast_set_query_body_references_table(
    body: &crate::sql::SetQueryBody,
    target_table: &str,
) -> bool {
    match body {
        crate::sql::SetQueryBody::Select(s) => ast_references_table(s, target_table),
        crate::sql::SetQueryBody::Parenthesized(q) => {
            ast_set_query_references_table(q, target_table)
        }
        crate::sql::SetQueryBody::SetOperation { left, right, .. } => {
            ast_set_query_body_references_table(left, target_table)
                || ast_set_query_body_references_table(right, target_table)
        }
    }
}

fn ast_expr_references_table(expr: &crate::sql::SqlExpr, target_table: &str) -> bool {
    match expr {
        crate::sql::SqlExpr::QualifiedIdentifier { qualifier, .. } => qualifier == target_table,
        crate::sql::SqlExpr::Identifier(_)
        | crate::sql::SqlExpr::Literal(_)
        | crate::sql::SqlExpr::Parameter(_)
        | crate::sql::SqlExpr::CountAll => false,
        crate::sql::SqlExpr::Subquery(sub) => ast_references_table(sub, target_table),
        crate::sql::SqlExpr::SetSubquery(sub) => ast_set_query_references_table(sub, target_table),
        crate::sql::SqlExpr::Binary { left, right, .. } => {
            ast_expr_references_table(left, target_table)
                || ast_expr_references_table(right, target_table)
        }
        crate::sql::SqlExpr::Unary { expr, .. }
        | crate::sql::SqlExpr::IsNull(expr)
        | crate::sql::SqlExpr::IsNotNull(expr) => ast_expr_references_table(expr, target_table),
        crate::sql::SqlExpr::InList { expr, list, .. } => {
            ast_expr_references_table(expr, target_table)
                || list
                    .iter()
                    .any(|e| ast_expr_references_table(e, target_table))
        }
        crate::sql::SqlExpr::InSubquery { expr, subquery, .. } => {
            ast_expr_references_table(expr, target_table)
                || ast_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::InSetSubquery { expr, subquery, .. } => {
            ast_expr_references_table(expr, target_table)
                || ast_set_query_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::Exists { subquery, .. } => {
            ast_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::SetExists { subquery, .. } => {
            ast_set_query_references_table(subquery, target_table)
        }
        crate::sql::SqlExpr::Aggregate { arg, .. } => ast_expr_references_table(arg, target_table),
        crate::sql::SqlExpr::Window {
            args, window_spec, ..
        } => {
            args.iter()
                .any(|e| ast_expr_references_table(e, target_table))
                || window_spec
                    .partition_by
                    .iter()
                    .any(|e| ast_expr_references_table(e, target_table))
                || window_spec
                    .order_by
                    .iter()
                    .any(|o| ast_expr_references_table(&o.expr, target_table))
        }
    }
}

fn plan_references_table(plan: &crate::query::optimizer::PhysicalPlan, target_table: &str) -> bool {
    if let Some(set_op) = &plan.set_op {
        return plan_references_table(&set_op.left, target_table)
            || plan_references_table(&set_op.right, target_table);
    }
    if plan.base_table.table_name == target_table
        || plan.base_table.table_alias.as_deref() == Some(target_table)
    {
        return true;
    }
    for join in &plan.joins {
        if join.right.table_name == target_table
            || join.right.table_alias.as_deref() == Some(target_table)
        {
            return true;
        }
    }
    false
}

fn validate_ast_cte_acyclicity(ctes: &[crate::sql::Cte]) -> Result<()> {
    let mut adj: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut is_rec = std::collections::HashMap::new();

    for cte in ctes {
        let mut deps = Vec::new();
        for other in ctes {
            if other.name == cte.name {
                continue;
            }
            let refs = match &cte.query {
                crate::sql::CteQuery::NonRecursive(select) => {
                    ast_references_table(select, &other.name)
                }
                crate::sql::CteQuery::SetOperation(set_q) => {
                    ast_set_query_references_table(set_q, &other.name)
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    ast_references_table(anchor, &other.name)
                        || ast_references_table(recursive, &other.name)
                }
            };
            if refs {
                deps.push(other.name.as_str());
            }
        }
        adj.insert(cte.name.as_str(), deps);
        is_rec.insert(
            cte.name.as_str(),
            matches!(cte.query, crate::sql::CteQuery::Recursive { .. }),
        );
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Unvisited,
        Visiting,
        Visited,
    }

    let mut state = std::collections::HashMap::new();
    for cte in ctes {
        state.insert(cte.name.as_str(), VisitState::Unvisited);
    }

    fn dfs<'a>(
        node: &'a str,
        adj: &std::collections::HashMap<&'a str, Vec<&'a str>>,
        is_rec: &std::collections::HashMap<&'a str, bool>,
        state: &mut std::collections::HashMap<&'a str, VisitState>,
        path: &mut Vec<&'a str>,
    ) -> Result<()> {
        state.insert(node, VisitState::Visiting);
        path.push(node);

        if let Some(neighbors) = adj.get(node) {
            for &neighbor in neighbors {
                match state
                    .get(neighbor)
                    .copied()
                    .unwrap_or(VisitState::Unvisited)
                {
                    VisitState::Visiting => {
                        path.push(neighbor);
                        let cycle_repr = path.join(" -> ");
                        let has_rec = path.iter().any(|n| is_rec.get(n).copied().unwrap_or(false));
                        if has_rec {
                            return Err(Error::MutualRecursion(cycle_repr));
                        }
                        return Err(Error::CyclicCteDependency(cycle_repr));
                    }
                    VisitState::Unvisited => {
                        dfs(neighbor, adj, is_rec, state, path)?;
                    }
                    VisitState::Visited => {}
                }
            }
        }

        path.pop();
        state.insert(node, VisitState::Visited);
        Ok(())
    }

    let mut path = Vec::new();
    for cte in ctes {
        if state.get(cte.name.as_str()).copied() == Some(VisitState::Unvisited) {
            dfs(cte.name.as_str(), &adj, &is_rec, &mut state, &mut path)?;
        }
    }

    Ok(())
}

fn make_non_recursive_bound_cte(
    plan: crate::query::optimizer::PhysicalPlan,
    schema: &crate::engine::types::Schema,
    config: &crate::query::materialize::MaterializeConfig,
    consumer_count: usize,
    mat_id_counter: &mut usize,
) -> crate::sql::BoundCteKind {
    let est_rows = plan.estimated_rows;
    let mut est_row_width = 16.0;
    for c in &schema.columns {
        est_row_width += match c.data_type {
            crate::engine::types::DataType::Null => 1.0,
            crate::engine::types::DataType::Boolean => 1.0,
            crate::engine::types::DataType::Int64 => 8.0,
            crate::engine::types::DataType::UInt64 => 8.0,
            crate::engine::types::DataType::Float64 => 8.0,
            crate::engine::types::DataType::String => 32.0,
            crate::engine::types::DataType::Bytes => 32.0,
        };
    }
    let est_bytes = (est_rows * est_row_width) as usize;
    let is_external = est_bytes > config.memory_budget_bytes;
    let (inline_cost, mat_cost, should_mat) =
        crate::query::cost::CostModel::evaluate_materialization_cost(
            plan.total_cost,
            est_rows,
            consumer_count,
            is_external,
        );
    let decision = match config.test_override {
        crate::query::materialize::MaterializeOverride::ForceInline => {
            crate::query::materialize::MaterializationDecision::Inline
        }
        crate::query::materialize::MaterializeOverride::ForceMaterialize => {
            crate::query::materialize::MaterializationDecision::Materialize
        }
        crate::query::materialize::MaterializeOverride::ForceExternalMaterialize => {
            crate::query::materialize::MaterializationDecision::ExternalMaterialize
        }
        crate::query::materialize::MaterializeOverride::Auto => {
            if consumer_count <= 1 {
                crate::query::materialize::MaterializationDecision::Inline
            } else if should_mat {
                if is_external {
                    crate::query::materialize::MaterializationDecision::ExternalMaterialize
                } else {
                    crate::query::materialize::MaterializationDecision::Materialize
                }
            } else {
                crate::query::materialize::MaterializationDecision::Inline
            }
        }
    };

    let materialize_id = match decision {
        crate::query::materialize::MaterializationDecision::Inline => None,
        crate::query::materialize::MaterializationDecision::Materialize
        | crate::query::materialize::MaterializationDecision::ExternalMaterialize => {
            *mat_id_counter += 1;
            Some(*mat_id_counter)
        }
    };

    let fingerprint =
        Some(crate::query::materialize::SubplanFingerprint::from_plan(&plan).to_hex());

    crate::sql::BoundCteKind::NonRecursive {
        plan,
        decision,
        materialize_id,
        consumer_count,
        estimated_inline_cost: inline_cost,
        estimated_materialize_cost: mat_cost,
        fingerprint,
    }
}

fn count_table_references_in_plan(
    plan: &crate::query::optimizer::PhysicalPlan,
    name: &str,
) -> usize {
    let mut count = 0;
    if plan.base_table.table_name == name {
        count += 1;
    }
    for join in &plan.joins {
        if join.right.table_name == name {
            count += 1;
        }
    }
    if let Some(set_op) = &plan.set_op {
        count += count_table_references_in_plan(&set_op.left, name);
        count += count_table_references_in_plan(&set_op.right, name);
    }
    count
}

fn count_cte_consumers(
    ctes: &[crate::sql::BoundCte],
    main_plan: &crate::query::optimizer::PhysicalPlan,
    name: &str,
) -> usize {
    let mut count = count_table_references_in_plan(main_plan, name);
    for cte in ctes {
        match &cte.kind {
            crate::sql::BoundCteKind::NonRecursive {
                plan: inner_plan, ..
            } => {
                count += count_table_references_in_plan(inner_plan, name);
            }
            crate::sql::BoundCteKind::Recursive {
                anchor_plan,
                recursive_plan,
                ..
            } => {
                count += count_table_references_in_plan(anchor_plan, name);
                count += count_table_references_in_plan(recursive_plan, name);
            }
        }
    }
    count
}

struct TxnBufferPoolCatalog<'a, 'b> {
    bp: &'a std::cell::RefCell<&'b mut crate::storage::buffer_pool::BufferPool>,
    ctes: Option<&'a [crate::sql::BoundCte]>,
}

impl<'a, 'b> crate::sql::TableCatalog for TxnBufferPoolCatalog<'a, 'b> {
    fn get_table_schema(&self, table_name: &str) -> Result<Schema> {
        if let Some(ctes) = self.ctes {
            for cte in ctes {
                if cte.name == table_name {
                    return Ok(cte.output_schema.clone());
                }
            }
        }
        let mut bp = self.bp.borrow_mut();
        let meta = crate::engine::catalog::Catalog::get_table(&mut bp, table_name)?
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        Ok(meta.schema)
    }

    fn get_table_indexes(
        &self,
        table_name: &str,
    ) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        if let Some(ctes) = self.ctes {
            for cte in ctes {
                if cte.name == table_name {
                    return Ok(Vec::new());
                }
            }
        }
        let mut bp = self.bp.borrow_mut();
        let meta = crate::engine::catalog::Catalog::get_table(&mut bp, table_name)?
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        crate::engine::catalog::Catalog::list_indexes_for_table(&mut bp, meta.table_id)
    }

    fn get_index(&self, index_name: &str) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        let mut bp = self.bp.borrow_mut();
        crate::engine::catalog::Catalog::get_index(&mut bp, index_name)
    }
}

/// Execution result returned by top-level SQL statements (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`).
#[derive(Debug, Clone, PartialEq)]
pub enum StatementResult {
    /// Result of an SQL `SELECT` query.
    Query(crate::query::QueryResult),
    /// Result of an SQL `INSERT` statement.
    Inserted {
        /// Number of rows successfully inserted.
        rows: usize,
    },
    /// Result of an SQL `UPDATE` statement.
    Updated {
        /// Number of rows successfully updated.
        rows: usize,
    },
    /// Result of an SQL `DELETE` statement.
    Deleted {
        /// Number of rows successfully deleted.
        rows: usize,
    },
    /// Result of an SQL `CREATE TABLE` statement.
    CreatedTable {
        /// Name of the created table.
        table: String,
    },
    /// Result of an SQL `DROP TABLE` statement.
    DroppedTable {
        /// Name of the dropped table.
        table: String,
    },
    /// Result of an SQL `CREATE INDEX` statement.
    CreatedIndex {
        /// Name of the created index.
        index: String,
    },
    /// Result of an SQL `DROP INDEX` statement.
    DroppedIndex {
        /// Name of the dropped index.
        index: String,
    },
    /// Result of an SQL `ANALYZE` statement.
    AnalyzedTable {
        /// Name of the analyzed table.
        table: String,
    },
}

impl StatementResult {
    /// Unwraps this result as a `QueryResult`, returning an error if it is not a query.
    pub fn into_query(self) -> Result<crate::query::QueryResult> {
        match self {
            StatementResult::Query(qr) => Ok(qr),
            other => Err(Error::QueryExecutionError(format!(
                "expected query result, found statement result: {other:?}"
            ))),
        }
    }

    /// Returns the number of rows affected by an `INSERT`, `UPDATE`, or `DELETE` statement,
    /// or the number of rows returned by a `SELECT` query.
    pub fn rows_affected(&self) -> usize {
        match self {
            StatementResult::Query(qr) => qr.rows.len(),
            StatementResult::Inserted { rows }
            | StatementResult::Updated { rows }
            | StatementResult::Deleted { rows } => *rows,
            StatementResult::CreatedTable { .. }
            | StatementResult::DroppedTable { .. }
            | StatementResult::CreatedIndex { .. }
            | StatementResult::DroppedIndex { .. }
            | StatementResult::AnalyzedTable { .. } => 0,
        }
    }
}

/// Lifecycle states representing a transaction's progression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TxnState {
    /// Transaction created, pending activation.
    Created,
    /// Transaction is currently active and can accept reads and mutations.
    #[default]
    Active,
    /// Transaction has successfully committed.
    Committed,
    /// Transaction has been aborted and rolled back.
    Aborted,
}

impl TxnState {
    /// Validates and applies a transaction lifecycle state transition.
    pub fn transition_to(&self, target: Self) -> Result<Self> {
        match (self, target) {
            (Self::Created, Self::Active) => Ok(Self::Active),
            (Self::Created, Self::Aborted) => Ok(Self::Aborted),
            (Self::Active, Self::Committed) => Ok(Self::Committed),
            (Self::Active, Self::Aborted) => Ok(Self::Aborted),
            (current, target) => Err(Error::InvalidTransactionState {
                current: format!("{current}"),
                action: format!("transition to {target}"),
            }),
        }
    }
}

impl std::fmt::Display for TxnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "Created"),
            Self::Active => write!(f, "Active"),
            Self::Committed => write!(f, "Committed"),
            Self::Aborted => write!(f, "Aborted"),
        }
    }
}

/// An explicit atomic transaction handle coordinating operations across the B+ Tree index,
/// Record Store, and Buffer Pool within a single WAL transaction context (`TxnId`).
#[derive(Debug)]
pub struct Transaction<'db> {
    db: &'db mut TreeDb,
    txn_id: TxnId,
    state: TxnState,
    isolation_context: crate::engine::isolation::IsolationContext,
    owned_snapshot_id: Option<crate::engine::isolation::SnapshotId>,
    write_set: crate::engine::isolation::TransactionWriteSet,
    initial_root_page_id: PageId,
    initial_freelist_head: PageId,
    initial_free_page_count: u32,
    #[allow(dead_code)]
    initial_page_count: u32,
    pub(crate) has_catalog_mutations: bool,
}

impl<'db> Transaction<'db> {
    #[allow(dead_code)]
    pub(crate) fn new(
        db: &'db mut TreeDb,
        txn_id: TxnId,
        initial_root_page_id: PageId,
        initial_freelist_head: PageId,
        initial_free_page_count: u32,
        initial_page_count: u32,
    ) -> Self {
        let commit_seq = db.current_commit_seq();
        let snapshot = db.snapshot_registry.register(commit_seq);
        let isolation_context = crate::engine::isolation::IsolationContext::new(
            crate::engine::isolation::IsolationLevel::ReadCommitted,
            txn_id,
            snapshot,
        );
        Self::new_with_isolation(
            db,
            txn_id,
            isolation_context,
            initial_root_page_id,
            initial_freelist_head,
            initial_free_page_count,
            initial_page_count,
        )
    }

    pub(crate) fn new_with_isolation(
        db: &'db mut TreeDb,
        txn_id: TxnId,
        isolation_context: crate::engine::isolation::IsolationContext,
        initial_root_page_id: PageId,
        initial_freelist_head: PageId,
        initial_free_page_count: u32,
        initial_page_count: u32,
    ) -> Self {
        let snap_id = isolation_context.snapshot.snapshot_id;
        Self {
            db,
            txn_id,
            state: TxnState::Active,
            owned_snapshot_id: Some(snap_id),
            isolation_context,
            write_set: crate::engine::isolation::TransactionWriteSet::new(),
            initial_root_page_id,
            initial_freelist_head,
            initial_free_page_count,
            initial_page_count,
            has_catalog_mutations: false,
        }
    }

    /// Returns the unique transaction identifier.
    pub const fn id(&self) -> TxnId {
        self.txn_id
    }

    /// Returns the current lifecycle state of the transaction.
    pub const fn state(&self) -> TxnState {
        self.state
    }

    /// Sets or clears a deterministic test failure injection point.
    pub fn set_test_failure_point(
        &mut self,
        point: Option<crate::engine::failure::StorageFailurePoint>,
    ) {
        self.db.set_test_failure_point(point);
    }

    /// Returns the currently active test failure injection point, if any.
    pub fn test_failure_point(&self) -> Option<crate::engine::failure::StorageFailurePoint> {
        self.db.test_failure_point()
    }

    /// Returns a reference to the immutable isolation context for this transaction.
    pub fn isolation_context(&self) -> &crate::engine::isolation::IsolationContext {
        &self.isolation_context
    }

    /// Returns a reference to the active point-in-time snapshot.
    pub fn snapshot(&self) -> &crate::engine::isolation::Snapshot {
        &self.isolation_context.snapshot
    }

    /// Sets the point-in-time snapshot for this transaction.
    pub fn set_snapshot(&mut self, snapshot: crate::engine::isolation::Snapshot) {
        if let Some(owned) = self.owned_snapshot_id.take() {
            self.db.snapshot_registry.release(owned);
        }
        self.isolation_context.snapshot = snapshot;
    }

    /// Returns the baseline single-version visibility policy.
    pub fn visibility_policy(&self) -> crate::engine::isolation::CurrentVersionVisibility {
        crate::engine::isolation::CurrentVersionVisibility
    }

    /// Returns a reference to the transaction-local write set.
    pub fn write_set(&self) -> &crate::engine::isolation::TransactionWriteSet {
        &self.write_set
    }

    /// Returns a mutable reference to the transaction-local write set.
    pub fn write_set_mut(&mut self) -> &mut crate::engine::isolation::TransactionWriteSet {
        &mut self.write_set
    }

    /// Refreshes the statement snapshot for statement-level isolation (e.g. READ COMMITTED).
    pub fn statement_snapshot(&mut self) -> crate::engine::isolation::Snapshot {
        let commit_seq = self.db.current_commit_seq();
        if let Some(old_id) = self.owned_snapshot_id.take() {
            self.db.snapshot_registry.release(old_id);
        }
        let new_snap = self.db.snapshot_registry.register(commit_seq);
        self.owned_snapshot_id = Some(new_snap.snapshot_id);
        self.isolation_context.snapshot = new_snap;
        new_snap
    }

    /// Returns a reference to the underlying database.
    pub fn db(&self) -> &TreeDb {
        self.db
    }

    /// Returns a mutable reference to the underlying database.
    pub fn db_mut(&mut self) -> &mut TreeDb {
        self.db
    }

    /// Returns true if the transaction is in the active state.
    pub const fn is_active(&self) -> bool {
        matches!(self.state, TxnState::Active)
    }

    /// Retrieves an owned copy of the value associated with `key` within this transaction.
    ///
    /// Observes prior mutations (inserts, updates, deletes) made within the same transaction.
    pub fn get(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        self.ensure_active()?;
        match self.db.tree.get(&mut self.db.buffer_pool, key)? {
            Some(record_id) => self.db.record_store.read_visible_payload(
                &mut self.db.buffer_pool,
                record_id,
                Some(self.txn_id),
                &self.isolation_context.snapshot,
            ),
            None => Ok(None),
        }
    }

    /// Inserts or updates a key-value mapping within this transaction.
    pub fn put(&mut self, key: Key, value: &[u8]) -> Result<()> {
        self.ensure_active()?;
        self.db
            .buffer_pool
            .check_fault(crate::engine::failure::StorageFailurePoint::BeforeFirstMutation)?;
        let prev_rid = self.db.tree.get(&mut self.db.buffer_pool, key)?;
        if let Some(old_rid) = prev_rid {
            let (head_meta, _) = self
                .db
                .record_store
                .read_version(&mut self.db.buffer_pool, old_rid)?
                .ok_or(Error::InvalidRecordId(old_rid))?;
            let created_by_self = head_meta.xmin == self.txn_id;

            if !created_by_self {
                if let Some(created_at) = head_meta.created_at {
                    if created_at.0 > self.isolation_context.snapshot.as_of_commit_seq.0 {
                        let conflict =
                            crate::engine::isolation::TransactionConflict::WriteWriteConflict {
                                table: "__kv__".to_string(),
                                key: format!("{key}"),
                                expected_version: None,
                                observed_version: Some(old_rid),
                            };
                        self.write_set.record_conflict(conflict.clone());
                        return Err(Error::TransactionConflict(conflict));
                    }
                }

                if let Some(del_seq) = head_meta.deleted_at {
                    if del_seq.0 > self.isolation_context.snapshot.as_of_commit_seq.0 {
                        let conflict =
                            crate::engine::isolation::TransactionConflict::StaleVersionWrite {
                                table: "__kv__".to_string(),
                                key: format!("{key}"),
                                target_version: old_rid,
                            };
                        self.write_set.record_conflict(conflict.clone());
                        return Err(Error::TransactionConflict(conflict));
                    }
                }
            }

            let is_vis = self.db.record_store.is_record_visible(
                &mut self.db.buffer_pool,
                old_rid,
                Some(self.txn_id),
                &self.isolation_context.snapshot,
            )?;
            if is_vis {
                if let Ok(Some((mut old_meta, _))) = self
                    .db
                    .record_store
                    .read_version(&mut self.db.buffer_pool, old_rid)
                {
                    old_meta.xmax = Some(self.txn_id);
                    self.db.record_store.update_version_metadata(
                        &mut self.db.buffer_pool,
                        old_rid,
                        &old_meta,
                    )?;
                }
                let new_meta =
                    crate::storage::record_store::VersionMetadata::new(self.txn_id, Some(old_rid));
                let new_rid = self.db.record_store.insert_version(
                    &mut self.db.buffer_pool,
                    &new_meta,
                    value,
                )?;
                self.db
                    .tree
                    .update(&mut self.db.buffer_pool, key, new_rid)?;
                self.write_set.record_update(old_rid, new_rid);
                self.write_set.record_row_update(
                    "__kv__",
                    key,
                    format!("{key}"),
                    old_rid,
                    new_rid,
                    self.isolation_context.snapshot.as_of_commit_seq,
                );
            } else {
                let new_meta =
                    crate::storage::record_store::VersionMetadata::new(self.txn_id, Some(old_rid));
                let new_rid = self.db.record_store.insert_version(
                    &mut self.db.buffer_pool,
                    &new_meta,
                    value,
                )?;
                self.db
                    .tree
                    .update(&mut self.db.buffer_pool, key, new_rid)?;
                self.write_set.record_insert(new_rid);
                self.write_set.record_row_insert(
                    "__kv__",
                    key,
                    format!("{key}"),
                    new_rid,
                    self.isolation_context.snapshot.as_of_commit_seq,
                );
            }
        } else {
            let new_meta = crate::storage::record_store::VersionMetadata::new(self.txn_id, None);
            let new_rid =
                self.db
                    .record_store
                    .insert_version(&mut self.db.buffer_pool, &new_meta, value)?;
            self.db
                .tree
                .insert(&mut self.db.buffer_pool, key, new_rid)?;
            self.write_set.record_insert(new_rid);
            self.write_set.record_row_insert(
                "__kv__",
                key,
                format!("{key}"),
                new_rid,
                self.isolation_context.snapshot.as_of_commit_seq,
            );
        }
        self.db
            .buffer_pool
            .check_fault(crate::engine::failure::StorageFailurePoint::AfterPrimaryIndexModified)?;
        Ok(())
    }

    /// Updates an existing key-value mapping within this transaction.
    pub fn update(&mut self, key: Key, value: &[u8]) -> Result<()> {
        self.put(key, value)
    }

    /// Deletes a key and its associated value within this transaction.
    ///
    /// Returns `Ok(Some(Vec<u8>))` with the deleted value bytes if present, or `Ok(None)` if absent.
    pub fn delete(&mut self, key: Key) -> Result<Option<Vec<u8>>> {
        self.ensure_active()?;
        self.db
            .buffer_pool
            .check_fault(crate::engine::failure::StorageFailurePoint::BeforeFirstMutation)?;
        match self.db.tree.get(&mut self.db.buffer_pool, key)? {
            Some(old_rid) => {
                let (head_meta, _) = self
                    .db
                    .record_store
                    .read_version(&mut self.db.buffer_pool, old_rid)?
                    .ok_or(Error::InvalidRecordId(old_rid))?;
                let created_by_self = head_meta.xmin == self.txn_id;

                if !created_by_self {
                    if let Some(created_at) = head_meta.created_at {
                        if created_at.0 > self.isolation_context.snapshot.as_of_commit_seq.0 {
                            let conflict =
                                crate::engine::isolation::TransactionConflict::WriteWriteConflict {
                                    table: "__kv__".to_string(),
                                    key: format!("{key}"),
                                    expected_version: None,
                                    observed_version: Some(old_rid),
                                };
                            self.write_set.record_conflict(conflict.clone());
                            return Err(Error::TransactionConflict(conflict));
                        }
                    }

                    if let Some(del_seq) = head_meta.deleted_at {
                        if del_seq.0 > self.isolation_context.snapshot.as_of_commit_seq.0 {
                            let conflict =
                                crate::engine::isolation::TransactionConflict::StaleVersionWrite {
                                    table: "__kv__".to_string(),
                                    key: format!("{key}"),
                                    target_version: old_rid,
                                };
                            self.write_set.record_conflict(conflict.clone());
                            return Err(Error::TransactionConflict(conflict));
                        } else {
                            return Ok(None);
                        }
                    }
                }

                match self.db.record_store.read_visible(
                    &mut self.db.buffer_pool,
                    old_rid,
                    Some(self.txn_id),
                    &self.isolation_context.snapshot,
                )? {
                    Some((visible_rid, mut old_meta, val)) => {
                        if !created_by_self && visible_rid != old_rid {
                            let conflict =
                                crate::engine::isolation::TransactionConflict::WriteWriteConflict {
                                    table: "__kv__".to_string(),
                                    key: format!("{key}"),
                                    expected_version: Some(visible_rid),
                                    observed_version: Some(old_rid),
                                };
                            self.write_set.record_conflict(conflict.clone());
                            return Err(Error::TransactionConflict(conflict));
                        }
                        old_meta.xmax = Some(self.txn_id);
                        self.db.record_store.update_version_metadata(
                            &mut self.db.buffer_pool,
                            old_rid,
                            &old_meta,
                        )?;
                        self.write_set.record_delete(old_rid);
                        self.write_set.record_row_delete(
                            "__kv__",
                            key,
                            format!("{key}"),
                            visible_rid,
                            self.isolation_context.snapshot.as_of_commit_seq,
                        );
                        Ok(Some(val))
                    }
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }

    /// Performs an ordered range scan over keys in `[start, end)` within this transaction.
    pub fn scan(&mut self, start: Option<Key>, end: Option<Key>) -> Result<Vec<(Key, Vec<u8>)>> {
        self.ensure_active()?;
        let rids = self.db.tree.scan(&mut self.db.buffer_pool, start, end)?;
        let mut results = Vec::with_capacity(rids.len());

        for (k, rid) in rids {
            if let Some(val) = self.db.record_store.read_visible_payload(
                &mut self.db.buffer_pool,
                rid,
                Some(self.txn_id),
                &self.isolation_context.snapshot,
            )? {
                results.push((k, val));
            }
        }

        Ok(results)
    }

    /// Creates a new table in the catalog within this transaction.
    pub fn create_table(
        &mut self,
        name: &str,
        schema: crate::engine::types::Schema,
    ) -> Result<crate::engine::catalog::TableId> {
        self.ensure_active()?;
        let res =
            crate::engine::catalog::Catalog::create_table(&mut self.db.buffer_pool, name, schema);
        if res.is_ok() {
            self.write_set.record_table_mutation(name);
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
        res
    }

    /// Opens an existing table handle for relational operations within this transaction.
    pub fn open_table(&mut self, name: &str) -> Result<crate::engine::table::Table<'_>> {
        self.ensure_active()?;
        let metadata = crate::engine::catalog::Catalog::get_table(&mut self.db.buffer_pool, name)?
            .ok_or_else(|| Error::TableNotFound(name.to_string()))?;
        let tree = crate::index::bplus_tree::BPlusTree::from_root(metadata.root_page_id);
        let record_store =
            crate::storage::record_store::RecordStore::with_last_page(metadata.record_page_id);
        let index_metas = crate::engine::catalog::Catalog::list_indexes_for_table(
            &mut self.db.buffer_pool,
            metadata.table_id,
        )?;
        let mut indexes = Vec::with_capacity(index_metas.len());
        for idx_meta in index_metas {
            let sec_tree =
                crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);
            indexes.push((idx_meta, sec_tree));
        }

        let mut table = crate::engine::table::Table::new_with_isolation(
            metadata,
            tree,
            record_store,
            indexes,
            &mut self.db.buffer_pool,
            Some(self.isolation_context.clone()),
            Some(&mut self.write_set),
        );
        table.set_visibility_map(Some(&mut self.db.visibility_map));
        Ok(table)
    }

    /// Executes a vacuum pass on a specific table within the context of this transaction.
    pub fn vacuum_table(
        &mut self,
        table_name: &str,
        config: &crate::engine::vacuum::VacuumConfig,
    ) -> Result<crate::engine::vacuum::VacuumResult> {
        self.ensure_active()?;
        let mut table = self.open_table(table_name)?;
        table.vacuum(config)
    }

    /// Executes a vacuum pass across all tables within the context of this transaction.
    pub fn vacuum(
        &mut self,
        config: &crate::engine::vacuum::VacuumConfig,
    ) -> Result<crate::engine::vacuum::VacuumResult> {
        self.ensure_active()?;
        let horizon = crate::engine::isolation::ReclamationHorizon::new(
            self.isolation_context.snapshot.as_of_commit_seq,
        );
        let table_names: Vec<String> = {
            let tables = crate::engine::catalog::Catalog::list_tables(&mut self.db.buffer_pool)?;
            tables.into_iter().map(|t| t.name).collect()
        };

        let mut total_result = crate::engine::vacuum::VacuumResult {
            reclamation_horizon: horizon.oldest_required_commit_seq,
            active_snapshots: self.db.snapshot_registry.active_count(),
            ..Default::default()
        };

        for tname in table_names {
            if total_result.versions_examined >= config.max_versions as u64
                || total_result.pages_examined >= config.max_pages as u64
            {
                break;
            }
            let remaining = crate::engine::vacuum::VacuumConfig {
                max_versions: config
                    .max_versions
                    .saturating_sub(total_result.versions_examined as usize),
                max_pages: config
                    .max_pages
                    .saturating_sub(total_result.pages_examined as usize),
            };
            let mut table = self.open_table(&tname)?;
            let t_res = table.vacuum_with_horizon(&remaining, horizon)?;
            total_result.merge(&t_res);
        }

        Ok(total_result)
    }

    /// Creates a new persistent secondary index within this transaction.
    pub fn create_index(
        &mut self,
        table_name: &str,
        index_name: &str,
        column_name: &str,
        unique: bool,
    ) -> Result<crate::engine::catalog::IndexId> {
        self.create_composite_index(table_name, index_name, &[column_name], unique)
    }

    /// Creates a new persistent composite secondary index within this transaction.
    pub fn create_composite_index(
        &mut self,
        table_name: &str,
        index_name: &str,
        column_names: &[&str],
        unique: bool,
    ) -> Result<crate::engine::catalog::IndexId> {
        self.create_covering_index(table_name, index_name, column_names, &[], unique)
    }

    /// Creates a new persistent covering secondary index within this transaction.
    pub fn create_covering_index(
        &mut self,
        table_name: &str,
        index_name: &str,
        column_names: &[&str],
        include_column_names: &[&str],
        unique: bool,
    ) -> Result<crate::engine::catalog::IndexId> {
        self.ensure_active()?;
        let res = crate::engine::catalog::Catalog::create_covering_index(
            &mut self.db.buffer_pool,
            table_name,
            index_name,
            column_names,
            include_column_names,
            unique,
        );
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.index_generation += 1;
            self.db.plan_cache.invalidate_all();
            self.db.feedback_registry.clear_table(table_name);
        }
        res
    }

    /// Drops a persistent secondary index and reclaims its pages within this transaction.
    pub fn drop_index(&mut self, index_name: &str) -> Result<()> {
        self.ensure_active()?;
        let res = crate::engine::catalog::Catalog::drop_index(&mut self.db.buffer_pool, index_name);
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.index_generation += 1;
            self.db.plan_cache.invalidate_all();
            self.db.feedback_registry.clear_all();
        }
        res
    }

    /// Retrieves metadata for a secondary index by name.
    pub fn get_index(
        &mut self,
        name: &str,
    ) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_index(&mut self.db.buffer_pool, name)
    }

    /// Retrieves metadata for a secondary index by `IndexId`.
    pub fn get_index_by_id(
        &mut self,
        id: crate::engine::catalog::IndexId,
    ) -> Result<Option<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_index_by_id(&mut self.db.buffer_pool, id)
    }

    /// Lists all secondary indexes registered in the catalog.
    pub fn list_indexes(&mut self) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::list_indexes(&mut self.db.buffer_pool)
    }

    /// Lists all secondary indexes registered for a specific table.
    pub fn list_indexes_for_table(
        &mut self,
        table_name: &str,
    ) -> Result<Vec<crate::engine::catalog::IndexMetadata>> {
        self.ensure_active()?;
        let table = self
            .get_table(table_name)?
            .ok_or_else(|| Error::TableNotFound(table_name.to_string()))?;
        crate::engine::catalog::Catalog::list_indexes_for_table(
            &mut self.db.buffer_pool,
            table.table_id,
        )
    }

    /// Performs a secondary index lookup returning matching `RecordId`s.
    pub fn index_lookup(
        &mut self,
        table_name: &str,
        index_name: &str,
        value: &crate::engine::types::Value,
    ) -> Result<Vec<crate::index::node::RecordId>> {
        self.ensure_active()?;
        let mut table = self.open_table(table_name)?;
        table.index_lookup_rids(index_name, value)
    }

    /// Performs a secondary index range scan returning matching `(Value, RecordId)` pairs.
    pub fn index_scan(
        &mut self,
        table_name: &str,
        index_name: &str,
        start: Option<&crate::engine::types::Value>,
        end: Option<&crate::engine::types::Value>,
    ) -> Result<Vec<(crate::engine::types::Value, crate::index::node::RecordId)>> {
        self.ensure_active()?;
        let start_key = start.map(crate::index::secondary::IndexKey::from_value);
        let end_key = end.map(crate::index::secondary::IndexKey::from_value);
        let mut table = self.open_table(table_name)?;
        table.scan_index_entries(index_name, start_key.as_ref(), end_key.as_ref())
    }

    /// Validates the structural invariants of a named secondary index.
    pub fn validate_index(&mut self, index_name: &str) -> Result<()> {
        self.ensure_active()?;
        let idx_meta =
            crate::engine::catalog::Catalog::get_index(&mut self.db.buffer_pool, index_name)?
                .ok_or_else(|| Error::IndexNotFound(index_name.to_string()))?;
        let sec_tree = crate::index::secondary::SecondaryIndex::from_root(idx_meta.root_page_id);
        sec_tree.validate_tree(&mut self.db.buffer_pool, idx_meta.unique)
    }

    /// Constructs a programmatic query against a named table within this transaction.
    pub fn query(&mut self, table_name: &str) -> Result<crate::query::Query<'_>> {
        let table = self.open_table(table_name)?;
        Ok(crate::query::Query::new(table))
    }

    /// Plans a parsed `SelectStatement` AST into an optimized physical execution plan (`PhysicalPlan`).
    pub fn plan_select(
        &mut self,
        ast: &crate::sql::SelectStatement,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_select_with_outer_ctes_and_params(ast, &[], None)
    }

    /// Plans a parsed `SelectStatement` AST with supplied parameter values.
    pub fn plan_select_with_params(
        &mut self,
        ast: &crate::sql::SelectStatement,
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_select_with_outer_ctes_and_params(ast, &[], params)
    }

    fn bind_and_validate_ctes_with_params(
        &mut self,
        ctes: &[crate::sql::Cte],
        visible_ctes: &mut Vec<crate::sql::BoundCte>,
        params: Option<&[Value]>,
    ) -> Result<()> {
        validate_ast_cte_acyclicity(ctes)?;
        let mut declared_names = std::collections::HashSet::new();
        let mut mat_id_counter = 0;
        for cte in ctes {
            if !declared_names.insert(&cte.name) {
                return Err(Error::DuplicateCteName(cte.name.clone()));
            }

            match &cte.query {
                crate::sql::CteQuery::NonRecursive(select_stmt) => {
                    // Bind and plan the CTE query using currently visible CTEs
                    let cte_plan = self.plan_select_with_outer_ctes_and_params(
                        select_stmt,
                        visible_ctes,
                        params,
                    )?;

                    // Derive the CTE output schema from cte_plan projections
                    let projected_schema =
                        self.derive_cte_output_schema(&cte_plan, visible_ctes)?;

                    // If explicit column list was provided, validate and rename columns
                    let final_schema = if let Some(cols) = &cte.columns {
                        if cols.len() != projected_schema.columns.len() {
                            return Err(Error::CteColumnCountMismatch {
                                cte: cte.name.clone(),
                                expected: cols.len(),
                                found: projected_schema.columns.len(),
                            });
                        }
                        let mut seen = std::collections::HashSet::new();
                        let mut renamed_cols = Vec::with_capacity(cols.len());
                        for (col_name, orig_col) in cols.iter().zip(projected_schema.columns.iter())
                        {
                            if !seen.insert(col_name) {
                                return Err(Error::DuplicateCteColumnName {
                                    cte: cte.name.clone(),
                                    column: col_name.clone(),
                                });
                            }
                            renamed_cols.push(crate::engine::types::Column::new(
                                col_name,
                                orig_col.data_type,
                                orig_col.nullable,
                            ));
                        }
                        crate::engine::types::Schema::new_projected(renamed_cols)
                    } else {
                        projected_schema
                    };

                    // Track dependencies
                    let mut dependencies = Vec::new();
                    collect_cte_dependencies(&cte_plan, visible_ctes, &mut dependencies);

                    let cte_kind = make_non_recursive_bound_cte(
                        cte_plan,
                        &final_schema,
                        &self.db.materialize_config,
                        1,
                        &mut mat_id_counter,
                    );

                    // Check for cyclic dependencies in visible_ctes + this CTE
                    let mut temp_ctes = visible_ctes.clone();
                    temp_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema.clone(),
                        kind: cte_kind.clone(),
                        dependencies: dependencies.clone(),
                    });
                    validate_cte_acyclicity(&temp_ctes)?;

                    visible_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema,
                        kind: cte_kind,
                        dependencies,
                    });
                }
                crate::sql::CteQuery::SetOperation(set_query) => {
                    let cte_plan = self.plan_set_query_with_outer_ctes_and_params(
                        set_query,
                        visible_ctes,
                        params,
                    )?;
                    let projected_schema =
                        self.derive_cte_output_schema(&cte_plan, visible_ctes)?;

                    let final_schema = if let Some(cols) = &cte.columns {
                        if cols.len() != projected_schema.columns.len() {
                            return Err(Error::CteColumnCountMismatch {
                                cte: cte.name.clone(),
                                expected: cols.len(),
                                found: projected_schema.columns.len(),
                            });
                        }
                        let mut seen = std::collections::HashSet::new();
                        let mut renamed_cols = Vec::with_capacity(cols.len());
                        for (col_name, orig_col) in cols.iter().zip(projected_schema.columns.iter())
                        {
                            if !seen.insert(col_name) {
                                return Err(Error::DuplicateCteColumnName {
                                    cte: cte.name.clone(),
                                    column: col_name.clone(),
                                });
                            }
                            renamed_cols.push(crate::engine::types::Column::new(
                                col_name,
                                orig_col.data_type,
                                orig_col.nullable,
                            ));
                        }
                        crate::engine::types::Schema::new_projected(renamed_cols)
                    } else {
                        projected_schema
                    };

                    let mut dependencies = Vec::new();
                    collect_cte_dependencies(&cte_plan, visible_ctes, &mut dependencies);

                    let cte_kind = make_non_recursive_bound_cte(
                        cte_plan,
                        &final_schema,
                        &self.db.materialize_config,
                        1,
                        &mut mat_id_counter,
                    );

                    let mut temp_ctes = visible_ctes.clone();
                    temp_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema.clone(),
                        kind: cte_kind.clone(),
                        dependencies: dependencies.clone(),
                    });
                    validate_cte_acyclicity(&temp_ctes)?;

                    visible_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema,
                        kind: cte_kind,
                        dependencies,
                    });
                }
                crate::sql::CteQuery::Recursive { anchor, recursive } => {
                    // 1. Check anchor does not reference cte.name
                    if ast_references_table(anchor, &cte.name) {
                        return Err(Error::RecursiveCteAnchorSelfReference(cte.name.clone()));
                    }

                    // 2. Bind and plan anchor
                    let anchor_plan =
                        self.plan_select_with_outer_ctes_and_params(anchor, visible_ctes, params)?;
                    let projected_schema =
                        self.derive_cte_output_schema(&anchor_plan, visible_ctes)?;

                    // 3. Rename columns if explicit columns given
                    let final_schema = if let Some(cols) = &cte.columns {
                        if cols.len() != projected_schema.columns.len() {
                            return Err(Error::CteColumnCountMismatch {
                                cte: cte.name.clone(),
                                expected: cols.len(),
                                found: projected_schema.columns.len(),
                            });
                        }
                        let mut seen = std::collections::HashSet::new();
                        let mut renamed_cols = Vec::with_capacity(cols.len());
                        for (col_name, orig_col) in cols.iter().zip(projected_schema.columns.iter())
                        {
                            if !seen.insert(col_name) {
                                return Err(Error::DuplicateCteColumnName {
                                    cte: cte.name.clone(),
                                    column: col_name.clone(),
                                });
                            }
                            renamed_cols.push(crate::engine::types::Column::new(
                                col_name,
                                orig_col.data_type,
                                orig_col.nullable,
                            ));
                        }
                        crate::engine::types::Schema::new_projected(renamed_cols)
                    } else {
                        projected_schema
                    };

                    let anchor_kind = make_non_recursive_bound_cte(
                        anchor_plan.clone(),
                        &final_schema,
                        &self.db.materialize_config,
                        1,
                        &mut mat_id_counter,
                    );

                    // 4. Create scope where cte.name is available with final_schema
                    let mut rec_visible_ctes = visible_ctes.clone();
                    rec_visible_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema.clone(),
                        kind: anchor_kind,
                        dependencies: Vec::new(),
                    });

                    // 5. Plan recursive member
                    let rec_plan = self.plan_select_with_outer_ctes_and_params(
                        recursive,
                        &rec_visible_ctes,
                        params,
                    )?;

                    // 6. Check unsupported recursive aggregation or window functions
                    let self_ref_in_rec = plan_references_table(&rec_plan, &cte.name);
                    if self_ref_in_rec {
                        if !rec_plan.group_by.is_empty() || !rec_plan.aggregates.is_empty() {
                            return Err(Error::UnsupportedRecursiveAggregation(cte.name.clone()));
                        }
                        if !rec_plan.window_exprs.is_empty() {
                            return Err(Error::UnsupportedRecursiveWindow(cte.name.clone()));
                        }
                    }

                    // 7. Derive recursive member output schema
                    let rec_projected_schema =
                        self.derive_cte_output_schema(&rec_plan, &rec_visible_ctes)?;

                    // 8. Validate schema compatibility between anchor and recursive member
                    if final_schema.columns.len() != rec_projected_schema.columns.len() {
                        return Err(Error::RecursiveCteColumnCountMismatch {
                            cte: cte.name.clone(),
                            anchor_cols: final_schema.columns.len(),
                            rec_cols: rec_projected_schema.columns.len(),
                        });
                    }
                    for (a_col, r_col) in final_schema
                        .columns
                        .iter()
                        .zip(rec_projected_schema.columns.iter())
                    {
                        if a_col.data_type != r_col.data_type {
                            return Err(Error::RecursiveCteSchemaMismatch {
                                cte: cte.name.clone(),
                                anchor_col: a_col.name.clone(),
                                anchor_type: format!("{:?}", a_col.data_type),
                                rec_col: r_col.name.clone(),
                                rec_type: format!("{:?}", r_col.data_type),
                            });
                        }
                    }

                    // 9. Collect dependencies on earlier CTEs (from both anchor and rec member, excluding cte.name)
                    let mut dependencies = Vec::new();
                    collect_cte_dependencies(&anchor_plan, visible_ctes, &mut dependencies);
                    collect_cte_dependencies(&rec_plan, visible_ctes, &mut dependencies);

                    let mut temp_ctes = visible_ctes.clone();
                    temp_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema.clone(),
                        kind: crate::sql::BoundCteKind::Recursive {
                            anchor_plan: anchor_plan.clone(),
                            recursive_plan: rec_plan.clone(),
                            max_iterations: DEFAULT_MAX_RECURSION_ITERATIONS,
                            max_rows: DEFAULT_MAX_RECURSION_ROWS,
                        },
                        dependencies: dependencies.clone(),
                    });
                    validate_cte_acyclicity(&temp_ctes)?;

                    visible_ctes.push(crate::sql::BoundCte {
                        name: cte.name.clone(),
                        output_schema: final_schema,
                        kind: crate::sql::BoundCteKind::Recursive {
                            anchor_plan,
                            recursive_plan: rec_plan,
                            max_iterations: DEFAULT_MAX_RECURSION_ITERATIONS,
                            max_rows: DEFAULT_MAX_RECURSION_ROWS,
                        },
                        dependencies,
                    });
                }
            }
        }
        Ok(())
    }

    /// Plans a parsed `SelectStatement` AST with in-scope outer CTE definitions.
    pub fn plan_select_with_outer_ctes(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.plan_select_with_outer_ctes_and_params(ast, outer_ctes, None)
    }

    /// Validates and binds a parsed `SelectStatement` AST into a `BoundSelect`.
    pub fn bind_select(
        &mut self,
        ast: &crate::sql::SelectStatement,
    ) -> Result<crate::sql::binder::BoundSelect> {
        self.bind_select_with_outer_ctes(ast, &[])
    }

    /// Validates and binds a parsed `SelectStatement` AST with in-scope outer CTE definitions.
    pub fn bind_select_with_outer_ctes(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::sql::binder::BoundSelect> {
        let (bound, _, _) = self.bind_select_with_outer_ctes_and_params(ast, outer_ctes, None)?;
        Ok(bound)
    }

    /// Validates and binds a parsed `SelectStatement` AST with in-scope outer CTE definitions and parameters.
    pub fn bind_select_with_outer_ctes_and_params(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<(
        crate::sql::binder::BoundSelect,
        Vec<crate::query::optimizer::TableOptInfo>,
        Vec<String>,
    )> {
        self.ensure_active()?;
        let mut visible_ctes: Vec<crate::sql::BoundCte> = outer_ctes.to_vec();

        if let Some(with_clause) = &ast.with {
            self.bind_and_validate_ctes_with_params(&with_clause.ctes, &mut visible_ctes, params)?;
        }

        let rewrite_res = {
            let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
            let catalog = TxnBufferPoolCatalog {
                bp: &bp_cell,
                ctes: Some(&visible_ctes),
            };
            crate::query::QueryRewriter::rewrite_select(ast, Some(&catalog))?
        };
        let rewritten_ast = rewrite_res.statement;
        let rewrite_logs = rewrite_res.rewrites;

        // If the FROM or JOIN relations contain non-flattened derived tables, plan them as statement-local CTEs
        let mut mat_id_counter = 0;
        if let crate::sql::ast::TableRef::Derived { query, alias } = &rewritten_ast.from {
            let inner_plan = match query {
                crate::sql::ast::DerivedQuery::Select(select_stmt) => {
                    self.plan_select_with_outer_ctes_and_params(select_stmt, &visible_ctes, params)?
                }
                crate::sql::ast::DerivedQuery::SetOperation(set_stmt) => {
                    self.plan_set_query_with_outer_ctes_and_params(set_stmt, &visible_ctes, params)?
                }
            };
            let output_schema = self.derive_cte_output_schema(&inner_plan, &visible_ctes)?;
            let kind = make_non_recursive_bound_cte(
                inner_plan,
                &output_schema,
                &self.db.materialize_config,
                1,
                &mut mat_id_counter,
            );
            visible_ctes.push(crate::sql::binder::BoundCte {
                name: alias.clone(),
                output_schema,
                kind,
                dependencies: Vec::new(),
            });
        }

        for j in &rewritten_ast.joins {
            if let crate::sql::ast::TableRef::Derived { query, alias } = &j.table {
                let inner_plan = match query {
                    crate::sql::ast::DerivedQuery::Select(select_stmt) => self
                        .plan_select_with_outer_ctes_and_params(
                            select_stmt,
                            &visible_ctes,
                            params,
                        )?,
                    crate::sql::ast::DerivedQuery::SetOperation(set_stmt) => self
                        .plan_set_query_with_outer_ctes_and_params(
                            set_stmt,
                            &visible_ctes,
                            params,
                        )?,
                };
                let output_schema = self.derive_cte_output_schema(&inner_plan, &visible_ctes)?;
                let kind = make_non_recursive_bound_cte(
                    inner_plan,
                    &output_schema,
                    &self.db.materialize_config,
                    1,
                    &mut mat_id_counter,
                );
                visible_ctes.push(crate::sql::binder::BoundCte {
                    name: alias.clone(),
                    output_schema,
                    kind,
                    dependencies: Vec::new(),
                });
            }
        }

        // Now plan the main query body with visible_ctes
        let mut table_infos = Vec::new();
        let mut bindings = Vec::new();

        // 1. Resolve FROM table
        {
            let from_name = rewritten_ast.from.name();
            let from_alias = rewritten_ast.from.alias();
            let (schema, indexes, is_cte) =
                self.resolve_table_for_planning(from_name, &visible_ctes)?;
            let stats = if is_cte {
                None
            } else {
                self.db.get_statistics(from_name).cloned()
            };
            table_infos.push(
                crate::query::optimizer::TableOptInfo::new(from_name, from_alias, schema, indexes)
                    .with_cte(is_cte)
                    .with_stats(stats),
            );
        }

        // 2. Resolve JOIN tables
        for j in &rewritten_ast.joins {
            let join_name = j.table.name();
            let join_alias = j.table.alias();
            let (schema, indexes, is_cte) =
                self.resolve_table_for_planning(join_name, &visible_ctes)?;
            let stats = if is_cte {
                None
            } else {
                self.db.get_statistics(join_name).cloned()
            };
            table_infos.push(
                crate::query::optimizer::TableOptInfo::new(join_name, join_alias, schema, indexes)
                    .with_cte(is_cte)
                    .with_stats(stats),
            );
        }

        for (idx, t) in table_infos.iter().enumerate() {
            let is_semi_or_anti = if idx == 0 {
                false
            } else {
                let j = &rewritten_ast.joins[idx - 1];
                j.join_type == crate::sql::ast::JoinType::Semi
                    || j.join_type == crate::sql::ast::JoinType::Anti
            };
            bindings.push(
                crate::sql::TableBinding::new(&t.name, t.alias.as_deref(), &t.schema)
                    .with_semi_or_anti(is_semi_or_anti),
            );
        }

        let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
        let catalog = TxnBufferPoolCatalog {
            bp: &bp_cell,
            ctes: Some(&visible_ctes),
        };
        let mut bound = crate::sql::binder::bind_select_multi_with_catalog_and_params(
            &rewritten_ast,
            &bindings,
            Some(&catalog),
            params,
        )?;
        bound.ctes = visible_ctes;
        Ok((bound, table_infos, rewrite_logs))
    }

    /// Plans a parsed `SelectStatement` AST with in-scope outer CTE definitions and positional parameters.
    pub fn plan_select_with_outer_ctes_and_params(
        &mut self,
        ast: &crate::sql::SelectStatement,
        outer_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        let (bound, table_infos, rewrite_logs) =
            self.bind_select_with_outer_ctes_and_params(ast, outer_ctes, params)?;

        let param_sig = params.map(crate::query::ParamPlanningSignature::from_values);
        let res_sig = self.resource_planning_signature();
        let mut plan = crate::query::optimizer::Optimizer::optimize_with_resource_feedback(
            &bound,
            &table_infos,
            Some(&self.db.feedback_registry),
            param_sig.as_ref(),
            Some(&res_sig),
            self.db.statistics_generation,
        )?;
        plan.logical_rewrites = rewrite_logs;

        // Refine CTE decisions with exact consumer counts
        let mut refined_ctes = Vec::with_capacity(plan.ctes.len());
        let mut mat_counter = 0;
        for cte in &plan.ctes {
            let mut refined_cte = cte.clone();
            if let crate::sql::BoundCteKind::NonRecursive {
                plan: inner_plan, ..
            } = &cte.kind
            {
                let consumer_count = count_cte_consumers(&plan.ctes, &plan, &cte.name);
                refined_cte.kind = make_non_recursive_bound_cte(
                    inner_plan.clone(),
                    &cte.output_schema,
                    &self.db.materialize_config,
                    consumer_count,
                    &mut mat_counter,
                );
            }
            refined_ctes.push(refined_cte);
        }
        plan.ctes = refined_ctes;

        Ok(plan)
    }

    /// Plans a parsed `SetQuery` AST into an optimized physical execution plan (`PhysicalPlan`).
    pub fn plan_set_query(
        &mut self,
        ast: &crate::sql::SetQuery,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_set_query_with_outer_ctes_and_params(ast, &[], None)
    }

    /// Plans a parsed `SetQuery` AST with supplied parameter values.
    pub fn plan_set_query_with_params(
        &mut self,
        ast: &crate::sql::SetQuery,
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        self.plan_set_query_with_outer_ctes_and_params(ast, &[], params)
    }

    /// Plans a parsed `SetQuery` AST with in-scope outer CTE definitions.
    pub fn plan_set_query_with_outer_ctes(
        &mut self,
        ast: &crate::sql::SetQuery,
        outer_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.plan_set_query_with_outer_ctes_and_params(ast, outer_ctes, None)
    }

    /// Plans a parsed `SetQuery` AST with in-scope outer CTE definitions and positional parameters.
    pub fn plan_set_query_with_outer_ctes_and_params(
        &mut self,
        ast: &crate::sql::SetQuery,
        outer_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let mut visible_ctes: Vec<crate::sql::BoundCte> = outer_ctes.to_vec();

        if let Some(with_clause) = &ast.with {
            self.bind_and_validate_ctes_with_params(&with_clause.ctes, &mut visible_ctes, params)?;
        }

        let mut plan = self.plan_set_query_body_with_params(&ast.body, &visible_ctes, params)?;

        if !ast.order_by.is_empty() {
            let schema = self.derive_cte_output_schema(&plan, &visible_ctes)?;
            plan.order_by =
                self.bind_order_by_for_schema(&schema, &ast.order_by, &visible_ctes, params)?;
            plan.order_by_sort_required = true;
        }

        if let Some(limit) = ast.limit {
            plan.limit = Some(limit as usize);
        }
        if let Some(offset) = ast.offset {
            plan.offset = Some(offset as usize);
        }
        plan.ctes = visible_ctes;

        // Refine CTE decisions with exact consumer counts
        let mut refined_ctes = Vec::with_capacity(plan.ctes.len());
        let mut mat_counter = 0;
        for cte in &plan.ctes {
            let mut refined_cte = cte.clone();
            if let crate::sql::BoundCteKind::NonRecursive {
                plan: inner_plan, ..
            } = &cte.kind
            {
                let consumer_count = count_cte_consumers(&plan.ctes, &plan, &cte.name);
                refined_cte.kind = make_non_recursive_bound_cte(
                    inner_plan.clone(),
                    &cte.output_schema,
                    &self.db.materialize_config,
                    consumer_count,
                    &mut mat_counter,
                );
            }
            refined_ctes.push(refined_cte);
        }
        plan.ctes = refined_ctes;

        Ok(plan)
    }

    /// Validates and binds a list of `OrderByExpr` AST elements against an output schema and catalog.
    pub fn bind_order_by_for_schema(
        &mut self,
        schema: &Schema,
        order_by: &[crate::sql::OrderByExpr],
        visible_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<Vec<crate::query::SortKey>> {
        let mut bound_order_by = Vec::with_capacity(order_by.len());
        for order_item in order_by {
            let bound_expr = match &order_item.expr {
                crate::sql::SqlExpr::Identifier(ident) => {
                    if schema.columns.iter().any(|c| &c.name == ident) {
                        crate::query::expr::Expr::Column(ident.clone())
                    } else {
                        return Err(Error::ColumnNotFound(ident.clone()));
                    }
                }
                _ => {
                    let binding = crate::sql::TableBinding::new("", None, schema);
                    let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
                    let catalog = TxnBufferPoolCatalog {
                        bp: &bp_cell,
                        ctes: Some(visible_ctes),
                    };
                    let mut ctx = crate::sql::binder::BindingContext::with_parent_and_catalog(
                        vec![binding],
                        None,
                        Some(&catalog),
                    );
                    ctx.params = params;
                    crate::sql::binder::bind_expr(&order_item.expr, &ctx)?
                }
            };
            let direction = match order_item.direction {
                crate::sql::OrderDirection::Asc => crate::query::SortDirection::Ascending,
                crate::sql::OrderDirection::Desc => crate::query::SortDirection::Descending,
            };
            bound_order_by.push(crate::query::SortKey::new(bound_expr, direction));
        }
        Ok(bound_order_by)
    }

    fn plan_set_query_body_with_params(
        &mut self,
        body: &crate::sql::SetQueryBody,
        visible_ctes: &[crate::sql::BoundCte],
        params: Option<&[Value]>,
    ) -> Result<crate::query::optimizer::PhysicalPlan> {
        match body {
            crate::sql::SetQueryBody::Select(statement) => {
                self.plan_select_with_outer_ctes_and_params(statement, visible_ctes, params)
            }
            crate::sql::SetQueryBody::Parenthesized(inner) => {
                self.plan_set_query_with_outer_ctes_and_params(inner, visible_ctes, params)
            }
            crate::sql::SetQueryBody::SetOperation { op, left, right } => {
                let left_plan = self.plan_set_query_body_with_params(left, visible_ctes, params)?;
                let right_plan =
                    self.plan_set_query_body_with_params(right, visible_ctes, params)?;
                let left_schema = self.derive_cte_output_schema(&left_plan, visible_ctes)?;
                let right_schema = self.derive_cte_output_schema(&right_plan, visible_ctes)?;

                if left_schema.columns.len() != right_schema.columns.len() {
                    return Err(Error::SetOperationColumnCountMismatch {
                        left_cols: left_schema.columns.len(),
                        right_cols: right_schema.columns.len(),
                    });
                }

                let mut unified_cols = Vec::with_capacity(left_schema.columns.len());
                for (idx, (l_col, r_col)) in left_schema
                    .columns
                    .iter()
                    .zip(right_schema.columns.iter())
                    .enumerate()
                {
                    let unified_type =
                        crate::sql::binder::unify_set_types(l_col.data_type, r_col.data_type)
                            .ok_or_else(|| Error::SetOperationTypeMismatch {
                                col_idx: idx + 1,
                                left_type: format!("{:?}", l_col.data_type),
                                right_type: format!("{:?}", r_col.data_type),
                            })?;
                    unified_cols.push(crate::engine::types::Column::new(
                        l_col.name.clone(),
                        unified_type,
                        l_col.nullable || r_col.nullable,
                    ));
                }
                let unified_schema = crate::engine::types::Schema::new_projected(unified_cols);

                Ok(crate::query::optimizer::PhysicalPlan::new_set_op(
                    crate::query::optimizer::PhysicalSetOp {
                        op: *op,
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        schema: unified_schema,
                    },
                ))
            }
        }
    }

    fn resolve_table_for_planning(
        &mut self,
        table_name: &str,
        visible_ctes: &[crate::sql::BoundCte],
    ) -> Result<(
        crate::engine::types::Schema,
        Vec<crate::engine::catalog::IndexMetadata>,
        bool,
    )> {
        // CTEs shadow physical tables
        for cte in visible_ctes {
            if cte.name == table_name {
                return Ok((cte.output_schema.clone(), Vec::new(), true));
            }
        }
        let table = self.open_table(table_name)?;
        let schema = table.schema().clone();
        let indexes = table.indexes().iter().map(|(m, _)| m.clone()).collect();
        Ok((schema, indexes, false))
    }

    fn derive_cte_output_schema(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        visible_ctes: &[crate::sql::BoundCte],
    ) -> Result<crate::engine::types::Schema> {
        if let Some(set_op) = &plan.set_op {
            return Ok(set_op.schema.clone());
        }
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();

        let (base_schema, _, _) =
            self.resolve_table_for_planning(&plan.base_table.table_name, visible_ctes)?;

        let current_schema = if plan.joins.is_empty() {
            base_schema
        } else {
            let mut cols = Vec::new();
            let q = plan.base_table.qualifier();
            let base_is_nullable = plan.joins.iter().any(|j| {
                j.join_type == crate::sql::ast::JoinType::Right
                    || j.join_type == crate::sql::ast::JoinType::Full
            });
            for col in &base_schema.columns {
                cols.push(crate::engine::types::Column::new(
                    format!("{q}.{}", col.name),
                    col.data_type,
                    col.nullable || base_is_nullable,
                ));
            }
            for join in &plan.joins {
                if join.join_type == crate::sql::ast::JoinType::Semi
                    || join.join_type == crate::sql::ast::JoinType::Anti
                {
                    continue;
                }
                let (right_schema, _, _) =
                    self.resolve_table_for_planning(&join.right.table_name, visible_ctes)?;
                let rq = join.right.qualifier();
                for col in &right_schema.columns {
                    let mut right_col = col.clone();
                    if join.join_type == crate::sql::ast::JoinType::Left
                        || join.join_type == crate::sql::ast::JoinType::Full
                    {
                        right_col.nullable = true;
                    }
                    cols.push(crate::engine::types::Column::new(
                        format!("{rq}.{}", right_col.name),
                        right_col.data_type,
                        right_col.nullable,
                    ));
                }
            }
            crate::engine::types::Schema::new_projected(cols)
        };

        let intermediate_schema = if is_aggregate {
            let mut grouped_cols = Vec::new();
            for gb in &plan.group_by {
                let dt = gb.infer_type(&current_schema)?;
                let name = match gb {
                    crate::query::expr::Expr::Column(n) => n.clone(),
                    _ => format!("{gb}"),
                };
                grouped_cols.push(crate::engine::types::Column::new(name, dt, true));
            }
            for agg in &plan.aggregates {
                let dt = agg.infer_type(&current_schema)?;
                grouped_cols.push(crate::engine::types::Column::new(
                    agg.display_name(),
                    dt,
                    true,
                ));
            }
            crate::engine::types::Schema::new_projected(grouped_cols)
        } else {
            current_schema
        };

        let intermediate_schema = if !plan.window_exprs.is_empty() {
            let mut w_cols = intermediate_schema.columns.clone();
            for we in &plan.window_exprs {
                w_cols.push(crate::engine::types::Column::new(
                    we.display_name.clone(),
                    we.output_type,
                    true,
                ));
            }
            crate::engine::types::Schema::new_projected(w_cols)
        } else {
            intermediate_schema
        };

        let mut out_cols = Vec::with_capacity(plan.projections.len());
        for (idx, (expr, alias)) in plan.projections.iter().enumerate() {
            let dt = expr.infer_type(&intermediate_schema)?;
            let name = match alias {
                Some(alias_name) => alias_name.clone(),
                None => match expr {
                    crate::query::expr::Expr::Column(col_name) => {
                        if let Some(dot_idx) = col_name.find('.') {
                            col_name[dot_idx + 1..].to_string()
                        } else {
                            col_name.clone()
                        }
                    }
                    _ => format!("col_{idx}"),
                },
            };
            let nullable = match expr {
                crate::query::expr::Expr::Column(col_name) => intermediate_schema
                    .get_column(col_name)
                    .map(|c| c.nullable)
                    .unwrap_or(true),
                crate::query::expr::Expr::Literal(Value::Null) => true,
                _ => true,
            };
            out_cols.push(crate::engine::types::Column::new(name, dt, nullable));
        }

        Ok(crate::engine::types::Schema::new_projected(out_cols))
    }
}

fn collect_cte_dependencies(
    plan: &crate::query::optimizer::PhysicalPlan,
    visible_ctes: &[crate::sql::BoundCte],
    deps: &mut Vec<String>,
) {
    if let Some(set_op) = &plan.set_op {
        collect_cte_dependencies(&set_op.left, visible_ctes, deps);
        collect_cte_dependencies(&set_op.right, visible_ctes, deps);
        return;
    }
    let mut check_table = |t_name: &str| {
        for v in visible_ctes {
            if v.name == t_name && !deps.contains(&v.name) {
                deps.push(v.name.clone());
            }
        }
    };

    check_table(&plan.base_table.table_name);
    for j in &plan.joins {
        check_table(&j.right.table_name);
    }
    if let Some(rf) = &plan.base_table.residual_filter {
        collect_cte_deps_from_expr(rf, visible_ctes, deps);
    }
    for j in &plan.joins {
        collect_cte_deps_from_expr(&j.on, visible_ctes, deps);
        if let Some(rf) = &j.right.residual_filter {
            collect_cte_deps_from_expr(rf, visible_ctes, deps);
        }
    }
    if let Some(post) = &plan.post_join_filter {
        collect_cte_deps_from_expr(post, visible_ctes, deps);
    }
    if let Some(h) = &plan.having {
        collect_cte_deps_from_expr(h, visible_ctes, deps);
    }
    for (p, _) in &plan.projections {
        collect_cte_deps_from_expr(p, visible_ctes, deps);
    }
    for o in &plan.order_by {
        collect_cte_deps_from_expr(&o.expression, visible_ctes, deps);
    }
}

fn collect_cte_deps_from_expr(
    expr: &crate::query::expr::Expr,
    visible_ctes: &[crate::sql::BoundCte],
    deps: &mut Vec<String>,
) {
    match expr {
        crate::query::expr::Expr::Subquery(sub) => {
            collect_cte_dependencies(&sub.plan, visible_ctes, deps);
        }
        crate::query::expr::Expr::Binary { left, right, .. } => {
            collect_cte_deps_from_expr(left, visible_ctes, deps);
            collect_cte_deps_from_expr(right, visible_ctes, deps);
        }
        crate::query::expr::Expr::Unary { expr, .. }
        | crate::query::expr::Expr::IsNull(expr)
        | crate::query::expr::Expr::IsNotNull(expr) => {
            collect_cte_deps_from_expr(expr, visible_ctes, deps);
        }
        crate::query::expr::Expr::InList { expr, list, .. } => {
            collect_cte_deps_from_expr(expr, visible_ctes, deps);
            for item in list {
                collect_cte_deps_from_expr(item, visible_ctes, deps);
            }
        }
        crate::query::expr::Expr::Literal(_)
        | crate::query::expr::Expr::Column(_)
        | crate::query::expr::Expr::OuterColumn { .. }
        | crate::query::expr::Expr::Parameter { .. } => {}
    }
}

fn validate_cte_acyclicity(ctes: &[crate::sql::BoundCte]) -> Result<()> {
    let mut adj = std::collections::HashMap::new();
    for cte in ctes {
        adj.insert(cte.name.as_str(), &cte.dependencies);
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum VisitState {
        Unvisited,
        Visiting,
        Visited,
    }

    let mut state = std::collections::HashMap::new();
    for cte in ctes {
        state.insert(cte.name.as_str(), VisitState::Unvisited);
    }

    fn dfs<'a>(
        node: &'a str,
        adj: &std::collections::HashMap<&'a str, &'a Vec<String>>,
        state: &mut std::collections::HashMap<&'a str, VisitState>,
        path: &mut Vec<&'a str>,
        ctes: &[crate::sql::BoundCte],
    ) -> Result<()> {
        state.insert(node, VisitState::Visiting);
        path.push(node);

        if let Some(neighbors) = adj.get(node) {
            for neighbor in *neighbors {
                let n_str = neighbor.as_str();
                match state.get(n_str).copied().unwrap_or(VisitState::Unvisited) {
                    VisitState::Visiting => {
                        path.push(n_str);
                        let cycle_repr = path.join(" -> ");
                        if ctes.iter().any(|c| {
                            (c.name == n_str || path.contains(&c.name.as_str())) && c.is_recursive()
                        }) {
                            return Err(Error::MutualRecursion(cycle_repr));
                        }
                        return Err(Error::CyclicCteDependency(cycle_repr));
                    }
                    VisitState::Unvisited => {
                        dfs(n_str, adj, state, path, ctes)?;
                    }
                    VisitState::Visited => {}
                }
            }
        }

        path.pop();
        state.insert(node, VisitState::Visited);
        Ok(())
    }

    let mut path = Vec::new();
    for cte in ctes {
        if state.get(cte.name.as_str()).copied() == Some(VisitState::Unvisited) {
            dfs(cte.name.as_str(), &adj, &mut state, &mut path, ctes)?;
        }
    }

    Ok(())
}

impl<'db> Transaction<'db> {
    /// Parses, binds, and produces an optimized physical execution plan (`PhysicalPlan`) for the given SQL query or DML filter.
    pub fn plan_sql(&mut self, sql: &str) -> Result<crate::query::optimizer::PhysicalPlan> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => self.plan_select(&select_stmt),
            crate::sql::Statement::SetQuery(set_stmt) => self.plan_set_query(&set_stmt),
            crate::sql::Statement::Explain(explain_stmt) => match explain_stmt.query.as_ref() {
                crate::sql::Statement::Select(select_stmt) => self.plan_select(select_stmt),
                crate::sql::Statement::SetQuery(set_stmt) => self.plan_set_query(set_stmt),
                _ => Err(Error::UnsupportedExplainTarget(format!(
                    "{}",
                    explain_stmt.query
                ))),
            },
            crate::sql::Statement::Update(update_stmt) => {
                let select_ast = crate::sql::SelectStatement {
                    with: None,
                    projection: vec![crate::sql::SelectItem::Wildcard],
                    from: crate::sql::TableRef::new(&update_stmt.table_name),
                    joins: Vec::new(),
                    selection: update_stmt.selection,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                };
                self.plan_select(&select_ast)
            }
            crate::sql::Statement::Delete(delete_stmt) => {
                let select_ast = crate::sql::SelectStatement {
                    with: None,
                    projection: vec![crate::sql::SelectItem::Wildcard],
                    from: crate::sql::TableRef::new(&delete_stmt.table_name),
                    joins: Vec::new(),
                    selection: delete_stmt.selection,
                    group_by: Vec::new(),
                    having: None,
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                };
                self.plan_select(&select_ast)
            }
            _ => Err(Error::QueryExecutionError(format!(
                "cannot plan statement: {stmt}"
            ))),
        }
    }

    /// Generates a human-readable text explanation of the optimized physical execution plan for the given SQL query.
    pub fn explain_sql(&mut self, sql: &str) -> Result<String> {
        let plan = self.plan_sql(sql)?;
        let val_ctx = self.create_validation_context()?;
        Ok(crate::query::PhysicalPlanValidator::format_validation_diagnostics(&plan, &val_ctx))
    }

    /// Generates deterministic validation diagnostics including physical plan fingerprint for a SQL query.
    pub fn explain_sql_with_fingerprint(&mut self, sql: &str) -> Result<String> {
        let plan = self.plan_sql(sql)?;
        let val_ctx = self.create_validation_context()?;
        Ok(
            crate::query::PhysicalPlanValidator::format_validation_diagnostics_with_fingerprint(
                &plan, &val_ctx,
            ),
        )
    }

    /// Computes and returns the deterministic physical plan fingerprint for a SQL query.
    pub fn explain_plan_fingerprint(
        &mut self,
        sql: &str,
    ) -> Result<crate::query::fingerprint::PhysicalPlanFingerprint> {
        let plan = self.plan_sql(sql)?;
        Ok(plan.fingerprint())
    }

    /// Generates deterministic validation and plan explanation diagnostics for an arbitrary physical plan.
    pub fn explain_plan_diagnostics(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
    ) -> String {
        let ctx = match self.create_validation_context() {
            Ok(c) => c,
            Err(e) => return format!("Plan validation failed:\nreason: {e}"),
        };
        crate::query::PhysicalPlanValidator::format_validation_diagnostics(plan, &ctx)
    }

    /// Executes an arbitrary top-level SQL statement (`SELECT`, `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, `DELETE`, `EXPLAIN`)
    /// within this transaction.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_statement(&mut self, sql: &str) -> Result<StatementResult> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Explain(explain_stmt) => {
                let plan = match explain_stmt.query.as_ref() {
                    crate::sql::Statement::Select(select_stmt) => self.plan_select(select_stmt)?,
                    crate::sql::Statement::SetQuery(set_stmt) => self.plan_set_query(set_stmt)?,
                    _ => {
                        return Err(Error::UnsupportedExplainTarget(format!(
                            "{}",
                            explain_stmt.query
                        )))
                    }
                };
                let plan_str = if explain_stmt.analyze {
                    let start = std::time::Instant::now();
                    let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                    self.record_plan_execution_feedback(&plan, &root_op, None);
                    let metrics = crate::query::QueryExecutionMetrics::new(
                        start.elapsed(),
                        res.rows.len(),
                        Some(root_op),
                    );
                    format!(
                        "{}\nValidation: OK",
                        crate::query::metrics::format_plan_with_metrics(&plan, &metrics)
                    )
                } else {
                    let val_ctx = self.create_validation_context()?;
                    crate::query::PhysicalPlanValidator::format_validation_diagnostics(
                        &plan, &val_ctx,
                    )
                };
                let schema = Schema::new_projected(vec![crate::engine::types::Column::new(
                    "plan",
                    DataType::String,
                    false,
                )]);
                let rows = vec![Row::new(vec![Value::String(plan_str)])];
                Ok(StatementResult::Query(crate::query::QueryResult::new(
                    schema, rows,
                )))
            }
            crate::sql::Statement::Select(select_stmt) => {
                let plan = self.plan_select(&select_stmt)?;
                let (query_result, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, None);
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let plan = self.plan_set_query(&set_stmt)?;
                let (query_result, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, None);
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::CreateTable(create_stmt) => {
                self.execute_create_table(&create_stmt)
            }
            crate::sql::Statement::DropTable(drop_stmt) => self.execute_drop_table(&drop_stmt),
            crate::sql::Statement::CreateIndex(create_stmt) => {
                self.execute_create_index(&create_stmt)
            }
            crate::sql::Statement::DropIndex(drop_stmt) => self.execute_drop_index(&drop_stmt),
            crate::sql::Statement::Insert(insert_stmt) => self.execute_insert(&insert_stmt),
            crate::sql::Statement::Update(update_stmt) => self.execute_update(&update_stmt),
            crate::sql::Statement::Delete(delete_stmt) => self.execute_delete(&delete_stmt),
            crate::sql::Statement::Analyze(analyze_stmt) => self.execute_analyze(&analyze_stmt),
        }
    }

    /// Parses, binds, optimizes, and executes an SQL `SELECT` statement within this transaction.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_sql(&mut self, sql: &str) -> Result<crate::query::QueryResult> {
        self.ensure_active()?;
        let res = self.execute_statement(sql)?;
        res.into_query()
    }

    /// Parses, binds, and executes an SQL statement using unoptimized reference relational evaluation.
    ///
    /// Observes prior uncommitted mutations made within the same transaction.
    pub fn execute_sql_reference(&mut self, sql: &str) -> Result<crate::query::QueryResult> {
        self.ensure_active()?;
        crate::query::reference::ReferenceEvaluator::execute_sql(self, sql)
    }

    /// Parses, binds, optimizes, and executes an SQL statement, returning both the query result
    /// and fine-grained runtime execution metrics.
    pub fn execute_sql_with_metrics(
        &mut self,
        sql: &str,
    ) -> Result<(
        crate::query::QueryResult,
        crate::query::QueryExecutionMetrics,
    )> {
        self.ensure_active()?;
        let start = std::time::Instant::now();
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => {
                let plan = self.plan_select(&select_stmt)?;
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, None);
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                );
                Ok((qr, metrics))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let plan = self.plan_set_query(&set_stmt)?;
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, None);
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                );
                Ok((qr, metrics))
            }
            other => {
                let res = match other {
                    crate::sql::Statement::Explain(explain_stmt) => {
                        let plan = match explain_stmt.query.as_ref() {
                            crate::sql::Statement::Select(select_stmt) => {
                                self.plan_select(select_stmt)?
                            }
                            crate::sql::Statement::SetQuery(set_stmt) => {
                                self.plan_set_query(set_stmt)?
                            }
                            _ => {
                                return Err(Error::UnsupportedExplainTarget(format!(
                                    "{}",
                                    explain_stmt.query
                                )))
                            }
                        };
                        let plan_str = if explain_stmt.analyze {
                            let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                            self.record_plan_execution_feedback(&plan, &root_op, None);
                            let metrics = crate::query::QueryExecutionMetrics::new(
                                start.elapsed(),
                                res.rows.len(),
                                Some(root_op),
                            );
                            crate::query::metrics::format_plan_with_metrics(&plan, &metrics)
                        } else {
                            plan.to_string()
                        };
                        let schema =
                            Schema::new_projected(vec![crate::engine::types::Column::new(
                                "plan",
                                DataType::String,
                                false,
                            )]);
                        let rows = vec![Row::new(vec![Value::String(plan_str)])];
                        crate::query::QueryResult::new(schema, rows)
                    }
                    crate::sql::Statement::CreateTable(create_stmt) => {
                        self.execute_create_table(&create_stmt)?.into_query()?
                    }
                    crate::sql::Statement::DropTable(drop_stmt) => {
                        self.execute_drop_table(&drop_stmt)?.into_query()?
                    }
                    crate::sql::Statement::CreateIndex(create_stmt) => {
                        self.execute_create_index(&create_stmt)?.into_query()?
                    }
                    crate::sql::Statement::DropIndex(drop_stmt) => {
                        self.execute_drop_index(&drop_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Insert(insert_stmt) => {
                        self.execute_insert(&insert_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Update(update_stmt) => {
                        self.execute_update(&update_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Delete(delete_stmt) => {
                        self.execute_delete(&delete_stmt)?.into_query()?
                    }
                    crate::sql::Statement::Analyze(analyze_stmt) => {
                        self.execute_analyze(&analyze_stmt)?.into_query()?
                    }
                    _ => unreachable!(),
                };
                let metrics =
                    crate::query::QueryExecutionMetrics::new(start.elapsed(), res.rows.len(), None);
                Ok((res, metrics))
            }
        }
    }

    fn execute_create_table(
        &mut self,
        stmt: &crate::sql::CreateTableStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let mut columns = Vec::with_capacity(stmt.columns.len());
        let mut pk_idx = None;

        for (i, col_def) in stmt.columns.iter().enumerate() {
            if col_def.is_primary_key {
                pk_idx = Some(i);
            }
            columns.push(crate::engine::types::Column::new(
                &col_def.name,
                col_def.data_type,
                col_def.nullable,
            ));
        }

        let pk_index = pk_idx.ok_or_else(|| {
            Error::TableSchemaMismatch("table must define a primary key column".to_string())
        })?;

        let schema = crate::engine::types::Schema::new(columns, pk_index)?;
        self.create_table(&stmt.table_name, schema)?;
        Ok(StatementResult::CreatedTable {
            table: stmt.table_name.clone(),
        })
    }

    fn execute_drop_table(
        &mut self,
        stmt: &crate::sql::DropTableStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        self.drop_table(&stmt.table_name)?;
        Ok(StatementResult::DroppedTable {
            table: stmt.table_name.clone(),
        })
    }

    fn execute_create_index(
        &mut self,
        stmt: &crate::sql::CreateIndexStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let bound = {
            let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
            let catalog = TxnBufferPoolCatalog {
                bp: &bp_cell,
                ctes: None,
            };
            crate::sql::binder::bind_create_index(stmt, &catalog)?
        };
        let cols_refs: Vec<&str> = bound.column_names.iter().map(|s| s.as_str()).collect();
        let inc_refs: Vec<&str> = bound
            .include_column_names
            .iter()
            .map(|s| s.as_str())
            .collect();
        self.create_covering_index(
            &bound.table_name,
            &bound.name,
            &cols_refs,
            &inc_refs,
            bound.unique,
        )?;
        Ok(StatementResult::CreatedIndex { index: bound.name })
    }

    fn execute_drop_index(
        &mut self,
        stmt: &crate::sql::DropIndexStatement,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        {
            let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
            let catalog = TxnBufferPoolCatalog {
                bp: &bp_cell,
                ctes: None,
            };
            crate::sql::binder::bind_drop_index(stmt, &catalog)?;
        }
        self.drop_index(&stmt.name)?;
        Ok(StatementResult::DroppedIndex {
            index: stmt.name.clone(),
        })
    }

    /// Prepares an SQL query string into a reusable `PreparedStatement` with inferred parameter metadata.
    pub fn prepare_sql(&mut self, sql: &str) -> Result<crate::sql::PreparedStatement> {
        self.ensure_active()?;
        let statement = crate::sql::parse_statement(sql)?;
        let bp_cell = std::cell::RefCell::new(&mut self.db.buffer_pool);
        let catalog = TxnBufferPoolCatalog {
            bp: &bp_cell,
            ctes: None,
        };
        let parameters = crate::sql::infer_statement_parameters(&statement, &catalog)?;
        Ok(crate::sql::PreparedStatement::new(
            sql, statement, parameters,
        ))
    }

    fn validate_prepared_params(
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<()> {
        if params.len() != stmt.parameter_count() {
            return Err(Error::ParameterCountMismatch {
                expected: stmt.parameter_count(),
                received: params.len(),
            });
        }
        for param_meta in stmt.parameters() {
            if let Some(expected_type) = param_meta.inferred_type {
                let val = &params[param_meta.index - 1];
                if !val.is_null() {
                    let val_type = val.data_type();
                    if val_type != expected_type {
                        let can_coerce = matches!(
                            (val, expected_type),
                            (Value::Int64(i), DataType::UInt64) if *i >= 0
                        ) || matches!(
                            (val, expected_type),
                            (Value::Int64(_), DataType::Float64)
                        ) || matches!(
                            (val, expected_type),
                            (Value::UInt64(u), DataType::Int64) if *u <= i64::MAX as u64
                        ) || matches!(
                            (val, expected_type),
                            (Value::UInt64(_), DataType::Float64)
                        );
                        if !can_coerce {
                            return Err(Error::ParameterTypeMismatch {
                                param_index: param_meta.index,
                                expected: expected_type.to_string(),
                                found: val_type.to_string(),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Executes a reusable `PreparedStatement` with the provided parameter values within this transaction.
    pub fn execute_prepared(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;

        match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                let res_sig = self.resource_planning_signature();
                let plan = if self.has_catalog_mutations {
                    self.plan_select_with_params(select_stmt, Some(params))?
                } else {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig.clone(),
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let maybe_cached = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = maybe_cached {
                        let is_marked = self
                            .db
                            .plan_cache
                            .get_entry(&key)
                            .map(|e| e.needs_reoptimization())
                            .unwrap_or(false);
                        let val_ok = self
                            .create_validation_context()
                            .map(|c| {
                                crate::query::PhysicalPlanValidator::validate(&cached_plan, &c)
                                    .is_ok()
                            })
                            .unwrap_or(false);
                        let should_replan = is_marked
                            || !val_ok
                            || self.check_plan_cache_should_replan(
                                &cached_plan,
                                Some(param_sig.clone()),
                            );

                        if should_replan {
                            self.db.plan_cache.remove(&key);
                            let new_plan =
                                self.plan_select_with_params(select_stmt, Some(params))?;
                            let (tables, indexes) =
                                crate::query::extract_plan_references(&new_plan);
                            let validity = crate::query::PlanValidity::new(
                                self.db.catalog_generation,
                                self.db.index_generation,
                                self.db.statistics_generation,
                                tables,
                                indexes,
                            )
                            .with_resource_signature(Some(res_sig.clone()));
                            self.db
                                .plan_cache
                                .insert(key.clone(), new_plan.clone(), validity);
                            new_plan
                        } else {
                            cached_plan
                        }
                    } else {
                        let new_plan = self.plan_select_with_params(select_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        )
                        .with_resource_signature(Some(res_sig.clone()));
                        self.db
                            .plan_cache
                            .insert(key.clone(), new_plan.clone(), validity);
                        new_plan
                    }
                };
                let (query_result, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, Some(param_sig.clone()));
                if !self.has_catalog_mutations {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig));
                    if self.check_plan_cache_should_replan(&plan, Some(key.param_signature.clone()))
                    {
                        self.db.plan_cache.mark_needs_reoptimization(&key);
                    }
                }
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                let res_sig = self.resource_planning_signature();
                let plan = if self.has_catalog_mutations {
                    self.plan_set_query_with_params(set_stmt, Some(params))?
                } else {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig.clone(),
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let maybe_cached = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = maybe_cached {
                        let is_marked = self
                            .db
                            .plan_cache
                            .get_entry(&key)
                            .map(|e| e.needs_reoptimization())
                            .unwrap_or(false);
                        let val_ok = self
                            .create_validation_context()
                            .map(|c| {
                                crate::query::PhysicalPlanValidator::validate(&cached_plan, &c)
                                    .is_ok()
                            })
                            .unwrap_or(false);
                        let should_replan = is_marked
                            || !val_ok
                            || self.check_plan_cache_should_replan(
                                &cached_plan,
                                Some(param_sig.clone()),
                            );

                        if should_replan {
                            self.db.plan_cache.remove(&key);
                            let new_plan =
                                self.plan_set_query_with_params(set_stmt, Some(params))?;
                            let (tables, indexes) =
                                crate::query::extract_plan_references(&new_plan);
                            let validity = crate::query::PlanValidity::new(
                                self.db.catalog_generation,
                                self.db.index_generation,
                                self.db.statistics_generation,
                                tables,
                                indexes,
                            )
                            .with_resource_signature(Some(res_sig.clone()));
                            self.db
                                .plan_cache
                                .insert(key.clone(), new_plan.clone(), validity);
                            new_plan
                        } else {
                            cached_plan
                        }
                    } else {
                        let new_plan = self.plan_set_query_with_params(set_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        )
                        .with_resource_signature(Some(res_sig.clone()));
                        self.db
                            .plan_cache
                            .insert(key.clone(), new_plan.clone(), validity);
                        new_plan
                    }
                };
                let (query_result, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, Some(param_sig.clone()));
                if !self.has_catalog_mutations {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig));
                    if self.check_plan_cache_should_replan(&plan, Some(key.param_signature.clone()))
                    {
                        self.db.plan_cache.mark_needs_reoptimization(&key);
                    }
                }
                Ok(StatementResult::Query(query_result))
            }
            crate::sql::Statement::Insert(insert_stmt) => {
                self.execute_insert_with_params(insert_stmt, Some(params))
            }
            crate::sql::Statement::Update(update_stmt) => {
                self.execute_update_with_params(update_stmt, Some(params))
            }
            crate::sql::Statement::Delete(delete_stmt) => {
                self.execute_delete_with_params(delete_stmt, Some(params))
            }
            crate::sql::Statement::Explain(explain_stmt) => {
                let plan = match explain_stmt.query.as_ref() {
                    crate::sql::Statement::Select(select_stmt) => {
                        self.plan_select_with_params(select_stmt, Some(params))?
                    }
                    crate::sql::Statement::SetQuery(set_stmt) => {
                        self.plan_set_query_with_params(set_stmt, Some(params))?
                    }
                    _ => {
                        return Err(Error::UnsupportedExplainTarget(format!(
                            "{}",
                            explain_stmt.query
                        )))
                    }
                };
                let plan_str = if explain_stmt.analyze {
                    let start = std::time::Instant::now();
                    let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                    self.record_plan_execution_feedback(
                        &plan,
                        &root_op,
                        Some(crate::query::ParamPlanningSignature::from_values(params)),
                    );
                    let metrics = crate::query::QueryExecutionMetrics::new(
                        start.elapsed(),
                        res.rows.len(),
                        Some(root_op),
                    );
                    crate::query::metrics::format_plan_with_metrics(&plan, &metrics)
                } else {
                    plan.to_string()
                };
                let schema = Schema::new_projected(vec![crate::engine::types::Column::new(
                    "plan",
                    DataType::String,
                    false,
                )]);
                let rows = vec![Row::new(vec![Value::String(plan_str)])];
                Ok(StatementResult::Query(crate::query::QueryResult::new(
                    schema, rows,
                )))
            }
            crate::sql::Statement::CreateTable(create_stmt) => {
                self.execute_create_table(create_stmt)
            }
            crate::sql::Statement::DropTable(drop_stmt) => self.execute_drop_table(drop_stmt),
            crate::sql::Statement::CreateIndex(create_stmt) => {
                self.execute_create_index(create_stmt)
            }
            crate::sql::Statement::DropIndex(drop_stmt) => self.execute_drop_index(drop_stmt),
            crate::sql::Statement::Analyze(analyze_stmt) => self.execute_analyze(analyze_stmt),
        }
    }

    /// Executes a prepared query statement returning a `QueryResult`.
    pub fn execute_prepared_query(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<crate::query::QueryResult> {
        let res = self.execute_prepared(stmt, params)?;
        res.into_query()
    }

    /// Executes a prepared query statement returning both the query result and runtime metrics.
    pub fn execute_prepared_with_metrics(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<(
        crate::query::QueryResult,
        crate::query::QueryExecutionMetrics,
    )> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;
        let start = std::time::Instant::now();

        match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                let res_sig = self.resource_planning_signature();
                let (plan, is_hit) = if self.has_catalog_mutations {
                    (
                        self.plan_select_with_params(select_stmt, Some(params))?,
                        false,
                    )
                } else {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig.clone(),
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let maybe_cached = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = maybe_cached {
                        let is_marked = self
                            .db
                            .plan_cache
                            .get_entry(&key)
                            .map(|e| e.needs_reoptimization())
                            .unwrap_or(false);
                        let val_ok = self
                            .create_validation_context()
                            .map(|c| {
                                crate::query::PhysicalPlanValidator::validate(&cached_plan, &c)
                                    .is_ok()
                            })
                            .unwrap_or(false);
                        let should_replan = is_marked
                            || !val_ok
                            || self.check_plan_cache_should_replan(
                                &cached_plan,
                                Some(param_sig.clone()),
                            );

                        if should_replan {
                            self.db.plan_cache.remove(&key);
                            let new_plan =
                                self.plan_select_with_params(select_stmt, Some(params))?;
                            let (tables, indexes) =
                                crate::query::extract_plan_references(&new_plan);
                            let validity = crate::query::PlanValidity::new(
                                self.db.catalog_generation,
                                self.db.index_generation,
                                self.db.statistics_generation,
                                tables,
                                indexes,
                            )
                            .with_resource_signature(Some(res_sig.clone()));
                            self.db
                                .plan_cache
                                .insert(key.clone(), new_plan.clone(), validity);
                            (new_plan, false)
                        } else {
                            (cached_plan, true)
                        }
                    } else {
                        let new_plan = self.plan_select_with_params(select_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        )
                        .with_resource_signature(Some(res_sig.clone()));
                        self.db
                            .plan_cache
                            .insert(key.clone(), new_plan.clone(), validity);
                        (new_plan, false)
                    }
                };
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, Some(param_sig.clone()));
                if !self.has_catalog_mutations {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig));
                    if self.check_plan_cache_should_replan(&plan, Some(key.param_signature.clone()))
                    {
                        self.db.plan_cache.mark_needs_reoptimization(&key);
                    }
                }
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                )
                .with_plan_cache_hit(is_hit);
                Ok((qr, metrics))
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let param_sig = crate::query::ParamPlanningSignature::from_values(params);
                let res_sig = self.resource_planning_signature();
                let (plan, is_hit) = if self.has_catalog_mutations {
                    (
                        self.plan_set_query_with_params(set_stmt, Some(params))?,
                        false,
                    )
                } else {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig.clone(),
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let maybe_cached = self.db.plan_cache.get(
                        &key,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    );
                    if let Some(cached_plan) = maybe_cached {
                        let is_marked = self
                            .db
                            .plan_cache
                            .get_entry(&key)
                            .map(|e| e.needs_reoptimization())
                            .unwrap_or(false);
                        let val_ok = self
                            .create_validation_context()
                            .map(|c| {
                                crate::query::PhysicalPlanValidator::validate(&cached_plan, &c)
                                    .is_ok()
                            })
                            .unwrap_or(false);
                        let should_replan = is_marked
                            || !val_ok
                            || self.check_plan_cache_should_replan(
                                &cached_plan,
                                Some(param_sig.clone()),
                            );

                        if should_replan {
                            self.db.plan_cache.remove(&key);
                            let new_plan =
                                self.plan_set_query_with_params(set_stmt, Some(params))?;
                            let (tables, indexes) =
                                crate::query::extract_plan_references(&new_plan);
                            let validity = crate::query::PlanValidity::new(
                                self.db.catalog_generation,
                                self.db.index_generation,
                                self.db.statistics_generation,
                                tables,
                                indexes,
                            )
                            .with_resource_signature(Some(res_sig.clone()));
                            self.db
                                .plan_cache
                                .insert(key.clone(), new_plan.clone(), validity);
                            (new_plan, false)
                        } else {
                            (cached_plan, true)
                        }
                    } else {
                        let new_plan = self.plan_set_query_with_params(set_stmt, Some(params))?;
                        let (tables, indexes) = crate::query::extract_plan_references(&new_plan);
                        let validity = crate::query::PlanValidity::new(
                            self.db.catalog_generation,
                            self.db.index_generation,
                            self.db.statistics_generation,
                            tables,
                            indexes,
                        )
                        .with_resource_signature(Some(res_sig.clone()));
                        self.db
                            .plan_cache
                            .insert(key.clone(), new_plan.clone(), validity);
                        (new_plan, false)
                    }
                };
                let (qr, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
                self.record_plan_execution_feedback(&plan, &root_op, Some(param_sig.clone()));
                if !self.has_catalog_mutations {
                    let key = crate::query::PlanCacheKey::new(
                        &stmt.sql,
                        param_sig,
                        self.db.catalog_generation,
                        self.db.index_generation,
                        self.db.statistics_generation,
                    )
                    .with_resource_signature(Some(res_sig));
                    if self.check_plan_cache_should_replan(&plan, Some(key.param_signature.clone()))
                    {
                        self.db.plan_cache.mark_needs_reoptimization(&key);
                    }
                }
                let metrics = crate::query::QueryExecutionMetrics::new(
                    start.elapsed(),
                    qr.rows.len(),
                    Some(root_op),
                )
                .with_plan_cache_hit(is_hit);
                Ok((qr, metrics))
            }
            _ => {
                let res = self.execute_prepared(stmt, params)?.into_query()?;
                let metrics =
                    crate::query::QueryExecutionMetrics::new(start.elapsed(), res.rows.len(), None);
                Ok((res, metrics))
            }
        }
    }

    /// Explains the physical execution plan for a prepared statement with concrete parameters.
    pub fn explain_prepared(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<String> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;
        let plan = match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                self.plan_select_with_params(select_stmt, Some(params))?
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                self.plan_set_query_with_params(set_stmt, Some(params))?
            }
            _ => {
                return Err(Error::UnsupportedExplainTarget(format!(
                    "{}",
                    stmt.statement
                )))
            }
        };
        Ok(plan.to_string())
    }

    /// Explains and analyzes the physical execution plan for a prepared statement with concrete parameters.
    pub fn explain_prepared_with_metrics(
        &mut self,
        stmt: &crate::sql::PreparedStatement,
        params: &[Value],
    ) -> Result<String> {
        self.ensure_active()?;
        Self::validate_prepared_params(stmt, params)?;
        let start = std::time::Instant::now();
        let plan = match &stmt.statement {
            crate::sql::Statement::Select(select_stmt) => {
                self.plan_select_with_params(select_stmt, Some(params))?
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                self.plan_set_query_with_params(set_stmt, Some(params))?
            }
            _ => {
                return Err(Error::UnsupportedExplainTarget(format!(
                    "{}",
                    stmt.statement
                )))
            }
        };
        let (res, root_op) = self.execute_physical_plan_with_metrics(&plan)?;
        let metrics = crate::query::QueryExecutionMetrics::new(
            start.elapsed(),
            res.rows.len(),
            Some(root_op),
        );
        Ok(crate::query::metrics::format_plan_with_metrics(
            &plan, &metrics,
        ))
    }

    fn execute_insert(&mut self, stmt: &crate::sql::InsertStatement) -> Result<StatementResult> {
        self.execute_insert_with_params(stmt, None)
    }

    fn execute_insert_with_params(
        &mut self,
        stmt: &crate::sql::InsertStatement,
        params: Option<&[Value]>,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let mut table = self.open_table(&stmt.table_name)?;
        let schema = table.schema().clone();

        // 1. Resolve column mapping
        let col_indices: Vec<usize> = if let Some(col_names) = &stmt.columns {
            let mut indices = Vec::with_capacity(col_names.len());
            let mut seen = std::collections::HashSet::new();
            for name in col_names {
                if !seen.insert(name) {
                    return Err(Error::TableSchemaMismatch(format!(
                        "duplicate column '{name}' in INSERT column list"
                    )));
                }
                let idx = schema
                    .columns
                    .iter()
                    .position(|c| &c.name == name)
                    .ok_or_else(|| Error::ColumnNotFound(name.clone()))?;
                indices.push(idx);
            }
            indices
        } else {
            (0..schema.columns.len()).collect()
        };

        // 2. Validate and assemble each row
        let mut rows_to_insert = Vec::with_capacity(stmt.values.len());
        let empty_row = crate::engine::types::Row::new(Vec::new());

        for value_tuple in &stmt.values {
            if value_tuple.len() != col_indices.len() {
                return Err(Error::ColumnCountMismatch {
                    expected: col_indices.len(),
                    found: value_tuple.len(),
                });
            }

            let mut row_values = vec![Value::Null; schema.columns.len()];
            let mut populated = vec![false; schema.columns.len()];

            for (expr, &target_idx) in value_tuple.iter().zip(col_indices.iter()) {
                let target_col = &schema.columns[target_idx];
                let mut ctx =
                    crate::sql::BindingContext::new_single(&stmt.table_name, None, &schema);
                ctx.params = params;
                let bound_expr = crate::sql::bind_expr(expr, &ctx)?;
                let mut evaluated_val = bound_expr.evaluate(&empty_row, &schema)?;

                // Type coercion for numeric literals
                evaluated_val = match (evaluated_val, target_col.data_type) {
                    (Value::Int64(i), DataType::UInt64) if i >= 0 => Value::UInt64(i as u64),
                    (Value::Int64(i), DataType::Float64) => Value::Float64(i as f64),
                    (Value::UInt64(u), DataType::Int64) if u <= i64::MAX as u64 => {
                        Value::Int64(u as i64)
                    }
                    (Value::UInt64(u), DataType::Float64) => Value::Float64(u as f64),
                    (val, _) => val,
                };

                // Validate nullability and type compatibility
                if evaluated_val.is_null() {
                    if !target_col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: target_col.name.clone(),
                        });
                    }
                } else if evaluated_val.data_type() != target_col.data_type {
                    return Err(Error::TypeMismatch {
                        column: target_col.name.clone(),
                        expected: target_col.data_type.to_string(),
                        found: evaluated_val.data_type().to_string(),
                    });
                }

                row_values[target_idx] = evaluated_val;
                populated[target_idx] = true;
            }

            // Check any omitted columns: must be nullable
            for (i, is_pop) in populated.iter().enumerate() {
                if !is_pop {
                    let col = &schema.columns[i];
                    if !col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: col.name.clone(),
                        });
                    }
                    row_values[i] = Value::Null;
                }
            }

            rows_to_insert.push(crate::engine::types::Row::new(row_values));
        }

        // 3. Perform inserts through Table handle
        let mut inserted_count = 0;
        for row in rows_to_insert {
            table.insert(&row)?;
            inserted_count += 1;
        }

        Ok(StatementResult::Inserted {
            rows: inserted_count,
        })
    }

    fn execute_update(&mut self, stmt: &crate::sql::UpdateStatement) -> Result<StatementResult> {
        self.execute_update_with_params(stmt, None)
    }

    fn execute_update_with_params(
        &mut self,
        stmt: &crate::sql::UpdateStatement,
        params: Option<&[Value]>,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let schema = {
            let table = self.open_table(&stmt.table_name)?;
            table.schema().clone()
        };

        // 1. Validate assignments
        let mut col_assignments: Vec<(usize, crate::query::expr::Expr)> =
            Vec::with_capacity(stmt.assignments.len());
        let mut assigned_cols = std::collections::HashSet::new();

        for assign in &stmt.assignments {
            if !assigned_cols.insert(&assign.column_name) {
                return Err(Error::TableSchemaMismatch(format!(
                    "duplicate assignment to column '{}' in UPDATE",
                    assign.column_name
                )));
            }

            let col_idx = schema
                .columns
                .iter()
                .position(|c| c.name == assign.column_name)
                .ok_or_else(|| Error::ColumnNotFound(assign.column_name.clone()))?;

            // Reject primary key column modification
            if col_idx == schema.primary_key_idx {
                return Err(Error::InvalidPrimaryKey(format!(
                    "cannot update primary key column '{}'",
                    assign.column_name
                )));
            }

            let mut ctx = crate::sql::BindingContext::new_single(&stmt.table_name, None, &schema);
            ctx.params = params;
            let bound_expr = crate::sql::bind_expr(&assign.expr, &ctx)?;
            let expr_type = bound_expr.infer_type(&schema)?;
            let target_col = &schema.columns[col_idx];

            if expr_type != DataType::Null && expr_type != target_col.data_type {
                let can_coerce = matches!(
                    (&bound_expr, target_col.data_type),
                    (crate::query::expr::Expr::Literal(Value::Int64(i)), DataType::UInt64) if *i >= 0
                ) || matches!(
                    (&bound_expr, target_col.data_type),
                    (
                        crate::query::expr::Expr::Literal(Value::Int64(_)),
                        DataType::Float64
                    )
                ) || matches!(
                    (&bound_expr, target_col.data_type),
                    (crate::query::expr::Expr::Parameter { value: Value::Int64(i), .. }, DataType::UInt64) if *i >= 0
                ) || matches!(
                    (&bound_expr, target_col.data_type),
                    (
                        crate::query::expr::Expr::Parameter {
                            value: Value::Int64(_),
                            ..
                        },
                        DataType::Float64
                    )
                );
                if !can_coerce {
                    return Err(Error::TypeMismatch {
                        column: target_col.name.clone(),
                        expected: target_col.data_type.to_string(),
                        found: expr_type.to_string(),
                    });
                }
            }

            col_assignments.push((col_idx, bound_expr));
        }

        // 2. Use optimizer to select access path and candidate rows
        let select_ast = crate::sql::SelectStatement {
            with: None,
            projection: vec![crate::sql::SelectItem::Wildcard],
            from: crate::sql::TableRef::new(&stmt.table_name),
            joins: Vec::new(),
            selection: stmt.selection.clone(),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        let mut plan = self.plan_select_with_params(&select_ast, params)?;

        // 3. Fetch candidate rows
        let candidate_rows = if !plan.joins.is_empty() {
            let qr = self.execute_physical_plan(&plan)?;
            qr.rows
        } else {
            self.pre_evaluate_uncorrelated_subqueries(&mut plan, &[])?;
            let multi_index_config = self.db.multi_index_config.clone();
            let mut table = self.open_table(&stmt.table_name)?;
            let initial_rows = match &plan.base_table.access_path {
                crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                    match table.get(key)? {
                        Some(row) => vec![row],
                        None => Vec::new(),
                    }
                }
                crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                    table.scan(start.as_ref(), end.as_ref())?
                }
                crate::query::optimizer::AccessPath::SecondaryIndexScan {
                    index_name,
                    scan_type,
                    ..
                }
                | crate::query::optimizer::AccessPath::IndexOnlyScan {
                    index_name,
                    scan_type,
                    ..
                } => match scan_type {
                    crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                        table.index_lookup(index_name, val)?
                    }
                    crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals)
                    | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                        table.composite_index_lookup(index_name, vals)?
                    }
                    crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                        table.index_scan(index_name, start.as_ref(), end.as_ref())?
                    }
                    crate::query::optimizer::SecondaryScanType::SkipScan {
                        skipped_prefix_columns,
                        constrained_columns,
                    } => {
                        let constrained_values: Vec<Value> =
                            constrained_columns.iter().map(|(_, v)| v.clone()).collect();
                        let (rows, _) = table.index_skip_scan(
                            index_name,
                            skipped_prefix_columns.len(),
                            &constrained_values,
                        )?;
                        rows
                    }
                },
                crate::query::optimizer::AccessPath::MultiIndexScan {
                    strategy,
                    child_scans,
                    ..
                } => {
                    let (rows, _) = table.multi_index_scan_with_config(
                        *strategy,
                        child_scans,
                        &multi_index_config,
                    )?;
                    rows
                }
                crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
                crate::query::optimizer::AccessPath::EmptyResult => Vec::new(),
            };

            match &plan.base_table.residual_filter {
                Some(filter) => self.filter_rows_with_outer(initial_rows, &schema, filter, &[])?,
                None => initial_rows,
            }
        };

        // 4. Update each matching row
        let mut updated_rows = Vec::new();
        for old_row in &candidate_rows {
            let mut new_values = old_row.values.clone();

            for (col_idx, expr) in &col_assignments {
                let target_col = &schema.columns[*col_idx];
                let mut val = self.evaluate_expr_with_outer(expr, old_row, &schema, &[])?;

                val = match (val, target_col.data_type) {
                    (Value::Int64(i), DataType::UInt64) if i >= 0 => Value::UInt64(i as u64),
                    (Value::Int64(i), DataType::Float64) => Value::Float64(i as f64),
                    (Value::UInt64(u), DataType::Int64) if u <= i64::MAX as u64 => {
                        Value::Int64(u as i64)
                    }
                    (Value::UInt64(u), DataType::Float64) => Value::Float64(u as f64),
                    (v, _) => v,
                };

                if val.is_null() {
                    if !target_col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: target_col.name.clone(),
                        });
                    }
                } else if val.data_type() != target_col.data_type {
                    return Err(Error::TypeMismatch {
                        column: target_col.name.clone(),
                        expected: target_col.data_type.to_string(),
                        found: val.data_type().to_string(),
                    });
                }

                new_values[*col_idx] = val;
            }

            updated_rows.push(crate::engine::types::Row::new(new_values));
        }

        let mut updated_count = 0;
        let mut table = self.open_table(&stmt.table_name)?;
        for new_row in updated_rows {
            table.update(&new_row)?;
            updated_count += 1;
        }

        // Record scan feedback for DML
        let param_sig = params.map(crate::query::ParamPlanningSignature::from_values);
        let scan_key = crate::query::FeedbackKey::scan(
            &plan.base_table.table_name,
            plan.base_table.access_path.to_string(),
            plan.base_table
                .residual_filter
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_default(),
            param_sig,
            self.db.statistics_generation,
        );
        let obs = crate::query::FeedbackObservation::new(
            plan.base_table.estimated_rows,
            candidate_rows.len() as f64,
        );
        self.db.feedback_registry.record(scan_key, obs);

        Ok(StatementResult::Updated {
            rows: updated_count,
        })
    }

    fn execute_delete(&mut self, stmt: &crate::sql::DeleteStatement) -> Result<StatementResult> {
        self.execute_delete_with_params(stmt, None)
    }

    fn execute_delete_with_params(
        &mut self,
        stmt: &crate::sql::DeleteStatement,
        params: Option<&[Value]>,
    ) -> Result<StatementResult> {
        self.ensure_active()?;
        let schema = {
            let table = self.open_table(&stmt.table_name)?;
            table.schema().clone()
        };

        // 1. Use optimizer to select access path and candidate rows
        let select_ast = crate::sql::SelectStatement {
            with: None,
            projection: vec![crate::sql::SelectItem::Wildcard],
            from: crate::sql::TableRef::new(&stmt.table_name),
            joins: Vec::new(),
            selection: stmt.selection.clone(),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        };
        let mut plan = self.plan_select_with_params(&select_ast, params)?;

        // 2. Fetch candidate rows
        let candidate_rows = if !plan.joins.is_empty() {
            let qr = self.execute_physical_plan(&plan)?;
            qr.rows
        } else {
            self.pre_evaluate_uncorrelated_subqueries(&mut plan, &[])?;
            let multi_index_config = self.db.multi_index_config.clone();
            let mut table = self.open_table(&stmt.table_name)?;
            let initial_rows = match &plan.base_table.access_path {
                crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                    match table.get(key)? {
                        Some(row) => vec![row],
                        None => Vec::new(),
                    }
                }
                crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                    table.scan(start.as_ref(), end.as_ref())?
                }
                crate::query::optimizer::AccessPath::SecondaryIndexScan {
                    index_name,
                    scan_type,
                    ..
                }
                | crate::query::optimizer::AccessPath::IndexOnlyScan {
                    index_name,
                    scan_type,
                    ..
                } => match scan_type {
                    crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                        table.index_lookup(index_name, val)?
                    }
                    crate::query::optimizer::SecondaryScanType::CompositePointLookup(vals)
                    | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                        table.composite_index_lookup(index_name, vals)?
                    }
                    crate::query::optimizer::SecondaryScanType::RangeScan { start, end } => {
                        table.index_scan(index_name, start.as_ref(), end.as_ref())?
                    }
                    crate::query::optimizer::SecondaryScanType::SkipScan {
                        skipped_prefix_columns,
                        constrained_columns,
                    } => {
                        let constrained_values: Vec<Value> =
                            constrained_columns.iter().map(|(_, v)| v.clone()).collect();
                        let (rows, _) = table.index_skip_scan(
                            index_name,
                            skipped_prefix_columns.len(),
                            &constrained_values,
                        )?;
                        rows
                    }
                },
                crate::query::optimizer::AccessPath::MultiIndexScan {
                    strategy,
                    child_scans,
                    ..
                } => {
                    let (rows, _) = table.multi_index_scan_with_config(
                        *strategy,
                        child_scans,
                        &multi_index_config,
                    )?;
                    rows
                }
                crate::query::optimizer::AccessPath::FullTableScan => table.scan(None, None)?,
                crate::query::optimizer::AccessPath::EmptyResult => Vec::new(),
            };

            match &plan.base_table.residual_filter {
                Some(filter) => self.filter_rows_with_outer(initial_rows, &schema, filter, &[])?,
                None => initial_rows,
            }
        };

        // 3. Delete matching rows by primary key
        let mut deleted_count = 0;
        let mut table = self.open_table(&stmt.table_name)?;

        for row in &candidate_rows {
            let pk_val = &row.values[schema.primary_key_idx];
            if table.delete(pk_val)?.is_some() {
                deleted_count += 1;
            }
        }

        // Record scan feedback for DML
        let param_sig = params.map(crate::query::ParamPlanningSignature::from_values);
        let scan_key = crate::query::FeedbackKey::scan(
            &plan.base_table.table_name,
            plan.base_table.access_path.to_string(),
            plan.base_table
                .residual_filter
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_default(),
            param_sig,
            self.db.statistics_generation,
        );
        let obs = crate::query::FeedbackObservation::new(
            plan.base_table.estimated_rows,
            candidate_rows.len() as f64,
        );
        self.db.feedback_registry.record(scan_key, obs);

        Ok(StatementResult::Deleted {
            rows: deleted_count,
        })
    }

    fn execute_analyze(&mut self, stmt: &crate::sql::AnalyzeStatement) -> Result<StatementResult> {
        self.ensure_active()?;
        self.collect_statistics(&stmt.table_name)?;
        Ok(StatementResult::AnalyzedTable {
            table: stmt.table_name.clone(),
        })
    }

    /// Derives and registers runtime statistics for the named table within this transaction.
    pub fn collect_statistics(
        &mut self,
        table_name: &str,
    ) -> Result<crate::query::stats::TableStatistics> {
        self.ensure_active()?;
        let mut table = self.open_table(table_name)?;
        let stats = table.collect_statistics()?;
        self.db.set_statistics(stats.clone());
        Ok(stats)
    }

    /// Retrieves registered statistics for a table if available.
    pub fn get_statistics(
        &self,
        table_name: &str,
    ) -> Option<&crate::query::stats::TableStatistics> {
        self.db.get_statistics(table_name)
    }

    /// Sets or overrides statistics for a table.
    pub fn set_statistics(&mut self, stats: crate::query::stats::TableStatistics) {
        self.db.set_statistics(stats);
    }

    /// Removes registered statistics for a table.
    pub fn clear_statistics(&mut self, table_name: &str) {
        self.db.clear_statistics(table_name);
    }

    /// Returns a reference to the active external sort configuration.
    pub fn sort_config(&self) -> &crate::query::external_sort::SortConfig {
        self.db.sort_config()
    }

    /// Returns a mutable reference to the active external sort configuration.
    pub fn sort_config_mut(&mut self) -> &mut crate::query::external_sort::SortConfig {
        self.db.sort_config_mut()
    }

    /// Sets the active external sort configuration.
    pub fn set_sort_config(&mut self, config: crate::query::external_sort::SortConfig) {
        self.db.set_sort_config(config);
    }

    /// Returns a reference to the active external aggregation configuration.
    pub fn aggregate_config(&self) -> &crate::query::external_aggregate::AggregateConfig {
        self.db.aggregate_config()
    }

    /// Returns a mutable reference to the active external aggregation configuration.
    pub fn aggregate_config_mut(
        &mut self,
    ) -> &mut crate::query::external_aggregate::AggregateConfig {
        self.db.aggregate_config_mut()
    }

    /// Sets the active external aggregation configuration.
    pub fn set_aggregate_config(
        &mut self,
        config: crate::query::external_aggregate::AggregateConfig,
    ) {
        self.db.set_aggregate_config(config);
    }

    /// Returns a reference to the active external multi-index candidate configuration.
    pub fn multi_index_config(&self) -> &crate::query::external_multi_index::MultiIndexConfig {
        self.db.multi_index_config()
    }

    /// Returns a mutable reference to the active external multi-index candidate configuration.
    pub fn multi_index_config_mut(
        &mut self,
    ) -> &mut crate::query::external_multi_index::MultiIndexConfig {
        self.db.multi_index_config_mut()
    }

    /// Sets the active external multi-index candidate configuration.
    pub fn set_multi_index_config(
        &mut self,
        config: crate::query::external_multi_index::MultiIndexConfig,
    ) {
        self.db.set_multi_index_config(config);
    }

    /// Returns a reference to the active materialization configuration.
    pub fn materialize_config(&self) -> &crate::query::materialize::MaterializeConfig {
        self.db.materialize_config()
    }

    /// Sets the active materialization configuration.
    pub fn set_materialize_config(&mut self, config: crate::query::materialize::MaterializeConfig) {
        self.db.set_materialize_config(config);
    }

    /// Sets a test-only materialization override.
    pub fn set_materialize_override(
        &mut self,
        test_override: crate::query::materialize::MaterializeOverride,
    ) {
        self.db.set_materialize_override(test_override);
    }

    /// Constructs a `PlanValidationContext` from the current transaction, catalog, and resource state.
    pub fn create_validation_context(&mut self) -> Result<crate::query::PlanValidationContext> {
        let mut ctx = crate::query::PlanValidationContext::new()
            .with_generations(
                self.db.catalog_generation,
                self.db.index_generation,
                self.db.statistics_generation,
            )
            .with_resource_signature(Some(self.resource_planning_signature()))
            .with_catalog_authoritative(true);

        let tables = crate::engine::catalog::Catalog::list_tables(&mut self.db.buffer_pool)?;
        for tbl_meta in tables {
            let indexes = crate::engine::catalog::Catalog::list_indexes_for_table(
                &mut self.db.buffer_pool,
                tbl_meta.table_id,
            )?;
            ctx = ctx.with_table(crate::query::TableValidationInfo {
                name: tbl_meta.name.clone(),
                alias: None,
                schema: tbl_meta.schema.clone(),
                indexes,
                is_cte: false,
            });
        }
        Ok(ctx)
    }

    /// Validates a physical plan against this transaction's active catalog and resource contracts.
    pub fn validate_physical_plan(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
    ) -> Result<crate::query::ValidatedPhysicalPlan> {
        let ctx = self.create_validation_context()?;
        crate::query::PhysicalPlanValidator::validate(plan, &ctx).map_err(crate::error::Error::from)
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction after validating it.
    pub fn execute_physical_plan(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
    ) -> Result<crate::query::QueryResult> {
        let validated = self.validate_physical_plan(plan)?;
        self.execute_validated_physical_plan(&validated)
    }

    /// Executes a pre-validated `ValidatedPhysicalPlan` against this transaction.
    pub fn execute_validated_physical_plan(
        &mut self,
        plan: &crate::query::ValidatedPhysicalPlan,
    ) -> Result<crate::query::QueryResult> {
        self.execute_validated_physical_plan_with_outer(plan, &[])
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction with outer scope contexts after validating it.
    pub fn execute_physical_plan_with_outer(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<crate::query::QueryResult> {
        let validated = self.validate_physical_plan(plan)?;
        self.execute_validated_physical_plan_with_outer(&validated, outer_contexts)
    }

    /// Executes a pre-validated `ValidatedPhysicalPlan` against this transaction with outer scope contexts.
    pub fn execute_validated_physical_plan_with_outer(
        &mut self,
        plan: &crate::query::ValidatedPhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<crate::query::QueryResult> {
        let mut cte_store = std::collections::HashMap::new();
        let (qr, _metrics) = self.execute_physical_plan_internal_with_metrics(
            plan.plan(),
            outer_contexts,
            &mut cte_store,
        )?;
        Ok(qr)
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction, returning both query result
    /// and metrics after validating the plan.
    pub fn execute_physical_plan_with_metrics(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        let validated = self.validate_physical_plan(plan)?;
        self.execute_validated_physical_plan_with_metrics(&validated)
    }

    /// Executes a fully optimized `PhysicalPlan` against this transaction using an explicit `ExecutionContext`.
    pub fn execute_physical_plan_with_context(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        ctx: &mut crate::query::ExecutionContext,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        let validated = self.validate_physical_plan(plan)?;
        self.execute_validated_physical_plan_with_context(&validated, ctx)
    }

    /// Executes a pre-validated `ValidatedPhysicalPlan` against this transaction using an explicit `ExecutionContext`.
    pub fn execute_validated_physical_plan_with_context(
        &mut self,
        plan: &crate::query::ValidatedPhysicalPlan,
        ctx: &mut crate::query::ExecutionContext,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        let mut cte_store = std::collections::HashMap::new();
        self.execute_physical_plan_internal_with_context(plan.plan(), &[], &mut cte_store, ctx)
    }

    /// Executes an SQL query using an explicit `ExecutionContext`.
    pub fn execute_sql_with_context(
        &mut self,
        sql: &str,
        ctx: &mut crate::query::ExecutionContext,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        self.ensure_active()?;
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => {
                let plan = self.plan_select(&select_stmt)?;
                self.execute_physical_plan_with_context(&plan, ctx)
            }
            crate::sql::Statement::SetQuery(set_stmt) => {
                let plan = self.plan_set_query(&set_stmt)?;
                self.execute_physical_plan_with_context(&plan, ctx)
            }
            _ => Err(Error::UnsupportedStatement(sql.to_string())),
        }
    }

    /// Executes a pre-validated `ValidatedPhysicalPlan` against this transaction, returning both query result and metrics.
    pub fn execute_validated_physical_plan_with_metrics(
        &mut self,
        plan: &crate::query::ValidatedPhysicalPlan,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        let mut cte_store = std::collections::HashMap::new();
        self.execute_physical_plan_internal_with_metrics(plan.plan(), &[], &mut cte_store)
    }

    fn execute_physical_plan_internal(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<crate::query::QueryResult> {
        let (res, _metrics) =
            self.execute_physical_plan_internal_with_metrics(plan, outer_contexts, cte_store)?;
        Ok(res)
    }

    fn execute_physical_plan_internal_with_metrics(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        let mut ctx = crate::query::ExecutionContext::for_txn_with_isolation(
            self.txn_id,
            self.isolation_context.clone(),
        );
        self.execute_physical_plan_internal_with_context(plan, outer_contexts, cte_store, &mut ctx)
    }

    fn execute_physical_plan_internal_with_context(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
        ctx: &mut crate::query::ExecutionContext,
    ) -> Result<(crate::query::QueryResult, crate::query::OperatorMetrics)> {
        self.ensure_active()?;
        ctx.check_cancellation()?;
        let op_start = std::time::Instant::now();

        // 0. Materialize any CTEs defined on this plan (in declaration order)
        let mut cte_metrics = Vec::new();
        for cte in &plan.ctes {
            if !cte_store.contains_key(&cte.name) {
                match &cte.kind {
                    crate::sql::BoundCteKind::NonRecursive {
                        plan: cte_plan,
                        decision,
                        materialize_id,
                        consumer_count,
                        ..
                    } => match decision {
                        crate::query::materialize::MaterializationDecision::Materialize
                        | crate::query::materialize::MaterializationDecision::ExternalMaterialize =>
                        {
                            let force_spill = matches!(
                                    decision,
                                    crate::query::materialize::MaterializationDecision::ExternalMaterialize
                                );
                            let (res, cte_op) = self.execute_physical_plan_internal_with_context(
                                cte_plan,
                                outer_contexts,
                                cte_store,
                                ctx,
                            )?;
                            let mat_id = materialize_id.unwrap_or(1);
                            let (storage, mat_metrics) =
                                crate::query::materialize::MaterializeOperator::materialize_rows(
                                    mat_id,
                                    res.rows.clone(),
                                    &cte.output_schema,
                                    *consumer_count,
                                    self.materialize_config(),
                                    force_spill,
                                )?;
                            let is_external = matches!(
                                storage,
                                crate::query::materialize::MaterializedStorage::Spilled { .. }
                            );

                            let mut mat_op = crate::query::OperatorMetrics::new(
                                crate::query::OperatorKind::Materialize {
                                    id: mat_id,
                                    name: Some(cte.name.clone()),
                                    external: is_external,
                                },
                                Some(cte_plan.estimated_rows),
                                Some(cte_plan.total_cost),
                            );
                            mat_op.actual_rows = mat_metrics.rows_materialized;
                            mat_op.input_rows = mat_metrics.rows_materialized;
                            mat_op.counters.materialize_id = Some(mat_id);
                            mat_op.counters.rows_materialized = mat_metrics.rows_materialized;
                            mat_op.counters.bytes_materialized = mat_metrics.bytes_materialized;
                            mat_op.counters.materialize_consumers = *consumer_count;
                            mat_op.counters.peak_memory_bytes = mat_metrics.peak_memory_bytes;
                            mat_op.counters.materialize_spill_bytes = mat_metrics.spill_bytes;
                            mat_op.counters.materialize_spill_files = mat_metrics.spill_files;
                            mat_op.add_child(cte_op);
                            cte_metrics.push(mat_op);

                            cte_store
                                .insert(cte.name.clone(), (cte.output_schema.clone(), res.rows));
                        }
                        crate::query::materialize::MaterializationDecision::Inline => {}
                    },
                    crate::sql::BoundCteKind::Recursive {
                        anchor_plan,
                        recursive_plan,
                        max_iterations,
                        max_rows,
                    } => {
                        let (anchor_res, anchor_op) = self
                            .execute_physical_plan_internal_with_context(
                                anchor_plan,
                                outer_contexts,
                                cte_store,
                                ctx,
                            )?;
                        let mut accumulated_rows = anchor_res.rows;
                        if accumulated_rows.len() > *max_rows {
                            return Err(Error::RecursiveCteRowLimit {
                                cte: cte.name.clone(),
                                rows: accumulated_rows.len(),
                                limit: *max_rows,
                            });
                        }
                        let mut working_rows = accumulated_rows.clone();
                        let mut iterations = 0;

                        while !working_rows.is_empty() {
                            ctx.check_cancellation()?;
                            iterations += 1;
                            if iterations > *max_iterations {
                                return Err(Error::RecursiveCteIterationLimit {
                                    cte: cte.name.clone(),
                                    iterations,
                                    limit: *max_iterations,
                                });
                            }

                            cte_store.insert(
                                cte.name.clone(),
                                (cte.output_schema.clone(), working_rows),
                            );

                            let (next_res, _next_op) = self
                                .execute_physical_plan_internal_with_context(
                                    recursive_plan,
                                    outer_contexts,
                                    cte_store,
                                    ctx,
                                )?;
                            let next_working_rows = next_res.rows;

                            if next_working_rows.is_empty() {
                                break;
                            }

                            if accumulated_rows.len() + next_working_rows.len() > *max_rows {
                                return Err(Error::RecursiveCteRowLimit {
                                    cte: cte.name.clone(),
                                    rows: accumulated_rows.len() + next_working_rows.len(),
                                    limit: *max_rows,
                                });
                            }

                            accumulated_rows.extend(next_working_rows.clone());
                            working_rows = next_working_rows;
                        }

                        cte_store.insert(
                            cte.name.clone(),
                            (cte.output_schema.clone(), accumulated_rows.clone()),
                        );

                        let mut rec_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::RecursiveCte {
                                name: cte.name.clone(),
                                iterations,
                            },
                            Some(anchor_plan.estimated_rows),
                            Some(anchor_plan.total_cost),
                        );
                        rec_metric.actual_rows = accumulated_rows.len();
                        rec_metric.input_rows = accumulated_rows.len();
                        rec_metric.add_child(anchor_op);
                        cte_metrics.push(rec_metric);
                    }
                }
            }
        }

        let mut plan = plan.clone();
        self.pre_evaluate_uncorrelated_subqueries_internal(&mut plan, outer_contexts, cte_store)?;

        if let Some(set_op) = &plan.set_op {
            let (left_res, left_op) = self.execute_physical_plan_internal_with_context(
                &set_op.left,
                outer_contexts,
                cte_store,
                ctx,
            )?;
            let (right_res, right_op) = self.execute_physical_plan_internal_with_context(
                &set_op.right,
                outer_contexts,
                cte_store,
                ctx,
            )?;
            let left_count = left_res.rows.len();
            let right_count = right_res.rows.len();
            let set_rows = crate::query::SetOperationOperator::apply(
                left_res.rows,
                right_res.rows,
                &set_op.schema,
                &set_op.op,
            )?;
            let mut set_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::SetOperation {
                    op: set_op.op.to_string(),
                },
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            set_metric.input_rows = left_count + right_count;
            set_metric.actual_rows = set_rows.len();
            set_metric.add_child(left_op);
            set_metric.add_child(right_op);

            let mut current_op = set_metric;
            let mut current_rows = set_rows;

            if !plan.order_by.is_empty() && plan.order_by_sort_required {
                let (sorted_rows, sort_metrics) = crate::query::SortOperator::apply_with_config(
                    current_rows,
                    &set_op.schema,
                    &plan.order_by,
                    &self.db.sort_config,
                )?;
                let mut sort_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::Sort,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                sort_metric.input_rows = sort_metrics.rows_input;
                sort_metric.actual_rows = sorted_rows.len();
                sort_metric.counters.runs_created = sort_metrics.runs_created;
                sort_metric.counters.bytes_spilled = sort_metrics.bytes_spilled;
                sort_metric.counters.merge_passes = sort_metrics.merge_passes;
                sort_metric.counters.peak_memory_bytes = sort_metrics.peak_memory_bytes;
                if sort_metrics.peak_memory_bytes > 0 {
                    ctx.resource_manager
                        .reserve_memory(sort_metrics.peak_memory_bytes)?;
                    ctx.resource_manager
                        .release_memory(sort_metrics.peak_memory_bytes);
                }
                sort_metric.counters.sort_required = true;
                sort_metric.counters.sort_eliminated = false;
                sort_metric.add_child(current_op);
                current_op = sort_metric;
                current_rows = sorted_rows;
            } else if !plan.order_by.is_empty() {
                current_op.counters.sort_eliminated = true;
                current_op.counters.sort_required = false;
            }

            if plan.offset.is_some() || plan.limit.is_some() {
                let paginated_rows =
                    crate::query::OffsetLimitOperator::apply(current_rows, plan.offset, plan.limit);
                let mut limit_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::LimitOffset,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                limit_metric.input_rows = current_op.actual_rows;
                limit_metric.actual_rows = paginated_rows.len();
                limit_metric.add_child(current_op);
                current_op = limit_metric;
                current_rows = paginated_rows;
            }

            current_op.execution_time = op_start.elapsed();
            return Ok((
                crate::query::QueryResult::new(set_op.schema.clone(), current_rows),
                current_op,
            ));
        }

        ctx.check_cancellation()?;

        // 1. Execute base table scan
        let (mut current_schema, mut current_rows, mut current_op) = {
            if let Some((cte_schema, cte_rows)) = cte_store.get(&plan.base_table.table_name) {
                let raw_schema = cte_schema.clone();
                let initial_rows = cte_rows.clone();
                let mut scan_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::TableScan {
                        table_name: plan.base_table.table_name.clone(),
                        access_path: "CTE Scan".to_string(),
                    },
                    Some(plan.base_table.estimated_rows),
                    Some(plan.base_table.cost),
                );
                scan_metric.input_rows = initial_rows.len();
                scan_metric.actual_rows = initial_rows.len();
                scan_metric.counters.tuples_fetched = initial_rows.len();
                scan_metric.counters.rows_evaluated = initial_rows.len();

                let (filtered_rows, table_op) = match &plan.base_table.residual_filter {
                    Some(filter) => {
                        let input_rows_len = initial_rows.len();
                        let filtered = self.filter_rows_with_outer_internal(
                            initial_rows,
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut filter_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Filter,
                            Some(plan.base_table.estimated_rows),
                            Some(plan.base_table.cost),
                        );
                        filter_metric.input_rows = input_rows_len;
                        filter_metric.actual_rows = filtered.len();
                        filter_metric.counters.rows_evaluated = input_rows_len;
                        filter_metric.counters.rows_passed = filtered.len();
                        filter_metric.counters.rows_rejected =
                            input_rows_len.saturating_sub(filtered.len());
                        filter_metric.add_child(scan_metric);
                        (filtered, filter_metric)
                    }
                    None => (initial_rows, scan_metric),
                };

                let schema = if plan.joins.is_empty() {
                    raw_schema
                } else {
                    let q = plan.base_table.qualifier();
                    let cols: Vec<crate::engine::types::Column> = raw_schema
                        .columns
                        .iter()
                        .map(|c| {
                            crate::engine::types::Column::new(
                                format!("{q}.{}", c.name),
                                c.data_type,
                                c.nullable,
                            )
                        })
                        .collect();
                    crate::engine::types::Schema::new_projected(cols)
                };

                (schema, filtered_rows, table_op)
            } else if let Some(cte) = plan
                .ctes
                .iter()
                .find(|c| c.name == plan.base_table.table_name)
            {
                let (res, inner_op) = match &cte.kind {
                    crate::sql::BoundCteKind::NonRecursive {
                        plan: inner_plan, ..
                    } => self.execute_physical_plan_internal_with_context(
                        inner_plan,
                        outer_contexts,
                        cte_store,
                        ctx,
                    )?,
                    crate::sql::BoundCteKind::Recursive { .. } => {
                        return Err(Error::TableNotFound(plan.base_table.table_name.clone()));
                    }
                };
                let raw_schema = cte.output_schema.clone();
                let initial_rows = res.rows;
                let mut scan_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::TableScan {
                        table_name: plan.base_table.table_name.clone(),
                        access_path: "Inlined CTE Scan".to_string(),
                    },
                    Some(plan.base_table.estimated_rows),
                    Some(plan.base_table.cost),
                );
                scan_metric.input_rows = initial_rows.len();
                scan_metric.actual_rows = initial_rows.len();
                scan_metric.counters.tuples_fetched = initial_rows.len();
                scan_metric.counters.rows_evaluated = initial_rows.len();
                scan_metric.add_child(inner_op);

                let (filtered_rows, table_op) = match &plan.base_table.residual_filter {
                    Some(filter) => {
                        let input_rows_len = initial_rows.len();
                        let filtered = self.filter_rows_with_outer_internal(
                            initial_rows,
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut filter_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Filter,
                            Some(plan.base_table.estimated_rows),
                            Some(plan.base_table.cost),
                        );
                        filter_metric.input_rows = input_rows_len;
                        filter_metric.actual_rows = filtered.len();
                        filter_metric.counters.rows_evaluated = input_rows_len;
                        filter_metric.counters.rows_passed = filtered.len();
                        filter_metric.counters.rows_rejected =
                            input_rows_len.saturating_sub(filtered.len());
                        filter_metric.add_child(scan_metric);
                        (filtered, filter_metric)
                    }
                    None => (initial_rows, scan_metric),
                };

                let schema = if plan.joins.is_empty() {
                    raw_schema
                } else {
                    let q = plan.base_table.qualifier();
                    let cols: Vec<crate::engine::types::Column> = raw_schema
                        .columns
                        .iter()
                        .map(|c| {
                            crate::engine::types::Column::new(
                                format!("{q}.{}", c.name),
                                c.data_type,
                                c.nullable,
                            )
                        })
                        .collect();
                    crate::engine::types::Schema::new_projected(cols)
                };

                (schema, filtered_rows, table_op)
            } else {
                let multi_index_config = self.db.multi_index_config.clone();
                let mut table = self.open_table(&plan.base_table.table_name)?;
                let raw_schema = table.schema().clone();
                let mut index_lookups = 0;
                let mut is_index_only = false;
                let mut skip_scan_stats: Option<crate::engine::table::SkipScanStats> = None;
                let mut multi_index_stats: Option<crate::engine::table::MultiIndexScanStats> = None;

                let is_index_agg = matches!(
                    plan.aggregate_strategy,
                    Some(crate::query::optimizer::AggregateStrategy::Index)
                );
                let order_preserves_limit =
                    plan.order_by.is_empty() || !plan.order_by_sort_required;
                let can_early_stop = order_preserves_limit
                    && plan.group_by.is_empty()
                    && plan.aggregates.is_empty()
                    && plan.window_exprs.is_empty()
                    && plan.joins.is_empty()
                    && plan.limit.is_some()
                    && !matches!(
                        plan.base_table.access_path,
                        crate::query::optimizer::AccessPath::MultiIndexScan { .. }
                            | crate::query::optimizer::AccessPath::SecondaryIndexScan {
                                scan_type: crate::query::optimizer::SecondaryScanType::SkipScan { .. },
                                ..
                            }
                            | crate::query::optimizer::AccessPath::IndexOnlyScan {
                                scan_type: crate::query::optimizer::SecondaryScanType::SkipScan { .. },
                                ..
                            }
                    );
                let early_stop_bound = if can_early_stop {
                    Some(plan.offset.unwrap_or(0) + plan.limit.unwrap())
                } else {
                    None
                };
                let scan_dir = plan.base_table.scan_direction;
                let mut early_terminated = false;
                let mut early_index_entries = 0;

                let mut late_cand_rows = 0;
                let mut late_fetched_rows = 0;

                let initial_rows = match &plan.base_table.access_path {
                    crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                        index_lookups = 1;
                        match table.get(key)? {
                            Some(row) => vec![row],
                            None => Vec::new(),
                        }
                    }
                    crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                        index_lookups = 1;
                        if is_index_agg {
                            Vec::new()
                        } else if let Some(k) = early_stop_bound {
                            let has_subquery = plan
                                .base_table
                                .residual_filter
                                .as_ref()
                                .map(|f| f.contains_subquery())
                                .unwrap_or(false);
                            if outer_contexts.is_empty() && !has_subquery {
                                let (rows, examined) = table.scan_with_filter_and_limit(
                                    start.as_ref(),
                                    end.as_ref(),
                                    Some(k),
                                    scan_dir,
                                    |row| match &plan.base_table.residual_filter {
                                        Some(f) => match f.evaluate(row, &raw_schema)? {
                                            Value::Boolean(b) => Ok(b),
                                            _ => Ok(false),
                                        },
                                        None => Ok(true),
                                    },
                                )?;
                                early_index_entries = examined;
                                if rows.len() >= k {
                                    early_terminated = true;
                                }
                                rows
                            } else {
                                let mut rows = Vec::new();
                                let all_rows = table.scan(start.as_ref(), end.as_ref())?;
                                for row in all_rows {
                                    early_index_entries += 1;
                                    let keep = match &plan.base_table.residual_filter {
                                        Some(f) => {
                                            let res = self.evaluate_expr_with_outer_internal(
                                                f,
                                                &row,
                                                &raw_schema,
                                                outer_contexts,
                                                cte_store,
                                            )?;
                                            match res {
                                                Value::Boolean(b) => b,
                                                _ => false,
                                            }
                                        }
                                        None => true,
                                    };
                                    if keep {
                                        rows.push(row);
                                        if rows.len() >= k {
                                            early_terminated = true;
                                            break;
                                        }
                                    }
                                }
                                rows
                            }
                        } else if scan_dir == crate::query::operator::SortDirection::Descending
                            && outer_contexts.is_empty()
                        {
                            let (rows, examined) = table.scan_with_filter_and_limit(
                                start.as_ref(),
                                end.as_ref(),
                                None,
                                scan_dir,
                                |_| Ok(true),
                            )?;
                            early_index_entries = examined;
                            rows
                        } else {
                            table.scan(start.as_ref(), end.as_ref())?
                        }
                    }
                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                        index_name,
                        scan_type,
                        ..
                    } => {
                        if plan.base_table.late_materialization {
                            index_lookups = 1;
                            let (meta, entries) =
                                table.secondary_index_leaf_entries(index_name, scan_type)?;
                            drop(table);

                            let mut surviving_rids = Vec::new();
                            for entry in entries {
                                late_cand_rows += 1;
                                early_index_entries += 1;
                                // 1. Decode index partial row
                                let partial_row = crate::engine::table::Table::decode_secondary_leaf_entry_partial(
                                    &raw_schema,
                                    &meta,
                                    &entry,
                                )?;

                                // 2. Evaluate index-covered filter if any
                                let index_passed = match &plan.base_table.index_covered_filter {
                                    Some(cov_f) => {
                                        let res = self.evaluate_expr_with_outer_internal(
                                            cov_f,
                                            &partial_row,
                                            &raw_schema,
                                            outer_contexts,
                                            cte_store,
                                        )?;
                                        match res {
                                            Value::Boolean(b) => b,
                                            _ => false,
                                        }
                                    }
                                    None => true,
                                };

                                if index_passed {
                                    surviving_rids.push(entry.rid);
                                }
                            }

                            // 3. Surviving candidates: fetch base records
                            let base_records: Vec<crate::engine::types::Row> = {
                                let mut tbl = self.open_table(&plan.base_table.table_name)?;
                                let mut fetched = Vec::new();
                                for rid in surviving_rids {
                                    if let Some(r) = tbl.get_by_rid(rid)? {
                                        late_fetched_rows += 1;
                                        fetched.push(r);
                                    }
                                }
                                fetched
                            };

                            // 4. Evaluate base-record filter if any and apply early termination
                            let mut rows = Vec::new();
                            for base_row in base_records {
                                let base_passed = match &plan.base_table.base_record_filter {
                                    Some(b_f) => {
                                        let res = self.evaluate_expr_with_outer_internal(
                                            b_f,
                                            &base_row,
                                            &raw_schema,
                                            outer_contexts,
                                            cte_store,
                                        )?;
                                        match res {
                                            Value::Boolean(b) => b,
                                            _ => false,
                                        }
                                    }
                                    None => true,
                                };

                                if base_passed {
                                    rows.push(base_row);
                                    if let Some(k) = early_stop_bound {
                                        if rows.len() >= k {
                                            early_terminated = true;
                                            break;
                                        }
                                    }
                                }
                            }
                            rows
                        } else {
                            match scan_type {
                                crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                                    index_lookups = 1;
                                    table.index_lookup(index_name, val)?
                                }
                                crate::query::optimizer::SecondaryScanType::CompositePointLookup(
                                    vals,
                                )
                                | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                                    index_lookups = 1;
                                    table.composite_index_lookup(index_name, vals)?
                                }
                                crate::query::optimizer::SecondaryScanType::RangeScan {
                                    start,
                                    end,
                                } => {
                                    index_lookups = 1;
                                    if is_index_agg {
                                        Vec::new()
                                    } else if let Some(k) = early_stop_bound {
                                        let has_subquery = plan
                                            .base_table
                                            .residual_filter
                                            .as_ref()
                                            .map(|f| f.contains_subquery())
                                            .unwrap_or(false);
                                        if outer_contexts.is_empty() && !has_subquery {
                                            let (rows, examined) = table
                                                .index_scan_with_filter_and_limit(
                                                    index_name,
                                                    start.as_ref(),
                                                    end.as_ref(),
                                                    Some(k),
                                                    scan_dir,
                                                    |row| match &plan.base_table.residual_filter {
                                                        Some(f) => {
                                                            match f.evaluate(row, &raw_schema)? {
                                                                Value::Boolean(b) => Ok(b),
                                                                _ => Ok(false),
                                                            }
                                                        }
                                                        None => Ok(true),
                                                    },
                                                )?;
                                            early_index_entries = examined;
                                            if rows.len() >= k {
                                                early_terminated = true;
                                            }
                                            rows
                                        } else {
                                            let all_rows = table.index_scan(
                                                index_name,
                                                start.as_ref(),
                                                end.as_ref(),
                                            )?;
                                            let mut rows = Vec::new();
                                            for row in all_rows {
                                                early_index_entries += 1;
                                                let keep = match &plan.base_table.residual_filter {
                                                    Some(f) => {
                                                        let res =
                                                            self.evaluate_expr_with_outer_internal(
                                                                f,
                                                                &row,
                                                                &raw_schema,
                                                                outer_contexts,
                                                                cte_store,
                                                            )?;
                                                        match res {
                                                            Value::Boolean(b) => b,
                                                            _ => false,
                                                        }
                                                    }
                                                    None => true,
                                                };
                                                if keep {
                                                    rows.push(row);
                                                    if rows.len() >= k {
                                                        early_terminated = true;
                                                        break;
                                                    }
                                                }
                                            }
                                            rows
                                        }
                                    } else if scan_dir
                                        == crate::query::operator::SortDirection::Descending
                                        && outer_contexts.is_empty()
                                    {
                                        let (rows, examined) = table
                                            .index_scan_with_filter_and_limit(
                                                index_name,
                                                start.as_ref(),
                                                end.as_ref(),
                                                None,
                                                scan_dir,
                                                |_| Ok(true),
                                            )?;
                                        early_index_entries = examined;
                                        rows
                                    } else {
                                        table.index_scan(index_name, start.as_ref(), end.as_ref())?
                                    }
                                }
                                crate::query::optimizer::SecondaryScanType::SkipScan {
                                    skipped_prefix_columns,
                                    constrained_columns,
                                } => {
                                    let constrained_values: Vec<Value> = constrained_columns
                                        .iter()
                                        .map(|(_, v)| v.clone())
                                        .collect();
                                    let (rows, stats) = table.index_skip_scan(
                                        index_name,
                                        skipped_prefix_columns.len(),
                                        &constrained_values,
                                    )?;
                                    index_lookups = stats.skip_scan_probes;
                                    skip_scan_stats = Some(stats);
                                    rows
                                }
                            }
                        }
                    }
                    crate::query::optimizer::AccessPath::IndexOnlyScan {
                        index_name,
                        scan_type,
                        ..
                    } => {
                        is_index_only = true;
                        match scan_type {
                            crate::query::optimizer::SecondaryScanType::SkipScan {
                                skipped_prefix_columns,
                                constrained_columns,
                            } => {
                                let constrained_values: Vec<Value> =
                                    constrained_columns.iter().map(|(_, v)| v.clone()).collect();
                                let (rows, stats) = table.index_only_skip_scan(
                                    index_name,
                                    skipped_prefix_columns.len(),
                                    &constrained_values,
                                )?;
                                index_lookups = stats.skip_scan_probes;
                                skip_scan_stats = Some(stats);
                                rows
                            }
                            _ => {
                                index_lookups = 1;
                                if is_index_agg {
                                    Vec::new()
                                } else if let Some(k) = early_stop_bound {
                                    let has_subquery = plan
                                        .base_table
                                        .residual_filter
                                        .as_ref()
                                        .map(|f| f.contains_subquery())
                                        .unwrap_or(false);
                                    if outer_contexts.is_empty() && !has_subquery {
                                        let (rows, examined) = table.index_only_scan_with_options(
                                            index_name,
                                            scan_type,
                                            Some(k),
                                            scan_dir,
                                            |row| match &plan.base_table.residual_filter {
                                                Some(f) => match f.evaluate(row, &raw_schema)? {
                                                    Value::Boolean(b) => Ok(b),
                                                    _ => Ok(false),
                                                },
                                                None => Ok(true),
                                            },
                                        )?;
                                        early_index_entries = examined;
                                        if rows.len() >= k {
                                            early_terminated = true;
                                        }
                                        rows
                                    } else {
                                        let all_rows =
                                            table.index_only_scan(index_name, scan_type)?;
                                        let mut rows = Vec::new();
                                        for row in all_rows {
                                            early_index_entries += 1;
                                            let keep = match &plan.base_table.residual_filter {
                                                Some(f) => {
                                                    let res = self
                                                        .evaluate_expr_with_outer_internal(
                                                            f,
                                                            &row,
                                                            &raw_schema,
                                                            outer_contexts,
                                                            cte_store,
                                                        )?;
                                                    match res {
                                                        Value::Boolean(b) => b,
                                                        _ => false,
                                                    }
                                                }
                                                None => true,
                                            };
                                            if keep {
                                                rows.push(row);
                                                if rows.len() >= k {
                                                    early_terminated = true;
                                                    break;
                                                }
                                            }
                                        }
                                        rows
                                    }
                                } else if scan_dir
                                    == crate::query::operator::SortDirection::Descending
                                    && outer_contexts.is_empty()
                                {
                                    let (rows, examined) = table.index_only_scan_with_options(
                                        index_name,
                                        scan_type,
                                        None,
                                        scan_dir,
                                        |_| Ok(true),
                                    )?;
                                    early_index_entries = examined;
                                    rows
                                } else {
                                    table.index_only_scan(index_name, scan_type)?
                                }
                            }
                        }
                    }
                    crate::query::optimizer::AccessPath::MultiIndexScan {
                        strategy,
                        child_scans,
                        ..
                    } => {
                        let (rows, stats) = table.multi_index_scan_with_config(
                            *strategy,
                            child_scans,
                            &multi_index_config,
                        )?;

                        index_lookups = stats.indexes_used;
                        multi_index_stats = Some(stats);
                        rows
                    }
                    crate::query::optimizer::AccessPath::FullTableScan => {
                        if is_index_agg {
                            Vec::new()
                        } else if let Some(k) = early_stop_bound {
                            let has_subquery = plan
                                .base_table
                                .residual_filter
                                .as_ref()
                                .map(|f| f.contains_subquery())
                                .unwrap_or(false);
                            if outer_contexts.is_empty() && !has_subquery {
                                let (rows, examined) = table.scan_with_filter_and_limit(
                                    None,
                                    None,
                                    Some(k),
                                    scan_dir,
                                    |row| match &plan.base_table.residual_filter {
                                        Some(f) => match f.evaluate(row, &raw_schema)? {
                                            Value::Boolean(b) => Ok(b),
                                            _ => Ok(false),
                                        },
                                        None => Ok(true),
                                    },
                                )?;
                                early_index_entries = examined;
                                if rows.len() >= k {
                                    early_terminated = true;
                                }
                                rows
                            } else {
                                let all_rows = table.scan(None, None)?;
                                let mut rows = Vec::new();
                                for row in all_rows {
                                    early_index_entries += 1;
                                    let keep = match &plan.base_table.residual_filter {
                                        Some(f) => {
                                            let res = self.evaluate_expr_with_outer_internal(
                                                f,
                                                &row,
                                                &raw_schema,
                                                outer_contexts,
                                                cte_store,
                                            )?;
                                            match res {
                                                Value::Boolean(b) => b,
                                                _ => false,
                                            }
                                        }
                                        None => true,
                                    };
                                    if keep {
                                        rows.push(row);
                                        if rows.len() >= k {
                                            early_terminated = true;
                                            break;
                                        }
                                    }
                                }
                                rows
                            }
                        } else if scan_dir == crate::query::operator::SortDirection::Descending
                            && outer_contexts.is_empty()
                        {
                            let (rows, examined) = table.scan_with_filter_and_limit(
                                None,
                                None,
                                None,
                                scan_dir,
                                |_| Ok(true),
                            )?;
                            early_index_entries = examined;
                            rows
                        } else {
                            table.scan(None, None)?
                        }
                    }
                    crate::query::optimizer::AccessPath::EmptyResult => Vec::new(),
                };

                let mut scan_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::TableScan {
                        table_name: plan.base_table.table_name.clone(),
                        access_path: plan.base_table.access_path.to_string(),
                    },
                    Some(plan.base_table.estimated_rows),
                    Some(plan.base_table.cost),
                );
                scan_metric.input_rows = initial_rows.len();
                scan_metric.actual_rows = initial_rows.len();
                scan_metric.counters.index_lookups = index_lookups;
                scan_metric.counters.rows_evaluated = initial_rows.len();
                scan_metric.counters.early_terminated = early_terminated;
                if plan.base_table.row_goal.is_some() {
                    scan_metric.counters.row_goal =
                        plan.base_table.row_goal.as_ref().map(|g| g.k() as usize);
                    scan_metric.counters.rows_examined =
                        early_index_entries.max(initial_rows.len());
                    scan_metric.counters.rows_qualified = initial_rows.len();
                    scan_metric.counters.row_goal_satisfied = early_terminated
                        || scan_metric
                            .counters
                            .row_goal
                            .map(|g| initial_rows.len() >= g)
                            .unwrap_or(false);
                }
                if let Some(stats) = &skip_scan_stats {
                    scan_metric.counters.distinct_prefix_values = stats.distinct_prefix_values;
                    scan_metric.counters.skip_scan_probes = stats.skip_scan_probes;
                    scan_metric.counters.index_entries_examined = stats.index_entries_examined;
                }
                if let Some(stats) = &multi_index_stats {
                    scan_metric.counters.indexes_used = stats.indexes_used;
                    scan_metric.counters.candidate_rows_before_set_operation =
                        stats.candidate_rows_before_set_operation;
                    scan_metric.counters.candidate_rows_after_set_operation =
                        stats.candidate_rows_after_set_operation;
                    scan_metric.counters.record_ids_intersected = stats.record_ids_intersected;
                    scan_metric.counters.record_ids_unioned = stats.record_ids_unioned;
                    scan_metric.counters.duplicate_record_ids_removed =
                        stats.duplicate_record_ids_removed;
                    scan_metric.counters.base_records_fetched = stats.base_records_fetched;
                    scan_metric.counters.index_entries_examined =
                        stats.candidate_rows_before_set_operation;
                    scan_metric.counters.tuples_fetched = stats.base_records_fetched;
                    scan_metric.counters.runs_created = stats.spill_runs_created;
                    scan_metric.counters.bytes_spilled = stats.spill_bytes_written;
                    scan_metric.counters.peak_memory_bytes = stats.peak_candidate_memory;
                }
                scan_metric.counters.columns_required = plan.base_table.required_columns.len();
                scan_metric.counters.columns_pruned = plan.base_table.pruned_columns.len();
                scan_metric.counters.late_materialization_enabled =
                    plan.base_table.late_materialization;
                scan_metric.counters.index_only = is_index_only;
                if plan.base_table.late_materialization {
                    scan_metric.counters.candidate_rows = late_cand_rows;
                    scan_metric.counters.rows_fetched = late_fetched_rows;
                    scan_metric.counters.base_records_fetched = late_fetched_rows;
                    scan_metric.counters.base_records_avoided =
                        late_cand_rows.saturating_sub(late_fetched_rows);
                    scan_metric.counters.tuples_fetched = late_fetched_rows;
                } else if is_index_only {
                    scan_metric.counters.tuples_fetched = 0;
                    scan_metric.counters.base_records_fetched = 0;
                    scan_metric.counters.base_records_avoided = initial_rows.len();
                    if skip_scan_stats.is_none() && multi_index_stats.is_none() {
                        scan_metric.counters.index_entries_examined = if early_index_entries > 0 {
                            early_index_entries
                        } else {
                            initial_rows.len()
                        };
                    }
                    scan_metric.counters.covered_rows_output = initial_rows.len();
                } else if multi_index_stats.is_none() {
                    scan_metric.counters.tuples_fetched = initial_rows.len();
                    scan_metric.counters.base_records_fetched = initial_rows.len();
                    scan_metric.counters.base_records_avoided = 0;
                    if skip_scan_stats.is_none() {
                        scan_metric.counters.index_entries_examined = if early_index_entries > 0 {
                            early_index_entries
                        } else if index_lookups > 0 {
                            initial_rows.len()
                        } else {
                            0
                        };
                    }
                    scan_metric.counters.covered_rows_output = 0;
                }

                scan_metric.counters.visibility_checks_evaluated =
                    self.db.visibility_map.stats.visibility_checks_evaluated;
                scan_metric.counters.visibility_checks_skipped =
                    self.db.visibility_map.stats.visibility_checks_skipped;
                scan_metric.counters.visibility_map_hits =
                    self.db.visibility_map.stats.page_map_hits
                        + self.db.visibility_map.stats.record_map_hits;

                let (filtered_rows, table_op) = match &plan.base_table.residual_filter {
                    Some(filter)
                        if early_stop_bound.is_none() && !plan.base_table.late_materialization =>
                    {
                        let input_rows_len = initial_rows.len();
                        let filtered = self.filter_rows_with_outer_internal(
                            initial_rows,
                            &raw_schema,
                            filter,
                            outer_contexts,
                            cte_store,
                        )?;
                        let mut filter_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Filter,
                            Some(plan.base_table.estimated_rows),
                            Some(plan.base_table.cost),
                        );
                        filter_metric.input_rows = input_rows_len;
                        filter_metric.actual_rows = filtered.len();
                        filter_metric.counters.rows_evaluated = input_rows_len;
                        filter_metric.counters.rows_passed = filtered.len();
                        filter_metric.counters.rows_rejected =
                            input_rows_len.saturating_sub(filtered.len());
                        filter_metric.add_child(scan_metric);
                        (filtered, filter_metric)
                    }
                    Some(_) => {
                        let mut filter_metric = crate::query::OperatorMetrics::new(
                            crate::query::OperatorKind::Filter,
                            Some(plan.base_table.estimated_rows),
                            Some(plan.base_table.cost),
                        );
                        let eval_count = early_index_entries.max(initial_rows.len());
                        filter_metric.input_rows = eval_count;
                        filter_metric.actual_rows = initial_rows.len();
                        filter_metric.counters.rows_evaluated = eval_count;
                        filter_metric.counters.rows_passed = initial_rows.len();
                        filter_metric.counters.rows_rejected =
                            eval_count.saturating_sub(initial_rows.len());
                        filter_metric.counters.early_terminated = early_terminated;
                        filter_metric.add_child(scan_metric);
                        (initial_rows, filter_metric)
                    }
                    None => (initial_rows, scan_metric),
                };

                let schema = if plan.joins.is_empty() {
                    raw_schema
                } else {
                    let q = plan.base_table.qualifier();
                    let cols: Vec<crate::engine::types::Column> = raw_schema
                        .columns
                        .iter()
                        .map(|c| {
                            crate::engine::types::Column::new(
                                format!("{q}.{}", c.name),
                                c.data_type,
                                c.nullable,
                            )
                        })
                        .collect();
                    crate::engine::types::Schema::new_projected(cols)
                };

                (schema, filtered_rows, table_op)
            }
        };

        // 2. Execute Joins sequentially
        for join in &plan.joins {
            if join.strategy == crate::query::optimizer::JoinStrategy::IndexProbe {
                if let Some((next_rows, probe_metric)) = self.execute_index_probe_semi_anti(
                    &current_rows,
                    &current_schema,
                    join,
                    &current_op,
                )? {
                    current_rows = next_rows;
                    current_op = probe_metric;
                    continue;
                }
            }
            let (right_schema, right_rows, right_op) = {
                if let Some((cte_schema, cte_rows)) = cte_store.get(&join.right.table_name) {
                    let raw_schema = cte_schema.clone();
                    let initial_rows = cte_rows.clone();
                    let mut scan_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::TableScan {
                            table_name: join.right.table_name.clone(),
                            access_path: "CTE Scan".to_string(),
                        },
                        Some(join.right.estimated_rows),
                        Some(join.right.cost),
                    );
                    scan_metric.input_rows = initial_rows.len();
                    scan_metric.actual_rows = initial_rows.len();
                    scan_metric.counters.tuples_fetched = initial_rows.len();
                    scan_metric.counters.rows_evaluated = initial_rows.len();

                    let (filtered_rows, table_op) = match &join.right.residual_filter {
                        Some(filter) => {
                            let input_rows_len = initial_rows.len();
                            let filtered = self.filter_rows_with_outer_internal(
                                initial_rows,
                                &raw_schema,
                                filter,
                                outer_contexts,
                                cte_store,
                            )?;
                            let mut filter_metric = crate::query::OperatorMetrics::new(
                                crate::query::OperatorKind::Filter,
                                Some(join.right.estimated_rows),
                                Some(join.right.cost),
                            );
                            filter_metric.input_rows = input_rows_len;
                            filter_metric.actual_rows = filtered.len();
                            filter_metric.counters.rows_evaluated = input_rows_len;
                            filter_metric.counters.rows_passed = filtered.len();
                            filter_metric.counters.rows_rejected =
                                input_rows_len.saturating_sub(filtered.len());
                            filter_metric.add_child(scan_metric);
                            (filtered, filter_metric)
                        }
                        None => (initial_rows, scan_metric),
                    };

                    let q = join.right.qualifier();
                    let cols: Vec<crate::engine::types::Column> = raw_schema
                        .columns
                        .iter()
                        .map(|c| {
                            crate::engine::types::Column::new(
                                format!("{q}.{}", c.name),
                                c.data_type,
                                c.nullable,
                            )
                        })
                        .collect();
                    (
                        crate::engine::types::Schema::new_projected(cols),
                        filtered_rows,
                        table_op,
                    )
                } else if let Some(cte) = plan.ctes.iter().find(|c| c.name == join.right.table_name)
                {
                    let (res, inner_op) = match &cte.kind {
                        crate::sql::BoundCteKind::NonRecursive {
                            plan: inner_plan, ..
                        } => self.execute_physical_plan_internal_with_context(
                            inner_plan,
                            outer_contexts,
                            cte_store,
                            ctx,
                        )?,
                        crate::sql::BoundCteKind::Recursive { .. } => {
                            return Err(Error::TableNotFound(join.right.table_name.clone()));
                        }
                    };
                    let raw_schema = cte.output_schema.clone();
                    let initial_rows = res.rows;
                    let mut scan_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::TableScan {
                            table_name: join.right.table_name.clone(),
                            access_path: "Inlined CTE Scan".to_string(),
                        },
                        Some(join.right.estimated_rows),
                        Some(join.right.cost),
                    );
                    scan_metric.input_rows = initial_rows.len();
                    scan_metric.actual_rows = initial_rows.len();
                    scan_metric.counters.tuples_fetched = initial_rows.len();
                    scan_metric.counters.rows_evaluated = initial_rows.len();
                    scan_metric.add_child(inner_op);

                    let (filtered_rows, table_op) = match &join.right.residual_filter {
                        Some(filter) => {
                            let input_rows_len = initial_rows.len();
                            let filtered = self.filter_rows_with_outer_internal(
                                initial_rows,
                                &raw_schema,
                                filter,
                                outer_contexts,
                                cte_store,
                            )?;
                            let mut filter_metric = crate::query::OperatorMetrics::new(
                                crate::query::OperatorKind::Filter,
                                Some(join.right.estimated_rows),
                                Some(join.right.cost),
                            );
                            filter_metric.input_rows = input_rows_len;
                            filter_metric.actual_rows = filtered.len();
                            filter_metric.counters.rows_evaluated = input_rows_len;
                            filter_metric.counters.rows_passed = filtered.len();
                            filter_metric.counters.rows_rejected =
                                input_rows_len.saturating_sub(filtered.len());
                            filter_metric.add_child(scan_metric);
                            (filtered, filter_metric)
                        }
                        None => (initial_rows, scan_metric),
                    };

                    let q = join.right.qualifier();
                    let cols: Vec<crate::engine::types::Column> = raw_schema
                        .columns
                        .iter()
                        .map(|c| {
                            crate::engine::types::Column::new(
                                format!("{q}.{}", c.name),
                                c.data_type,
                                c.nullable,
                            )
                        })
                        .collect();
                    (
                        crate::engine::types::Schema::new_projected(cols),
                        filtered_rows,
                        table_op,
                    )
                } else {
                    let multi_index_config = self.db.multi_index_config.clone();
                    let mut table = self.open_table(&join.right.table_name)?;
                    let raw_schema = table.schema().clone();
                    let mut index_lookups = 0;
                    let mut is_index_only = false;
                    let mut skip_scan_stats: Option<crate::engine::table::SkipScanStats> = None;
                    let mut multi_index_stats: Option<crate::engine::table::MultiIndexScanStats> =
                        None;
                    let initial_rows = match &join.right.access_path {
                        crate::query::optimizer::AccessPath::PrimaryPointLookup { key } => {
                            index_lookups = 1;
                            match table.get(key)? {
                                Some(row) => vec![row],
                                None => Vec::new(),
                            }
                        }
                        crate::query::optimizer::AccessPath::PrimaryRangeScan { start, end } => {
                            index_lookups = 1;
                            table.scan(start.as_ref(), end.as_ref())?
                        }
                        crate::query::optimizer::AccessPath::SecondaryIndexScan {
                            index_name,
                            scan_type,
                            ..
                        } => match scan_type {
                            crate::query::optimizer::SecondaryScanType::PointLookup(val) => {
                                index_lookups = 1;
                                table.index_lookup(index_name, val)?
                            }
                            crate::query::optimizer::SecondaryScanType::CompositePointLookup(
                                vals,
                            )
                            | crate::query::optimizer::SecondaryScanType::PrefixScan(vals) => {
                                index_lookups = 1;
                                table.composite_index_lookup(index_name, vals)?
                            }
                            crate::query::optimizer::SecondaryScanType::RangeScan {
                                start,
                                end,
                            } => {
                                index_lookups = 1;
                                table.index_scan(index_name, start.as_ref(), end.as_ref())?
                            }
                            crate::query::optimizer::SecondaryScanType::SkipScan {
                                skipped_prefix_columns,
                                constrained_columns,
                            } => {
                                let constrained_values: Vec<Value> =
                                    constrained_columns.iter().map(|(_, v)| v.clone()).collect();
                                let (rows, stats) = table.index_skip_scan(
                                    index_name,
                                    skipped_prefix_columns.len(),
                                    &constrained_values,
                                )?;
                                index_lookups = stats.skip_scan_probes;
                                skip_scan_stats = Some(stats);
                                rows
                            }
                        },
                        crate::query::optimizer::AccessPath::IndexOnlyScan {
                            index_name,
                            scan_type,
                            ..
                        } => {
                            is_index_only = true;
                            match scan_type {
                                crate::query::optimizer::SecondaryScanType::SkipScan {
                                    skipped_prefix_columns,
                                    constrained_columns,
                                } => {
                                    let constrained_values: Vec<Value> = constrained_columns
                                        .iter()
                                        .map(|(_, v)| v.clone())
                                        .collect();
                                    let (rows, stats) = table.index_only_skip_scan(
                                        index_name,
                                        skipped_prefix_columns.len(),
                                        &constrained_values,
                                    )?;
                                    index_lookups = stats.skip_scan_probes;
                                    skip_scan_stats = Some(stats);
                                    rows
                                }
                                _ => {
                                    index_lookups = 1;
                                    table.index_only_scan(index_name, scan_type)?
                                }
                            }
                        }
                        crate::query::optimizer::AccessPath::MultiIndexScan {
                            strategy,
                            child_scans,
                            ..
                        } => {
                            let (rows, stats) = table.multi_index_scan_with_config(
                                *strategy,
                                child_scans,
                                &multi_index_config,
                            )?;
                            index_lookups = stats.indexes_used;
                            multi_index_stats = Some(stats);
                            rows
                        }

                        crate::query::optimizer::AccessPath::FullTableScan => {
                            table.scan(None, None)?
                        }
                        crate::query::optimizer::AccessPath::EmptyResult => Vec::new(),
                    };

                    let mut scan_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::TableScan {
                            table_name: join.right.table_name.clone(),
                            access_path: join.right.access_path.to_string(),
                        },
                        Some(join.right.estimated_rows),
                        Some(join.right.cost),
                    );
                    scan_metric.input_rows = initial_rows.len();
                    scan_metric.actual_rows = initial_rows.len();
                    scan_metric.counters.index_lookups = index_lookups;
                    scan_metric.counters.rows_evaluated = initial_rows.len();
                    if let Some(stats) = &skip_scan_stats {
                        scan_metric.counters.distinct_prefix_values = stats.distinct_prefix_values;
                        scan_metric.counters.skip_scan_probes = stats.skip_scan_probes;
                        scan_metric.counters.index_entries_examined = stats.index_entries_examined;
                    }
                    if let Some(stats) = &multi_index_stats {
                        scan_metric.counters.indexes_used = stats.indexes_used;
                        scan_metric.counters.candidate_rows_before_set_operation =
                            stats.candidate_rows_before_set_operation;
                        scan_metric.counters.candidate_rows_after_set_operation =
                            stats.candidate_rows_after_set_operation;
                        scan_metric.counters.record_ids_intersected = stats.record_ids_intersected;
                        scan_metric.counters.record_ids_unioned = stats.record_ids_unioned;
                        scan_metric.counters.duplicate_record_ids_removed =
                            stats.duplicate_record_ids_removed;
                        scan_metric.counters.base_records_fetched = stats.base_records_fetched;
                        scan_metric.counters.index_entries_examined =
                            stats.candidate_rows_before_set_operation;
                        scan_metric.counters.tuples_fetched = stats.base_records_fetched;
                        scan_metric.counters.runs_created = stats.spill_runs_created;
                        scan_metric.counters.bytes_spilled = stats.spill_bytes_written;
                        scan_metric.counters.peak_memory_bytes = stats.peak_candidate_memory;
                    }
                    if is_index_only {
                        scan_metric.counters.tuples_fetched = 0;
                        scan_metric.counters.base_records_fetched = 0;
                        scan_metric.counters.base_records_avoided = initial_rows.len();
                        if skip_scan_stats.is_none() && multi_index_stats.is_none() {
                            scan_metric.counters.index_entries_examined = initial_rows.len();
                        }
                        scan_metric.counters.covered_rows_output = initial_rows.len();
                    } else if multi_index_stats.is_none() {
                        scan_metric.counters.tuples_fetched = initial_rows.len();
                        scan_metric.counters.base_records_fetched = initial_rows.len();
                        scan_metric.counters.base_records_avoided = 0;
                        if skip_scan_stats.is_none() {
                            scan_metric.counters.index_entries_examined = if index_lookups > 0 {
                                initial_rows.len()
                            } else {
                                0
                            };
                        }
                        scan_metric.counters.covered_rows_output = 0;
                    }

                    let (filtered_rows, table_op) = match &join.right.residual_filter {
                        Some(filter) => {
                            let filtered = self.filter_rows_with_outer_internal(
                                initial_rows.clone(),
                                &raw_schema,
                                filter,
                                outer_contexts,
                                cte_store,
                            )?;
                            let mut filter_metric = crate::query::OperatorMetrics::new(
                                crate::query::OperatorKind::Filter,
                                Some(join.right.estimated_rows),
                                Some(join.right.cost),
                            );
                            filter_metric.input_rows = initial_rows.len();
                            filter_metric.actual_rows = filtered.len();
                            filter_metric.counters.rows_evaluated = initial_rows.len();
                            filter_metric.counters.rows_passed = filtered.len();
                            filter_metric.counters.rows_rejected =
                                initial_rows.len().saturating_sub(filtered.len());
                            filter_metric.add_child(scan_metric);
                            (filtered, filter_metric)
                        }
                        None => (initial_rows, scan_metric),
                    };

                    let q = join.right.qualifier();
                    let cols: Vec<crate::engine::types::Column> = raw_schema
                        .columns
                        .iter()
                        .map(|c| {
                            crate::engine::types::Column::new(
                                format!("{q}.{}", c.name),
                                c.data_type,
                                c.nullable,
                            )
                        })
                        .collect();
                    (
                        crate::engine::types::Schema::new_projected(cols),
                        filtered_rows,
                        table_op,
                    )
                }
            };

            let left_count = current_rows.len();
            let right_count = right_rows.len();
            let candidate_pairs = left_count * right_count;

            let join_row_goal = join.row_goal.as_ref().map(|rg| rg.k() as usize);

            let (next_schema, next_rows, join_exec_metrics) = if outer_contexts.is_empty() {
                if join.strategy == crate::query::optimizer::JoinStrategy::Merge {
                    if let Some(equi_plan) = crate::query::external_hash_join::analyze_equi_join(
                        &join.on,
                        &current_schema,
                        &right_schema,
                    ) {
                        let merge_op = crate::query::merge_join::MergeJoinOperator::new(
                            &current_schema,
                            &right_schema,
                            join.join_type,
                            equi_plan,
                            !join.left_sort,
                            !join.right_sort,
                            self.db.join_config.clone(),
                        )
                        .with_row_goal(join_row_goal);
                        merge_op.execute(current_rows, right_rows)?
                    } else {
                        crate::query::operator::JoinOperator::apply_with_config_and_row_goal(
                            current_rows,
                            &current_schema,
                            &right_rows,
                            &right_schema,
                            join.join_type,
                            &join.on,
                            &self.db.join_config,
                            join_row_goal,
                        )?
                    }
                } else {
                    crate::query::operator::JoinOperator::apply_with_config_and_row_goal(
                        current_rows,
                        &current_schema,
                        &right_rows,
                        &right_schema,
                        join.join_type,
                        &join.on,
                        &self.db.join_config,
                        join_row_goal,
                    )?
                }
            } else {
                let (ns, nr) = self.join_rows_with_outer_internal(
                    current_rows,
                    &current_schema,
                    &right_rows,
                    &right_schema,
                    join.join_type,
                    &join.on,
                    outer_contexts,
                    cte_store,
                )?;
                let metrics = crate::query::external_hash_join::JoinExecutionMetrics {
                    rows_left: left_count,
                    rows_right: right_count,
                    rows_output: nr.len(),
                    candidate_pairs,
                    matched_rows: nr.len(),
                    ..Default::default()
                };
                (ns, nr, metrics)
            };

            let op_kind = if join.strategy == crate::query::optimizer::JoinStrategy::Merge {
                crate::query::OperatorKind::MergeJoin {
                    join_type: format!("{:?}", join.join_type),
                }
            } else if join_exec_metrics.runs_created > 0 || join_exec_metrics.bytes_spilled > 0 {
                crate::query::OperatorKind::ExternalHashJoin {
                    join_type: format!("{:?}", join.join_type),
                }
            } else if crate::query::external_hash_join::analyze_equi_join(
                &join.on,
                &current_schema,
                &right_schema,
            )
            .is_some()
            {
                match join.join_type {
                    crate::sql::JoinType::Semi => crate::query::OperatorKind::HashSemiJoin,
                    crate::sql::JoinType::Anti => crate::query::OperatorKind::HashAntiJoin,
                    _ => crate::query::OperatorKind::HashJoin {
                        join_type: format!("{:?}", join.join_type),
                    },
                }
            } else {
                match join.join_type {
                    crate::sql::JoinType::Semi => crate::query::OperatorKind::NestedLoopSemiJoin,
                    crate::sql::JoinType::Anti => crate::query::OperatorKind::NestedLoopAntiJoin,
                    _ => crate::query::OperatorKind::NestedLoopJoin {
                        join_type: format!("{:?}", join.join_type),
                    },
                }
            };

            let mut join_metric = crate::query::OperatorMetrics::new(
                op_kind,
                Some(join.estimated_rows),
                Some(join.cost),
            );
            join_metric.input_rows = left_count + right_count;
            join_metric.actual_rows = next_rows.len();
            join_metric.counters.rows_left = left_count;
            join_metric.counters.rows_right = right_count;
            join_metric.counters.candidate_pairs = join_exec_metrics.candidate_pairs;
            join_metric.counters.matched_rows = join_exec_metrics.matched_rows;
            join_metric.counters.matched_outer_rows = join_exec_metrics.matched_outer_rows;
            join_metric.counters.unmatched_outer_rows = join_exec_metrics.unmatched_outer_rows;
            join_metric.counters.hash_build_rows = join_exec_metrics.hash_build_rows;
            join_metric.counters.hash_probe_rows = join_exec_metrics.hash_probe_rows;
            join_metric.counters.rows_output = next_rows.len();
            join_metric.counters.row_goal = join_row_goal;
            join_metric.counters.early_terminated = join_exec_metrics.early_terminated;
            join_metric.counters.row_goal_satisfied = join_exec_metrics.row_goal_satisfied;
            join_metric.counters.rows_examined = if join_exec_metrics.hash_probe_rows > 0 {
                join_exec_metrics.hash_build_rows + join_exec_metrics.hash_probe_rows
            } else if join_exec_metrics.merge_join_left_rows > 0 {
                join_exec_metrics.merge_join_left_rows + join_exec_metrics.merge_join_right_rows
            } else {
                join_exec_metrics.candidate_pairs
            };
            join_metric.counters.probe_rows_after_row_goal =
                join_exec_metrics.probe_rows_after_row_goal;
            join_metric.counters.duplicate_runs_processed =
                join_exec_metrics.duplicate_runs_processed;
            join_metric.counters.spill_partitions = join_exec_metrics.spill_partitions;
            join_metric.counters.runs_created = join_exec_metrics.runs_created;
            join_metric.counters.bytes_spilled = join_exec_metrics.bytes_spilled;
            join_metric.counters.partition_passes = join_exec_metrics.partition_passes;
            join_metric.counters.peak_memory_bytes = join_exec_metrics.peak_memory_bytes;
            join_metric.counters.runtime_filter_enabled = join_exec_metrics.runtime_filter_enabled;
            join_metric.counters.runtime_filter_type = join_exec_metrics.runtime_filter_type;
            join_metric.counters.runtime_filter_build_rows =
                join_exec_metrics.runtime_filter_build_rows;
            join_metric.counters.runtime_filter_distinct_keys =
                join_exec_metrics.runtime_filter_distinct_keys;
            join_metric.counters.runtime_filter_memory_bytes =
                join_exec_metrics.runtime_filter_memory_bytes;
            join_metric.counters.runtime_filter_probe_rows =
                join_exec_metrics.runtime_filter_probe_rows;
            join_metric.counters.runtime_filter_rejected_rows =
                join_exec_metrics.runtime_filter_rejected_rows;
            join_metric.counters.runtime_filter_passed_rows =
                join_exec_metrics.runtime_filter_passed_rows;
            join_metric.counters.runtime_filter_null_probe_rows =
                join_exec_metrics.runtime_filter_null_probe_rows;
            join_metric.counters.runtime_filter_false_positive_rows =
                join_exec_metrics.runtime_filter_false_positive_rows;
            join_metric.counters.merge_join_left_rows = join_exec_metrics.merge_join_left_rows;
            join_metric.counters.merge_join_right_rows = join_exec_metrics.merge_join_right_rows;
            join_metric.counters.merge_join_comparisons = join_exec_metrics.merge_join_comparisons;
            join_metric.counters.merge_join_key_matches = join_exec_metrics.merge_join_key_matches;
            join_metric.counters.merge_join_duplicate_left_rows =
                join_exec_metrics.merge_join_duplicate_left_rows;
            join_metric.counters.merge_join_duplicate_right_rows =
                join_exec_metrics.merge_join_duplicate_right_rows;
            join_metric.counters.merge_join_output_rows = join_exec_metrics.merge_join_output_rows;
            join_metric.counters.merge_join_unmatched_left_rows =
                join_exec_metrics.merge_join_unmatched_left_rows;
            join_metric.counters.merge_join_unmatched_right_rows =
                join_exec_metrics.merge_join_unmatched_right_rows;
            join_metric.counters.merge_join_null_left_keys =
                join_exec_metrics.merge_join_null_left_keys;
            join_metric.counters.merge_join_null_right_keys =
                join_exec_metrics.merge_join_null_right_keys;
            join_metric.counters.merge_join_residual_evaluations =
                join_exec_metrics.merge_join_residual_evaluations;
            join_metric.counters.merge_join_residual_rejections =
                join_exec_metrics.merge_join_residual_rejections;
            join_metric.counters.merge_join_sort_left = join_exec_metrics.merge_join_sort_left;
            join_metric.counters.merge_join_sort_right = join_exec_metrics.merge_join_sort_right;
            join_metric.add_child(current_op);
            join_metric.add_child(right_op);

            current_schema = next_schema;
            current_rows = next_rows;
            current_op = join_metric;
        }

        // 3. Post-Join Filter
        if let Some(post_filter) = &plan.post_join_filter {
            let pre_count = current_rows.len();
            let filtered_rows = self.filter_rows_with_outer_internal(
                current_rows,
                &current_schema,
                post_filter,
                outer_contexts,
                cte_store,
            )?;
            let mut filter_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::Filter,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            filter_metric.input_rows = pre_count;
            filter_metric.actual_rows = filtered_rows.len();
            filter_metric.counters.rows_evaluated = pre_count;
            filter_metric.counters.rows_passed = filtered_rows.len();
            filter_metric.counters.rows_rejected = pre_count.saturating_sub(filtered_rows.len());
            filter_metric.add_child(current_op);
            current_op = filter_metric;
            current_rows = filtered_rows;
        }

        // 4. Aggregation / Group By / Having
        let is_aggregate = !plan.group_by.is_empty() || !plan.aggregates.is_empty();
        let (mut working_schema, mut working_rows) = if is_aggregate {
            let pre_agg_count = current_rows.len();
            let (grouped_schema, grouped_rows, agg_metric_op) = match plan.aggregate_strategy {
                Some(crate::query::optimizer::AggregateStrategy::Streaming { sort_required }) => {
                    let (schema, rows, stream_metrics) =
                        crate::query::streaming_aggregate::StreamingAggregateOperator::new(
                            &current_schema,
                            &plan.group_by,
                            &plan.aggregates,
                            sort_required,
                            Some(self.db.sort_config.clone()),
                        )
                        .execute(current_rows)?;

                    let mut metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::StreamingAggregate,
                        Some(plan.estimated_rows),
                        Some(plan.total_cost),
                    );
                    metric.input_rows = stream_metrics.input_rows;
                    metric.actual_rows = rows.len();
                    metric.counters.streaming_aggregate_input_rows = stream_metrics.input_rows;
                    metric.counters.streaming_aggregate_output_groups =
                        stream_metrics.output_groups;
                    metric.counters.streaming_aggregate_group_transitions =
                        stream_metrics.group_transitions;
                    metric.counters.streaming_aggregate_max_group_size =
                        stream_metrics.max_group_size;
                    metric.counters.streaming_aggregate_aggregate_updates =
                        stream_metrics.aggregate_updates;
                    metric.counters.streaming_aggregate_sort_required =
                        stream_metrics.sort_required;
                    metric.counters.streaming_aggregate_sort_spilled = stream_metrics.sort_spilled;
                    metric.counters.streaming_aggregate_memory_bytes = stream_metrics.memory_bytes;
                    metric.counters.rows_evaluated = stream_metrics.input_rows;
                    metric.add_child(current_op);

                    (schema, rows, metric)
                }
                Some(crate::query::optimizer::AggregateStrategy::Index) => {
                    let mut out_cols = Vec::with_capacity(plan.aggregates.len());
                    for agg_expr in &plan.aggregates {
                        let dt = agg_expr.infer_type(&current_schema)?;
                        let name = agg_expr.display_name();
                        out_cols.push(crate::engine::types::Column::new(name, dt, true));
                    }
                    let grouped_schema = crate::engine::types::Schema::new_projected(out_cols);

                    let mut table = self.open_table(&plan.base_table.table_name)?;
                    let raw_schema = table.schema().clone();

                    let mut agg_values = Vec::with_capacity(plan.aggregates.len());
                    for agg in &plan.aggregates {
                        match agg {
                            crate::query::expr::AggregateExpr::CountAll => {
                                let (idx_name, scan_type, start, end) = match &plan
                                    .base_table
                                    .access_path
                                {
                                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    }
                                    | crate::query::optimizer::AccessPath::IndexOnlyScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    } => (Some(index_name.as_str()), Some(scan_type), None, None),
                                    crate::query::optimizer::AccessPath::PrimaryRangeScan {
                                        start,
                                        end,
                                    } => (None, None, start.as_ref(), end.as_ref()),
                                    _ => (None, None, None, None),
                                };
                                let count = table.count_index_entries(
                                    idx_name,
                                    scan_type,
                                    start,
                                    end,
                                    plan.base_table.residual_filter.as_ref(),
                                    None,
                                )?;
                                agg_values.push(Value::UInt64(count));
                            }
                            crate::query::expr::AggregateExpr::Function {
                                func: crate::query::expr::AggregateFunction::Count,
                                expr,
                            } => {
                                let count_col = match expr {
                                    crate::query::expr::Expr::Column(col) => {
                                        let pure =
                                            col.split('.').next_back().unwrap_or(col.as_str());
                                        Some(pure)
                                    }
                                    _ => None,
                                };
                                let (idx_name, scan_type, start, end) = match &plan
                                    .base_table
                                    .access_path
                                {
                                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    }
                                    | crate::query::optimizer::AccessPath::IndexOnlyScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    } => (Some(index_name.as_str()), Some(scan_type), None, None),
                                    crate::query::optimizer::AccessPath::PrimaryRangeScan {
                                        start,
                                        end,
                                    } => (None, None, start.as_ref(), end.as_ref()),
                                    _ => (None, None, None, None),
                                };
                                let count = table.count_index_entries(
                                    idx_name,
                                    scan_type,
                                    start,
                                    end,
                                    plan.base_table.residual_filter.as_ref(),
                                    count_col,
                                )?;
                                agg_values.push(Value::UInt64(count));
                            }
                            crate::query::expr::AggregateExpr::Function {
                                func: crate::query::expr::AggregateFunction::Min,
                                expr,
                            } => {
                                let target_col = match expr {
                                    crate::query::expr::Expr::Column(col) => {
                                        col.split('.').next_back().unwrap_or(col.as_str())
                                    }
                                    _ => "",
                                };
                                let (idx_name, start, end): (
                                    Option<String>,
                                    Option<&Value>,
                                    Option<&Value>,
                                ) = match &plan.base_table.access_path {
                                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    }
                                    | crate::query::optimizer::AccessPath::IndexOnlyScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    } => match scan_type {
                                        crate::query::optimizer::SecondaryScanType::RangeScan {
                                            start,
                                            end,
                                        } => {
                                            (Some(index_name.clone()), start.as_ref(), end.as_ref())
                                        }
                                        _ => (Some(index_name.clone()), None, None),
                                    },
                                    crate::query::optimizer::AccessPath::PrimaryRangeScan {
                                        start,
                                        end,
                                    } => (None, start.as_ref(), end.as_ref()),
                                    _ => {
                                        let is_pk =
                                            raw_schema.columns.first().map(|c| c.name.as_str())
                                                == Some(target_col);
                                        if is_pk {
                                            (None, None, None)
                                        } else {
                                            let sec_name = table
                                                .indexes
                                                .iter()
                                                .find(|(m, _)| {
                                                    m.column_indices.first().map(|&idx| {
                                                        raw_schema.columns[idx].name.as_str()
                                                    }) == Some(target_col)
                                                })
                                                .map(|(m, _)| m.name.clone());
                                            (sec_name, None, None)
                                        }
                                    }
                                };
                                let min_val = table.index_min_max(
                                    idx_name.as_deref(),
                                    target_col,
                                    true,
                                    start,
                                    end,
                                    plan.base_table.residual_filter.as_ref(),
                                )?;
                                agg_values.push(min_val.unwrap_or(Value::Null));
                            }
                            crate::query::expr::AggregateExpr::Function {
                                func: crate::query::expr::AggregateFunction::Max,
                                expr,
                            } => {
                                let target_col = match expr {
                                    crate::query::expr::Expr::Column(col) => {
                                        col.split('.').next_back().unwrap_or(col.as_str())
                                    }
                                    _ => "",
                                };
                                let (idx_name, start, end): (
                                    Option<String>,
                                    Option<&Value>,
                                    Option<&Value>,
                                ) = match &plan.base_table.access_path {
                                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    }
                                    | crate::query::optimizer::AccessPath::IndexOnlyScan {
                                        index_name,
                                        scan_type,
                                        ..
                                    } => match scan_type {
                                        crate::query::optimizer::SecondaryScanType::RangeScan {
                                            start,
                                            end,
                                        } => {
                                            (Some(index_name.clone()), start.as_ref(), end.as_ref())
                                        }
                                        _ => (Some(index_name.clone()), None, None),
                                    },
                                    crate::query::optimizer::AccessPath::PrimaryRangeScan {
                                        start,
                                        end,
                                    } => (None, start.as_ref(), end.as_ref()),
                                    _ => {
                                        let is_pk =
                                            raw_schema.columns.first().map(|c| c.name.as_str())
                                                == Some(target_col);
                                        if is_pk {
                                            (None, None, None)
                                        } else {
                                            let sec_name = table
                                                .indexes
                                                .iter()
                                                .find(|(m, _)| {
                                                    m.column_indices.first().map(|&idx| {
                                                        raw_schema.columns[idx].name.as_str()
                                                    }) == Some(target_col)
                                                })
                                                .map(|(m, _)| m.name.clone());
                                            (sec_name, None, None)
                                        }
                                    }
                                };
                                let max_val = table.index_min_max(
                                    idx_name.as_deref(),
                                    target_col,
                                    false,
                                    start,
                                    end,
                                    plan.base_table.residual_filter.as_ref(),
                                )?;
                                agg_values.push(max_val.unwrap_or(Value::Null));
                            }
                            _ => {
                                return Err(Error::QueryExecutionError(
                                    "unsupported index aggregate function".into(),
                                ));
                            }
                        }
                    }

                    let grouped_rows = vec![Row::new(agg_values)];
                    let mut agg_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::IndexAggregate,
                        Some(plan.estimated_rows),
                        Some(plan.total_cost),
                    );
                    agg_metric.input_rows = 1;
                    agg_metric.actual_rows = 1;
                    agg_metric.counters.early_terminated = true;
                    agg_metric.counters.visibility_checks_evaluated =
                        self.db.visibility_map.stats.visibility_checks_evaluated;
                    agg_metric.counters.visibility_checks_skipped =
                        self.db.visibility_map.stats.visibility_checks_skipped;
                    agg_metric.counters.visibility_map_hits =
                        self.db.visibility_map.stats.page_map_hits
                            + self.db.visibility_map.stats.record_map_hits;
                    agg_metric.add_child(current_op);

                    (grouped_schema, grouped_rows, agg_metric)
                }
                _ => {
                    let (grouped_schema, grouped_rows, agg_metrics) =
                        crate::query::AggregateOperator::apply_with_config(
                            current_rows,
                            &current_schema,
                            &plan.group_by,
                            &plan.aggregates,
                            &self.db.aggregate_config,
                        )?;
                    let mut agg_metric = crate::query::OperatorMetrics::new(
                        crate::query::OperatorKind::Aggregate,
                        Some(plan.estimated_rows),
                        Some(plan.total_cost),
                    );
                    agg_metric.input_rows = pre_agg_count;
                    agg_metric.actual_rows = grouped_rows.len();
                    agg_metric.counters.group_count = agg_metrics.groups_created;
                    agg_metric.counters.spill_partitions = agg_metrics.spill_partitions;
                    agg_metric.counters.runs_created = agg_metrics.runs_created;
                    agg_metric.counters.bytes_spilled = agg_metrics.bytes_spilled;
                    agg_metric.counters.merge_passes = agg_metrics.merge_passes;
                    agg_metric.counters.peak_memory_bytes = agg_metrics.peak_memory_bytes;
                    agg_metric.counters.rows_evaluated = pre_agg_count;
                    agg_metric.add_child(current_op);

                    (grouped_schema, grouped_rows, agg_metric)
                }
            };
            let num_groups = grouped_rows.len();

            let (having_rows, final_rows) = match &plan.having {
                Some(predicate) => {
                    let filtered = self.filter_rows_with_outer_internal(
                        grouped_rows.clone(),
                        &grouped_schema,
                        predicate,
                        outer_contexts,
                        cte_store,
                    )?;
                    (Some(filtered.clone()), filtered)
                }
                None => (None, grouped_rows),
            };

            if let Some(having_filtered) = having_rows {
                let mut having_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::Filter,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                having_metric.input_rows = num_groups;
                having_metric.actual_rows = having_filtered.len();
                having_metric.counters.rows_evaluated = num_groups;
                having_metric.counters.rows_passed = having_filtered.len();
                having_metric.counters.rows_rejected =
                    num_groups.saturating_sub(having_filtered.len());
                having_metric.add_child(agg_metric_op);
                current_op = having_metric;
            } else {
                current_op = agg_metric_op;
            }

            (grouped_schema, final_rows)
        } else {
            (current_schema, current_rows)
        };

        // 5. Window Operator
        if !plan.window_exprs.is_empty() {
            let pre_win_count = working_rows.len();
            let (win_schema, win_rows) = crate::query::WindowOperator::apply(
                working_rows,
                &working_schema,
                &plan.window_exprs,
            )?;
            let mut win_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::Window,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            win_metric.input_rows = pre_win_count;
            win_metric.actual_rows = win_rows.len();
            win_metric.add_child(current_op);
            current_op = win_metric;
            working_schema = win_schema;
            working_rows = win_rows;
        }

        // 6. Sort / Top-N Operator (ORDER BY)
        let (sorted_rows, top_n_executed) = if !plan.order_by.is_empty()
            && plan.order_by_sort_required
        {
            if plan.order_by_top_n {
                let (top_n_rows, top_n_metrics) = crate::query::TopNOperator::apply_with_config(
                    working_rows,
                    &working_schema,
                    &plan.order_by,
                    plan.limit,
                    plan.offset,
                    &self.db.sort_config,
                )?;
                let mut top_n_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::TopN,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                top_n_metric.input_rows = top_n_metrics.rows_input;
                top_n_metric.actual_rows = top_n_rows.len();
                top_n_metric.counters.top_n_candidate_capacity = top_n_metrics.candidate_capacity;
                top_n_metric.counters.top_n_rows_retained = top_n_metrics.rows_retained;
                top_n_metric.counters.top_n_rows_discarded = top_n_metrics.rows_discarded;
                top_n_metric.counters.top_n_early_terminated = top_n_metrics.early_terminated;
                top_n_metric.counters.early_terminated = top_n_metrics.early_terminated;
                top_n_metric.counters.peak_memory_bytes = top_n_metrics.peak_memory_bytes;
                if top_n_metrics.peak_memory_bytes > 0 {
                    ctx.resource_manager
                        .reserve_memory(top_n_metrics.peak_memory_bytes)?;
                    ctx.resource_manager
                        .release_memory(top_n_metrics.peak_memory_bytes);
                }
                top_n_metric.counters.top_n_limit = top_n_metrics.limit.unwrap_or(0);
                top_n_metric.counters.top_n_offset = top_n_metrics.offset.unwrap_or(0);
                top_n_metric.counters.sort_required = true;
                top_n_metric.counters.sort_eliminated = false;
                top_n_metric.add_child(current_op);
                current_op = top_n_metric;
                (top_n_rows, true)
            } else {
                let (sorted, sort_metrics) = crate::query::SortOperator::apply_with_config(
                    working_rows,
                    &working_schema,
                    &plan.order_by,
                    &self.db.sort_config,
                )?;
                let mut sort_metric = crate::query::OperatorMetrics::new(
                    crate::query::OperatorKind::Sort,
                    Some(plan.estimated_rows),
                    Some(plan.total_cost),
                );
                sort_metric.input_rows = sort_metrics.rows_input;
                sort_metric.actual_rows = sorted.len();
                sort_metric.counters.runs_created = sort_metrics.runs_created;
                sort_metric.counters.bytes_spilled = sort_metrics.bytes_spilled;
                sort_metric.counters.merge_passes = sort_metrics.merge_passes;
                sort_metric.counters.peak_memory_bytes = sort_metrics.peak_memory_bytes;
                if sort_metrics.peak_memory_bytes > 0 {
                    ctx.resource_manager
                        .reserve_memory(sort_metrics.peak_memory_bytes)?;
                    ctx.resource_manager
                        .release_memory(sort_metrics.peak_memory_bytes);
                }
                sort_metric.counters.sort_required = true;
                sort_metric.counters.sort_eliminated = false;
                sort_metric.add_child(current_op);
                current_op = sort_metric;
                (sorted, false)
            }
        } else {
            if !plan.order_by.is_empty() {
                current_op.counters.sort_eliminated = true;
                current_op.counters.sort_required = false;
            }
            (working_rows, false)
        };

        // 7. Projection
        let pre_proj_count = sorted_rows.len();
        let (projected_schema, projected_rows) = crate::query::ProjectOperator::apply_named(
            sorted_rows,
            &working_schema,
            &plan.projections,
        )?;
        let mut proj_metric = crate::query::OperatorMetrics::new(
            crate::query::OperatorKind::Project,
            Some(plan.estimated_rows),
            Some(plan.total_cost),
        );
        proj_metric.input_rows = pre_proj_count;
        proj_metric.actual_rows = projected_rows.len();
        proj_metric.add_child(current_op);
        current_op = proj_metric;

        // 8. Pagination (OFFSET / LIMIT)
        let paginated_rows = if !top_n_executed && (plan.offset.is_some() || plan.limit.is_some()) {
            let pre_limit_count = projected_rows.len();
            let paginated =
                crate::query::OffsetLimitOperator::apply(projected_rows, plan.offset, plan.limit);
            let mut limit_metric = crate::query::OperatorMetrics::new(
                crate::query::OperatorKind::LimitOffset,
                Some(plan.estimated_rows),
                Some(plan.total_cost),
            );
            limit_metric.input_rows = pre_limit_count;
            limit_metric.actual_rows = paginated.len();
            limit_metric.add_child(current_op);
            current_op = limit_metric;
            paginated
        } else {
            projected_rows
        };

        current_op.execution_time = op_start.elapsed();

        // If CTEs were materialized in this plan, attach them to the top operator
        for cte_metric in cte_metrics {
            current_op.add_child(cte_metric);
        }

        if current_op
            .find_first(&|m| m.counters.early_terminated)
            .is_some()
        {
            ctx.termination_status = crate::query::ExecutionTerminationStatus::CompletedEarly;
        }

        if !ctx.resource_manager.is_clean() {
            return Err(Error::InternalExecutionError(format!(
                "Resource leak detected after execution: memory={} bytes, temp_files={}, temp_bytes={}",
                ctx.resource_manager.active_memory_reservations(),
                ctx.resource_manager.active_temp_files(),
                ctx.resource_manager.active_temp_bytes(),
            )));
        }

        Ok((
            crate::query::QueryResult::new(projected_schema, paginated_rows),
            current_op,
        ))
    }

    /// Pre-evaluates all uncorrelated subqueries in the physical plan.
    pub fn pre_evaluate_uncorrelated_subqueries(
        &mut self,
        plan: &mut crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<()> {
        let mut cte_store = std::collections::HashMap::new();
        self.pre_evaluate_uncorrelated_subqueries_internal(plan, outer_contexts, &mut cte_store)
    }

    fn execute_index_probe_semi_anti(
        &mut self,
        current_rows: &[Row],
        current_schema: &Schema,
        join: &crate::query::optimizer::PhysicalJoin,
        current_op: &crate::query::OperatorMetrics,
    ) -> Result<Option<(Vec<Row>, crate::query::OperatorMetrics)>> {
        let index_name = match &join.index_name {
            Some(idx) => idx.clone(),
            None => return Ok(None),
        };

        let mut table = self.open_table(&join.right.table_name)?;
        let raw_schema = table.schema().clone();
        let right_qualifier = join.right.qualifier();

        let on_conjuncts = crate::query::optimizer::split_conjunction(&join.on);
        let mut equi_pair: Option<(crate::query::expr::Expr, String)> = None;
        let mut residual_on: Vec<crate::query::expr::Expr> = Vec::new();

        for conj in &on_conjuncts {
            if equi_pair.is_none() {
                if let crate::query::expr::Expr::Binary {
                    left,
                    op: crate::query::expr::BinaryOp::Eq,
                    right,
                } = conj
                {
                    let l_refs = crate::query::optimizer::expr_referenced_qualifiers(left);
                    let r_refs = crate::query::optimizer::expr_referenced_qualifiers(right);

                    if l_refs.len() == 1 && r_refs.len() == 1 {
                        let l_q = l_refs.iter().next().unwrap();
                        let r_q = r_refs.iter().next().unwrap();

                        if l_q == right_qualifier && r_q != right_qualifier {
                            if let crate::query::expr::Expr::Column(inner_col) = left.as_ref() {
                                let in_name = inner_col.split('.').next_back().unwrap_or(inner_col);
                                equi_pair = Some((*right.clone(), in_name.to_string()));
                                continue;
                            }
                        } else if r_q == right_qualifier && l_q != right_qualifier {
                            if let crate::query::expr::Expr::Column(inner_col) = right.as_ref() {
                                let in_name = inner_col.split('.').next_back().unwrap_or(inner_col);
                                equi_pair = Some((*left.clone(), in_name.to_string()));
                                continue;
                            }
                        }
                    }
                }
            }
            residual_on.push(conj.clone());
        }

        let (outer_expr, _) = match equi_pair {
            Some(p) => p,
            None => return Ok(None),
        };

        let cols: Vec<crate::engine::types::Column> = raw_schema
            .columns
            .iter()
            .map(|c| {
                crate::engine::types::Column::new(
                    format!("{right_qualifier}.{}", c.name),
                    c.data_type,
                    c.nullable,
                )
            })
            .collect();
        let right_schema = Schema::new_projected(cols);

        let mut combined_cols = current_schema.columns.clone();
        combined_cols.extend(right_schema.columns.clone());
        let combined_schema = Schema::new_projected(combined_cols);

        let is_pk = index_name == "__primary__";
        let is_semi = join.join_type == crate::sql::JoinType::Semi;

        let mut out_rows = Vec::new();
        let mut index_probes = 0;
        let mut successful_probes = 0;
        let mut failed_probes = 0;
        let mut matched_outer_rows = 0;
        let mut unmatched_outer_rows = 0;
        let mut inner_rows_fetched = 0;
        let mut candidate_pairs = 0;
        let row_goal_k = join.row_goal.as_ref().map(|rg| rg.k() as usize);
        let mut early_terminated = false;
        let mut row_goal_satisfied = false;

        for outer_row in current_rows {
            let outer_val = match outer_expr.evaluate(outer_row, current_schema)? {
                Value::Null => {
                    unmatched_outer_rows += 1;
                    if !is_semi {
                        out_rows.push(outer_row.clone());
                        if let Some(k) = row_goal_k {
                            if out_rows.len() >= k {
                                early_terminated = true;
                                row_goal_satisfied = true;
                                break;
                            }
                        }
                    }
                    continue;
                }
                v => v,
            };

            index_probes += 1;
            let mut match_found = false;

            if is_pk {
                if let Some(inner_row) = table.get(&outer_val)? {
                    inner_rows_fetched += 1;
                    candidate_pairs += 1;

                    let passes_residual = match &join.right.residual_filter {
                        Some(f) => {
                            let val = f.evaluate(&inner_row, &raw_schema)?;
                            val == Value::Boolean(true)
                        }
                        None => true,
                    };

                    if passes_residual {
                        let passes_on = if residual_on.is_empty() {
                            true
                        } else {
                            let mut combined_values =
                                Vec::with_capacity(outer_row.values.len() + inner_row.values.len());
                            combined_values.extend_from_slice(&outer_row.values);
                            combined_values.extend_from_slice(&inner_row.values);
                            let combined_row = Row::new(combined_values);

                            let mut all_ok = true;
                            for r_expr in &residual_on {
                                if r_expr.evaluate(&combined_row, &combined_schema)?
                                    != Value::Boolean(true)
                                {
                                    all_ok = false;
                                    break;
                                }
                            }
                            all_ok
                        };

                        if passes_on {
                            match_found = true;
                        }
                    }
                }
            } else {
                let rids = table.index_lookup_rids(&index_name, &outer_val)?;
                let has_residuals = join.right.residual_filter.is_some() || !residual_on.is_empty();

                if !has_residuals {
                    if !rids.is_empty() {
                        match_found = true;
                        candidate_pairs += 1;
                    }
                } else {
                    for rid in rids {
                        candidate_pairs += 1;
                        if let Some(inner_row) = table.get_row_by_rid(rid)? {
                            inner_rows_fetched += 1;
                            let passes_residual = match &join.right.residual_filter {
                                Some(f) => {
                                    let val = f.evaluate(&inner_row, &raw_schema)?;
                                    val == Value::Boolean(true)
                                }
                                None => true,
                            };

                            if passes_residual {
                                let passes_on = if residual_on.is_empty() {
                                    true
                                } else {
                                    let mut combined_values = Vec::with_capacity(
                                        outer_row.values.len() + inner_row.values.len(),
                                    );
                                    combined_values.extend_from_slice(&outer_row.values);
                                    combined_values.extend_from_slice(&inner_row.values);
                                    let combined_row = Row::new(combined_values);

                                    let mut all_ok = true;
                                    for r_expr in &residual_on {
                                        if r_expr.evaluate(&combined_row, &combined_schema)?
                                            != Value::Boolean(true)
                                        {
                                            all_ok = false;
                                            break;
                                        }
                                    }
                                    all_ok
                                };

                                if passes_on {
                                    match_found = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if match_found {
                successful_probes += 1;
                matched_outer_rows += 1;
                if is_semi {
                    out_rows.push(outer_row.clone());
                    if let Some(k) = row_goal_k {
                        if out_rows.len() >= k {
                            early_terminated = true;
                            row_goal_satisfied = true;
                            break;
                        }
                    }
                }
            } else {
                failed_probes += 1;
                unmatched_outer_rows += 1;
                if !is_semi {
                    out_rows.push(outer_row.clone());
                    if let Some(k) = row_goal_k {
                        if out_rows.len() >= k {
                            early_terminated = true;
                            row_goal_satisfied = true;
                            break;
                        }
                    }
                }
            }
        }

        let op_kind = if is_semi {
            crate::query::OperatorKind::IndexSemiProbe {
                index_name: index_name.clone(),
            }
        } else {
            crate::query::OperatorKind::IndexAntiProbe {
                index_name: index_name.clone(),
            }
        };

        let mut join_metric =
            crate::query::OperatorMetrics::new(op_kind, Some(join.estimated_rows), Some(join.cost));
        join_metric.input_rows = current_rows.len() + inner_rows_fetched;
        join_metric.actual_rows = out_rows.len();
        join_metric.counters.rows_left = current_rows.len();
        join_metric.counters.rows_right = inner_rows_fetched;
        join_metric.counters.matched_outer_rows = matched_outer_rows;
        join_metric.counters.unmatched_outer_rows = unmatched_outer_rows;
        join_metric.counters.index_probes = index_probes;
        join_metric.counters.successful_probes = successful_probes;
        join_metric.counters.failed_probes = failed_probes;
        join_metric.counters.candidate_pairs = candidate_pairs;
        join_metric.counters.rows_output = out_rows.len();
        join_metric.counters.row_goal = row_goal_k;
        join_metric.counters.early_terminated = early_terminated;
        join_metric.counters.row_goal_satisfied = row_goal_satisfied;
        join_metric.counters.rows_examined = index_probes + inner_rows_fetched;

        let mut inner_scan_metric = crate::query::OperatorMetrics::new(
            crate::query::OperatorKind::TableScan {
                table_name: join.right.table_name.clone(),
                access_path: format!("IndexProbe({index_name})"),
            },
            Some(join.right.estimated_rows),
            Some(join.right.cost),
        );
        inner_scan_metric.input_rows = inner_rows_fetched;
        inner_scan_metric.actual_rows = inner_rows_fetched;
        inner_scan_metric.counters.index_lookups = index_probes;
        inner_scan_metric.counters.tuples_fetched = inner_rows_fetched;

        join_metric.add_child(current_op.clone());
        join_metric.add_child(inner_scan_metric);

        Ok(Some((out_rows, join_metric)))
    }

    fn pre_evaluate_uncorrelated_subqueries_internal(
        &mut self,
        plan: &mut crate::query::optimizer::PhysicalPlan,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<()> {
        if let Some(set_op) = &mut plan.set_op {
            self.pre_evaluate_uncorrelated_subqueries_internal(
                &mut set_op.left,
                outer_contexts,
                cte_store,
            )?;
            self.pre_evaluate_uncorrelated_subqueries_internal(
                &mut set_op.right,
                outer_contexts,
                cte_store,
            )?;
            return Ok(());
        }
        let mut subquery_cache = std::collections::HashMap::new();
        if let Some(filter) = &mut plan.base_table.residual_filter {
            self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                filter,
                outer_contexts,
                cte_store,
                &mut subquery_cache,
            )?;
        }
        for join in &mut plan.joins {
            self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                &mut join.on,
                outer_contexts,
                cte_store,
                &mut subquery_cache,
            )?;
            if let Some(filter) = &mut join.right.residual_filter {
                self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                    filter,
                    outer_contexts,
                    cte_store,
                    &mut subquery_cache,
                )?;
            }
        }
        if let Some(post_f) = &mut plan.post_join_filter {
            self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                post_f,
                outer_contexts,
                cte_store,
                &mut subquery_cache,
            )?;
        }
        if let Some(having) = &mut plan.having {
            self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                having,
                outer_contexts,
                cte_store,
                &mut subquery_cache,
            )?;
        }
        for (proj, _) in &mut plan.projections {
            self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                proj,
                outer_contexts,
                cte_store,
                &mut subquery_cache,
            )?;
        }
        for sort_key in &mut plan.order_by {
            self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                &mut sort_key.expression,
                outer_contexts,
                cte_store,
                &mut subquery_cache,
            )?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn pre_evaluate_uncorrelated_subqueries_in_expr(
        &mut self,
        expr: &mut crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<()> {
        let mut subquery_cache = std::collections::HashMap::new();
        self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
            expr,
            outer_contexts,
            cte_store,
            &mut subquery_cache,
        )
    }

    fn pre_evaluate_uncorrelated_subqueries_in_expr_cached(
        &mut self,
        expr: &mut crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
        subquery_cache: &mut std::collections::HashMap<
            String,
            crate::query::expr::MaterializedSubqueryResult,
        >,
    ) -> Result<()> {
        match expr {
            crate::query::expr::Expr::Subquery(sub) => {
                if !sub.is_correlated && sub.materialized.is_none() {
                    let fp = crate::query::materialize::SubplanFingerprint::from_plan(&sub.plan)
                        .to_hex();
                    if let Some(cached) = subquery_cache.get(&fp) {
                        sub.materialized = Some(cached.clone());
                    } else {
                        let mat = self.evaluate_subquery_materialized_internal(
                            sub,
                            outer_contexts,
                            cte_store,
                        )?;
                        subquery_cache.insert(fp, mat.clone());
                        sub.materialized = Some(mat);
                    }
                }
            }
            crate::query::expr::Expr::Binary { left, right, .. } => {
                self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                    left,
                    outer_contexts,
                    cte_store,
                    subquery_cache,
                )?;
                self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                    right,
                    outer_contexts,
                    cte_store,
                    subquery_cache,
                )?;
            }
            crate::query::expr::Expr::Unary { expr, .. }
            | crate::query::expr::Expr::IsNull(expr)
            | crate::query::expr::Expr::IsNotNull(expr) => {
                self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                    expr,
                    outer_contexts,
                    cte_store,
                    subquery_cache,
                )?;
            }
            crate::query::expr::Expr::InList { expr, list, .. } => {
                self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                    expr,
                    outer_contexts,
                    cte_store,
                    subquery_cache,
                )?;
                for item in list {
                    self.pre_evaluate_uncorrelated_subqueries_in_expr_cached(
                        item,
                        outer_contexts,
                        cte_store,
                        subquery_cache,
                    )?;
                }
            }
            crate::query::expr::Expr::Literal(_)
            | crate::query::expr::Expr::Column(_)
            | crate::query::expr::Expr::OuterColumn { .. }
            | crate::query::expr::Expr::Parameter { .. } => {}
        }
        Ok(())
    }

    /// Evaluates a subquery into a materialized in-memory representation.
    pub fn evaluate_subquery_materialized(
        &mut self,
        subquery: &crate::query::expr::SubqueryExpr,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<crate::query::expr::MaterializedSubqueryResult> {
        let mut cte_store = std::collections::HashMap::new();
        self.evaluate_subquery_materialized_internal(subquery, outer_contexts, &mut cte_store)
    }

    fn evaluate_subquery_materialized_internal(
        &mut self,
        subquery: &crate::query::expr::SubqueryExpr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<crate::query::expr::MaterializedSubqueryResult> {
        match &subquery.kind {
            crate::query::expr::SubqueryKind::Scalar { .. } => {
                let res =
                    self.execute_physical_plan_internal(&subquery.plan, outer_contexts, cte_store)?;
                if res.rows.is_empty() {
                    Ok(crate::query::expr::MaterializedSubqueryResult::Scalar(
                        Value::Null,
                    ))
                } else if res.rows.len() == 1 {
                    Ok(crate::query::expr::MaterializedSubqueryResult::Scalar(
                        res.rows[0].values[0].clone(),
                    ))
                } else {
                    Err(Error::ScalarSubqueryMultipleRows)
                }
            }
            crate::query::expr::SubqueryKind::In { .. } => {
                let res =
                    self.execute_physical_plan_internal(&subquery.plan, outer_contexts, cte_store)?;
                let in_vals: Vec<Value> =
                    res.rows.into_iter().map(|r| r.values[0].clone()).collect();
                Ok(crate::query::expr::MaterializedSubqueryResult::In(in_vals))
            }
            crate::query::expr::SubqueryKind::Exists { .. } => {
                let res =
                    self.execute_physical_plan_internal(&subquery.plan, outer_contexts, cte_store)?;
                let exists = !res.rows.is_empty();
                Ok(crate::query::expr::MaterializedSubqueryResult::Exists(
                    exists,
                ))
            }
        }
    }

    /// Evaluates an expression against a row in the current table and any enclosing outer scopes.
    pub fn evaluate_expr_with_outer(
        &mut self,
        expr: &crate::query::expr::Expr,
        row: &Row,
        schema: &Schema,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<Value> {
        let mut cte_store = std::collections::HashMap::new();
        self.evaluate_expr_with_outer_internal(expr, row, schema, outer_contexts, &mut cte_store)
    }

    pub(crate) fn evaluate_expr_with_outer_internal(
        &mut self,
        expr: &crate::query::expr::Expr,
        row: &Row,
        schema: &Schema,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<Value> {
        match expr {
            crate::query::expr::Expr::Subquery(sub) => {
                if let Some(mat) = &sub.materialized {
                    match mat {
                        crate::query::expr::MaterializedSubqueryResult::Scalar(val) => {
                            Ok(val.clone())
                        }
                        crate::query::expr::MaterializedSubqueryResult::In(in_vals) => {
                            if let crate::query::expr::SubqueryKind::In { expr, negated } =
                                &sub.kind
                            {
                                let left = self.evaluate_expr_with_outer_internal(
                                    expr,
                                    row,
                                    schema,
                                    outer_contexts,
                                    cte_store,
                                )?;
                                Ok(crate::query::expr::eval_in_3vl(&left, in_vals, *negated))
                            } else {
                                unreachable!()
                            }
                        }
                        crate::query::expr::MaterializedSubqueryResult::Exists(b) => {
                            if let crate::query::expr::SubqueryKind::Exists { negated } = &sub.kind
                            {
                                let res = if *negated { !*b } else { *b };
                                Ok(Value::Boolean(res))
                            } else {
                                unreachable!()
                            }
                        }
                    }
                } else {
                    let mut nested = outer_contexts.to_vec();
                    nested.push((row, schema));
                    match &sub.kind {
                        crate::query::expr::SubqueryKind::Scalar { .. } => {
                            let res =
                                self.execute_physical_plan_internal(&sub.plan, &nested, cte_store)?;
                            if res.rows.is_empty() {
                                Ok(Value::Null)
                            } else if res.rows.len() == 1 {
                                Ok(res.rows[0].values[0].clone())
                            } else {
                                Err(Error::ScalarSubqueryMultipleRows)
                            }
                        }
                        crate::query::expr::SubqueryKind::In { expr, negated } => {
                            let left = self.evaluate_expr_with_outer_internal(
                                expr,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            let res =
                                self.execute_physical_plan_internal(&sub.plan, &nested, cte_store)?;
                            let in_vals: Vec<Value> =
                                res.rows.into_iter().map(|r| r.values[0].clone()).collect();
                            Ok(crate::query::expr::eval_in_3vl(&left, &in_vals, *negated))
                        }
                        crate::query::expr::SubqueryKind::Exists { negated } => {
                            let res =
                                self.execute_physical_plan_internal(&sub.plan, &nested, cte_store)?;
                            let has_rows = !res.rows.is_empty();
                            let b = if *negated { !has_rows } else { has_rows };
                            Ok(Value::Boolean(b))
                        }
                    }
                }
            }
            crate::query::expr::Expr::InList {
                expr,
                list,
                negated,
            } => {
                let target = self.evaluate_expr_with_outer_internal(
                    expr,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                let mut vals = Vec::with_capacity(list.len());
                for item in list {
                    vals.push(self.evaluate_expr_with_outer_internal(
                        item,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?);
                }
                Ok(crate::query::expr::eval_in_3vl(&target, &vals, *negated))
            }
            crate::query::expr::Expr::Binary { left, op, right } => match op {
                crate::query::expr::BinaryOp::And => {
                    let l = self.evaluate_expr_with_outer_internal(
                        left,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    match l {
                        Value::Boolean(false) => Ok(Value::Boolean(false)),
                        Value::Boolean(true) => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            match r {
                                Value::Boolean(false) => Ok(Value::Boolean(false)),
                                Value::Boolean(true) | Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        other => Err(Error::InvalidBooleanExpression {
                            found: other.data_type().to_string(),
                        }),
                    }
                }
                crate::query::expr::BinaryOp::Or => {
                    let l = self.evaluate_expr_with_outer_internal(
                        left,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    match l {
                        Value::Boolean(true) => Ok(Value::Boolean(true)),
                        Value::Boolean(false) => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            match r {
                                Value::Boolean(rb) => Ok(Value::Boolean(rb)),
                                Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        Value::Null => {
                            let r = self.evaluate_expr_with_outer_internal(
                                right,
                                row,
                                schema,
                                outer_contexts,
                                cte_store,
                            )?;
                            match r {
                                Value::Boolean(true) => Ok(Value::Boolean(true)),
                                Value::Boolean(false) | Value::Null => Ok(Value::Null),
                                other => Err(Error::InvalidBooleanExpression {
                                    found: other.data_type().to_string(),
                                }),
                            }
                        }
                        other => Err(Error::InvalidBooleanExpression {
                            found: other.data_type().to_string(),
                        }),
                    }
                }
                _ => {
                    let l = self.evaluate_expr_with_outer_internal(
                        left,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    let r = self.evaluate_expr_with_outer_internal(
                        right,
                        row,
                        schema,
                        outer_contexts,
                        cte_store,
                    )?;
                    let tmp_schema = Schema::new_projected(Vec::new());
                    let tmp_row = Row::new(Vec::new());
                    let bin_expr = crate::query::expr::Expr::Binary {
                        left: Box::new(crate::query::expr::Expr::Literal(l)),
                        op: *op,
                        right: Box::new(crate::query::expr::Expr::Literal(r)),
                    };
                    bin_expr.evaluate(&tmp_row, &tmp_schema)
                }
            },
            crate::query::expr::Expr::Unary { op, expr } => {
                let val = self.evaluate_expr_with_outer_internal(
                    expr,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                let tmp_schema = Schema::new_projected(Vec::new());
                let tmp_row = Row::new(Vec::new());
                let un_expr = crate::query::expr::Expr::Unary {
                    op: *op,
                    expr: Box::new(crate::query::expr::Expr::Literal(val)),
                };
                un_expr.evaluate(&tmp_row, &tmp_schema)
            }
            crate::query::expr::Expr::IsNull(inner) => {
                let val = self.evaluate_expr_with_outer_internal(
                    inner,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                Ok(Value::Boolean(val.is_null()))
            }
            crate::query::expr::Expr::IsNotNull(inner) => {
                let val = self.evaluate_expr_with_outer_internal(
                    inner,
                    row,
                    schema,
                    outer_contexts,
                    cte_store,
                )?;
                Ok(Value::Boolean(!val.is_null()))
            }
            crate::query::expr::Expr::Literal(_)
            | crate::query::expr::Expr::Column(_)
            | crate::query::expr::Expr::OuterColumn { .. }
            | crate::query::expr::Expr::Parameter { .. } => {
                expr.evaluate_with_outer(row, schema, outer_contexts)
            }
        }
    }

    /// Filters rows evaluating predicate against active row and outer contexts.
    pub fn filter_rows_with_outer(
        &mut self,
        rows: Vec<Row>,
        schema: &Schema,
        predicate: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<Vec<Row>> {
        let mut cte_store = std::collections::HashMap::new();
        self.filter_rows_with_outer_internal(
            rows,
            schema,
            predicate,
            outer_contexts,
            &mut cte_store,
        )
    }

    fn filter_rows_with_outer_internal(
        &mut self,
        rows: Vec<Row>,
        schema: &Schema,
        predicate: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<Vec<Row>> {
        let pred_type = predicate.infer_type(schema)?;
        if pred_type != DataType::Boolean && pred_type != DataType::Null {
            return Err(Error::InvalidBooleanExpression {
                found: pred_type.to_string(),
            });
        }

        let mut matched = Vec::new();
        for row in rows {
            let val = self.evaluate_expr_with_outer_internal(
                predicate,
                &row,
                schema,
                outer_contexts,
                cte_store,
            )?;
            if val == Value::Boolean(true) {
                matched.push(row);
            }
        }
        Ok(matched)
    }

    /// Performs nested-loop join evaluating join condition against combined row and outer contexts.
    #[allow(clippy::too_many_arguments)]
    pub fn join_rows_with_outer(
        &mut self,
        left_rows: Vec<Row>,
        left_schema: &Schema,
        right_rows: &[Row],
        right_schema: &Schema,
        join_type: crate::sql::ast::JoinType,
        on: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
    ) -> Result<(Schema, Vec<Row>)> {
        if outer_contexts.is_empty() {
            let (s, r, _) = crate::query::operator::JoinOperator::apply_with_config(
                left_rows,
                left_schema,
                right_rows,
                right_schema,
                join_type,
                on,
                &self.db.join_config,
            )?;
            return Ok((s, r));
        }
        let mut cte_store = std::collections::HashMap::new();
        self.join_rows_with_outer_internal(
            left_rows,
            left_schema,
            right_rows,
            right_schema,
            join_type,
            on,
            outer_contexts,
            &mut cte_store,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn join_rows_with_outer_internal(
        &mut self,
        left_rows: Vec<Row>,
        left_schema: &Schema,
        right_rows: &[Row],
        right_schema: &Schema,
        join_type: crate::sql::ast::JoinType,
        on: &crate::query::expr::Expr,
        outer_contexts: &[(&Row, &Schema)],
        cte_store: &mut std::collections::HashMap<String, (Schema, Vec<Row>)>,
    ) -> Result<(Schema, Vec<Row>)> {
        let mut combined_columns =
            Vec::with_capacity(left_schema.columns.len() + right_schema.columns.len());
        for col in &left_schema.columns {
            let mut left_col = col.clone();
            if join_type == crate::sql::ast::JoinType::Right
                || join_type == crate::sql::ast::JoinType::Full
            {
                left_col.nullable = true;
            }
            combined_columns.push(left_col);
        }
        for col in &right_schema.columns {
            let mut right_col = col.clone();
            if join_type == crate::sql::ast::JoinType::Left
                || join_type == crate::sql::ast::JoinType::Full
            {
                right_col.nullable = true;
            }
            combined_columns.push(right_col);
        }
        let combined_schema = Schema::new_projected(combined_columns);

        let left_null_values = vec![Value::Null; left_schema.columns.len()];
        let right_null_values = vec![Value::Null; right_schema.columns.len()];
        let mut result_rows = Vec::new();
        let mut right_matched = vec![false; right_rows.len()];

        for l_row in left_rows {
            let mut left_matched = false;
            for (r_idx, r_row) in right_rows.iter().enumerate() {
                let mut combined_vals = Vec::with_capacity(l_row.values.len() + r_row.values.len());
                combined_vals.extend_from_slice(&l_row.values);
                combined_vals.extend_from_slice(&r_row.values);
                let candidate_row = Row::new(combined_vals);

                let on_val = self.evaluate_expr_with_outer_internal(
                    on,
                    &candidate_row,
                    &combined_schema,
                    outer_contexts,
                    cte_store,
                )?;
                if on_val == Value::Boolean(true) {
                    left_matched = true;
                    right_matched[r_idx] = true;
                    result_rows.push(candidate_row);
                }
            }
            if !left_matched
                && (join_type == crate::sql::ast::JoinType::Left
                    || join_type == crate::sql::ast::JoinType::Full)
            {
                let mut combined_vals =
                    Vec::with_capacity(l_row.values.len() + right_null_values.len());
                combined_vals.extend_from_slice(&l_row.values);
                combined_vals.extend_from_slice(&right_null_values);
                result_rows.push(Row::new(combined_vals));
            }
        }

        if join_type == crate::sql::ast::JoinType::Right
            || join_type == crate::sql::ast::JoinType::Full
        {
            for (r_idx, r_row) in right_rows.iter().enumerate() {
                if !right_matched[r_idx] {
                    let mut combined_vals =
                        Vec::with_capacity(left_null_values.len() + r_row.values.len());
                    combined_vals.extend_from_slice(&left_null_values);
                    combined_vals.extend_from_slice(&r_row.values);
                    result_rows.push(Row::new(combined_vals));
                }
            }
        }

        Ok((combined_schema, result_rows))
    }

    /// Drops a table from the catalog and reclaims its pages within this transaction.
    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.ensure_active()?;
        let res = crate::engine::catalog::Catalog::drop_table(&mut self.db.buffer_pool, name);
        if res.is_ok() {
            self.has_catalog_mutations = true;
            self.db.catalog_generation += 1;
            self.db.plan_cache.invalidate_all();
        }
        res
    }

    /// Retrieves table metadata for `name` within this transaction.
    pub fn get_table(
        &mut self,
        name: &str,
    ) -> Result<Option<crate::engine::catalog::TableMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_table(&mut self.db.buffer_pool, name)
    }

    /// Retrieves table metadata for `table_id` within this transaction.
    pub fn get_table_by_id(
        &mut self,
        table_id: crate::engine::catalog::TableId,
    ) -> Result<Option<crate::engine::catalog::TableMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::get_table_by_id(&mut self.db.buffer_pool, table_id)
    }

    /// Lists all tables in the catalog within this transaction.
    pub fn list_tables(&mut self) -> Result<Vec<crate::engine::catalog::TableMetadata>> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::list_tables(&mut self.db.buffer_pool)
    }

    /// Validates the structural and referential integrity of the catalog within this transaction.
    pub fn validate_catalog(&mut self) -> Result<()> {
        self.ensure_active()?;
        crate::engine::catalog::Catalog::validate_catalog(&mut self.db.buffer_pool)
    }

    /// Executes a synchronous checkpoint during an active transaction.
    pub fn checkpoint(&mut self) -> Result<crate::wal::lsn::Lsn> {
        self.db.checkpoint()
    }

    /// Executes a synchronous checkpoint and truncates obsolete WAL records while preserving active history.
    pub fn checkpoint_and_truncate_wal(&mut self) -> Result<crate::wal::lsn::Lsn> {
        self.db.checkpoint_and_truncate_wal()
    }

    /// Validates the structural invariants of the B+ Tree.
    pub fn validate_tree(&mut self) -> Result<()> {
        self.db.validate_tree()
    }

    /// Returns a mutable reference to the underlying `BufferPool`.
    pub fn buffer_pool_mut(&mut self) -> &mut crate::storage::buffer_pool::BufferPool {
        self.db.buffer_pool_mut()
    }

    /// Returns diagnostic plan cache statistics.
    pub fn plan_cache_stats(&self) -> crate::query::plan_cache::PlanCacheStats {
        self.db.plan_cache_stats()
    }

    /// Returns a reference to the physical plan cache.
    pub fn plan_cache(&self) -> &crate::query::PlanCache {
        self.db.plan_cache()
    }

    /// Returns a mutable reference to the physical plan cache.
    pub fn plan_cache_mut(&mut self) -> &mut crate::query::PlanCache {
        self.db.plan_cache_mut()
    }

    /// Returns the current catalog generation.
    pub fn catalog_generation(&self) -> u64 {
        self.db.catalog_generation
    }

    /// Returns the current index generation.
    pub fn index_generation(&self) -> u64 {
        self.db.index_generation
    }

    /// Returns the current statistics generation.
    pub fn statistics_generation(&self) -> u64 {
        self.db.statistics_generation
    }

    /// Clears all entries from the physical plan cache.
    pub fn clear_plan_cache(&mut self) {
        self.db.clear_plan_cache();
    }

    /// Returns a reference to the runtime cardinality feedback registry.
    pub fn feedback_registry(&self) -> &crate::query::FeedbackRegistry {
        self.db.feedback_registry()
    }

    /// Returns a mutable reference to the runtime cardinality feedback registry.
    pub fn feedback_registry_mut(&mut self) -> &mut crate::query::FeedbackRegistry {
        self.db.feedback_registry_mut()
    }

    /// Returns the deterministic resource planning signature for current memory configurations.
    pub fn resource_planning_signature(&self) -> crate::query::ResourcePlanningSignature {
        self.db.resource_planning_signature()
    }

    /// Clears all runtime cardinality feedback entries.
    pub fn clear_feedback(&mut self) {
        self.db.clear_feedback();
    }

    /// Clears runtime cardinality feedback entries for the specified table.
    pub fn clear_table_feedback(&mut self, table_name: &str) {
        self.db.clear_table_feedback(table_name);
    }

    /// Checks whether a cached physical plan has accumulated runtime feedback crossing the replan threshold.
    pub fn check_plan_cache_should_replan(
        &self,
        plan: &crate::query::optimizer::PhysicalPlan,
        param_sig: Option<crate::query::ParamPlanningSignature>,
    ) -> bool {
        let stats_gen = self.db.statistics_generation;
        let res_sig = self.resource_planning_signature();

        // 0. Check fingerprint-specific feedback entry
        let fp_key = crate::query::FeedbackKey::new(
            crate::query::FeedbackContext::Scan {
                table_name: plan.base_table.table_name.clone(),
                predicate_repr: plan
                    .base_table
                    .residual_filter
                    .as_ref()
                    .map(|f| f.to_string())
                    .unwrap_or_default(),
                access_path_repr: plan.base_table.access_path.to_string(),
            },
            param_sig.clone(),
            stats_gen,
        )
        .with_resource_signature(Some(res_sig.clone()))
        .with_physical_fingerprint(Some(plan.fingerprint()));
        if let Some(entry) = self.db.feedback_registry.get(&fp_key) {
            if entry.should_replan() {
                return true;
            }
        }

        // 1. Check base table scan
        let scan_key = crate::query::FeedbackKey::scan(
            &plan.base_table.table_name,
            plan.base_table.access_path.to_string(),
            plan.base_table
                .residual_filter
                .as_ref()
                .map(|f| f.to_string())
                .unwrap_or_default(),
            param_sig.clone(),
            stats_gen,
        )
        .with_resource_signature(Some(res_sig.clone()));
        if let Some(entry) = self.db.feedback_registry.get(&scan_key) {
            if entry.should_replan() {
                return true;
            }
        }

        if let Some(rg) = &plan.base_table.row_goal {
            let index_name = match &plan.base_table.access_path {
                crate::query::optimizer::AccessPath::SecondaryIndexScan { index_name, .. }
                | crate::query::optimizer::AccessPath::IndexOnlyScan { index_name, .. } => {
                    index_name.clone()
                }
                crate::query::optimizer::AccessPath::PrimaryRangeScan { .. }
                | crate::query::optimizer::AccessPath::FullTableScan => "PRIMARY".to_string(),
                _ => String::new(),
            };
            if !index_name.is_empty() {
                let filter_str = plan
                    .base_table
                    .residual_filter
                    .as_ref()
                    .map(|f| f.to_string())
                    .unwrap_or_default();
                let ord_key = crate::query::FeedbackKey::ordered_scan(
                    &plan.base_table.table_name,
                    &index_name,
                    filter_str,
                    rg.k(),
                    param_sig.clone(),
                    stats_gen,
                )
                .with_resource_signature(Some(res_sig.clone()));
                if let Some(entry) = self.db.feedback_registry.get(&ord_key) {
                    if entry.should_replan() {
                        return true;
                    }
                }
            }
        }

        // 2. Check joins
        let mut left_tbl = plan.base_table.table_name.clone();
        for join in &plan.joins {
            let join_key = if join.join_type == crate::sql::JoinType::Semi
                || join.join_type == crate::sql::JoinType::Anti
            {
                crate::query::FeedbackKey::semi_anti(
                    &left_tbl,
                    &join.right.table_name,
                    &join.on,
                    join.join_type,
                    param_sig.clone(),
                    stats_gen,
                )
                .with_resource_signature(Some(res_sig.clone()))
            } else {
                crate::query::FeedbackKey::join(
                    &left_tbl,
                    &join.right.table_name,
                    &join.on,
                    join.join_type.to_string(),
                    param_sig.clone(),
                    stats_gen,
                )
                .with_resource_signature(Some(res_sig.clone()))
            };
            if let Some(entry) = self.db.feedback_registry.get(&join_key) {
                if entry.should_replan() {
                    return true;
                }
            }
            left_tbl = join.right.table_name.clone();
        }

        // 3. Check sort
        if !plan.order_by.is_empty() {
            let sort_key = crate::query::FeedbackKey::new(
                crate::query::FeedbackContext::Sort {
                    order_by: plan
                        .order_by
                        .iter()
                        .map(|k| k.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                },
                param_sig.clone(),
                stats_gen,
            )
            .with_resource_signature(Some(res_sig.clone()));
            if let Some(entry) = self.db.feedback_registry.get(&sort_key) {
                if entry.should_replan() {
                    return true;
                }
            }
        }

        // 4. Check aggregation
        if !plan.group_by.is_empty() || !plan.aggregates.is_empty() {
            let agg_key = crate::query::FeedbackKey::new(
                crate::query::FeedbackContext::Aggregate {
                    group_by: plan
                        .group_by
                        .iter()
                        .map(|g| g.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    aggregates: plan
                        .aggregates
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                },
                param_sig.clone(),
                stats_gen,
            )
            .with_resource_signature(Some(res_sig.clone()));
            if let Some(entry) = self.db.feedback_registry.get(&agg_key) {
                if entry.should_replan() {
                    return true;
                }
            }
        }

        false
    }

    /// Returns the active adaptive plan selection configuration.
    pub fn adaptive_config(&self) -> &crate::query::lifecycle::AdaptiveSelectionConfig {
        &self.db.adaptive_config
    }

    /// Sets the active adaptive plan selection configuration.
    pub fn set_adaptive_config(
        &mut self,
        config: crate::query::lifecycle::AdaptiveSelectionConfig,
    ) {
        self.db.adaptive_config = config;
    }

    /// Plans a query into a bounded candidate set, validates all alternatives, and executes adaptive selection.
    pub fn select_adaptive_plan(
        &mut self,
        ast: &crate::sql::SelectStatement,
        current_plan_fingerprint: Option<&crate::query::fingerprint::PhysicalPlanFingerprint>,
        params: Option<&[Value]>,
    ) -> Result<crate::query::lifecycle::AdaptiveSelectionResult> {
        self.ensure_active()?;
        let val_ctx = self.create_validation_context()?;
        let param_sig = params.map(crate::query::ParamPlanningSignature::from_values);
        let res_sig = self.resource_planning_signature();
        let stats_gen = self.db.statistics_generation;

        // Generate primary plan
        let primary_plan = self.plan_select_with_params(ast, params)?;
        let primary_cost = primary_plan.total_cost;
        let primary_rows = primary_plan.estimated_rows;
        let primary_props = crate::query::physical_properties::PhysicalProperties {
            ordering: primary_plan.provided_ordering.clone(),
            required_columns: None,
            row_goal: primary_plan.row_goal,
        };

        let mut candidate_set =
            crate::query::lifecycle::PlanCandidateSet::new(self.db.adaptive_config.max_candidates);

        let primary_cand = crate::query::lifecycle::PlanCandidate::new(
            primary_plan.clone(),
            primary_cost,
            primary_rows,
            primary_props,
            format!("{:?}", primary_plan.base_table.access_path),
        )
        .with_resource_signature(Some(res_sig.clone()));
        candidate_set.add_candidate(primary_cand);

        // If single table, also add candidate alternatives for all valid access paths
        let table_name = &primary_plan.base_table.table_name;
        if primary_plan.joins.is_empty() {
            if let Ok((table_schema, indexes, is_cte)) =
                self.resolve_table_for_planning(table_name, &[])
            {
                let stats = if is_cte {
                    None
                } else {
                    self.db.get_statistics(table_name).cloned()
                };
                let table_info = crate::query::optimizer::TableOptInfo::new(
                    table_name.clone(),
                    primary_plan.base_table.table_alias.clone(),
                    table_schema,
                    indexes,
                )
                .with_cte(is_cte)
                .with_stats(stats);

                let bound_select = crate::sql::binder::bind_select_multi_with_catalog_and_params(
                    ast,
                    &[crate::sql::TableBinding::new(
                        table_name,
                        primary_plan.base_table.table_alias.as_deref(),
                        &table_info.schema,
                    )],
                    None,
                    params,
                );

                if let Ok(bound) = bound_select {
                    if let Ok(cands) = crate::query::optimizer::Optimizer::generate_candidate_plans(
                        &bound,
                        &[table_info],
                        Some(&self.db.feedback_registry),
                        param_sig.as_ref(),
                        Some(&res_sig),
                        stats_gen,
                        self.db.adaptive_config.max_candidates,
                    ) {
                        for cand in cands.candidates() {
                            candidate_set.add_candidate(cand.clone());
                        }
                    }
                }
            }
        }

        // Validate all candidates
        candidate_set.validate_all(&val_ctx);

        // Run adaptive plan selection
        let mut selector =
            crate::query::lifecycle::AdaptivePlanSelector::new(self.db.adaptive_config.clone());
        let res = selector
            .select(
                &candidate_set,
                current_plan_fingerprint,
                Some(&self.db.feedback_registry),
                param_sig.as_ref(),
                Some(&res_sig),
                stats_gen,
                Some(&val_ctx),
            )
            .map_err(Error::from)?;

        Ok(res)
    }

    /// Generates deterministic adaptive selection diagnostics for a query string.
    pub fn explain_adaptive_plan(&mut self, sql: &str) -> Result<String> {
        let stmt = crate::sql::parse_statement(sql)?;
        match stmt {
            crate::sql::Statement::Select(select_stmt) => {
                let res = self.select_adaptive_plan(&select_stmt, None, None)?;
                let val_ctx = self.create_validation_context()?;
                let plan_diag = crate::query::PhysicalPlanValidator::format_validation_diagnostics(
                    res.selected_plan.plan(),
                    &val_ctx,
                );
                Ok(format!("{}\n\n{}", res.diagnostics, plan_diag))
            }
            _ => Err(Error::UnsupportedExplainTarget(sql.to_string())),
        }
    }

    /// Records execution feedback observations for a completed physical plan execution.
    pub fn record_plan_execution_feedback(
        &mut self,
        plan: &crate::query::optimizer::PhysicalPlan,
        root_op: &crate::query::OperatorMetrics,
        param_sig: Option<crate::query::ParamPlanningSignature>,
    ) {
        let stats_gen = self.db.statistics_generation;
        let res_sig = self.resource_planning_signature();

        // 1. Base Table Scan
        let base_scan_metric = root_op.find_first(&|m| match &m.kind {
            crate::query::OperatorKind::TableScan { table_name, .. } => {
                table_name == &plan.base_table.table_name
            }
            _ => false,
        });

        if let Some(scan_metric) = base_scan_metric {
            let key = crate::query::FeedbackKey::scan(
                &plan.base_table.table_name,
                plan.base_table.access_path.to_string(),
                plan.base_table
                    .residual_filter
                    .as_ref()
                    .map(|f| f.to_string())
                    .unwrap_or_default(),
                param_sig.clone(),
                stats_gen,
            )
            .with_resource_signature(Some(res_sig.clone()));
            let mut obs = crate::query::FeedbackObservation::new(
                plan.base_table.estimated_rows,
                scan_metric.actual_rows as f64,
            );
            obs.estimated_cost = Some(plan.base_table.cost);
            obs.input_rows = Some(scan_metric.input_rows);
            obs.output_rows = Some(scan_metric.actual_rows);
            if scan_metric.counters.distinct_prefix_values > 0 {
                obs.actual_distinct_prefixes = Some(scan_metric.counters.distinct_prefix_values);
            }
            let did_spill =
                scan_metric.counters.bytes_spilled > 0 || scan_metric.counters.runs_created > 0;
            if scan_metric.counters.indexes_used > 0
                || scan_metric.counters.candidate_rows_after_set_operation > 0
                || did_spill
                || scan_metric.counters.peak_memory_bytes > 0
            {
                obs.actual_candidates =
                    Some(scan_metric.counters.candidate_rows_after_set_operation);
                obs.spill_occurred = Some(did_spill);
            }
            let scan_res_obs = crate::query::ResourceObservation {
                memory_budget_bytes: res_sig.multi_index_budget,
                estimated_peak_memory_bytes: 0,
                actual_peak_memory_bytes: scan_metric.counters.peak_memory_bytes,
                estimated_spill_bytes: 0,
                actual_spill_bytes: scan_metric.counters.bytes_spilled,
                spill_runs: scan_metric.counters.runs_created,
                temporary_bytes: scan_metric.counters.bytes_spilled,
                temporary_files: scan_metric.counters.runs_created,
                execution_time_us: scan_metric.execution_time.as_micros() as u64,
                did_spill,
                is_clamped: false,
            };
            obs.resource = Some(scan_res_obs);
            self.db.feedback_registry.record(key, obs);

            if let Some(rg) = &plan.base_table.row_goal {
                let index_name = match &plan.base_table.access_path {
                    crate::query::optimizer::AccessPath::SecondaryIndexScan {
                        index_name, ..
                    }
                    | crate::query::optimizer::AccessPath::IndexOnlyScan { index_name, .. } => {
                        index_name.clone()
                    }
                    crate::query::optimizer::AccessPath::PrimaryRangeScan { .. }
                    | crate::query::optimizer::AccessPath::FullTableScan => "PRIMARY".to_string(),
                    _ => String::new(),
                };
                if !index_name.is_empty() {
                    let filter_str = plan
                        .base_table
                        .residual_filter
                        .as_ref()
                        .map(|f| f.to_string())
                        .unwrap_or_default();
                    let ord_key = crate::query::FeedbackKey::ordered_scan(
                        &plan.base_table.table_name,
                        &index_name,
                        filter_str,
                        rg.k(),
                        param_sig.clone(),
                        stats_gen,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let examined = scan_metric
                        .counters
                        .rows_examined
                        .max(scan_metric.counters.index_entries_examined)
                        as f64;
                    let qualified = scan_metric
                        .counters
                        .rows_qualified
                        .max(scan_metric.actual_rows) as f64;
                    let mut ord_obs = crate::query::FeedbackObservation::new(
                        plan.base_table
                            .estimated_rows_examined
                            .unwrap_or(plan.base_table.estimated_rows),
                        examined,
                    );
                    ord_obs.input_rows = Some(examined as usize);
                    ord_obs.output_rows = Some(qualified as usize);
                    self.db.feedback_registry.record(ord_key, ord_obs);
                }
            }
        }

        // 2. Joins
        let mut left_table_name = plan.base_table.table_name.clone();
        for join in &plan.joins {
            let right_table_name = join.right.table_name.clone();

            let right_scan_metric = root_op.find_first(&|m| match &m.kind {
                crate::query::OperatorKind::TableScan { table_name, .. } => {
                    table_name == &right_table_name
                }
                _ => false,
            });
            if let Some(r_metric) = right_scan_metric {
                let r_key = crate::query::FeedbackKey::scan(
                    &right_table_name,
                    join.right.access_path.to_string(),
                    join.right
                        .residual_filter
                        .as_ref()
                        .map(|f| f.to_string())
                        .unwrap_or_default(),
                    param_sig.clone(),
                    stats_gen,
                )
                .with_resource_signature(Some(res_sig.clone()));
                let mut r_obs = crate::query::FeedbackObservation::new(
                    join.right.estimated_rows,
                    r_metric.actual_rows as f64,
                );
                r_obs.estimated_cost = Some(join.right.cost);
                r_obs.input_rows = Some(r_metric.input_rows);
                r_obs.output_rows = Some(r_metric.actual_rows);
                let did_spill =
                    r_metric.counters.bytes_spilled > 0 || r_metric.counters.runs_created > 0;
                let r_res_obs = crate::query::ResourceObservation {
                    memory_budget_bytes: res_sig.multi_index_budget,
                    estimated_peak_memory_bytes: 0,
                    actual_peak_memory_bytes: r_metric.counters.peak_memory_bytes,
                    estimated_spill_bytes: 0,
                    actual_spill_bytes: r_metric.counters.bytes_spilled,
                    spill_runs: r_metric.counters.runs_created,
                    temporary_bytes: r_metric.counters.bytes_spilled,
                    temporary_files: r_metric.counters.runs_created,
                    execution_time_us: r_metric.execution_time.as_micros() as u64,
                    did_spill,
                    is_clamped: false,
                };
                r_obs.resource = Some(r_res_obs);
                self.db.feedback_registry.record(r_key, r_obs);
            }

            let join_op_metric = root_op.find_first(&|m| {
                matches!(
                    &m.kind,
                    crate::query::OperatorKind::NestedLoopJoin { .. }
                        | crate::query::OperatorKind::NestedLoopSemiJoin
                        | crate::query::OperatorKind::NestedLoopAntiJoin
                        | crate::query::OperatorKind::HashJoin { .. }
                        | crate::query::OperatorKind::HashSemiJoin
                        | crate::query::OperatorKind::HashAntiJoin
                        | crate::query::OperatorKind::ExternalHashJoin { .. }
                )
            });
            if let Some(j_metric) = join_op_metric {
                let j_key = if join.join_type == crate::sql::JoinType::Semi
                    || join.join_type == crate::sql::JoinType::Anti
                {
                    crate::query::FeedbackKey::semi_anti(
                        &left_table_name,
                        &right_table_name,
                        &join.on,
                        join.join_type,
                        param_sig.clone(),
                        stats_gen,
                    )
                    .with_resource_signature(Some(res_sig.clone()))
                } else {
                    crate::query::FeedbackKey::join(
                        &left_table_name,
                        &right_table_name,
                        &join.on,
                        join.join_type.to_string(),
                        param_sig.clone(),
                        stats_gen,
                    )
                    .with_resource_signature(Some(res_sig.clone()))
                };
                let mut j_obs = crate::query::FeedbackObservation::new(
                    join.estimated_rows,
                    j_metric.actual_rows as f64,
                );
                j_obs.estimated_cost = Some(join.cost);
                j_obs.input_rows = Some(j_metric.input_rows);
                j_obs.output_rows = Some(j_metric.actual_rows);
                j_obs.left_rows = Some(j_metric.counters.rows_left);
                j_obs.right_rows = Some(j_metric.counters.rows_right);
                let did_spill =
                    j_metric.counters.bytes_spilled > 0 || j_metric.counters.runs_created > 0;
                if did_spill {
                    j_obs.spill_occurred = Some(true);
                }
                let join_res_obs = crate::query::ResourceObservation {
                    memory_budget_bytes: res_sig.join_budget,
                    estimated_peak_memory_bytes: 0,
                    actual_peak_memory_bytes: j_metric.counters.peak_memory_bytes,
                    estimated_spill_bytes: 0,
                    actual_spill_bytes: j_metric.counters.bytes_spilled,
                    spill_runs: j_metric.counters.runs_created,
                    temporary_bytes: j_metric.counters.bytes_spilled,
                    temporary_files: j_metric.counters.runs_created,
                    execution_time_us: j_metric.execution_time.as_micros() as u64,
                    did_spill,
                    is_clamped: false,
                };
                j_obs.resource = Some(join_res_obs);
                self.db.feedback_registry.record(j_key, j_obs);

                if let Some(rg) = &join.row_goal {
                    let jrg_key = crate::query::FeedbackKey::join_row_goal(
                        join.join_type.to_string(),
                        join.strategy.to_string(),
                        left_table_name.clone(),
                        right_table_name.clone(),
                        join.on.to_string(),
                        rg.k(),
                        param_sig.clone(),
                        stats_gen,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let mut jrg_obs = crate::query::FeedbackObservation::new(
                        join.estimated_rows_examined.unwrap_or(join.estimated_rows),
                        (j_metric
                            .counters
                            .rows_examined
                            .max(j_metric.counters.rows_left)) as f64,
                    );
                    jrg_obs.input_rows = Some(j_metric.input_rows);
                    jrg_obs.output_rows = Some(j_metric.actual_rows);
                    jrg_obs.left_rows = Some(j_metric.counters.rows_left);
                    jrg_obs.right_rows = Some(j_metric.counters.rows_right);
                    self.db.feedback_registry.record(jrg_key, jrg_obs);
                }

                if j_metric.counters.runtime_filter_enabled {
                    let rf_key = crate::query::FeedbackKey::runtime_filter(
                        &left_table_name,
                        &right_table_name,
                        &join.on,
                        join.join_type.to_string(),
                        param_sig.clone(),
                        stats_gen,
                    )
                    .with_resource_signature(Some(res_sig.clone()));
                    let mut rf_obs = crate::query::FeedbackObservation::new(
                        j_metric.counters.runtime_filter_probe_rows as f64,
                        j_metric.counters.runtime_filter_passed_rows as f64,
                    );
                    rf_obs.input_rows = Some(j_metric.counters.runtime_filter_probe_rows);
                    rf_obs.output_rows = Some(j_metric.counters.runtime_filter_passed_rows);
                    let rf_res = crate::query::ResourceObservation {
                        memory_budget_bytes: res_sig.join_budget,
                        estimated_peak_memory_bytes: 0,
                        actual_peak_memory_bytes: j_metric.counters.runtime_filter_memory_bytes,
                        estimated_spill_bytes: 0,
                        actual_spill_bytes: 0,
                        spill_runs: 0,
                        temporary_bytes: 0,
                        temporary_files: 0,
                        execution_time_us: 0,
                        did_spill: false,
                        is_clamped: false,
                    };
                    rf_obs.resource = Some(rf_res);
                    self.db.feedback_registry.record(rf_key, rf_obs);
                }
            }

            left_table_name = right_table_name;
        }

        // 3. Sort
        let sort_metric =
            root_op.find_first(&|m| matches!(&m.kind, crate::query::OperatorKind::Sort));
        if let Some(s_metric) = sort_metric {
            if !plan.order_by.is_empty() {
                let sort_key = crate::query::FeedbackKey::new(
                    crate::query::FeedbackContext::Sort {
                        order_by: plan
                            .order_by
                            .iter()
                            .map(|k| k.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    },
                    param_sig.clone(),
                    stats_gen,
                )
                .with_resource_signature(Some(res_sig.clone()));
                let mut s_obs = crate::query::FeedbackObservation::new(
                    plan.estimated_rows,
                    s_metric.actual_rows as f64,
                );
                s_obs.input_rows = Some(s_metric.input_rows);
                s_obs.output_rows = Some(s_metric.actual_rows);
                let did_spill =
                    s_metric.counters.runs_created > 0 || s_metric.counters.bytes_spilled > 0;
                s_obs.spill_occurred = Some(did_spill);
                let sort_res_obs = crate::query::ResourceObservation {
                    memory_budget_bytes: res_sig.sort_budget,
                    estimated_peak_memory_bytes: 0,
                    actual_peak_memory_bytes: s_metric.counters.peak_memory_bytes,
                    estimated_spill_bytes: 0,
                    actual_spill_bytes: s_metric.counters.bytes_spilled,
                    spill_runs: s_metric.counters.runs_created,
                    temporary_bytes: s_metric.counters.bytes_spilled,
                    temporary_files: s_metric.counters.runs_created,
                    execution_time_us: s_metric.execution_time.as_micros() as u64,
                    did_spill,
                    is_clamped: false,
                };
                s_obs.resource = Some(sort_res_obs);
                self.db.feedback_registry.record(sort_key, s_obs);
            }
        }

        // 4. Aggregation
        let agg_metric =
            root_op.find_first(&|m| matches!(&m.kind, crate::query::OperatorKind::Aggregate));
        if let Some(a_metric) = agg_metric {
            if !plan.group_by.is_empty() || !plan.aggregates.is_empty() {
                let agg_key = crate::query::FeedbackKey::new(
                    crate::query::FeedbackContext::Aggregate {
                        group_by: plan
                            .group_by
                            .iter()
                            .map(|g| g.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                        aggregates: plan
                            .aggregates
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    },
                    param_sig.clone(),
                    stats_gen,
                )
                .with_resource_signature(Some(res_sig.clone()));
                let mut a_obs = crate::query::FeedbackObservation::new(
                    plan.estimated_rows,
                    a_metric.actual_rows as f64,
                );
                a_obs.input_rows = Some(a_metric.input_rows);
                a_obs.output_rows = Some(a_metric.actual_rows);
                let did_spill =
                    a_metric.counters.runs_created > 0 || a_metric.counters.bytes_spilled > 0;
                a_obs.spill_occurred = Some(did_spill);
                let agg_res_obs = crate::query::ResourceObservation {
                    memory_budget_bytes: res_sig.agg_budget,
                    estimated_peak_memory_bytes: 0,
                    actual_peak_memory_bytes: a_metric.counters.peak_memory_bytes,
                    estimated_spill_bytes: 0,
                    actual_spill_bytes: a_metric.counters.bytes_spilled,
                    spill_runs: a_metric.counters.runs_created,
                    temporary_bytes: a_metric.counters.bytes_spilled,
                    temporary_files: a_metric.counters.runs_created,
                    execution_time_us: a_metric.execution_time.as_micros() as u64,
                    did_spill,
                    is_clamped: false,
                };
                a_obs.resource = Some(agg_res_obs);
                self.db.feedback_registry.record(agg_key, a_obs);
            }
        }

        // 5. Materialization
        for cte in &plan.ctes {
            if let crate::sql::BoundCteKind::NonRecursive {
                plan: inner_plan,
                decision,
                consumer_count,
                ..
            } = &cte.kind
            {
                if let Some(mat_metric) = root_op.find_first(&|m| match &m.kind {
                    crate::query::OperatorKind::Materialize { name, .. } => {
                        name.as_deref() == Some(cte.name.as_str())
                    }
                    _ => false,
                }) {
                    let fp = crate::query::materialize::SubplanFingerprint::from_plan(inner_plan)
                        .to_hex();
                    let mat_key = crate::query::FeedbackKey::materialization(
                        fp,
                        decision.to_string(),
                        *consumer_count,
                        param_sig.clone(),
                        stats_gen,
                    )
                    .with_resource_signature(Some(res_sig.clone()));

                    let mut mat_obs = crate::query::FeedbackObservation::new(
                        inner_plan.estimated_rows,
                        mat_metric.actual_rows as f64,
                    );
                    mat_obs.input_rows = Some(mat_metric.input_rows);
                    mat_obs.output_rows = Some(mat_metric.actual_rows);
                    let did_spill = mat_metric.counters.materialize_spill_bytes > 0;
                    mat_obs.spill_occurred = Some(did_spill);
                    let mat_res = crate::query::ResourceObservation {
                        memory_budget_bytes: self.db.materialize_config.memory_budget_bytes,
                        estimated_peak_memory_bytes: mat_metric.counters.bytes_materialized,
                        actual_peak_memory_bytes: mat_metric.counters.peak_memory_bytes,
                        estimated_spill_bytes: 0,
                        actual_spill_bytes: mat_metric.counters.materialize_spill_bytes,
                        spill_runs: mat_metric.counters.materialize_spill_files,
                        temporary_bytes: mat_metric.counters.materialize_spill_bytes,
                        temporary_files: mat_metric.counters.materialize_spill_files,
                        execution_time_us: mat_metric.execution_time.as_micros() as u64,
                        did_spill,
                        is_clamped: false,
                    };
                    mat_obs.resource = Some(mat_res);
                    self.db.feedback_registry.record(mat_key, mat_obs);
                }
            }
        }
    }

    /// Validates all accumulated write intents against the latest committed database state
    /// prior to applying the commit.
    ///
    /// Verifies:
    /// 1. Transaction state is `Active`.
    /// 2. No operation within this transaction has encountered a conflict.
    /// 3. Each targeted logical row has not been superseded or modified by an intervening transaction.
    /// 4. Unique constraints are satisfied by the final transaction state against the committed state.
    /// 5. Record store version chains and index pointers remain structurally consistent.
    pub fn validate_commit(&mut self) -> Result<()> {
        if self.state != TxnState::Active {
            return Err(Error::TransactionConflict(
                crate::engine::isolation::TransactionConflict::InvalidTransactionState {
                    txn_id: self.txn_id,
                    current: format!("{}", self.state),
                    action: "validate_commit".to_string(),
                },
            ));
        }

        if let Some(conflict) = self.write_set.conflict() {
            return Err(Error::TransactionConflict(
                crate::engine::isolation::TransactionConflict::WriteSetValidationFailure {
                    txn_id: self.txn_id,
                    reason: format!(
                        "transaction encountered conflict during operation: {conflict}"
                    ),
                },
            ));
        }

        let intents: Vec<crate::engine::isolation::RowWriteIntent> =
            self.write_set.intents().values().cloned().collect();

        for intent in intents {
            if intent.table == "__kv__" {
                match intent.op_kind {
                    crate::engine::isolation::WriteOpKind::Insert => {
                        if let Some(target) = intent.target_version {
                            if let Ok(Some((target_meta, _))) = self
                                .db
                                .record_store
                                .read_version(&mut self.db.buffer_pool, target)
                            {
                                if let Some(del_seq) = target_meta.deleted_at {
                                    if del_seq.0 > intent.snapshot_commit_seq.0 {
                                        return Err(Error::TransactionConflict(
                                            crate::engine::isolation::TransactionConflict::ConcurrentLogicalKeyChange {
                                                table: intent.table,
                                                key: intent.key_repr,
                                                detail: "target row was deleted by intervening transaction"
                                                    .to_string(),
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    crate::engine::isolation::WriteOpKind::Update => {
                        if let Some(target) = intent.target_version {
                            if let Ok(Some((target_meta, _))) = self
                                .db
                                .record_store
                                .read_version(&mut self.db.buffer_pool, target)
                            {
                                if let Some(del_seq) = target_meta.deleted_at {
                                    if del_seq.0 > intent.snapshot_commit_seq.0 {
                                        return Err(Error::TransactionConflict(
                                            crate::engine::isolation::TransactionConflict::StaleVersionWrite {
                                                table: intent.table,
                                                key: intent.key_repr,
                                                target_version: target,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    crate::engine::isolation::WriteOpKind::Delete => {
                        if let Some(target) = intent.target_version {
                            if let Ok(Some((target_meta, _))) = self
                                .db
                                .record_store
                                .read_version(&mut self.db.buffer_pool, target)
                            {
                                if let Some(del_seq) = target_meta.deleted_at {
                                    if del_seq.0 > intent.snapshot_commit_seq.0 {
                                        return Err(Error::TransactionConflict(
                                            crate::engine::isolation::TransactionConflict::StaleVersionWrite {
                                                table: intent.table,
                                                key: intent.key_repr,
                                                target_version: target,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                let tbl_meta = match crate::engine::catalog::Catalog::get_table(
                    &mut self.db.buffer_pool,
                    &intent.table,
                )? {
                    Some(m) => m,
                    None => continue,
                };

                let idx_metas = crate::engine::catalog::Catalog::list_indexes_for_table(
                    &mut self.db.buffer_pool,
                    tbl_meta.table_id,
                )?;

                match intent.op_kind {
                    crate::engine::isolation::WriteOpKind::Insert
                    | crate::engine::isolation::WriteOpKind::Update => {
                        if let Some(new_rid) = intent.new_version {
                            if let Ok(Some((_, bytes))) = self
                                .db
                                .record_store
                                .read_version(&mut self.db.buffer_pool, new_rid)
                            {
                                if let Ok(row) = tbl_meta.schema.decode_tuple(&bytes) {
                                    for idx in &idx_metas {
                                        if idx.unique {
                                            let sec_tree =
                                                crate::index::secondary::SecondaryIndex::from_root(
                                                    idx.root_page_id,
                                                );
                                            let idx_key = idx.extract_index_key(&row);
                                            let matching_rids = sec_tree
                                                .lookup(&mut self.db.buffer_pool, &idx_key)?;
                                            for r in matching_rids {
                                                if r != new_rid && intent.target_version != Some(r)
                                                {
                                                    if let Ok(Some((r_meta, _))) = self
                                                        .db
                                                        .record_store
                                                        .read_version(&mut self.db.buffer_pool, r)
                                                    {
                                                        if r_meta.deleted_at.is_none()
                                                            && r_meta.xmax != Some(self.txn_id)
                                                        {
                                                            let vals_str = idx
                                                                .column_indices
                                                                .iter()
                                                                .map(|&i| {
                                                                    format!("{}", row.values[i])
                                                                })
                                                                .collect::<Vec<_>>()
                                                                .join(", ");
                                                            let key_repr =
                                                                if idx.column_indices.len() == 1 {
                                                                    vals_str
                                                                } else {
                                                                    format!("({vals_str})")
                                                                };
                                                            return Err(Error::TransactionConflict(
                                                                crate::engine::isolation::TransactionConflict::UniqueIndexConflict {
                                                                    index_name: idx.name.clone(),
                                                                    key: key_repr,
                                                                },
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    crate::engine::isolation::WriteOpKind::Delete => {
                        if let Some(target) = intent.target_version {
                            if let Ok(Some((target_meta, _))) = self
                                .db
                                .record_store
                                .read_version(&mut self.db.buffer_pool, target)
                            {
                                if let Some(del_seq) = target_meta.deleted_at {
                                    if del_seq.0 > intent.snapshot_commit_seq.0 {
                                        return Err(Error::TransactionConflict(
                                            crate::engine::isolation::TransactionConflict::StaleVersionWrite {
                                                table: intent.table,
                                                key: intent.key_repr,
                                                target_version: target,
                                            },
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Commits all mutations performed in this transaction, flushing the WAL through the commit LSN
    /// and allocating a monotonic logical commit sequence.
    pub fn commit(mut self) -> Result<()> {
        self.commit_in_place()
    }

    /// In-place commit validating lifecycle state transitions without consuming `self`.
    pub fn commit_in_place(&mut self) -> Result<()> {
        match self.state {
            TxnState::Active => {}
            TxnState::Committed => {
                return Err(Error::InvalidTransactionState {
                    current: "Committed".to_string(),
                    action: "commit".to_string(),
                });
            }
            TxnState::Aborted => {
                return Err(Error::InvalidTransactionState {
                    current: "Aborted".to_string(),
                    action: "commit".to_string(),
                });
            }
            TxnState::Created => {
                return Err(Error::InvalidTransactionState {
                    current: "Created".to_string(),
                    action: "commit".to_string(),
                });
            }
        }

        // Commit validation stage
        if let Err(err) = self.validate_commit() {
            let _ = self.abort_internal();
            return Err(err);
        }

        let commit_seq = self.db.allocate_commit_seq();

        // Stamp MVCC version metadata BEFORE calling commit_txn_with_seq.
        // commit_txn_with_seq clears active_txn from the buffer pool; any page writes
        // after that point won't generate WAL page-delta records, making them
        // invisible to ARIES redo after a crash. By writing metadata first (while
        // active_txn is still set), unpin_page(dirty=true) will WAL-log the changes.
        for &rid in self.write_set.inserted_records() {
            if let Ok(Some((mut meta, _))) = self
                .db
                .record_store
                .read_version(&mut self.db.buffer_pool, rid)
            {
                meta.created_at = Some(commit_seq);
                let _ = self.db.record_store.update_version_metadata(
                    &mut self.db.buffer_pool,
                    rid,
                    &meta,
                );
            }
        }
        for &old_rid in self.write_set.updated_records() {
            if let Ok(Some((mut meta, _))) = self
                .db
                .record_store
                .read_version(&mut self.db.buffer_pool, old_rid)
            {
                meta.deleted_at = Some(commit_seq);
                let _ = self.db.record_store.update_version_metadata(
                    &mut self.db.buffer_pool,
                    old_rid,
                    &meta,
                );
            }
        }
        for &old_rid in self.write_set.deleted_records() {
            if let Ok(Some((mut meta, _))) = self
                .db
                .record_store
                .read_version(&mut self.db.buffer_pool, old_rid)
            {
                meta.deleted_at = Some(commit_seq);
                let _ = self.db.record_store.update_version_metadata(
                    &mut self.db.buffer_pool,
                    old_rid,
                    &meta,
                );
            }
        }

        // Now commit (WAL flush): active_txn is cleared here, after metadata is written.
        let commit_res = self.db.commit_txn_with_seq(self.txn_id, commit_seq.0);
        match commit_res {
            Ok(_) => {
                if let Some(owned_id) = self.owned_snapshot_id.take() {
                    self.db.snapshot_registry.release(owned_id);
                }
                self.state = TxnState::Committed;
                if self.has_catalog_mutations {
                    self.db.catalog_generation += 1;
                    self.db.plan_cache.invalidate_all();
                }
                for &rid in self.write_set.inserted_records() {
                    self.db.visibility_map.invalidate_page(rid.page_id);
                }
                for &old_rid in self.write_set.updated_records() {
                    self.db.visibility_map.invalidate_record(old_rid);
                    self.db.visibility_map.invalidate_page(old_rid.page_id);
                }
                for &old_rid in self.write_set.deleted_records() {
                    self.db.visibility_map.invalidate_record(old_rid);
                    self.db.visibility_map.invalidate_page(old_rid.page_id);
                }
                Ok(())
            }
            Err(Error::InjectedFault(
                crate::engine::failure::StorageFailurePoint::AfterDurableCommitRecordFlush,
            )) => {
                if let Some(owned_id) = self.owned_snapshot_id.take() {
                    self.db.snapshot_registry.release(owned_id);
                }
                self.state = TxnState::Committed;
                if self.has_catalog_mutations {
                    self.db.catalog_generation += 1;
                    self.db.plan_cache.invalidate_all();
                }
                Err(Error::InjectedFault(
                    crate::engine::failure::StorageFailurePoint::AfterDurableCommitRecordFlush,
                ))
            }
            Err(e) => Err(e),
        }
    }

    /// Aborts all mutations performed in this transaction, rolling back physical page changes
    /// with Compensation Log Records (CLRs) and restoring pre-transaction database state.
    pub fn abort(mut self) -> Result<()> {
        self.abort_in_place()
    }

    /// In-place abort validating lifecycle state transitions without consuming `self`.
    pub fn abort_in_place(&mut self) -> Result<()> {
        match self.state {
            TxnState::Active => {}
            TxnState::Committed => {
                return Err(Error::InvalidTransactionState {
                    current: "Committed".to_string(),
                    action: "abort".to_string(),
                });
            }
            TxnState::Aborted => {
                return Err(Error::InvalidTransactionState {
                    current: "Aborted".to_string(),
                    action: "abort".to_string(),
                });
            }
            TxnState::Created => {
                self.state = TxnState::Aborted;
                return Ok(());
            }
        }
        self.abort_internal()?;
        Ok(())
    }

    /// Validates and applies an explicit transaction lifecycle state transition.
    pub fn transition_state(&mut self, target: TxnState) -> Result<()> {
        match (self.state, target) {
            (TxnState::Created, TxnState::Active) => {
                self.state = TxnState::Active;
                Ok(())
            }
            (TxnState::Active, TxnState::Committed) => self.commit_in_place(),
            (TxnState::Active, TxnState::Aborted) => self.abort_in_place(),
            (TxnState::Created, TxnState::Aborted) => {
                self.state = TxnState::Aborted;
                Ok(())
            }
            (current, next) => Err(Error::InvalidTransactionState {
                current: format!("{current}"),
                action: format!("transition to {next}"),
            }),
        }
    }

    fn ensure_active(&self) -> Result<()> {
        match self.state {
            TxnState::Active => Ok(()),
            TxnState::Committed => Err(Error::TransactionAlreadyCommitted(self.txn_id)),
            TxnState::Aborted => Err(Error::TransactionAlreadyAborted(self.txn_id)),
            TxnState::Created => Err(Error::InvalidTransactionState {
                current: "Created".to_string(),
                action: "perform operation".to_string(),
            }),
        }
    }

    fn abort_internal(&mut self) -> Result<()> {
        if self.state == TxnState::Active {
            // Revert xmax on all updated and deleted records
            for &old_rid in self.write_set.updated_records() {
                if let Ok(Some((mut meta, _))) = self
                    .db
                    .record_store
                    .read_version(&mut self.db.buffer_pool, old_rid)
                {
                    if meta.xmax == Some(self.txn_id) {
                        meta.xmax = None;
                        let _ = self.db.record_store.update_version_metadata(
                            &mut self.db.buffer_pool,
                            old_rid,
                            &meta,
                        );
                    }
                }
            }
            for &old_rid in self.write_set.deleted_records() {
                if let Ok(Some((mut meta, _))) = self
                    .db
                    .record_store
                    .read_version(&mut self.db.buffer_pool, old_rid)
                {
                    if meta.xmax == Some(self.txn_id) {
                        meta.xmax = None;
                        let _ = self.db.record_store.update_version_metadata(
                            &mut self.db.buffer_pool,
                            old_rid,
                            &meta,
                        );
                    }
                }
            }

            self.db.abort_txn(self.txn_id)?;
            for &rid in self.write_set.inserted_records() {
                self.db.visibility_map.invalidate_page(rid.page_id);
            }
            for &old_rid in self.write_set.updated_records() {
                self.db.visibility_map.invalidate_record(old_rid);
                self.db.visibility_map.invalidate_page(old_rid.page_id);
            }
            for &old_rid in self.write_set.deleted_records() {
                self.db.visibility_map.invalidate_record(old_rid);
                self.db.visibility_map.invalidate_page(old_rid.page_id);
            }
            if let Some(owned_id) = self.owned_snapshot_id.take() {
                self.db.snapshot_registry.release(owned_id);
            }
            // Restore initial header metadata and tree root
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .root_page_id = self.initial_root_page_id;
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .freelist_head = self.initial_freelist_head;
            self.db
                .buffer_pool
                .page_manager_mut()
                .header_mut()
                .free_page_count = self.initial_free_page_count;
            let _ = self
                .db
                .buffer_pool
                .page_manager_mut()
                .write_header_to_disk();
            self.db.tree =
                crate::index::bplus_tree::BPlusTree::from_root(self.initial_root_page_id);
            if self.has_catalog_mutations {
                self.db.catalog_generation += 1;
                self.db.index_generation += 1;
                self.db.plan_cache.invalidate_all();
            }
            self.state = TxnState::Aborted;
        }
        Ok(())
    }
}

impl<'db> Drop for Transaction<'db> {
    fn drop(&mut self) {
        if self.state == TxnState::Active {
            let _ = self.abort_internal();
        }
    }
}
