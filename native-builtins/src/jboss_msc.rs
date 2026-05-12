//! T19.1 — JBoss MSC (Modular Service Container) native glue.
//!
//! WildFly's boot sequence, after `org.jboss.modules.Main.main()` has
//! bootstrapped the module-loader, hands control to
//! `org.jboss.msc.service.ServiceContainer`.  The container is the async
//! service orchestrator at the heart of WildFly / Keycloak 16:
//!
//! * It indexes services by a hierarchical dotted [`ServiceName`]
//!   (e.g. `jboss.as.server.deployment.unit.keycloak-server.war`).
//! * Each service is a [`ServiceController`] driven through a state
//!   machine: `New → Down → Starting → Up` (with reverse for stop and
//!   `Failed` / `Removed` terminal states).
//! * Services declare [`Mode`]s — `Active`, `OnDemand`, `Passive`,
//!   `Lazy`, `Never` — that gate automatic startup.
//! * Dependencies form a DAG; the scheduler starts a service only after
//!   every dependency is `Up`.
//! * `start(StartContext)` callbacks run on worker threads in a small
//!   pool; the `StartContext.asynchronous()` → `complete()` sequence
//!   lets slow services defer their `Up` transition.
//!
//! The native surface here models the in-Rust graph explicitly so we
//! avoid dragging Keycloak's real MSC jar's bytecode through the
//! interpreter's hot paths. A parallel `synthetic_stub_fields` entry
//! in `classloading/src/class_manager.rs` reserves the minimum heap
//! layout each Java-facing class needs; MSC natives then write
//! back-references (e.g. `ServiceController.controller_id`) into those
//! slots so JDK bytecode that reads them finds consistent values.
//!
//! # Deadlock & panic safety
//!
//! * Circular dependencies are rejected at `addService` time.
//! * Worker threads catch panics via `std::panic::catch_unwind` and
//!   transition the failing service to `Failed` with the panic payload
//!   recorded as a message, so one bad service never tears down the
//!   container.
//! * All state mutation funnels through a single `Mutex` on the
//!   container; worker threads release the lock while invoking Java
//!   callbacks to prevent reentrancy deadlocks.
//!
//! See `docs/roadmap-100.md` T19.1 for the feature scope.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use rustjvm_types::{ObjectRef, Value};

use crate::{alloc_concurrent_synthetic, obj_arg};

// ===========================================================================
// ServiceName — hierarchical, immutable, interned-segment dotted name.
// ===========================================================================

/// Immutable dotted-hierarchy key (e.g. `jboss.as.server.foo`).
///
/// Canonical names are deduplicated via [`intern_service_name`] — two
/// `ServiceName`s with the same segment list share the same
/// `Arc<ServiceName>`, so pointer equality implies semantic equality.
#[derive(Debug)]
pub struct ServiceName {
    segments: Vec<Arc<str>>,
    canonical: String,
}

impl ServiceName {
    /// Create a `ServiceName` from an iterator of segments (e.g. from
    /// `ServiceName.of(String...)`).  Leading / trailing / duplicate
    /// dots inside a single segment are preserved — the JDK treats them
    /// as user data, we match that behaviour.
    pub fn of<I, S>(segments: I) -> Arc<ServiceName>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let segs: Vec<Arc<str>> = segments
            .into_iter()
            .map(|s| Arc::<str>::from(s.as_ref()))
            .collect();
        let canonical = segs
            .iter()
            .map(|s| s.as_ref())
            .collect::<Vec<_>>()
            .join(".");
        intern_service_name(ServiceName {
            segments: segs,
            canonical,
        })
    }

    /// Parse the canonical dotted form.  A single literal dot inside a
    /// segment requires the `of` constructor instead; we do the simple
    /// thing here (split on `.`) because every caller goes through
    /// [`Self::of`] for user-supplied strings.
    pub fn parse(dotted: &str) -> Arc<ServiceName> {
        Self::of(dotted.split('.'))
    }

    /// Append one segment and return a new [`ServiceName`].
    pub fn append(self: &Arc<ServiceName>, segment: &str) -> Arc<ServiceName> {
        let mut segs: Vec<String> = self.segments.iter().map(|s| s.to_string()).collect();
        segs.push(segment.to_string());
        Self::of(segs)
    }

    /// Drop the last segment; returns `None` for root names
    /// (length-0 or length-1 segment lists).
    pub fn parent(self: &Arc<ServiceName>) -> Option<Arc<ServiceName>> {
        if self.segments.len() <= 1 {
            return None;
        }
        let segs: Vec<String> = self.segments[..self.segments.len() - 1]
            .iter()
            .map(|s| s.to_string())
            .collect();
        Some(Self::of(segs))
    }

    /// Canonical dotted form.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// Number of segments.
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Whether the name has zero segments (pseudo-root).
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

impl PartialEq for ServiceName {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}
impl Eq for ServiceName {}

impl std::hash::Hash for ServiceName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.canonical.hash(state);
    }
}

/// Global intern table — one `Arc<ServiceName>` per canonical string.
/// Pointer equality on the `Arc` ≡ semantic equality on the name.
fn service_name_intern_table() -> &'static Mutex<HashMap<String, Arc<ServiceName>>> {
    static T: OnceLock<Mutex<HashMap<String, Arc<ServiceName>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn intern_service_name(name: ServiceName) -> Arc<ServiceName> {
    let mut t = service_name_intern_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = t.get(&name.canonical) {
        return existing.clone();
    }
    let arc = Arc::new(name);
    t.insert(arc.canonical.clone(), arc.clone());
    arc
}

// ===========================================================================
// ServiceController state machine + Mode + back-references.
// ===========================================================================

/// WildFly `ServiceController.State` values (ordinal-matched where
/// possible).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    New,
    Down,
    Starting,
    Up,
    Stopping,
    Failed,
    Removed,
}

impl ServiceState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::New => "NEW",
            ServiceState::Down => "DOWN",
            ServiceState::Starting => "STARTING",
            ServiceState::Up => "UP",
            ServiceState::Stopping => "STOPPING",
            ServiceState::Failed => "FAILED",
            ServiceState::Removed => "REMOVED",
        }
    }

    /// Ordinal that matches the JDK enum constant order; stored in the
    /// synthetic `ServiceController.state` field so Java code inspecting
    /// the ordinal gets consistent values.
    pub fn ordinal(self) -> i32 {
        match self {
            ServiceState::New => 0,
            ServiceState::Down => 1,
            ServiceState::Starting => 2,
            ServiceState::Up => 3,
            ServiceState::Stopping => 4,
            ServiceState::Failed => 5,
            ServiceState::Removed => 6,
        }
    }
}

