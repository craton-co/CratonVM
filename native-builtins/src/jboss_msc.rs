// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

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

    /// P2 (real `start()` drive): pick one service that is ready to start —
    /// `Down`/`New`, an auto-start mode (`Active`/`Passive`), and every
    /// dependency already `Up` — transition it to `Starting`, and return its
    /// id. Returns `None` when nothing is ready.
    ///
    /// Unlike [`Self::drain_tasks_locally`] this scans the whole service map
    /// rather than the task queue, so it is robust to services installed
    /// before their dependencies (the `add_service` back-edge wiring only
    /// records a dependent when the dependency already exists). The drive
    /// loop calls this repeatedly; re-entrant `addService`/`install` calls
    /// made by a running `start()` just add more services that a later scan
    /// picks up.
    ///
    /// The caller must NOT hold any container lock across the subsequent
    /// `start()` invocation; this method takes and releases `inner` itself.
    fn take_ready_start(&self) -> Option<u64> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut chosen: Option<(Arc<ServiceName>, u64)> = None;
        for (name, c) in state.services.iter() {
            if matches!(c.state, ServiceState::Down | ServiceState::New)
                && matches!(c.mode, Mode::Active | Mode::Passive)
                && can_start(&state.services, name)
            {
                chosen = Some((name.clone(), c.id));
                break;
            }
        }
        let (name, id) = chosen?;
        if let Some(c) = state.services.get_mut(&name) {
            c.state = ServiceState::Starting;
        }
        state.in_flight.insert(id);
        Some(id)
    }

    /// P2: finish a `start()` that returned normally. Unless the service
    /// marked itself async-pending (`StartContext.asynchronous()`), transition
    /// `Starting → Up` so dependents become start-eligible on the next scan.
    fn finish_start(&self, id: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let name = match state.by_id.get(&id).cloned() {
            Some(n) => n,
            None => return,
        };
        let async_pending = state
            .services
            .get(&name)
            .map(|c| c.async_pending)
            .unwrap_or(false);
        if async_pending {
            // Stays `Starting`; the service's later `complete()` finishes it.
            return;
        }
        if let Some(c) = state.services.get_mut(&name) {
            if matches!(c.state, ServiceState::Starting) {
                c.state = ServiceState::Up;
                c.failure_message = None;
            }
        }
        state.in_flight.remove(&id);
        schedule_dependents_of(&mut state, &name, &self.pool_cv);
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
/// `Arc<ServiceName>` via its canonical String.
///
/// In **real-JDK mode** the loaded `org.jboss.msc.service.ServiceName`
/// class declares four instance fields in this order:
///   0. `name`         : `String`  (the leaf segment)
///   1. `canonicalName`: `String`  (the dotted full name; written via the
///                                  `canonicalNameUpdater` AtomicRef CAS)
///   2. `parent`       : `ServiceName`
///   3. `hashCode`     : `int`     (cached, computed in the ctor)
///
/// Real Java bytecode (e.g. `ServiceName.equals(ServiceName)` at
/// `ServiceName.java:230`) reads both `name` (slot 0) and `hashCode`
/// (slot 3). When we leave `name` at its default null, the `equals`
/// fast path triggers `null.equals(o.name)` → `NullPointerException:
/// Cannot invoke equals on null`. WildFly's
/// `ConsoleAvailabilityService.addService` is the first caller that
/// trips this because it compares two synthesised
/// `getCapabilityServiceName()` results inside
/// `ServiceBuilderImpl.assertNotInstanceId`.
///
/// The fix populates the `name` field with the leaf segment (the part
/// after the last `.`) and seeds `hashCode` with the canonical name's
/// hash, so JDK equals/hashCode behave consistently across all of our
/// synthetic mirrors. Fields are addressed by name so this stays
/// correct regardless of the loaded class's exact field layout.
pub(crate) fn alloc_java_service_name(
    ctx: &mut dyn NativeContext,
    name: &Arc<ServiceName>,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceName", 2);
    let canonical_text = name.canonical();
    let canonical = ctx.create_string(canonical_text);
    // canonicalName is updated via an AtomicReferenceFieldUpdater in the
    // JDK ctor; using `set_field_by_name` is safe — it writes the same
    // slot the updater would CAS into.
    ctx.set_field_by_name(obj, "canonicalName", Value::Object(Some(canonical)));
    // Leaf segment for the `name` field. Use the whole canonical text if
    // there are no dots (matches the JDK ctor for a top-level
    // ServiceName.of(String)).
    let leaf = canonical_text.rsplit('.').next().unwrap_or(canonical_text);
    let leaf_str = ctx.create_string(leaf);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(leaf_str)));
    // Stable, deterministic hash that matches our equals contract on
    // synthetic mirrors. The exact value does not need to mirror the
    // JDK's `calculateHashCode` — `ServiceName.equals` compares both
    // sides' `hashCode` fields, so as long as two mirrors of the same
    // canonical name produce the same value we're good.
    let h: i32 = canonical_text.bytes().fold(0i32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as i32));
    ctx.set_field_by_name(obj, "hashCode", Value::Int(h));
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

