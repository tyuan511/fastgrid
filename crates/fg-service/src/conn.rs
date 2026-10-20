use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fg_core::{
    Capabilities, ConnectionConfig, ConnectionId, Dialect, Error, ObjectInfo, ObjectPath, Relation,
    Result, TableDetail, TableQuery, Value,
};
use fg_driver::{Driver, ExecOptions};
use fg_sql::{BuildOptions, build_count, build_select};
use tokio::runtime::Handle;
use tokio::sync::broadcast;

use crate::ServiceEvent;
use crate::cache::TtlCache;
use crate::pool::SessionPool;
use crate::task::TaskStream;

/// Runs futures on the service's tokio runtime. The public API is `async`, but the callers (the
/// GUI's executor, an MCP server) are not tokio: the driver code needs a tokio context, so every
/// public method hops onto the runtime and awaits the join handle, which works from anywhere.
#[derive(Clone)]
pub(crate) struct Rt {
    pub handle: Handle,
}

impl Rt {
    pub async fn run<T, F>(&self, fut: F) -> Result<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        self.handle
            .spawn(fut)
            .await
            .map_err(|e| Error::Other(format!("background task failed: {e}")))?
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BrowseOptions {
    /// Accept a hand-written `WHERE`. True for the GUI's Raw SQL mode, false for agents.
    pub allow_raw_where: bool,
}

impl Default for BrowseOptions {
    fn default() -> Self {
        Self {
            allow_raw_where: true,
        }
    }
}

/// What to forget after something changed behind the cache's back.
#[derive(Debug, Clone)]
pub enum Refresh {
    Everything,
    /// One table's structure, plus the object lists and relations that mention it.
    Table(ObjectPath),
    Namespace(Option<String>),
}

pub struct OpenConnection {
    pub(crate) rt: Rt,
    config: ConnectionConfig,
    driver: Arc<dyn Driver>,
    pool: Arc<SessionPool>,
    namespaces: TtlCache<(), Vec<String>>,
    objects: TtlCache<Option<String>, Arc<Vec<ObjectInfo>>>,
    details: TtlCache<ObjectPath, Arc<TableDetail>>,
    relations: TtlCache<Option<String>, Arc<Vec<Relation>>>,
    /// Shared with the service so a settings change applies to open connections.
    max_cell_bytes: Arc<AtomicUsize>,
    events: broadcast::Sender<ServiceEvent>,
}

impl OpenConnection {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        rt: Rt,
        config: ConnectionConfig,
        driver: Arc<dyn Driver>,
        pool: Arc<SessionPool>,
        ttl: Duration,
        max_cell_bytes: Arc<AtomicUsize>,
        events: broadcast::Sender<ServiceEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            rt,
            config,
            driver,
            pool,
            namespaces: TtlCache::new(ttl),
            objects: TtlCache::new(ttl),
            details: TtlCache::new(ttl),
            relations: TtlCache::new(ttl),
            max_cell_bytes,
            events,
        })
    }

    pub fn id(&self) -> &ConnectionId {
        &self.config.id
    }

    pub fn config(&self) -> &ConnectionConfig {
        &self.config
    }

    pub fn capabilities(&self) -> Capabilities {
        self.driver.capabilities()
    }

    pub fn dialect(&self) -> &'static dyn Dialect {
        self.driver.dialect()
    }

    pub async fn server_version(self: &Arc<Self>) -> Result<String> {
        let this = Arc::clone(self);
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                lease.session()?.server_version().await
            })
            .await
    }

    // ---- metadata (cached) -------------------------------------------------------------------

    pub async fn namespaces(self: &Arc<Self>) -> Result<Vec<String>> {
        if let Some(hit) = self.namespaces.get(&()) {
            return Ok(hit);
        }
        let this = Arc::clone(self);
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                let list = lease.session()?.list_namespaces().await?;
                this.namespaces.put((), list.clone());
                Ok(list)
            })
            .await
    }

    pub async fn objects(
        self: &Arc<Self>,
        namespace: Option<&str>,
    ) -> Result<Arc<Vec<ObjectInfo>>> {
        let key = namespace.map(str::to_owned);
        if let Some(hit) = self.objects.get(&key) {
            return Ok(hit);
        }
        let this = Arc::clone(self);
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                let list = Arc::new(lease.session()?.list_objects(key.as_deref()).await?);
                this.objects.put(key, Arc::clone(&list));
                Ok(list)
            })
            .await
    }

    pub async fn describe(self: &Arc<Self>, path: &ObjectPath) -> Result<Arc<TableDetail>> {
        if let Some(hit) = self.details.get(path) {
            return Ok(hit);
        }
        let this = Arc::clone(self);
        let path = path.clone();
        self.rt
            .run(async move { this.describe_inner(&path).await })
            .await
    }

    async fn describe_inner(&self, path: &ObjectPath) -> Result<Arc<TableDetail>> {
        if let Some(hit) = self.details.get(path) {
            return Ok(hit);
        }
        let lease = self.pool.acquire().await?;
        let detail = Arc::new(lease.session()?.describe(path).await?);
        self.details.put(path.clone(), Arc::clone(&detail));
        Ok(detail)
    }

    pub async fn relations(
        self: &Arc<Self>,
        namespace: Option<&str>,
    ) -> Result<Arc<Vec<Relation>>> {
        let key = namespace.map(str::to_owned);
        if let Some(hit) = self.relations.get(&key) {
            return Ok(hit);
        }
        let this = Arc::clone(self);
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                let list = Arc::new(lease.session()?.list_relations(key.as_deref()).await?);
                this.relations.put(key, Arc::clone(&list));
                Ok(list)
            })
            .await
    }

    /// `CREATE` statement of an object, where the engine can produce one. Not cached.
    pub async fn ddl(self: &Arc<Self>, path: &ObjectPath) -> Result<Option<String>> {
        let this = Arc::clone(self);
        let path = path.clone();
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                lease.session()?.ddl(&path).await
            })
            .await
    }

    /// Drop cached metadata (after DDL, or the user pressing refresh) and tell listeners.
    pub fn refresh(&self, what: &Refresh) {
        match what {
            Refresh::Everything => {
                self.namespaces.clear();
                self.objects.clear();
                self.details.clear();
                self.relations.clear();
            }
            Refresh::Table(path) => {
                self.details.remove(path);
                self.objects.remove(&path.namespace);
                self.relations.remove(&path.namespace);
            }
            Refresh::Namespace(ns) => {
                self.objects.remove(ns);
                self.relations.remove(ns);
                self.details.clear();
            }
        }
        // Nobody listening is fine.
        let _ = self.events.send(ServiceEvent::MetadataChanged {
            connection: self.config.id.clone(),
        });
    }

    // ---- statements --------------------------------------------------------------------------

    /// Run SQL (one or several statements) and stream the results. Dropping the stream stops it.
    pub async fn execute(
        self: &Arc<Self>,
        sql: &str,
        params: &[Value],
        options: ExecOptions,
    ) -> Result<TaskStream> {
        let this = Arc::clone(self);
        let (sql, params) = (sql.to_owned(), params.to_vec());
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                let stream = lease.session()?.execute(&sql, &params, options).await?;
                TaskStream::new(stream, lease)
            })
            .await
    }

    fn exec_options(&self) -> ExecOptions {
        let max = self.max_cell_bytes.load(Ordering::Relaxed);
        ExecOptions {
            max_cell_bytes: (max > 0).then_some(max),
            ..ExecOptions::default()
        }
    }

    async fn build_options(&self, q: &TableQuery, opts: BrowseOptions) -> Result<BuildOptions> {
        // Primary key as tie-breaker, so paging through an unsorted table is stable.
        let detail = self.describe_inner(&q.path).await?;
        Ok(BuildOptions {
            tie_breaker: detail
                .primary_key()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            allow_raw_where: opts.allow_raw_where,
        })
    }

    /// One page of a table or view: the declarative query compiled for this engine and streamed.
    pub async fn browse(
        self: &Arc<Self>,
        query: &TableQuery,
        opts: BrowseOptions,
    ) -> Result<TaskStream> {
        let this = Arc::clone(self);
        let query = query.clone();
        self.rt
            .run(async move {
                let build = this.build_options(&query, opts).await?;
                let built = build_select(&query, this.driver.dialect(), &build)?;
                let lease = this.pool.acquire().await?;
                let stream = lease
                    .session()?
                    .execute(&built.sql, &built.params, this.exec_options())
                    .await?;
                TaskStream::new(stream, lease)
            })
            .await
    }

    /// Exact `COUNT(*)` for the query's filters. Can be slow on big tables: run it in the
    /// background and show the page first.
    pub async fn count(self: &Arc<Self>, query: &TableQuery, opts: BrowseOptions) -> Result<u64> {
        let this = Arc::clone(self);
        let query = query.clone();
        self.rt
            .run(async move {
                let build = BuildOptions {
                    tie_breaker: Vec::new(),
                    allow_raw_where: opts.allow_raw_where,
                };
                let built = build_count(&query, this.driver.dialect(), &build)?;
                let lease = this.pool.acquire().await?;
                let stream = lease
                    .session()?
                    .execute(&built.sql, &built.params, ExecOptions::default())
                    .await?;
                let sets = TaskStream::new(stream, lease)?.collect().await?;
                match sets
                    .first()
                    .and_then(|s| s.rows.first())
                    .and_then(|r| r.first())
                {
                    Some(Value::Int(n)) => u64::try_from(*n).map_err(|_| bad_count()),
                    Some(Value::UInt(n)) => Ok(*n),
                    Some(Value::Decimal(s) | Value::Text(s)) => s.parse().map_err(|_| bad_count()),
                    _ => Err(bad_count()),
                }
            })
            .await
    }

    pub(crate) fn close(&self) {
        self.pool.close();
    }

    pub(crate) async fn measure_ping(self: &Arc<Self>) -> Result<Duration> {
        let this = Arc::clone(self);
        self.rt
            .run(async move {
                let lease = this.pool.acquire().await?;
                let started = Instant::now();
                lease.session()?.ping().await?;
                Ok(started.elapsed())
            })
            .await
    }
}

fn bad_count() -> Error {
    Error::Other("the database returned an unexpected row count".into())
}