/// `ServiceController.Mode` — controls automatic startup behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Start as soon as dependencies are `Up`.
    Active,
    /// Start only when a dependent requires the value.
    OnDemand,
    /// Start as soon as dependencies are `Up`, but services depending
    /// on a passive never force it up.
    Passive,
    /// Like `OnDemand` but stays `Up` once started.
    Lazy,
    /// Stay down regardless of demand.
    Never,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Active => "ACTIVE",
            Mode::OnDemand => "ON_DEMAND",
            Mode::Passive => "PASSIVE",
            Mode::Lazy => "LAZY",
            Mode::Never => "NEVER",
        }
    }
    pub fn parse(s: &str) -> Mode {
        match s {
            "ACTIVE" => Mode::Active,
            "ON_DEMAND" => Mode::OnDemand,
            "PASSIVE" => Mode::Passive,
            "LAZY" => Mode::Lazy,
            "NEVER" => Mode::Never,
            _ => Mode::Active,
        }
    }
    pub fn ordinal(self) -> i32 {
        match self {
            Mode::Active => 0,
            Mode::OnDemand => 1,
            Mode::Passive => 2,
            Mode::Lazy => 3,
            Mode::Never => 4,
        }
    }
}

/// A single service node in the container DAG.
///
/// `service_obj` is the Java-level `Service` instance (with `start`,
/// `stop`, `getValue`).  When the VM invokes its `start` the native
/// scheduler bridges back through [`NativeContext::invoke_virtual`].
#[derive(Debug)]
pub struct ServiceController {
    /// Unique monotonically-increasing ID; used as back-reference key.
    pub id: u64,
    pub name: Arc<ServiceName>,
    pub mode: Mode,
    pub state: ServiceState,
    pub dependencies: Vec<Arc<ServiceName>>,
    /// Raw pointer to the Java `Service` object; Send-safe because the
    /// container holds the strong reference and never drops it while a
    /// worker might dereference the pointer.
    pub service_obj: usize,
    /// Names of other controllers that depend on this one.
    pub dependents: Vec<Arc<ServiceName>>,
    /// Human-readable failure message, populated when `state == Failed`.
    pub failure_message: Option<String>,
    /// Async-start flag set by `StartContext::asynchronous()`; when
    /// true, the worker does NOT transition to `Up` on return from
    /// `start()`.  The service must call `complete()` to finish.
    pub async_pending: bool,
}

impl ServiceController {
    fn new(id: u64, name: Arc<ServiceName>, service_obj: usize) -> Self {
        Self {
            id,
            name,
            mode: Mode::Active,
            state: ServiceState::New,
            dependencies: Vec::new(),
            service_obj,
            dependents: Vec::new(),
            failure_message: None,
            async_pending: false,
        }
    }
}

// ===========================================================================
// ServiceContainer — the DAG + worker pool.
// ===========================================================================

/// The process-wide MSC registry.  One container is typical; WildFly's
/// `ServiceContainer.Factory.create()` returns the same singleton to
/// every caller so test runners that instantiate multiple factories
/// still share a single graph.
pub struct ServiceContainer {
    inner: Mutex<ContainerState>,
    /// Worker pool condvar — wakes workers when a new task is queued.
    pool_cv: Condvar,
    /// Monotonic ID allocator for new [`ServiceController`]s.
    next_id: AtomicU64,
}

struct ContainerState {
    /// Primary index: canonical ServiceName → controller.
    services: HashMap<Arc<ServiceName>, ServiceController>,
    /// Back-index: controller id → name (for fast lookup by ID).
    by_id: HashMap<u64, Arc<ServiceName>>,
    /// Pending work items the workers will pick up.
    task_queue: VecDeque<Task>,
    /// Set of service IDs currently being started/stopped; used to
    /// ensure `async_pending` completions find their controller.
    in_flight: HashSet<u64>,
    /// Shutdown flag — stops workers from accepting new tasks.
    shutdown: bool,
}

/// A single unit of work the scheduler dispatches to a worker.
enum Task {
    /// Drive `service_obj.start(StartContext)` for a controller, then
    /// (unless async_pending) transition to `Up`.
    Start(u64),
    /// Drive `service_obj.stop(StopContext)` for a controller, then
    /// transition to `Down`.
    Stop(u64),
}

impl ServiceContainer {
    fn new() -> Self {
        Self {
            inner: Mutex::new(ContainerState {
                services: HashMap::new(),
                by_id: HashMap::new(),
                task_queue: VecDeque::new(),
                in_flight: HashSet::new(),
                shutdown: false,
            }),
            pool_cv: Condvar::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Install a service with the given name, dependencies, and initial
    /// mode.  Returns the new controller's ID on success, or
    /// `CircularDependencyException` if installing this service would
    /// introduce a cycle.
    pub fn add_service(
        &self,
        name: Arc<ServiceName>,
        dependencies: Vec<Arc<ServiceName>>,
        mode: Mode,
        service_obj: usize,
    ) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        // Cycle-detection: walk proposed deps; if any transitively
        // references `name` back, reject.
        if would_cycle(&state.services, &name, &dependencies) {
            return Err(format!(
                "CircularDependencyException: installing {} would cycle",
                name.canonical()
            ));
        }

        let mut ctrl = ServiceController::new(id, name.clone(), service_obj);
        ctrl.mode = mode;
        ctrl.dependencies = dependencies.clone();
        ctrl.state = ServiceState::Down;
        for dep in &dependencies {
            if let Some(d) = state.services.get_mut(dep) {
                d.dependents.push(name.clone());
            }
        }
        state.services.insert(name.clone(), ctrl);
        state.by_id.insert(id, name.clone());

        // Schedule Active / Passive services whose deps are already Up.
        if matches!(mode, Mode::Active | Mode::Passive) {
            if can_start(&state.services, &name) {
                state.task_queue.push_back(Task::Start(id));
                self.pool_cv.notify_one();
            }
        }
        Ok(id)
    }

    /// Return a snapshot of a controller's state by name.
    pub fn get_state(&self, name: &Arc<ServiceName>) -> Option<ServiceState> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.services.get(name).map(|c| c.state)
    }