/// R63 (WildFly): `org/jboss/msc/service/Lockable.acquireWrite()/acquireRead()`
/// uses a hand-rolled AQS-style wait loop that calls `Object.wait()` when the
/// lock is contended. CratonVM's interpreter is effectively single-threaded for
/// MSC boot — the MSC ContainerExecutor runs tasks inline on the main thread —
/// so a recursive task path can re-enter `acquireWrite` while the bit is already
/// set. There is no other thread to call `notify()`, so `Object.wait()` blocks
/// forever. Since all "concurrent" task execution happens on one thread, locking
/// is unnecessary: shim acquire/release to no-ops so MSC boot proceeds.
fn native_lockable_lock_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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

/// R79 (WildFly): MSC's `IdentityHashSet$IdentityHashSetIterator.next()`
/// throws `ConcurrentModificationException` when `this$0.modCount`
/// drifts from the iterator's `expectedCount`. This happens during
/// `ServiceControllerImpl$RemoveChildrenTask.execute`, which walks
/// `controller.children` while concurrently calling `setMode(REMOVE)`
/// on each child — setMode propagates back into the same children set
/// and bumps modCount. The real MSC handles this via per-controller
/// `synchronized` blocks across distinct executor threads, but our
/// interpreter executes the worker tasks more eagerly and trips the
/// check. We replace `hasNext` and `next` with CME-tolerant variants
/// that re-read `this$0.table` / `this$0.modCount` on every call and
/// keep `expectedCount` in sync so the bytecode never trips the
/// `modCount != expectedCount` branch.
///
/// Field layout (matches jboss-msc 1.5.x bytecode):
/// * `this$0`        : outer `IdentityHashSet`
/// * `next`          : int, scan cursor
/// * `expectedCount` : int, copy of `this$0.modCount` at iterator construction
/// * `current`       : int, index of last returned element
/// * `hasNext`       : boolean, cached `hasNext()` result
/// * `table`         : `Object[]`, cached copy of `this$0.table`
fn ihs_iter_refresh_table(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    // Always re-read table from the outer set — concurrent resize
    // would otherwise leave us iterating a stale snapshot.
    let outer = match ctx.get_field_by_name(this, "this$0") {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let table = match ctx.get_field_by_name(outer, "table") {
        Value::Object(Some(t)) => t,
        _ => return None,
    };
    ctx.set_field_by_name(this, "table", Value::Object(Some(table)));
    // Keep expectedCount aligned so any *other* code path that reads
    // it (or the bytecode `next()` fallback) won't trip the CME check.
    let modc = match ctx.get_field_by_name(outer, "modCount") {
        Value::Int(i) => i,
        _ => 0,
    };
    ctx.set_field_by_name(this, "expectedCount", Value::Int(modc));
    Some(table)
}

fn native_ihs_iter_has_next(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // If we've already advanced and have a cached hit, return it.
    if let Value::Int(1) = ctx.get_field_by_name(this, "hasNext") {
        return Ok(Some(Value::Int(1)));
    }
    let table = match ihs_iter_refresh_table(ctx, this) {
        Some(t) => t,
        None => return Ok(Some(Value::Int(0))),
    };
    let len = ctx.array_length(table);
    let mut idx = match ctx.get_field_by_name(this, "next") {
        Value::Int(i) => i.max(0) as usize,
        _ => 0,
    };
    while idx < len {
        match ctx.get_array_element(table, idx) {
            Value::Object(Some(_)) => {
                ctx.set_field_by_name(this, "next", Value::Int(idx as i32));
                ctx.set_field_by_name(this, "hasNext", Value::Int(1));
                return Ok(Some(Value::Int(1)));
            }
            _ => idx += 1,
        }
    }
    ctx.set_field_by_name(this, "next", Value::Int(len as i32));
    ctx.set_field_by_name(this, "hasNext", Value::Int(0));
    Ok(Some(Value::Int(0)))
}

fn native_ihs_iter_next(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Re-sync table + expectedCount; advance to a non-null slot.
    let cached_has = matches!(ctx.get_field_by_name(this, "hasNext"), Value::Int(1));
    if !cached_has {
        // Inline hasNext() — same logic as native_ihs_iter_has_next.
        let table = match ihs_iter_refresh_table(ctx, this) {
            Some(t) => t,
            None => {
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::NoSuchElementException {
                        message: "IdentityHashSet iterator exhausted".to_string(),
                    },
                )));
            }
        };
        let len = ctx.array_length(table);
        let mut idx = match ctx.get_field_by_name(this, "next") {
            Value::Int(i) => i.max(0) as usize,
            _ => 0,
        };
        let mut found = false;
        while idx < len {
            if let Value::Object(Some(_)) = ctx.get_array_element(table, idx) {
                ctx.set_field_by_name(this, "next", Value::Int(idx as i32));
                ctx.set_field_by_name(this, "hasNext", Value::Int(1));
                found = true;
                break;
            }
            idx += 1;
        }
        if !found {
            ctx.set_field_by_name(this, "next", Value::Int(len as i32));
            ctx.set_field_by_name(this, "hasNext", Value::Int(0));
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NoSuchElementException {
                    message: "IdentityHashSet iterator exhausted".to_string(),
                },
            )));
        }
    } else {
        // hasNext was cached true: still re-sync expectedCount/table
        // so a concurrent modCount bump doesn't bite a subsequent call.
        let _ = ihs_iter_refresh_table(ctx, this);
    }
    let table = match ctx.get_field_by_name(this, "table") {
        Value::Object(Some(t)) => t,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NoSuchElementException {
                    message: "IdentityHashSet iterator: null table".to_string(),
                },
            )));
        }
    };
    let next_idx = match ctx.get_field_by_name(this, "next") {
        Value::Int(i) => i,
        _ => 0,
    };
    let len = ctx.array_length(table) as i32;
    if next_idx < 0 || next_idx >= len {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NoSuchElementException {
                message: "IdentityHashSet iterator past end".to_string(),
            },
        )));
    }
    let value = ctx.get_array_element(table, next_idx as usize);
    // current = next; next++; hasNext = false
    ctx.set_field_by_name(this, "current", Value::Int(next_idx));
    ctx.set_field_by_name(this, "next", Value::Int(next_idx + 1));
    ctx.set_field_by_name(this, "hasNext", Value::Int(0));
    // Skip nulls between current and next call (best-effort) and keep
    // expectedCount synced — the outer set may have shifted entries.
    Ok(Some(value))
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

