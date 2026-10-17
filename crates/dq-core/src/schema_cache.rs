//! Table structures kept per session, so paging, counting and cell reads do
//! not re-read the catalog on every request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

use crate::error::DbResult;
use crate::node::{NodeKind, ObjectRef};
use crate::schema::TableStructure;

/// Structures read from the catalog, by object. Whoever changes the schema
/// invalidates them; the TTL bounds how long a change made elsewhere (another
/// client) goes unnoticed.
pub struct StructureCache {
    ttl: Duration,
    entries: Mutex<HashMap<ObjectRef, Arc<Entry>>>,
}

/// Filled once. Callers that miss at the same time share one catalog read; a
/// failed read leaves it empty for the next caller to retry.
#[derive(Default)]
struct Entry(OnceCell<(Instant, Arc<TableStructure>)>);

impl Entry {
    /// Filled less than `ttl` ago, or still being read.
    fn fresh(&self, ttl: Duration) -> bool {
        self.0.get().is_none_or(|(read_at, _)| read_at.elapsed() < ttl)
    }
}

impl Default for StructureCache {
    fn default() -> Self {
        Self::new(Self::TTL)
    }
}

impl StructureCache {
    pub const TTL: Duration = Duration::from_secs(30);

    pub fn new(ttl: Duration) -> Self {
        Self { ttl, entries: Mutex::new(HashMap::new()) }
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<ObjectRef, Arc<Entry>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached structure of `object`, or the one `read` returns (then cached).
    pub async fn get<F>(&self, object: &ObjectRef, read: impl FnOnce() -> F) -> DbResult<Arc<TableStructure>>
    where
        F: Future<Output = DbResult<TableStructure>>,
    {
        let entry = {
            let mut entries = self.entries();
            match entries.get(object) {
                Some(entry) if entry.fresh(self.ttl) => entry.clone(),
                _ => {
                    entries.retain(|_, e| e.fresh(self.ttl));
                    let entry = Arc::new(Entry::default());
                    entries.insert(object.clone(), entry.clone());
                    entry
                }
            }
        };
        let (_, structure) =
            entry.0.get_or_try_init(|| async { Ok((Instant::now(), Arc::new(read().await?))) }).await?;
        Ok(structure.clone())
    }

    /// Replaces the entry of `object` with a structure just read.
    pub fn insert(&self, object: &ObjectRef, structure: TableStructure) {
        let entry = Entry(OnceCell::from((Instant::now(), Arc::new(structure))));
        self.entries().insert(object.clone(), Arc::new(entry));
    }

    /// Drops the entries inside `scope`: a database, a schema, a group or one
    /// table (the owner, for an index or trigger). `None` drops everything.
    pub fn invalidate(&self, scope: Option<&ObjectRef>) {
        let mut entries = self.entries();
        match scope {
            None => entries.clear(),
            Some(scope) => entries.retain(|object, _| !contains(scope, object)),
        }
    }
}

fn contains(scope: &ObjectRef, object: &ObjectRef) -> bool {
    match scope.kind {
        NodeKind::Database => object.database == scope.name,
        NodeKind::Schema => object.database == scope.database && object.schema.as_ref() == Some(&scope.name),
        NodeKind::Group => object.database == scope.database && object.schema == scope.schema,
        _ => {
            let table = scope.owner_table();
            let table = table.as_ref().unwrap_or(scope);
            object.database == table.database && object.schema == table.schema && object.name == table.name
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::DbError;

    fn table(database: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef::new(NodeKind::Table, database, Some(schema.into()), name)
    }

    fn structure(object: &ObjectRef, columns: usize) -> TableStructure {
        TableStructure {
            object: object.clone(),
            columns: (0..columns).map(|i| crate::schema::ColumnInfo::new_field(&format!("c{i}"))).collect(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            row_identity: Vec::new(),
            full_row_key: Vec::new(),
            comment: None,
            row_estimate: None,
            foreign_keys: Vec::new(),
            size: None,
        }
    }

    /// Reads through `cache`, counting catalog reads.
    async fn read(cache: &StructureCache, object: &ObjectRef, reads: &AtomicUsize) -> usize {
        cache
            .get(object, || async {
                let n = reads.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(structure(object, n))
            })
            .await
            .unwrap()
            .columns
            .len()
    }

    #[tokio::test]
    async fn hits_until_invalidated() {
        let cache = StructureCache::default();
        let (a, b) = (table("db", "public", "a"), table("db", "public", "b"));
        let reads = AtomicUsize::new(0);
        assert_eq!(read(&cache, &a, &reads).await, 1);
        assert_eq!(read(&cache, &a, &reads).await, 1, "second read is a hit");
        assert_eq!(read(&cache, &b, &reads).await, 2);
        cache.invalidate(Some(&a));
        assert_eq!(read(&cache, &a, &reads).await, 3, "only `a` was dropped");
        assert_eq!(read(&cache, &b, &reads).await, 2);
        cache.invalidate(None);
        assert_eq!(read(&cache, &b, &reads).await, 4);
    }

    #[tokio::test]
    async fn expires_after_the_ttl() {
        let cache = StructureCache::new(Duration::ZERO);
        let a = table("db", "public", "a");
        let reads = AtomicUsize::new(0);
        read(&cache, &a, &reads).await;
        read(&cache, &a, &reads).await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_reads_are_not_cached() {
        let cache = StructureCache::default();
        let a = table("db", "public", "a");
        let failed = cache.get(&a, || async { Err(DbError::not_found("gone")) }).await;
        assert!(failed.is_err());
        let reads = AtomicUsize::new(0);
        assert_eq!(read(&cache, &a, &reads).await, 1);
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_read() {
        let cache = StructureCache::default();
        let a = table("db", "public", "a");
        let reads = AtomicUsize::new(0);
        let (x, y) = tokio::join!(read(&cache, &a, &reads), read(&cache, &a, &reads));
        assert_eq!((x, y), (1, 1));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn insert_replaces_the_entry() {
        let cache = StructureCache::default();
        let a = table("db", "public", "a");
        let reads = AtomicUsize::new(0);
        read(&cache, &a, &reads).await;
        cache.insert(&a, structure(&a, 7));
        assert_eq!(read(&cache, &a, &reads).await, 7);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidates_by_scope() {
        let cache = StructureCache::default();
        let reads = AtomicUsize::new(0);
        let objects = [table("db", "public", "a"), table("db", "sales", "b"), table("other", "public", "c")];
        let fill = async || {
            for o in &objects {
                read(&cache, o, &reads).await;
            }
            reads.swap(0, Ordering::SeqCst)
        };
        assert_eq!(fill().await, 3);
        let schema = ObjectRef::new(NodeKind::Schema, "db", None, "sales");
        cache.invalidate(Some(&schema));
        assert_eq!(fill().await, 1, "only db.sales.b");
        let database = ObjectRef::new(NodeKind::Database, "db", None, "db");
        cache.invalidate(Some(&database));
        assert_eq!(fill().await, 2, "both tables of db");
        let index = ObjectRef::new(NodeKind::Index, "other", Some("public".into()), "c_idx").with_parent("c");
        cache.invalidate(Some(&index));
        assert_eq!(fill().await, 1, "the index's table");
    }
}