    pub fn get_id(&self, name: &Arc<ServiceName>) -> Option<u64> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.services.get(name).map(|c| c.id)
    }

    /// Ask the scheduler to start a service whose mode is `OnDemand` or
    /// `Lazy` (normally triggered by a dependent requesting the value).
    /// `Never`-mode services ignore the request.
    pub fn demand(&self, name: &Arc<ServiceName>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = state.services.get(name) {
            if matches!(c.mode, Mode::Never) {
                return;
            }
            let id = c.id;
            if matches!(c.state, ServiceState::Down | ServiceState::New)
                && can_start(&state.services, name)
            {
                state.task_queue.push_back(Task::Start(id));
                self.pool_cv.notify_one();
            }
        }
    }

    /// Mark a controller's `start()` as async-pending.  The worker will
    /// leave state at `Starting` until [`Self::complete_async`] is
    /// called.
    pub fn mark_async(&self, id: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(name) = state.by_id.get(&id).cloned() {
            if let Some(c) = state.services.get_mut(&name) {
                c.async_pending = true;
            }
        }
    }

    /// Complete an async-pending `start()` — transitions to `Up` and
    /// fires transitive-dependent starts.
    pub fn complete_async(&self, id: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let name = match state.by_id.get(&id).cloned() {
            Some(n) => n,
            None => return,
        };
        if let Some(c) = state.services.get_mut(&name) {
            c.async_pending = false;
            if matches!(c.state, ServiceState::Starting) {
                c.state = ServiceState::Up;
                c.failure_message = None;
            }
        }
        schedule_dependents_of(&mut state, &name, &self.pool_cv);
    }

    /// Record a start failure (from a caught panic or an explicit
    /// `StartException` thrown through native code).
    pub fn record_failure(&self, id: u64, message: String) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(name) = state.by_id.get(&id).cloned() {
            if let Some(c) = state.services.get_mut(&name) {
                c.state = ServiceState::Failed;
                c.failure_message = Some(message);
                c.async_pending = false;
            }
        }
    }

    /// Synchronously shut down every running service in reverse
    /// dependency order.
    pub fn shutdown(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        let order = reverse_topo_order(&state.services);
        for n in order {
            if let Some(c) = state.services.get_mut(&n) {
                if matches!(c.state, ServiceState::Up | ServiceState::Starting) {
                    c.state = ServiceState::Down;
                }
            }
        }
        self.pool_cv.notify_all();
    }

    /// Count services in a given state — used by tests and health
    /// checks.
    pub fn count_in(&self, s: ServiceState) -> usize {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.services.values().filter(|c| c.state == s).count()
    }

    /// Number of installed services.
    pub fn size(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .services
            .len()
    }

    /// Synchronously drive a single `Start(id)` task.  Normally the
    /// worker thread does this, but tests and the single-threaded
    /// shutdown path call it directly to avoid cross-thread Java
    /// invocations.
    fn run_start_local(&self, id: u64) {
        let name = {
            let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match state.by_id.get(&id) {
                Some(n) => n.clone(),
                None => return,
            }
        };
        // Transition Down -> Starting.
        {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = state.services.get_mut(&name) {
                if !matches!(c.state, ServiceState::Down | ServiceState::New) {
                    return;
                }
                c.state = ServiceState::Starting;
                state.in_flight.insert(id);
            } else {
                return;
            }
        }
        // Since we have no real Java callback to run locally, just
        // transition to Up and fire dependents.
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let still_starting = state
            .services
            .get(&name)
            .map(|c| matches!(c.state, ServiceState::Starting) && !c.async_pending)
            .unwrap_or(false);
        if still_starting {
            if let Some(c) = state.services.get_mut(&name) {
                c.state = ServiceState::Up;
            }
            state.in_flight.remove(&id);
            schedule_dependents_of(&mut state, &name, &self.pool_cv);
        }
    }

    /// Drain the task queue by running every pending `Start` task
    /// synchronously.  Useful for tests that want deterministic
    /// completion without spawning worker threads.
    pub fn drain_tasks_locally(&self) {
        loop {
            let task = {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.task_queue.pop_front()
            };
            match task {
                Some(Task::Start(id)) => self.run_start_local(id),
                Some(Task::Stop(id)) => {
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(name) = state.by_id.get(&id).cloned() {
                        if let Some(c) = state.services.get_mut(&name) {
                            c.state = ServiceState::Down;
                        }
                    }
                }
                None => break,
            }
        }
    }
}

/// Detect whether adding `(name -> deps)` to the services map would
/// introduce a cycle.
fn would_cycle(
    services: &HashMap<Arc<ServiceName>, ServiceController>,
    name: &Arc<ServiceName>,
    deps: &[Arc<ServiceName>],
) -> bool {
    // Starting from each dep, BFS along existing dependency edges
    // looking for `name`.  The proposed edges go name -> deps, so we
    // DFS outbound from the deps; a hit on `name` means the edge we're
    // about to add completes a cycle.
    let mut seen: HashSet<Arc<ServiceName>> = HashSet::new();
    let mut stack: Vec<Arc<ServiceName>> = deps.to_vec();
    while let Some(cur) = stack.pop() {
        if &cur == name {
            return true;
        }
        if !seen.insert(cur.clone()) {
            continue;
        }
        if let Some(c) = services.get(&cur) {
            for d in &c.dependencies {
                stack.push(d.clone());
            }
        }
    }
    false
}

/// Check whether every dependency of `name` is currently `Up`.
fn can_start(
    services: &HashMap<Arc<ServiceName>, ServiceController>,
    name: &Arc<ServiceName>,
) -> bool {
    let c = match services.get(name) {
        Some(c) => c,
        None => return false,
    };
    for dep in &c.dependencies {
        match services.get(dep) {
            Some(d) if matches!(d.state, ServiceState::Up) => {}
            _ => return false,
        }
    }
    true
}