// ===========================================================================
// P1 — GC roots for service objects held only by the Rust container.
//
// The container references Java objects (the `Service` instance whose
// `start()`/`stop()` we invoke, the synthetic `ServiceController` mirror, the
// child `ServiceTarget`, the in-flight `StartContext`) that live ONLY in
// process-global Rust side-tables — invisible to the frame / static / heap
// root scans. Without rooting them, a moving young GC reclaims or relocates
// them while the container still holds the address, and the next
// `invoke_virtual(service, "start", ...)` is a use-after-free (same class of
// bug as the cached-ClassLoader root gap). Mirrors the `lang_math` /
// `classloader` process-global cache root pattern (`roots.rs` steps 15–18 +
// the `gc.rs` companion remaps).
// ===========================================================================

/// GC-visible references held per controller id. Every `Some` field is a root
/// and a post-move remap target.
#[derive(Default, Clone, Copy)]
struct ServiceRoots {
    /// The Java `org.jboss.msc.Service` instance to drive `start`/`stop` on.
    service: Option<ObjectRef>,
    /// The synthetic `ServiceController` mirror returned by `install()` and
    /// `StartContext.getController()`.
    controller_mirror: Option<ObjectRef>,
    /// The real `ServiceTargetImpl` to hand back from
    /// `StartContext.getChildTarget()` so child installs route to the same
    /// (working) target.
    child_target: Option<ObjectRef>,
    /// The synthetic `StartContext` (kept live while the service is `Starting`
    /// / async-pending so a later `complete()` can still reach it).
    start_context: Option<ObjectRef>,
}

/// Process-global side-table: controller id → held Java refs.
fn service_roots() -> &'static Mutex<HashMap<u64, ServiceRoots>> {
    static T: OnceLock<Mutex<HashMap<u64, ServiceRoots>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// GC root scan for the container-held service objects. Companion remap is
/// [`gc_update_msc_service_refs`]. Uses a blocking lock (never held across a
/// Java allocation, so the allocating thread can never self-deadlock here).
pub fn gc_scan_msc_service_roots(out: &mut Vec<ObjectRef>) {
    let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
    for r in map.values() {
        for o in [r.service, r.controller_mirror, r.child_target, r.start_context]
            .into_iter()
            .flatten()
        {
            out.push(o);
        }
    }
}

/// Post-GC remap for the container-held service objects (companion to
/// [`gc_scan_msc_service_roots`]). After a moving collection the held objects
/// relocate; repoint every stored `ObjectRef` to its new address.
pub fn gc_update_msc_service_refs(pointer_map: &std::collections::HashMap<usize, usize>) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |slot: &mut Option<ObjectRef>| {
        if let Some(obj_ref) = slot.as_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    };
    let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
    for r in map.values_mut() {
        remap(&mut r.service);
        remap(&mut r.controller_mirror);
        remap(&mut r.child_target);
        remap(&mut r.start_context);
    }
}