/// After `name` transitions `Up`, walk its dependents and queue any
/// that are now start-eligible.
fn schedule_dependents_of(
    state: &mut ContainerState,
    name: &Arc<ServiceName>,
    pool_cv: &Condvar,
) {
    let dep_names: Vec<Arc<ServiceName>> = match state.services.get(name) {
        Some(c) => c.dependents.clone(),
        None => return,
    };
    for dn in dep_names {
        let (mode, cur_state, id) = match state.services.get(&dn) {
            Some(d) => (d.mode, d.state, d.id),
            None => continue,
        };
        if !matches!(cur_state, ServiceState::Down | ServiceState::New) {
            continue;
        }
        if !matches!(mode, Mode::Active | Mode::Passive | Mode::Lazy) {
            continue;
        }
        if can_start(&state.services, &dn) {
            state.task_queue.push_back(Task::Start(id));
            pool_cv.notify_one();
        }
    }
}

/// Produce a topologically sorted list of service names in reverse
/// dependency order — i.e. leaves first, roots last.  Used by
/// `shutdown()` so we stop services in the correct order.
fn reverse_topo_order(
    services: &HashMap<Arc<ServiceName>, ServiceController>,
) -> Vec<Arc<ServiceName>> {
    let mut order: Vec<Arc<ServiceName>> = Vec::with_capacity(services.len());
    let mut visited: HashSet<Arc<ServiceName>> = HashSet::new();

    fn dfs(
        services: &HashMap<Arc<ServiceName>, ServiceController>,
        name: &Arc<ServiceName>,
        visited: &mut HashSet<Arc<ServiceName>>,
        order: &mut Vec<Arc<ServiceName>>,
    ) {
        if !visited.insert(name.clone()) {
            return;
        }
        if let Some(c) = services.get(name) {
            // Walk dependents first so they land before `name` in the
            // output vector (leaves earliest → reverse dep order).
            for dep in &c.dependents {
                dfs(services, dep, visited, order);
            }
        }
        order.push(name.clone());
    }

    for name in services.keys() {
        dfs(services, name, &mut visited, &mut order);
    }
    order
}

// ===========================================================================
// Global container singleton + worker pool spawning
// ===========================================================================

/// Process-wide MSC singleton.  Lazily constructed on first access;
/// also starts the worker threads.
pub fn global_container() -> &'static Arc<ServiceContainer> {
    static INSTANCE: OnceLock<Arc<ServiceContainer>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let c = Arc::new(ServiceContainer::new());
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 4);
        for i in 0..parallelism {
            let c2 = c.clone();
            std::thread::Builder::new()
                .name(format!("msc-worker-{i}"))
                .spawn(move || worker_loop(c2))
                .expect("failed to spawn msc-worker");
        }
        c
    })
}

/// Worker main loop.  Pulls `Task`s off the queue and runs them.  Since
/// the `NativeContext` needed to call Java from a foreign thread is not
/// available from here, the worker only handles bookkeeping — it marks
/// the service `Up` / `Down` without invoking Java `start`/`stop`.  The
/// production wiring (when the subsystem-init phase lands in T19.2)
/// will install a real dispatcher via
/// [`install_task_dispatcher`](crate::jboss_msc::install_task_dispatcher).
fn worker_loop(container: Arc<ServiceContainer>) {
    loop {
        let task = {
            let mut state = container
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            loop {
                if state.shutdown && state.task_queue.is_empty() {
                    return;
                }
                if let Some(t) = state.task_queue.pop_front() {
                    break t;
                }
                let res = container
                    .pool_cv
                    .wait_timeout(state, Duration::from_millis(500))
                    .unwrap_or_else(|e| e.into_inner());
                state = res.0;
            }
        };
        // Wrap the actual task in `catch_unwind` so a panic in the
        // Java-side start/stop logic (or in our own bookkeeping)
        // cannot crash the entire container process.
        let container2 = container.clone();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || match task {
            Task::Start(id) => container2.run_start_local(id),
            Task::Stop(id) => {
                let mut state = container2
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(name) = state.by_id.get(&id).cloned() {
                    if let Some(c) = state.services.get_mut(&name) {
                        c.state = ServiceState::Down;
                    }
                }
            }
        }));
        if let Err(panic_payload) = outcome {
            let msg = panic_to_string(&panic_payload);
            // Which id failed?  We no longer know — the closure
            // already consumed `task`.  Scan for any `Starting`
            // service and flag it so the error is surfaced.
            let mut state = container
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let ids: Vec<u64> = state
                .services
                .values()
                .filter(|c| matches!(c.state, ServiceState::Starting))
                .map(|c| c.id)
                .collect();
            for id in ids {
                if let Some(name) = state.by_id.get(&id).cloned() {
                    if let Some(c) = state.services.get_mut(&name) {
                        c.state = ServiceState::Failed;
                        c.failure_message = Some(msg.clone());
                        c.async_pending = false;
                    }
                }
            }
        }
    }
}

fn panic_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "service start panicked (unknown payload)".to_string()
    }
}

// ===========================================================================
// Java ↔ Rust glue layer — natives registered with the method registry.
// ===========================================================================

// --- Field offsets (matched to synthetic_stub_fields in class_manager.rs) ---
//
// ServiceName        (2 fields): 0=segments (int[], Ids only), 1=canonical (String)
// ServiceController  (5 fields): 0=name, 1=mode (Int ordinal), 2=state (Int ordinal),
//                                3=value (Object), 4=listeners (ArrayList)
//                                Plus the id back-reference is stored in field 5
//                                (allocated as 6-slot object to leave room).
// ServiceContainer   (2 fields): 0=services_map (HashMap), 1=worker_pool_handle (Int)
// StartContext       (1 field):  0=controller_id (Long)
// StopContext        (1 field):  0=controller_id (Long)

const SN_FIELD_CANONICAL: usize = 1;
const SC_FIELD_NAME: usize = 0;
const SC_FIELD_MODE: usize = 1;
const SC_FIELD_STATE: usize = 2;
const SC_FIELD_VALUE: usize = 3;
#[allow(dead_code)]
const SC_FIELD_LISTENERS: usize = 4;
/// `ServiceController` is allocated with an extra trailing slot for its
/// Rust-side `id`, so Java bytecode accessing the 5 documented fields
/// stays within bounds while natives can still round-trip via the id.
const SC_FIELD_ID: usize = 5;
const SC_NUM_SLOTS: usize = 6;

const CTX_FIELD_CONTROLLER_ID: usize = 0;
const CTX_NUM_SLOTS: usize = 1;

/// Helper to allocate a Java `ServiceName` object wrapping an
/// `Arc<ServiceName>` via its canonical String.  Field 0 is unused
/// (could later be populated with the segments array); field 1 holds
/// the canonical String so `getCanonicalName()` can return it directly.
fn alloc_java_service_name(
    ctx: &mut dyn NativeContext,
    name: &Arc<ServiceName>,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceName", 2);
    let canonical = ctx.create_string(name.canonical());
    ctx.set_field(obj, SN_FIELD_CANONICAL, Value::Object(Some(canonical)));
    obj
}

/// Read an `Arc<ServiceName>` back out of a Java `ServiceName` object
/// by interning its canonical string.  Returns `None` if the canonical
/// field is missing/null.
fn read_java_service_name(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Option<Arc<ServiceName>> {
    let val = ctx.get_field(obj, SN_FIELD_CANONICAL);
    match val {
        Value::Object(Some(s)) => {
            let text = ctx.read_string(s)?;
            Some(ServiceName::parse(&text))
        }
        _ => None,
    }
}

/// Reflect the Rust controller state back into the Java mirror after a
/// transition.  Safe to call under contention — the update is a
/// best-effort mirror; the canonical state still lives in the Rust
/// container.
fn reflect_controller(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    container: &ServiceContainer,
    id: u64,
) {
    let state = container
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let name = match state.by_id.get(&id) {
        Some(n) => n.clone(),
        None => return,
    };
    let ctrl = match state.services.get(&name) {
        Some(c) => c,
        None => return,
    };
    ctx.set_field(obj, SC_FIELD_STATE, Value::Int(ctrl.state.ordinal()));
    ctx.set_field(obj, SC_FIELD_MODE, Value::Int(ctrl.mode.ordinal()));
    ctx.set_field(obj, SC_FIELD_ID, Value::Long(id as i64));
}

fn native_service_name_of_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let sn = ServiceName::parse(&s);
    Ok(Some(Value::Object(Some(alloc_java_service_name(ctx, &sn)))))
}

fn native_service_name_of_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: static ServiceName.of(String[] segments)
    let arr = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ServiceName.of: null segments array".into()),
                },
            )));
        }
    };
    let len = ctx.array_length(arr);
    let mut segs: Vec<String> = Vec::with_capacity(len);
    for i in 0..len {
        let el = ctx.get_array_element(arr, i);
        if let Value::Object(Some(s)) = el {
            if let Some(text) = ctx.read_string(s) {
                segs.push(text);
            }
        }
    }
    let sn = ServiceName::of(segs);
    Ok(Some(Value::Object(Some(alloc_java_service_name(ctx, &sn)))))
}

fn native_service_name_append(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let seg = match args.get(1).copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let parent = match read_java_service_name(ctx, this) {
        Some(p) => p,
        None => ServiceName::of(std::iter::empty::<&str>()),
    };
    let child = parent.append(&seg);
    Ok(Some(Value::Object(Some(alloc_java_service_name(ctx, &child)))))
}

fn native_service_name_get_canonical(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let canonical = read_java_service_name(ctx, this)
        .map(|n| n.canonical().to_string())
        .unwrap_or_default();
    let s = ctx.create_string(&canonical);
    Ok(Some(Value::Object(Some(s))))
}

fn native_service_name_get_parent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sn = read_java_service_name(ctx, this);
    let parent_obj = match sn.and_then(|s| s.parent()) {
        Some(p) => Value::Object(Some(alloc_java_service_name(ctx, &p))),
        None => Value::Object(None),
    };
    Ok(Some(parent_obj))
}

fn native_service_container_create(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Force the singleton to initialize so worker threads are alive.
    let _ = global_container();
    let obj = alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceContainer", 2);
    Ok(Some(Value::Object(Some(obj))))
}

fn native_service_container_add_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: addService(ServiceName, Service) -> ServiceController
    if args.len() < 3 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("ServiceContainer.addService: expected 3 args, got {}", args.len()),
        }));
    }
    let _this = obj_arg(args, 0)?;
    let sn_obj = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("addService: null ServiceName".into()),
                },
            )));
        }
    };
    let service_obj = match args.get(2).copied() {
        Some(Value::Object(Some(o))) => o.as_ptr() as usize,
        _ => 0,
    };
    let name = match read_java_service_name(ctx, sn_obj) {
        Some(n) => n,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "addService: could not read ServiceName canonical".into(),
            }));
        }
    };

    let container = global_container().clone();
    let id = container
        .add_service(name.clone(), Vec::new(), Mode::Active, service_obj)
        .map_err(|msg| {
            MethodCallFailed::InternalError(VmError::Internal { message: msg })
        })?;

    // Build the Java-side ServiceController mirror.
    let ctrl_obj = alloc_concurrent_synthetic(
        ctx,
        "org/jboss/msc/service/ServiceController",
        SC_NUM_SLOTS,
    );
    ctx.set_field(ctrl_obj, SC_FIELD_NAME, Value::Object(Some(sn_obj)));
    ctx.set_field(ctrl_obj, SC_FIELD_MODE, Value::Int(Mode::Active.ordinal()));
    ctx.set_field(ctrl_obj, SC_FIELD_STATE, Value::Int(ServiceState::Down.ordinal()));
    ctx.set_field(ctrl_obj, SC_FIELD_VALUE, Value::Object(None));
    ctx.set_field(ctrl_obj, SC_FIELD_ID, Value::Long(id as i64));

    // Run the task queue locally so the mirror's state reflects the
    // transition before we return.  When real Java `start()` callbacks
    // are wired in T19.2 this will shift onto the worker threads.
    container.drain_tasks_locally();
    reflect_controller(ctx, ctrl_obj, &container, id);

    Ok(Some(Value::Object(Some(ctrl_obj))))
}