// ===========================================================================
// P2/P3 — drive the real Java `service.start(StartContext)` callback.
//
// Gated behind `CRATONVM_MSC_REAL_START` (default-OFF): when off, the
// `ServiceBuilderImpl.install()` / `StartContext.getController()` etc. natives
// below are NOT registered, so WildFly's real MSC bytecode runs exactly as
// before (no regression risk). When on, `install()` extracts the service from
// the builder, registers it in the Rust container, and drives `start()`.
// `CRATONVM_DBG_MSC` traces the install/start sequence.
// ===========================================================================

/// Cached check of the `CRATONVM_MSC_REAL_START` gate.
fn msc_real_start_enabled() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("CRATONVM_MSC_REAL_START").is_some())
}

/// Cached check of the `CRATONVM_DBG_MSC` trace flag.
fn msc_dbg() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("CRATONVM_DBG_MSC").is_some())
}

thread_local! {
    /// Re-entrancy guard: only the outermost `install()` drives the start
    /// loop; nested `install()` calls made by a running `start()` just register
    /// + return, and the outer loop drains them iteratively.
    static DRIVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Read a jboss-msc `ServiceController$Mode` enum object by its `name` field
/// (robust to ordinal differences across MSC versions). Null → `Active`.
fn read_mode_by_name(ctx: &mut dyn NativeContext, mode_obj: Option<ObjectRef>) -> Mode {
    let m = match mode_obj {
        Some(o) => o,
        None => return Mode::Active,
    };
    match ctx.get_field_by_name(m, "name") {
        Value::Object(Some(s)) => ctx
            .read_string(s)
            .map(|t| Mode::parse(&t))
            .unwrap_or(Mode::Active),
        _ => Mode::Active,
    }
}

/// Read the dependency `ServiceName`s out of a `ServiceBuilderImpl.requires`
/// map (keys). Defensive: returns empty on any failure / null map.
/// Robustly extract a canonical `ServiceName` from a Java `ServiceName` object,
/// tolerating the real jboss-msc layout where `canonicalName` is computed
/// LAZILY (null until `getCanonicalName()` runs). Strategy:
///   1. the `canonicalName` field (populated by our `of()` natives, or lazily
///      by real code that already called `getCanonicalName()`),
///   2. the synthetic `canonical` field (synthetic-jdk layout),
///   3. reconstruct from the `name` (leaf) + `parent` chain — independent of
///      the lazy cache and of our `getCanonicalName` native override.
///
/// `read_java_service_name` (a slot-1 read) returns `None` for a real
/// ServiceName whose `canonicalName` cache is still null, which is exactly the
/// form WildFly's `AbstractControllerService` install passes — so the install
/// hook and dependency reader use this instead.
fn read_service_name_robust(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
) -> Option<Arc<ServiceName>> {
    for field in ["canonicalName", "canonical"] {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(obj, field) {
            if let Some(t) = ctx.read_string(s) {
                if !t.is_empty() {
                    return Some(ServiceName::parse(&t));
                }
            }
        }
    }
    // Reconstruct leaf-to-root from the name/parent chain.
    let mut segs: Vec<String> = Vec::new();
    let mut cur = Some(obj);
    let mut guard = 0u32;
    while let Some(o) = cur {
        guard += 1;
        if guard > 256 {
            break;
        }
        if let Value::Object(Some(s)) = ctx.get_field_by_name(o, "name") {
            if let Some(leaf) = ctx.read_string(s) {
                if !leaf.is_empty() {
                    segs.push(leaf);
                }
            }
        }
        cur = match ctx.get_field_by_name(o, "parent") {
            Value::Object(Some(p)) => Some(p),
            _ => None,
        };
    }
    if segs.is_empty() {
        return None;
    }
    segs.reverse();
    Some(ServiceName::of(segs))
}

/// Best-effort runtime class name of an object (for diagnostics).
fn obj_class_name(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Read the dependency `ServiceName`s out of a `ServiceBuilderImpl.requires`
/// map (keys). Defensive: returns empty on any failure / null map.
fn read_dep_names(
    ctx: &mut dyn NativeContext,
    requires_map: Option<ObjectRef>,
) -> Vec<Arc<ServiceName>> {
    let map = match requires_map {
        Some(m) => m,
        None => return Vec::new(),
    };
    // invoke_virtual prepends the receiver; `args` is parameters ONLY (none here).
    let set = match ctx.invoke_virtual(map, "keySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return Vec::new(),
    };
    let arr = match ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return Vec::new(),
    };
    let n = ctx.array_length(arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if let Value::Object(Some(sn)) = ctx.get_array_element(arr, i) {
            if let Some(name) = read_service_name_robust(ctx, sn) {
                out.push(name);
            }
        }
    }
    out
}

/// Allocate a synthetic `StartContext` carrying `controller_id`, and root it.
fn build_start_context(ctx: &mut dyn NativeContext, id: u64) -> ObjectRef {
    let sctx = alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/StartContext", CTX_NUM_SLOTS);
    ctx.set_field(sctx, CTX_FIELD_CONTROLLER_ID, Value::Long(id as i64));
    {
        let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.entry(id).or_default().start_context = Some(sctx);
    }
    sctx
}

/// Iteratively drive every ready service's real `start()` callback until none
/// remain. NEVER holds the container lock across `invoke_virtual`. The receiver
/// and `StartContext` are kept live across the call by the invoked frame's
/// own root scan; container-held refs are re-read from `service_roots` (which
/// the GC remaps) rather than from stale locals.
///
/// Fault handling (B7): a failing `start()` is never silently swallowed. The
/// service's name + failure are always surfaced via `tracing::error!` (not
/// gated behind the `CRATONVM_MSC_DBG` env), and the service is marked `Failed`
/// in the container so a half-started graph cannot present as healthy. We then
/// distinguish the two failure kinds:
///   * [`MethodCallFailed::ExceptionThrown`] — a real Java exception (e.g.
///     `StartException`). Real MSC catches these at the framework boundary and
///     marks the service `Failed` *without* aborting `install()`; dependents
///     stay blocked but independent services continue. We mirror that: record +
///     continue (now logged), so behaviour stays MSC-faithful.
///   * [`MethodCallFailed::InternalError`] — an uncatchable Rust/VM-level error.
///     There is no MSC "catch" for this; swallowing it would mask a real VM
///     defect. We propagate it out so the caller (`install()`) returns the
///     error through its result path instead of reporting a successful boot.
fn drive_starts(
    ctx: &mut dyn NativeContext,
    container: &ServiceContainer,
) -> Result<(), MethodCallFailed> {
    let mut guard: u32 = 0;
    loop {
        guard += 1;
        if guard > 200_000 {
            tracing::warn!(
                target: "jboss_msc",
                "drive_starts: re-entrancy guard limit hit ({guard}), stopping"
            );
            break;
        }
        let id = match container.take_ready_start() {
            Some(i) => i,
            None => break,
        };
        let svc = service_roots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .and_then(|r| r.service);
        let svc = match svc {
            Some(s) => s,
            None => {
                // Provides-only service (no `Service` instance): nothing to
                // run — transition straight to Up so dependents unblock.
                if msc_dbg() {
                    eprintln!("[msc] start id={id}: no service instance, marking Up");
                }
                container.finish_start(id);
                continue;
            }
        };
        let sctx = build_start_context(ctx, id);
        if msc_dbg() {
            eprintln!("[msc] -> start id={id}");
        }
        // invoke_virtual prepends the receiver; `args` is parameters ONLY.
        let res = ctx.invoke_virtual(
            svc,
            "start",
            "(Lorg/jboss/msc/service/StartContext;)V",
            &[Value::Object(Some(sctx))],
        );
        match res {
            Ok(_) => {
                if msc_dbg() {
                    eprintln!("[msc] <- start id={id} OK");
                }
                container.finish_start(id);
            }
            Err(e) => {
                // Always surface the failure (not just under CRATONVM_MSC_DBG):
                // record it in the container AND log it so a failed service is
                // never silently hidden.
                let name = {
                    let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state
                        .by_id
                        .get(&id)
                        .map(|n| n.canonical().to_string())
                        .unwrap_or_else(|| format!("<id {id}>"))
                };
                container.record_failure(id, format!("service start failed: {e:?}"));
                match e {
                    MethodCallFailed::ExceptionThrown(_) => {
                        // Catchable Java exception: MSC-faithful — mark Failed
                        // (done above) and keep draining other services.
                        tracing::error!(
                            target: "jboss_msc",
                            service = %name,
                            "MSC service start() threw — marked FAILED, boot continues: {e:?}"
                        );
                    }
                    MethodCallFailed::InternalError(_) => {
                        // Uncatchable VM-level error: do not swallow. Surface it
                        // through the result path so install() reports the real
                        // failure instead of a fake successful boot.
                        tracing::error!(
                            target: "jboss_msc",
                            service = %name,
                            "MSC service start() hit an internal VM error — propagating: {e:?}"
                        );
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(())
}

/// P2 hook: `org.jboss.msc.service.ServiceBuilderImpl.install()`.
///
/// Modern WildFly installs every service through the ServiceBuilder API
/// (`ServiceTarget.addService(name).provides(...).setInstance(svc).install()`),
/// NOT the legacy 2-arg `ServiceContainer.addService`. `install()` is the point
/// where the service object + name + deps are all attached, so we intercept it,
/// register the service in the Rust container, and drive its real `start()`.
fn native_service_builder_install(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let builder = obj_arg(args, 0)?;

    // ServiceBuilderImpl field layout (jboss-msc 1.5.x, reversed via `javap -p`):
    //   serviceId : ServiceName        (the primary name)
    //   service   : org.jboss.msc.Service (the instance to start; may be null)
    //   initialMode : ServiceController$Mode (null => ACTIVE)
    //   requires  : Map<ServiceName, Dependency>   (dependency names)
    //   serviceTarget : ServiceTargetImpl          (for getChildTarget)
    let sn_obj = match ctx.get_field_by_name(builder, "serviceId") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let name = sn_obj.and_then(|o| read_service_name_robust(ctx, o));
    let name = match name {
        Some(n) => n,
        None => {
            if msc_dbg() {
                let bcls = obj_class_name(ctx, builder);
                let sid = match sn_obj {
                    Some(o) => format!("ServiceName<{}>", obj_class_name(ctx, o)),
                    None => "null".to_string(),
                };
                eprintln!(
                    "[msc] install: unreadable serviceId (builder={bcls}, serviceId={sid}) — returning null controller"
                );
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    let service_ref = match ctx.get_field_by_name(builder, "service") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let mode = match ctx.get_field_by_name(builder, "initialMode") {
        Value::Object(o) => read_mode_by_name(ctx, o),
        _ => Mode::Active,
    };
    let child_target = match ctx.get_field_by_name(builder, "serviceTarget") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let deps = match ctx.get_field_by_name(builder, "requires") {
        Value::Object(Some(o)) => read_dep_names(ctx, Some(o)),
        _ => Vec::new(),
    };

    let container = global_container().clone();
    let id = match container.add_service(
        name.clone(),
        deps.clone(),
        mode,
        service_ref.map(|o| o.as_ptr() as usize).unwrap_or(0),
    ) {
        Ok(id) => id,
        Err(msg) => {
            if msc_dbg() {
                eprintln!("[msc] install {}: add_service error: {msg}", name.canonical());
            }
            return Ok(Some(Value::Object(None)));
        }
    };

    // Build the synthetic ServiceController mirror (returned to the caller and
    // from StartContext.getController()).
    let ctrl_obj = alloc_concurrent_synthetic(
        ctx,
        "org/jboss/msc/service/ServiceController",
        SC_NUM_SLOTS,
    );
    if let Some(sn) = sn_obj {
        ctx.set_field(ctrl_obj, SC_FIELD_NAME, Value::Object(Some(sn)));
    }
    ctx.set_field(ctrl_obj, SC_FIELD_MODE, Value::Int(mode.ordinal()));
    ctx.set_field(ctrl_obj, SC_FIELD_STATE, Value::Int(ServiceState::Down.ordinal()));
    ctx.set_field(ctrl_obj, SC_FIELD_VALUE, Value::Object(None));
    ctx.set_field(ctrl_obj, SC_FIELD_ID, Value::Long(id as i64));

    // Register every held Java ref as a GC root BEFORE driving start() (which
    // allocates heavily and can move/collect these).
    {
        let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        let r = map.entry(id).or_default();
        r.service = service_ref;
        r.controller_mirror = Some(ctrl_obj);
        r.child_target = child_target;
    }

    if msc_dbg() {
        eprintln!(
            "[msc] install id={id} name={} mode={mode:?} has_service={} deps={}",
            name.canonical(),
            service_ref.is_some(),
            deps.len()
        );
    }

    // Drive starts iteratively; only the outermost install drives. Nested
    // install() calls (made by a running start()) just register + return.
    // B7: an uncatchable VM-level start failure is propagated out of
    // `drive_starts` rather than swallowed — reset the re-entrancy flag and
    // return the error so a failed boot is surfaced, not faked as success.
    let was_driving = DRIVING.with(|d| d.replace(true));
    if !was_driving {
        let drive_res = drive_starts(ctx, &container);
        DRIVING.with(|d| d.set(false));
        drive_res?;
    }

    // Re-read the (possibly relocated) mirror from the GC-remapped side-table.
    let final_mirror = service_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|r| r.controller_mirror)
        .unwrap_or(ctrl_obj);
    Ok(Some(Value::Object(Some(final_mirror))))
}

/// `StartContext.getController()` → the synthetic ServiceController mirror.
fn native_start_context_get_controller(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let mirror = service_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|r| r.controller_mirror);
    Ok(Some(Value::Object(mirror)))
}

/// `StartContext.getChildTarget()` → the real ServiceTargetImpl captured from
/// the builder, so child installs route through the same working target.
fn native_start_context_get_child_target(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let tgt = service_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|r| r.child_target);
    Ok(Some(Value::Object(tgt)))
}

/// `StartContext.failed(StartException)` → mark the service failed.
fn native_start_context_failed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    global_container().record_failure(id, "service start failed (StartContext.failed)".to_string());
    Ok(None)
}

/// `ServiceController.getServiceContainer()` → a synthetic ServiceContainer
/// (forces the global container to exist; the mirror is a thin handle).
fn native_service_controller_get_service_container(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let _ = global_container();
    let obj = alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceContainer", 2);
    Ok(Some(Value::Object(Some(obj))))
}

// ---------------------------------------------------------------------------
// Registration — called from `lib.rs` once the essential natives have been
// wired up.
// ---------------------------------------------------------------------------

pub fn register_jboss_msc_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
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

    // R63 WildFly: Lockable acquire/release shims (see native_lockable_lock_noop).
    let lockable = "org/jboss/msc/service/Lockable";
    r.register(lockable, "acquireWrite", "()V", native_lockable_lock_noop);
    r.register(lockable, "acquireRead", "()V", native_lockable_lock_noop);
    r.register(lockable, "releaseWrite", "()V", native_lockable_lock_noop);
    r.register(lockable, "releaseRead", "()V", native_lockable_lock_noop);

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

    // R79 (WildFly): replace MSC IdentityHashSet's iterator with a
    // CME-tolerant native implementation. Eager worker scheduling in
    // our interpreter causes setMode(REMOVE) inside RemoveChildrenTask
    // to bump the outer set's modCount mid-iteration; the bytecode
    // `next()` then throws CME. The native re-reads the table and
    // re-syncs expectedCount on every call. See native_ihs_iter_next.
    let ihs_iter = "org/jboss/msc/service/IdentityHashSet$IdentityHashSetIterator";
    r.register(ihs_iter, "hasNext", "()Z", native_ihs_iter_has_next);
    r.register(ihs_iter, "next", "()Ljava/lang/Object;", native_ihs_iter_next);

    // R84 (WildFly): natively implement `org.jboss.logging.Logger.getMessageLogger`
    // overloads. The real implementation goes through `MethodHandles.lookup() +
    // privateLookupIn(intf) + Lookup.findClass("<intf>_$logger") +
    // Lookup.findConstructor(...)`. Our `MethodHandles$Lookup.accessClass` runs
    // the JDK Java code that calls `VerifyAccess.isClassAccessible`, which in
    // turn requires `isSamePackage(lookupClass, targetClass)` to match
    // ClassLoader identity AND package name. With our app/platform-loader
    // resolution (returning the singleton AppClassLoader for everything
    // non-bootstrap), this normally works — but the interface and the
    // generated `_$logger` impl can have subtly different module/loader views
    // during JBoss-Modules-mediated loading, causing IllegalAccessException →
    // IllegalArgumentException ("The given lookup does not have access to the
    // implementation class") to be thrown. The swallow then leaves the
    // resulting message-logger static fields null, cascading into NPEs
    // (already partially patched with backfills for ServiceLogger /
    // ElytronMessages — RemotingSubsystemRootResource and others have no
    // backfill, so their boot subsystems silently die).
    //
    // The native shim bypasses MethodHandles entirely:
    //   1. derive `<intf_name>_$logger` from the intf Class mirror,
    //   2. `Logger.getLogger(category)` to get the delegate log,
    //   3. `new <intf>_$logger(log)` (which super(log)s into DelegatingBasicLogger).
    //
    // Returns the freshly constructed message-logger or null on any error
    // (best-effort — caller usually stores into a static and downstream code
    // does null-tolerant `if (log == null) ...` checks via our shims).
    let logger = "org/jboss/logging/Logger";
    r.register(
        logger,
        "getMessageLogger",
        "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            let intf_mirror = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let category = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(native_construct_message_logger(ctx, intf_mirror, &category)))
        },
    );
    r.register(
        logger,
        "getMessageLogger",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/util/Locale;)Ljava/lang/Object;",
        |ctx, args| {
            let intf_mirror = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let category = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(native_construct_message_logger(ctx, intf_mirror, &category)))
        },
    );
    r.register(
        logger,
        "getMessageLogger",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Object;",
        |ctx, args| {
            // args: [lookup, intfClass, category]
            let intf_mirror = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let category = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(native_construct_message_logger(ctx, intf_mirror, &category)))
        },
    );
    r.register(
        logger,
        "getMessageLogger",
        "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/Class;Ljava/lang/String;Ljava/util/Locale;)Ljava/lang/Object;",
        |ctx, args| {
            let intf_mirror = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let category = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            Ok(Some(native_construct_message_logger(ctx, intf_mirror, &category)))
        },
    );

    // RWF86.2 — `LoggerProviders.findProvider()` short-circuit.  WildFly's
    // `Logger.<clinit>` triggers `LoggerProviders.<clinit>` which iterates
    // tryJBossLogManager -> tryLog4j2 -> trySlf4j -> tryLog4j -> tryJDK.
    // In our environment `java.util.logging.LogManager.getLogManager()`
    // returns the JDK default class (not `org.jboss.logmanager.LogManager`)
    // even though jboss-modules sets `java.util.logging.manager` — the JDK's
    // own bootstrap caches the LogManager singleton before the property is
    // set, so the if-acmp check in tryJBossLogManager pc=20 fails and
    // throws IllegalStateException.  The subsequent tryLog4j2 path NPEs
    // inside `logProvider` because the constructed Log4j2LoggerProvider
    // isn't fully wired in our env (LogManager.<clinit> -> ProviderUtil
    // ServiceLoader returns null).  Both downstream throwers cascade out
    // of `LoggerProviders.<clinit>` and abort WildFly boot at
    // `ServiceContainerImpl.<clinit>`.  Short-circuit by returning a fresh
    // `JDKLoggerProvider` instance — JDK Logger backing works fine via
    // our `LogManager.getLogger` native and matches what WildFly's
    // standalone.sh would set via `-Dorg.jboss.logging.provider=jdk`.
    r.register(
        "org/jboss/logging/LoggerProviders",
        "findProvider",
        "()Lorg/jboss/logging/LoggerProvider;",
        |ctx, _args| {
            let cls = "org/jboss/logging/JDKLoggerProvider";
            let obj = match ctx.new_object(cls) {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // <init>()V — AbstractLoggerProvider parent is null-safe.
            let _ = ctx.invoke(cls, "<init>", "()V", &[Value::Object(Some(obj))]);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // P2/P3 (WildFly real service.start): GATED behind CRATONVM_MSC_REAL_START
    // (default-OFF). When off, none of these are registered so WildFly's real
    // MSC bytecode runs unchanged. When on, we intercept ServiceBuilder.install
    // and drive the real start() callback. See native_service_builder_install.
    if msc_real_start_enabled() {
        let sbi = "org/jboss/msc/service/ServiceBuilderImpl";
        r.register(
            sbi,
            "install",
            "()Lorg/jboss/msc/service/ServiceController;",
            native_service_builder_install,
        );
        let start_ctx = "org/jboss/msc/service/StartContext";
        r.register(
            start_ctx,
            "getController",
            "()Lorg/jboss/msc/service/ServiceController;",
            native_start_context_get_controller,
        );
        r.register(
            start_ctx,
            "getChildTarget",
            "()Lorg/jboss/msc/service/ServiceTarget;",
            native_start_context_get_child_target,
        );
        r.register(
            start_ctx,
            "failed",
            "(Lorg/jboss/msc/service/StartException;)V",
            native_start_context_failed,
        );
        let ctrl = "org/jboss/msc/service/ServiceController";
        r.register(
            ctrl,
            "getServiceContainer",
            "()Lorg/jboss/msc/service/ServiceContainer;",
            native_service_controller_get_service_container,
        );
    }

    let _ = CTX_NUM_SLOTS; // silence unused constant when debug builds elide.
    r.set_category(__prev_cat);
}

/// Construct a `<intf>_$logger` instance natively, bypassing the
/// MethodHandles/Lookup path that JBoss Logging normally uses.
/// Returns `Value::Object(None)` on any failure.
fn native_construct_message_logger(
    ctx: &mut dyn NativeContext,
    intf_mirror: cratonvm_types::ObjectRef,
    category: &str,
) -> Value {
    // 1. Resolve the interface name.
    let intf_name = match crate::lang_class::mirror_class_name(ctx, intf_mirror) {
        Some(n) => n,
        None => return Value::Object(None),
    };
    let impl_name = format!("{intf_name}_$logger");

    // 2. Get the delegate Logger via `Logger.getLogger(category)`. Returns
    //    null on error — caller can still store the resulting object since
    //    most _$logger methods are null-tolerant via our shims.
    let cat_str = ctx.create_string(category);
    let log_obj = match ctx.invoke(
        "org/jboss/logging/Logger",
        "getLogger",
        "(Ljava/lang/String;)Lorg/jboss/logging/Logger;",
        &[Value::Object(Some(cat_str))],
    ) {
        Ok(Some(v)) => v,
        _ => Value::Object(None),
    };

    // 3. `new <impl_name>(log)` — the canonical generated constructor takes
    //    a single `Logger` parameter and `super(log)`s into
    //    `DelegatingBasicLogger`.
    let new_obj = match ctx.new_object(&impl_name) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Value::Object(None),
    };
    let init_args = [Value::Object(Some(new_obj)), log_obj];
    match ctx.invoke(
        &impl_name,
        "<init>",
        "(Lorg/jboss/logging/Logger;)V",
        &init_args,
    ) {
        Ok(_) => Value::Object(Some(new_obj)),
        Err(_) => {
            // Constructor failed — return the bare instance anyway. The
            // `log` field will be null but downstream NPE-tolerant shims
            // (DelegatingBasicLogger.is*Enabled, ServiceLogger_$logger
            // method no-ops) keep boot going. Beats a null return that
            // propagates into static fields whose readers do raw
            // invokeinterface without null checks.
            Value::Object(Some(new_obj))
        }
    }
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