fn native_service_controller_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_mode = match args.get(1).copied() {
        Some(Value::Object(Some(s))) => {
            let txt = ctx.read_string(s).unwrap_or_default();
            Mode::parse(&txt)
        }
        Some(Value::Int(i)) => match i {
            0 => Mode::Active,
            1 => Mode::OnDemand,
            2 => Mode::Passive,
            3 => Mode::Lazy,
            4 => Mode::Never,
            _ => Mode::Active,
        },
        _ => Mode::Active,
    };
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let container = global_container().clone();
    {
        let mut state = container
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(name) = state.by_id.get(&id).cloned() {
            if let Some(c) = state.services.get_mut(&name) {
                c.mode = new_mode;
            }
        }
    }
    if matches!(new_mode, Mode::OnDemand | Mode::Lazy) {
        // No auto-start; leave as Down until demanded.
    } else if matches!(new_mode, Mode::Active | Mode::Passive) {
        let name_opt = {
            let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
            state.by_id.get(&id).cloned()
        };
        if let Some(n) = name_opt {
            container.demand(&n);
            container.drain_tasks_locally();
        }
    }
    ctx.set_field(this, SC_FIELD_MODE, Value::Int(new_mode.ordinal()));
    reflect_controller(ctx, this, &container, id);
    Ok(None)
}

fn native_service_controller_get_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let container = global_container();
    let state_ord = {
        let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        state
            .by_id
            .get(&id)
            .and_then(|n| state.services.get(n))
            .map(|c| c.state.ordinal())
            .unwrap_or_else(|| ServiceState::New.ordinal())
    };
    Ok(Some(Value::Int(state_ord)))
}

fn native_start_context_asynchronous(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    global_container().mark_async(id);
    Ok(None)
}

fn native_start_context_complete(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    global_container().complete_async(id);
    Ok(None)
}

fn native_service_container_shutdown(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    global_container().shutdown();
    Ok(None)
}

/// R63 (WildFly): `DelegatingBasicLogger.isTraceEnabled()` returns
/// `this.log.isTraceEnabled()`, but in our synthetic-logger fixups
/// for ServiceLogger/ElytronMessages the `log` field is null. The
/// SecurityDomain$Builder.build call at line 1100 invokes
/// `isTraceEnabled` on the synthetic ElytronMessages_$logger and NPEs.
/// Shim to return false (trace disabled) so trace-gated code paths
/// take the fast no-trace branch. Same shim covers isDebugEnabled —
/// many WildFly call sites follow the same null-log pattern.
fn native_delegating_logger_returns_false(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// R63 (WildFly): `ServiceLogger_$logger.greeting(String)` is the
/// jboss-logging-generated boot banner emitter. Its bytecode does
/// `this.log.logf(FQCN, INFO, null, "JBoss MSC version %s", arg)` —
/// but the `log` instance field is null in our R55 synthetic backfill
/// of `ServiceLogger.ROOT/SERVICE/FAIL` (we couldn't materialize a
/// real `org.jboss.logging.Logger` proxy in the post-clinit fixup).
/// The `invokevirtual Logger.logf` on null then NPEs at line 41
/// (bci=16) inside `ServiceContainerImpl.<clinit>` line 88. The outer
/// <clinit> swallow keeps the VM alive but leaves SCI's late statics
/// unassigned — every subsequent SCI<init> trips on getstatic. By
/// shimming `greeting` as a no-op we let SCI<clinit> reach all its
/// `putstatic`s (executorSeq, SERIAL, ...) under its own clinit, so
/// the R55 post-clinit fixup is no longer the load-bearing path.
fn native_service_logger_greeting_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

/// Bind an externally-allocated `ServiceController` Java object to a
/// controller id (used by T19.2 subsystem glue to hand a pre-built
/// controller back to the interpreter).
#[allow(dead_code)]
pub fn bind_controller_id(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    id: u64,
) {
    ctx.set_field(obj, SC_FIELD_ID, Value::Long(id as i64));
}

// ---------------------------------------------------------------------------
// Registration — called from `lib.rs` once the essential natives have been
// wired up.
// ---------------------------------------------------------------------------

pub fn register_jboss_msc_natives(r: &mut NativeMethodRegistry) {
    let sn = "org/jboss/msc/service/ServiceName";
    r.register(
        sn,
        "of",
        "(Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        native_service_name_of_string,
    );
    r.register(
        sn,
        "of",
        "([Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        native_service_name_of_varargs,
    );
    r.register(
        sn,
        "append",
        "(Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        native_service_name_append,
    );
    r.register(
        sn,
        "getCanonicalName",
        "()Ljava/lang/String;",
        native_service_name_get_canonical,
    );
    r.register(
        sn,
        "getParent",
        "()Lorg/jboss/msc/service/ServiceName;",
        native_service_name_get_parent,
    );

    let cont = "org/jboss/msc/service/ServiceContainer";
    r.register(
        cont,
        "create",
        "()Lorg/jboss/msc/service/ServiceContainer;",
        native_service_container_create,
    );
    let factory = "org/jboss/msc/service/ServiceContainer$Factory";
    r.register(
        factory,
        "create",
        "()Lorg/jboss/msc/service/ServiceContainer;",
        native_service_container_create,
    );
    r.register(
        cont,
        "addService",
        "(Lorg/jboss/msc/service/ServiceName;Lorg/jboss/msc/Service;)Lorg/jboss/msc/service/ServiceController;",
        native_service_container_add_service,
    );
    r.register(
        cont,
        "shutdown",
        "()V",
        native_service_container_shutdown,
    );

    let ctrl = "org/jboss/msc/service/ServiceController";
    r.register(
        ctrl,
        "setMode",
        "(Lorg/jboss/msc/service/ServiceController$Mode;)V",
        native_service_controller_set_mode,
    );
    r.register(
        ctrl,
        "getState",
        "()Lorg/jboss/msc/service/ServiceController$State;",
        native_service_controller_get_state,
    );

    let start_ctx = "org/jboss/msc/service/StartContext";
    r.register(
        start_ctx,
        "asynchronous",
        "()V",
        native_start_context_asynchronous,
    );
    r.register(
        start_ctx,
        "complete",
        "()V",
        native_start_context_complete,
    );

    // R63: silence the boot-banner NPE inside SCI<clinit>. The bytecode
    // calls `ServiceLogger_$logger.greeting(String)` via invokeinterface
    // on `ServiceLogger.ROOT`, which dispatches to the generated impl;
    // the impl's first instruction reads `this.log` (null in our
    // post-clinit backfill) and NPEs. Returning a no-op from the impl
    // method lets SCI<clinit> continue past line 88.
    let logger_impl = "org/jboss/msc/service/ServiceLogger_$logger";
    r.register(
        logger_impl,
        "greeting",
        "(Ljava/lang/String;)V",
        native_service_logger_greeting_noop,
    );

    // R71 (WildFly): same `this.log == null` NPE pattern, but on the
    // service-failure code path. After MSC ServiceContainer install
    // completes, StartTask.startService eventually invokes
    // ControllerTask.run, whose catch(Throwable) block dispatches
    // `ServiceLogger.SERVICE.internalServiceError(t, name)` —
    // dispatches to ServiceLogger_$logger.internalServiceError, whose
    // first instruction is `this.log.logf(...)` and NPEs on null log.
    // Register no-op shims for every error/diagnostic method in the
    // ServiceLogger_$logger generated impl so any failure-path log
    // call from MSC degrades to silence rather than NPE.
    for (name, sig) in [
        ("startFailed", "(Lorg/jboss/msc/service/StartException;Lorg/jboss/msc/service/ServiceName;)V"),
        ("listenerFailed", "(Ljava/lang/Throwable;Ljava/lang/Object;)V"),
        ("exceptionAfterComplete", "(Ljava/lang/Throwable;Lorg/jboss/msc/service/ServiceName;)V"),
        ("stopFailed", "(Ljava/lang/Throwable;Lorg/jboss/msc/service/ServiceName;)V"),
        ("stopServiceMissing", "(Lorg/jboss/msc/service/ServiceName;)V"),
        ("uninjectFailed", "(Ljava/lang/Throwable;Lorg/jboss/msc/service/ServiceName;Lorg/jboss/msc/service/ValueInjection;)V"),
        ("internalServiceError", "(Ljava/lang/Throwable;Lorg/jboss/msc/service/ServiceName;)V"),
        ("uncaughtException", "(Ljava/lang/Throwable;Ljava/lang/Thread;)V"),
        ("profileOutputCloseFailed", "(Ljava/io/IOException;)V"),
        ("mbeanFailed", "(Ljava/lang/Exception;)V"),
        ("injectFailed", "(Ljava/lang/Throwable;Lorg/jboss/msc/service/ServiceName;)V"),
        ("mbeanServerNotAvailable", "(Ljava/lang/Exception;)V"),
    ] {
        r.register(logger_impl, name, sig, native_service_logger_greeting_noop);
    }

    // R63 (WildFly): shim DelegatingBasicLogger.isTraceEnabled/isDebugEnabled
    // to return false. The default impls do `this.log.isTraceEnabled()`,
    // but our synthetic backfills for ServiceLogger.ROOT/SERVICE/FAIL and
    // ElytronMessages.log leave `this.log` null. Returning false makes
    // every "if (log.isTraceEnabled()) ..." guard skip the inner work,
    // which is what we want for a non-logging boot.
    let dbl = "org/jboss/logging/DelegatingBasicLogger";
    r.register(dbl, "isTraceEnabled", "()Z", native_delegating_logger_returns_false);
    r.register(dbl, "isDebugEnabled", "()Z", native_delegating_logger_returns_false);
    r.register(dbl, "isInfoEnabled", "()Z", native_delegating_logger_returns_false);

    let _ = CTX_NUM_SLOTS; // silence unused constant when debug builds elide.
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: reset the intern table between tests so each run is
    // deterministic.  The global container can't be reset (it's bound
    // to a `OnceLock`) — instead the tests use fresh `ServiceName`s
    // prefixed with the test name so they don't collide.
    fn name(s: &str) -> Arc<ServiceName> {
        ServiceName::parse(s)
    }

    #[test]
    fn t19_1_service_name_of_single_segment() {
        let n = ServiceName::of(["jboss"]);
        assert_eq!(n.canonical(), "jboss");
        assert_eq!(n.len(), 1);
        assert!(n.parent().is_none());
    }

    #[test]
    fn t19_1_service_name_append_produces_child() {
        let p = ServiceName::of(["jboss", "as"]);
        let c = p.append("server");
        assert_eq!(c.canonical(), "jboss.as.server");
        assert_eq!(c.len(), 3);
        let pa = c.parent().expect("parent should exist");
        assert_eq!(pa.canonical(), "jboss.as");
        // Interning => pointer equality for same canonical string.
        assert!(Arc::ptr_eq(&p, &pa));
    }

    #[test]
    fn t19_1_service_container_add_service_returns_controller() {
        let c = ServiceContainer::new();
        let n = name("t19_1_add.a");
        let id = c
            .add_service(n.clone(), vec![], Mode::Active, 0xAA)
            .expect("add_service should succeed");
        assert!(id > 0);
        assert_eq!(c.size(), 1);
        assert!(c.get_state(&n).is_some());
    }

    #[test]
    fn t19_1_service_state_transitions_new_to_up_when_started() {
        let c = ServiceContainer::new();
        let n = name("t19_1_transition.x");
        c.add_service(n.clone(), vec![], Mode::Active, 1)
            .expect("install");
        c.drain_tasks_locally();
        assert_eq!(c.get_state(&n), Some(ServiceState::Up));
        assert_eq!(c.count_in(ServiceState::Up), 1);
    }

    #[test]
    fn t19_1_service_mode_on_demand_stays_down_until_dependent_needs_it() {
        let c = ServiceContainer::new();
        let n = name("t19_1_ondemand.svc");
        c.add_service(n.clone(), vec![], Mode::OnDemand, 0)
            .expect("install");
        c.drain_tasks_locally();
        // OnDemand without a demander stays Down.
        assert_eq!(c.get_state(&n), Some(ServiceState::Down));
        // Now demand it — drives the transition.
        c.demand(&n);
        c.drain_tasks_locally();
        assert_eq!(c.get_state(&n), Some(ServiceState::Up));
    }

    #[test]
    fn t19_1_service_dependency_transitive_start() {
        let c = ServiceContainer::new();
        let a = name("t19_1_trans.a");
        let b = name("t19_1_trans.b");
        let d = name("t19_1_trans.d");
        c.add_service(a.clone(), vec![], Mode::Active, 1).unwrap();
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 2).unwrap();
        c.add_service(d.clone(), vec![b.clone()], Mode::Active, 3).unwrap();
        c.drain_tasks_locally();
        // Keep draining: scheduling dependents may enqueue fresh work.
        for _ in 0..8 {
            c.drain_tasks_locally();
        }
        assert_eq!(c.get_state(&a), Some(ServiceState::Up));
        assert_eq!(c.get_state(&b), Some(ServiceState::Up));
        assert_eq!(c.get_state(&d), Some(ServiceState::Up));
    }

    #[test]
    fn t19_1_async_start_context_complete_transitions_to_up() {
        let c = ServiceContainer::new();
        let n = name("t19_1_async.svc");
        let id = c
            .add_service(n.clone(), vec![], Mode::Active, 0)
            .expect("install");
        // Simulate a Java service that called StartContext.asynchronous() —
        // transition to Starting, mark async, and then never let the sync
        // drain finalize it.
        {
            let mut st = c.inner.lock().unwrap();
            let nm = st.by_id.get(&id).cloned().unwrap();
            let ctrl = st.services.get_mut(&nm).unwrap();
            ctrl.state = ServiceState::Starting;
            ctrl.async_pending = true;
        }
        c.drain_tasks_locally();
        assert_eq!(c.get_state(&n), Some(ServiceState::Starting));
        // Complete async → Up.
        c.complete_async(id);
        assert_eq!(c.get_state(&n), Some(ServiceState::Up));
    }

    #[test]
    fn t19_1_service_container_shutdown_stops_all_in_reverse_dep_order() {
        let c = ServiceContainer::new();
        let a = name("t19_1_shut.a");
        let b = name("t19_1_shut.b");
        let d = name("t19_1_shut.d");
        c.add_service(a.clone(), vec![], Mode::Active, 1).unwrap();
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 2).unwrap();
        c.add_service(d.clone(), vec![b.clone()], Mode::Active, 3).unwrap();
        for _ in 0..6 {
            c.drain_tasks_locally();
        }
        assert_eq!(c.count_in(ServiceState::Up), 3);
        c.shutdown();
        // All services should be Down now.
        assert_eq!(c.count_in(ServiceState::Up), 0);
        assert_eq!(c.count_in(ServiceState::Down), 3);
    }

    #[test]
    fn t19_1_circular_dependency_rejected() {
        let c = ServiceContainer::new();
        let a = name("t19_1_cycle.a");
        let b = name("t19_1_cycle.b");
        c.add_service(a.clone(), vec![], Mode::Active, 0).unwrap();
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 0).unwrap();
        // Now adding a->[b] would cycle.  Installing a second `a`
        // isn't possible (duplicate name), so install `c` that depends
        // on `b`, then try to add a fresh `x` depending on c whose
        // deps loop back through a... easier: call `would_cycle`
        // directly.
        let state = c.inner.lock().unwrap();
        let cycle = would_cycle(&state.services, &a, &[b.clone()]);
        assert!(cycle, "inserting a -> b (with existing b -> a) must cycle");
    }

    #[test]
    fn t19_1_service_name_interning_ptr_equality() {
        let a = ServiceName::of(["x", "y", "z"]);
        let b = ServiceName::of(["x", "y", "z"]);
        let c = ServiceName::parse("x.y.z");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn t19_1_reverse_topo_order_visits_leaves_first() {
        let c = ServiceContainer::new();
        let a = name("t19_1_topo.a");
        let b = name("t19_1_topo.b");
        let d = name("t19_1_topo.d");
        c.add_service(a.clone(), vec![], Mode::Active, 0).unwrap();
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 0).unwrap();
        c.add_service(d.clone(), vec![b.clone()], Mode::Active, 0).unwrap();
        let state = c.inner.lock().unwrap();
        let order = reverse_topo_order(&state.services);
        // Every name appears exactly once.
        assert_eq!(order.len(), 3);
        // Each leaf must come strictly before its dependency root — so
        // `d` (top of chain) comes before `b`, and `b` before `a`.
        let idx_a = order.iter().position(|n| n == &a).unwrap();
        let idx_b = order.iter().position(|n| n == &b).unwrap();
        let idx_d = order.iter().position(|n| n == &d).unwrap();
        assert!(idx_d < idx_b, "d should come before b in reverse topo");
        assert!(idx_b < idx_a, "b should come before a in reverse topo");
    }

    #[test]
    fn t19_1_record_failure_transitions_to_failed() {
        let c = ServiceContainer::new();
        let n = name("t19_1_fail.svc");
        let id = c
            .add_service(n.clone(), vec![], Mode::Active, 0)
            .unwrap();
        // Force into Starting then fail.
        {
            let mut st = c.inner.lock().unwrap();
            let nm = st.by_id.get(&id).cloned().unwrap();
            st.services.get_mut(&nm).unwrap().state = ServiceState::Starting;
        }
        c.record_failure(id, "boom".into());
        assert_eq!(c.get_state(&n), Some(ServiceState::Failed));
    }

    #[test]
    fn t19_1_mode_and_state_string_forms_match_jdk_enum() {
        assert_eq!(Mode::Active.as_str(), "ACTIVE");
        assert_eq!(Mode::OnDemand.as_str(), "ON_DEMAND");
        assert_eq!(ServiceState::Up.as_str(), "UP");
        assert_eq!(ServiceState::Down.as_str(), "DOWN");
        assert_eq!(ServiceState::Failed.as_str(), "FAILED");
        assert_eq!(Mode::parse("ACTIVE"), Mode::Active);
        assert_eq!(Mode::parse("ON_DEMAND"), Mode::OnDemand);
        assert_eq!(Mode::parse("garbage"), Mode::Active);
    }

    #[test]
    fn t19_1_passive_mode_schedules_like_active() {
        let c = ServiceContainer::new();
        let n = name("t19_1_passive.svc");
        c.add_service(n.clone(), vec![], Mode::Passive, 0).unwrap();
        c.drain_tasks_locally();
        assert_eq!(c.get_state(&n), Some(ServiceState::Up));
    }

    #[test]
    fn t19_1_never_mode_stays_down_even_under_demand() {
        let c = ServiceContainer::new();
        let n = name("t19_1_never.svc");
        c.add_service(n.clone(), vec![], Mode::Never, 0).unwrap();
        c.demand(&n);
        c.drain_tasks_locally();
        // Never mode bypasses demand entirely.
        assert_eq!(c.get_state(&n), Some(ServiceState::Down));
    }
}
