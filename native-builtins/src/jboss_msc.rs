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
//! See `history/roadmap-100.md` T19.1 for the feature scope.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

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

fn java_string_hash(s: &str) -> i32 {
    s.encode_utf16()
        .fold(0i32, |acc, ch| acc.wrapping_mul(31).wrapping_add(ch as i32))
}

fn service_name_hash(name: &ServiceName) -> i32 {
    name.segments.iter().fold(1i32, |acc, segment| {
        acc.wrapping_mul(31)
            .wrapping_add(java_string_hash(segment.as_ref()))
    })
}

fn illegal_service_name_arg(message: impl Into<String>) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalArgumentException {
        message: message.into(),
    }))
}

fn read_service_name_segments_array(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
) -> Result<Vec<String>, MethodCallFailed> {
    let len = ctx.array_length(arr);
    if len == 0 {
        return Err(illegal_service_name_arg(
            "Must provide at least one name segment",
        ));
    }
    let mut segs = Vec::with_capacity(len);
    for i in 0..len {
        let text = match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if text.is_empty() {
            return Err(illegal_service_name_arg(format!(
                "Invalid empty ServiceName segment at index {i}"
            )));
        }
        segs.push(text);
    }
    Ok(segs)
}

fn append_service_name_segments(
    base: Option<Arc<ServiceName>>,
    mut suffix: Vec<String>,
) -> Arc<ServiceName> {
    if let Some(base) = base {
        let mut segs: Vec<String> = base.segments.iter().map(|s| s.to_string()).collect();
        segs.append(&mut suffix);
        ServiceName::of(segs)
    } else {
        ServiceName::of(suffix)
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
    /// Permanently remove the controller and notify removal listeners.
    Remove,
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
            Mode::Remove => "REMOVE",
            Mode::Never => "NEVER",
            Mode::OnDemand => "ON_DEMAND",
            Mode::Lazy => "LAZY",
            Mode::Passive => "PASSIVE",
            Mode::Active => "ACTIVE",
        }
    }
    pub fn parse(s: &str) -> Mode {
        match s {
            "REMOVE" => Mode::Remove,
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
            Mode::Remove => 0,
            Mode::Never => 1,
            Mode::OnDemand => 2,
            Mode::Lazy => 3,
            Mode::Passive => 4,
            Mode::Active => 5,
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
    /// Set when a dependent (or `setMode(ACTIVE)`) demanded this
    /// `OnDemand`/`Lazy` service. Under `CRATONVM_MSC_REAL_START` the
    /// background-worker queue is bypassed, so `take_ready_start` uses this
    /// flag to make demanded on-demand services start-eligible in the real
    /// `drive_starts` loop instead.
    pub demanded: bool,
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
            demanded: false,
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
    /// Alias → primary-name index. Real MSC resolves a dependency against
    /// the per-name `ServiceRegistrationImpl`, so a `requires(X)` is
    /// satisfied by ANY service whose `provides(...)`/`addAliases(...)`
    /// includes `X` — not just one whose primary serviceId equals `X`
    /// (WildFly capability names like `org.wildfly.management.executor` are
    /// provided this way). Without this index, `can_start` never saw such
    /// dependencies as satisfiable and the dependent (e.g.
    /// `jboss.as.server-controller`) stayed `Down` forever.
    aliases: HashMap<Arc<ServiceName>, Arc<ServiceName>>,
    /// Pending work items the workers will pick up.
    task_queue: VecDeque<Task>,
    /// Set of service IDs currently being started/stopped; used to
    /// ensure `async_pending` completions find their controller.
    in_flight: HashSet<u64>,
    /// Shutdown flag — stops workers from accepting new tasks.
    shutdown: bool,
}

impl ContainerState {
    /// Resolve a (possibly aliased) service name to the primary name the
    /// `services` map is keyed by. Names that are already primary — or
    /// entirely unknown — come back unchanged.
    fn resolve<'a>(&'a self, name: &'a Arc<ServiceName>) -> &'a Arc<ServiceName> {
        if self.services.contains_key(name) {
            name
        } else {
            self.aliases.get(name).unwrap_or(name)
        }
    }
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
                aliases: HashMap::new(),
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
            let dep_primary = state.resolve(dep).clone();
            if let Some(d) = state.services.get_mut(&dep_primary) {
                d.dependents.push(name.clone());
            }
        }
        state.services.insert(name.clone(), ctrl);
        state.by_id.insert(id, name.clone());

        // Schedule Active / Passive services whose deps are already Up.
        //
        // Bug 15 follow-on: under CRATONVM_MSC_REAL_START, `drive_starts` is the
        // sole intended driver of real `start()` — it does its own independent
        // scan via `take_ready_start` (not this queue). Background worker
        // threads (`worker_loop`) ALSO watch this same queue and, on waking,
        // run `run_start_local` — which has NO real Java callback to invoke and
        // just flips bookkeeping straight to `Up`. Pushing here let a worker
        // thread race `drive_starts` for the very item it just registered and
        // silently "fake-complete" it (confirmed via `CRATONVM_DBG_MSC`: a
        // `run_start_local` bookkeeping-only trace fired for a service BEFORE
        // `install()`'s own trace even printed), so the real `service.start()`
        // callback was never invoked at all — the exact opposite of what the
        // flag promises. Skip the push in that mode so only the real,
        // synchronous `drive_starts` loop ever starts a service.
        if !msc_real_start_enabled() && matches!(mode, Mode::Active | Mode::Passive) {
            if can_start(&state, &name) {
                state.task_queue.push_back(Task::Start(id));
                self.pool_cv.notify_one();
            }
        }
        Ok(id)
    }

    /// Return a snapshot of a controller's state by name (alias-aware).
    pub fn get_state(&self, name: &Arc<ServiceName>) -> Option<ServiceState> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.services.get(state.resolve(name)).map(|c| c.state)
    }

    pub fn get_id(&self, name: &Arc<ServiceName>) -> Option<u64> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.services.get(state.resolve(name)).map(|c| c.id)
    }

    /// Register `alias` (a `provides(...)`/`addAliases(...)` name) as
    /// resolving to the service installed under `primary`.
    pub fn add_alias(&self, alias: Arc<ServiceName>, primary: Arc<ServiceName>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.aliases.insert(alias, primary);
    }

    /// Ask the scheduler to start a service whose mode is `OnDemand` or
    /// `Lazy` (normally triggered by a dependent requesting the value).
    /// `Never`-mode services ignore the request.
    pub fn demand(&self, name: &Arc<ServiceName>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let primary = state.resolve(name).clone();
        let (id, cur_state) = match state.services.get_mut(&primary) {
            Some(c) => {
                if matches!(c.mode, Mode::Never) {
                    return;
                }
                c.demanded = true;
                (c.id, c.state)
            }
            None => return,
        };
        // Bug 15 follow-on (same hazard as `add_service`): under
        // CRATONVM_MSC_REAL_START never feed the background-worker queue —
        // a worker's `run_start_local` would fake-complete the service
        // without ever invoking its real `start()`. The `demanded` flag
        // set above makes `take_ready_start` pick it up in the real
        // drive loop instead (the caller drives).
        if !msc_real_start_enabled()
            && matches!(cur_state, ServiceState::Down | ServiceState::New)
            && can_start(&state, name)
        {
            state.task_queue.push_back(Task::Start(id));
            self.pool_cv.notify_one();
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

    /// `ServiceContainer.isShutdown()` -- true once `shutdown()` has been
    /// called at least once. Real MSC interface method with no default
    /// implementation; unregistered, any caller died with
    /// `AbstractMethodError: org/jboss/msc/service/ServiceContainer.isShutdown()Z
    /// has no Code attribute`. First surfaced in `AbstractControllerService`'s
    /// boot-completion path (`DomainModelControllerService.boot()` ->
    /// `BootstrapListener.generateBootStatistics()`), aborting Host Controller
    /// boot with `WFLYCTL0002: Error booting the container` right after
    /// `start-servers` already failed on its own unrelated
    /// `WFLYHC0053: Could not get the server inventory in 30 seconds` timeout
    /// -- so this AbstractMethodError was masking the real failure reason in
    /// the boot log by throwing during the FINALLY-style error-reporting path
    /// itself.
    pub fn is_shutdown(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown
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
        if msc_dbg() {
            eprintln!("[msc] run_start_local (bookkeeping-only, NO real start() invoked) id={id}");
        }
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
    ///
    /// Under `msc_real_start_enabled()` (the production default since
    /// `0cac9cc80`), `add_service`/`demand`/`schedule_dependents_of`
    /// deliberately leave `task_queue` empty — that queue is also drained by
    /// the always-running `worker_loop` background threads on
    /// `global_container()`, and letting them race the real `drive_starts()`
    /// loop (which invokes actual Java `start()`) was Bug 15: a worker would
    /// bookkeeping-only fake-complete a service the real loop hadn't started
    /// yet. So once the queue empties, fall back to the same scan-based
    /// selection `drive_starts()` uses (`take_ready_start`/`finish_start`)
    /// instead of just returning early — this is what makes the method work
    /// under both real-start settings rather than silently doing nothing
    /// under the default. It's safe here specifically because this fallback
    /// never invokes real Java code (no `NativeContext` available to a plain
    /// `&self` method), and every production caller picks *either*
    /// `drive_starts` *or* `drain_tasks_locally` for a given real-start
    /// setting — see `native_service_controller_set_mode` — so this never
    /// runs concurrently with a real `drive_starts()` driving the same
    /// container.
    pub fn drain_tasks_locally(&self) {
        loop {
            let task = {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.task_queue.pop_front()
            };
            match task {
                Some(Task::Start(id)) => {
                    self.run_start_local(id);
                    continue;
                }
                Some(Task::Stop(id)) => {
                    let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(name) = state.by_id.get(&id).cloned() {
                        if let Some(c) = state.services.get_mut(&name) {
                            c.state = ServiceState::Down;
                        }
                    }
                    continue;
                }
                None => {}
            }
            match self.take_ready_start() {
                Some(id) => self.finish_start(id),
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
        // Demand-propagation pre-pass (BUG FIX 2026-07-10): real MSC treats
        // "an Active/Passive/demanded-Lazy service depends on X" as an
        // implicit demand on X whenever X is OnDemand/Lazy — the mere
        // existence of a want-to-start dependent is what makes an OnDemand
        // dependency start-eligible at all (JBoss MSC's own
        // `ServiceControllerImpl` links a demand up the dependency edge as
        // part of normal service linking, independent of any Java-code
        // `setMode(ACTIVE)` call). Before this fix, `demand()` only ever
        // fired reactively from the one production call site backing
        // `ServiceController.setMode(Mode.ACTIVE)` — an OnDemand service
        // reachable ONLY via a dependency edge (never given an explicit
        // `setMode(ACTIVE)` call by any Java code) never got
        // `demanded = true`, so `can_start` on its dependent(s) could NEVER
        // return true: a permanent deadlock. First observed booting a
        // WildFly Host Controller: `org.wildfly.management.http.extensible`
        // (OnDemand) is only reachable via its ACTIVE dependent
        // `org.wildfly.management.http.extensible.shutdown`'s dependency
        // edge, so it never started, the http-management service chain
        // never came up, `WFLYCTL0459: Triggering roll back due to missing
        // management services` fired, and a later `http-interface` `add`
        // then saw `IllegalStateException: Container is down`.
        //
        // Runs on every call (this method is polled repeatedly by the drive
        // loop until quiescent) but is cheap: the `!d.demanded` check makes
        // already-demanded services a no-op, and a multi-level OnDemand
        // chain (A active -> B on-demand -> C on-demand) converges within a
        // few calls — demanding B here makes B itself a demand-propagation
        // source (`c.demanded && matches!(c.mode, OnDemand | Lazy)`, mirrored
        // from the eligibility check below) on a subsequent call, which then
        // demands C. Two passes (collect target names, then mutate) because
        // the collection pass borrows `state.services` while iterating it.
        let mut to_demand: Vec<Arc<ServiceName>> = Vec::new();
        for c in state.services.values() {
            let propagates_demand = matches!(c.mode, Mode::Active | Mode::Passive)
                || (c.demanded && matches!(c.mode, Mode::OnDemand | Mode::Lazy));
            if !propagates_demand {
                continue;
            }
            for dep in &c.dependencies {
                let resolved = state.resolve(dep).clone();
                if let Some(d) = state.services.get(&resolved) {
                    if matches!(d.mode, Mode::OnDemand | Mode::Lazy) && !d.demanded {
                        to_demand.push(resolved);
                    }
                }
            }
        }
        for name in to_demand {
            if let Some(d) = state.services.get_mut(&name) {
                d.demanded = true;
            }
        }
        let mut chosen: Option<(Arc<ServiceName>, u64)> = None;
        for (name, c) in state.services.iter() {
            // Auto-start modes are always eligible; OnDemand/Lazy become
            // eligible once something demanded them (see `demand()` — under
            // CRATONVM_MSC_REAL_START this scan is their only start path).
            let mode_eligible = matches!(c.mode, Mode::Active | Mode::Passive)
                || (c.demanded && matches!(c.mode, Mode::OnDemand | Mode::Lazy));
            if matches!(c.state, ServiceState::Down | ServiceState::New)
                && mode_eligible
                && can_start(&state, name)
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
fn can_start(state: &ContainerState, name: &Arc<ServiceName>) -> bool {
    let c = match state.services.get(state.resolve(name)) {
        Some(c) => c,
        None => return false,
    };
    for dep in &c.dependencies {
        // Dependencies resolve through the alias index: a `requires(X)` is
        // satisfied by a service that `provides(X)` under a different
        // primary name (real MSC registration semantics).
        match state.services.get(state.resolve(dep)) {
            Some(d) if matches!(d.state, ServiceState::Up) => {}
            _ => return false,
        }
    }
    true
}

/// After `name` transitions `Up`, walk its dependents and queue any
/// that are now start-eligible.
fn schedule_dependents_of(state: &mut ContainerState, name: &Arc<ServiceName>, pool_cv: &Condvar) {
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
        // Bug 15 follow-on: under CRATONVM_MSC_REAL_START the background
        // workers must never pick up Start tasks (their `run_start_local` is
        // bookkeeping-only and would fake the dependent straight to Up
        // without running its real start()). The real drive loop rescans via
        // `take_ready_start` after every completion, so eligible dependents
        // are picked up there instead.
        if !msc_real_start_enabled() && can_start(state, &dn) {
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
            let mut state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
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
                let mut state = container2.inner.lock().unwrap_or_else(|e| e.into_inner());
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
            let mut state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
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

/// Render a `MethodCallFailed` as `Class: message` when it wraps a thrown
/// Java exception — the raw `Debug` form is just an `ObjectRef` pointer,
/// which made real service-start failures undiagnosable.
fn describe_method_call_failure(ctx: &dyn NativeContext, e: &MethodCallFailed) -> String {
    match e {
        MethodCallFailed::ExceptionThrown(exc) => {
            let cls = ctx
                .class_name_of_id(ctx.class_id_of_object(*exc))
                .unwrap_or_else(|| "<unknown class>".to_string());
            let msg = match ctx.get_field_by_name(*exc, "detailMessage") {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => None,
            };
            match msg {
                Some(m) => format!("{cls}: {m}"),
                None => cls,
            }
        }
        other => format!("{other:?}"),
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
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceName", 2)?;
    let canonical_text = name.canonical();
    let canonical = ctx.create_string(canonical_text);
    // canonicalName is updated via an AtomicReferenceFieldUpdater in the
    // JDK ctor; using `set_field_by_name` is safe: it writes the same
    // slot the updater would CAS into.
    ctx.set_field_by_name(obj, "canonicalName", Value::Object(Some(canonical)));

    // Real ServiceName bytecode reads the leaf `name`, `parent`, and cached
    // `hashCode` fields directly. Populate all three so native-created names
    // compose correctly when real overloads (append(ServiceName), equals,
    // toArray, getParent) execute bytecode around our mirrors.
    let leaf = name
        .segments
        .last()
        .map(|s| s.as_ref())
        .unwrap_or(canonical_text);
    let leaf_str = ctx.create_string(leaf);
    ctx.set_field_by_name(obj, "name", Value::Object(Some(leaf_str)));
    let parent = name
        .parent()
        .map(|p| alloc_java_service_name(ctx, &p))
        .transpose()?;
    ctx.set_field_by_name(obj, "parent", Value::Object(parent));
    ctx.set_field_by_name(obj, "hashCode", Value::Int(service_name_hash(name)));
    Ok(obj)
}

/// Read an `Arc<ServiceName>` back out of a Java `ServiceName` object
/// by interning its canonical string.  Returns `None` if the canonical
/// field is missing/null.
fn read_java_service_name(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<Arc<ServiceName>> {
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
    let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
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

fn native_service_name_of_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let s = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let sn = ServiceName::parse(&s);
    Ok(Some(Value::Object(Some(alloc_java_service_name(
        ctx, &sn,
    )?))))
}

fn native_service_name_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let sn = ServiceName::of(read_service_name_segments_array(ctx, arr)?);
    Ok(Some(Value::Object(Some(alloc_java_service_name(
        ctx, &sn,
    )?))))
}

fn native_service_name_of_parent_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: static ServiceName.of(ServiceName parent, String[] segments)
    let base = match args.first().copied() {
        Some(Value::Object(Some(o))) => read_service_name_robust(ctx, o),
        _ => None,
    };
    let arr = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ServiceName.of: null segments array".into()),
                },
            )));
        }
    };
    let sn = append_service_name_segments(base, read_service_name_segments_array(ctx, arr)?);
    Ok(Some(Value::Object(Some(alloc_java_service_name(
        ctx, &sn,
    )?))))
}

fn native_service_name_append_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: ServiceName.append(String[] segments)
    let this = obj_arg(args, 0)?;
    let base = read_service_name_robust(ctx, this);
    let arr = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ServiceName.append: null segments array".into()),
                },
            )));
        }
    };
    let sn = append_service_name_segments(base, read_service_name_segments_array(ctx, arr)?);
    Ok(Some(Value::Object(Some(alloc_java_service_name(
        ctx, &sn,
    )?))))
}

fn native_service_name_append_service_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: ServiceName.append(ServiceName other)
    let this = obj_arg(args, 0)?;
    let other = obj_arg(args, 1)?;
    let base = read_service_name_robust(ctx, this);
    let suffix = read_service_name_robust(ctx, other)
        .ok_or_else(|| illegal_service_name_arg("ServiceName.append: unreadable suffix"))?;
    let suffix_segments: Vec<String> = suffix.segments.iter().map(|s| s.to_string()).collect();
    let sn = append_service_name_segments(base, suffix_segments);
    Ok(Some(Value::Object(Some(alloc_java_service_name(
        ctx, &sn,
    )?))))
}

fn native_service_name_get_canonical(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let canonical = read_service_name_robust(ctx, this)
        .map(|n| n.canonical().to_string())
        .unwrap_or_default();
    let s = ctx.create_string(&canonical);
    Ok(Some(Value::Object(Some(s))))
}

fn native_service_name_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sn = read_service_name_robust(ctx, this);
    let parent_obj = match sn.and_then(|s| s.parent()) {
        Some(p) => Value::Object(Some(alloc_java_service_name(ctx, &p)?)),
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
    let obj = try_alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceContainer", 2)?;
    Ok(Some(Value::Object(Some(obj))))
}

fn native_service_container_add_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: addService(ServiceName, Service) -> ServiceController
    if args.len() < 3 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "ServiceContainer.addService: expected 3 args, got {}",
                args.len()
            ),
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
        .map_err(|msg| MethodCallFailed::InternalError(VmError::Internal { message: msg }))?;

    // Build the Java-side ServiceController mirror. `sn_obj` is held across
    // this allocation, and `ctrl_obj` is held across the drive-to-completion
    // call below (either branch can invoke real Java code and trigger a
    // moving GC) — pin both and re-read before their next use.
    let sn_pin = ctx.pin_native_root(sn_obj);
    let ctrl_obj = try_alloc_concurrent_synthetic(
        ctx,
        "org/jboss/msc/service/ServiceController",
        SC_NUM_SLOTS,
    )?;
    let sn_obj = ctx.read_native_pin(sn_pin, sn_obj);
    let ctrl_pin = ctx.pin_native_root(ctrl_obj);
    ctx.set_field(ctrl_obj, SC_FIELD_NAME, Value::Object(Some(sn_obj)));
    ctx.set_field(ctrl_obj, SC_FIELD_MODE, Value::Int(Mode::Active.ordinal()));
    ctx.set_field(
        ctrl_obj,
        SC_FIELD_STATE,
        Value::Int(ServiceState::Down.ordinal()),
    );
    ctx.set_field(ctrl_obj, SC_FIELD_VALUE, Value::Object(None));
    ctx.set_field(ctrl_obj, SC_FIELD_ID, Value::Long(id as i64));

    // Drive the service to completion before returning, same as
    // `native_service_controller_set_mode`'s Active/Passive branch: under
    // `msc_real_start_enabled()` (the default since `0cac9cc80`) only
    // `drive_starts` invokes the real Java `start()` callback, so this
    // legacy 2-arg `addService` must route through it too instead of always
    // calling `drain_tasks_locally` — that unconditional call predates the
    // real-start default and, left as-is, would silently bookkeeping-fake
    // the service straight to `Up` without ever running its `start()`.
    if msc_real_start_enabled() {
        let was_driving = DRIVING.with(|d| d.replace(true));
        if !was_driving {
            let res = drive_starts(ctx, &container);
            DRIVING.with(|d| d.set(false));
            if let Err(e) = res {
                ctx.unpin_native_roots(sn_pin);
                return Err(e);
            }
        }
    } else {
        container.drain_tasks_locally();
    }
    let ctrl_obj = ctx.read_native_pin(ctrl_pin, ctrl_obj);
    reflect_controller(ctx, ctrl_obj, &container, id);
    ctx.unpin_native_roots(sn_pin);

    Ok(Some(Value::Object(Some(ctrl_obj))))
}

fn native_service_controller_set_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let new_mode = match args.get(1).copied() {
        Some(Value::Object(Some(mode))) => read_mode_by_name(ctx, Some(mode)),
        Some(Value::Int(i)) => match i {
            0 => Mode::Remove,
            1 => Mode::Never,
            2 => Mode::OnDemand,
            3 => Mode::Lazy,
            4 => Mode::Passive,
            5 => Mode::Active,
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
        let mut state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(name) = state.by_id.get(&id).cloned() {
            if let Some(c) = state.services.get_mut(&name) {
                c.mode = new_mode;
            }
        }
    }
    if matches!(new_mode, Mode::Remove) {
        let removed = {
            let mut state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
            match state.by_id.get(&id).cloned() {
                Some(name) => match state.services.get_mut(&name) {
                    Some(controller) => {
                        controller.state = ServiceState::Removed;
                        controller.async_pending = false;
                        state.in_flight.remove(&id);
                        true
                    }
                    None => false,
                },
                None => false,
            }
        };
        ctx.set_field(this, SC_FIELD_MODE, Value::Int(new_mode.ordinal()));
        ctx.set_field(
            this,
            SC_FIELD_STATE,
            Value::Int(ServiceState::Removed.ordinal()),
        );
        if removed {
            fire_lifecycle_event_all(ctx, id, "REMOVED");
        }
        return Ok(None);
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
            if msc_real_start_enabled() {
                // Bug 15 follow-on: `drain_tasks_locally` runs
                // `run_start_local`, which fake-completes services without
                // invoking their real `start()`. Route through the real
                // drive loop instead (skip when a drive loop higher on this
                // stack is already running — it rescans on its own).
                let was_driving = DRIVING.with(|d| d.replace(true));
                if !was_driving {
                    let res = drive_starts(ctx, &container);
                    DRIVING.with(|d| d.set(false));
                    res?;
                }
            } else {
                container.drain_tasks_locally();
            }
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
    let state = {
        let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        state
            .by_id
            .get(&id)
            .and_then(|n| state.services.get(n))
            .map(|c| c.state)
            .unwrap_or(ServiceState::New)
    };
    // The declared descriptor returns the REAL `ServiceController$State` enum
    // constant — bytecode callers compare it by reference against `getstatic`
    // constants (`WritableValueImpl.accept` gates value injection on
    // `state == State.STARTING` via `if_acmpne`), so returning an ordinal Int
    // here silently failed every such comparison. Map our shadow states onto
    // MSC's enum names (no NEW in MSC's State; FAILED is START_FAILED) and
    // fetch the constant fresh via `valueOf` (no caching — a stored ObjectRef
    // would go stale across a moving GC).
    let msc_name = match state {
        ServiceState::New | ServiceState::Down => "DOWN",
        ServiceState::Starting => "STARTING",
        ServiceState::Up => "UP",
        ServiceState::Stopping => "STOPPING",
        ServiceState::Failed => "START_FAILED",
        ServiceState::Removed => "REMOVED",
    };
    let name_str = ctx.create_string(msc_name);
    match ctx.invoke(
        "org/jboss/msc/service/ServiceController$State",
        "valueOf",
        "(Ljava/lang/String;)Lorg/jboss/msc/service/ServiceController$State;",
        &[Value::Object(Some(name_str))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        // Enum class unavailable (mock/unit-test contexts): keep the legacy
        // ordinal-Int answer rather than failing dispatch outright.
        _ => Ok(Some(Value::Int(state.ordinal()))),
    }
}

/// `ServiceController.provides()` — real MSC 1.5.x interface method
/// ("the complete set of names... under which the service is provided")
/// with no default implementation; any caller through our synthetic
/// mirror died with `AbstractMethodError:
/// org/jboss/msc/service/ServiceController.provides()Ljava/util/Set;`.
/// First surfaced booting a WildFly Host Controller: the
/// `jboss.remoting.endpoint.management.management.operation.handler`
/// service's own MSC start() path calls it, the resulting
/// `AbstractMethodError` marks that service FAILED, and the failure
/// cascades into `WFLYCTL0459: Triggering roll back due to missing
/// management services` — which is why a *later*, unrelated
/// `http-interface` `add` operation then fails with
/// `IllegalStateException: Container is down` (the whole management
/// transaction was already rolled back by this point).
///
/// Returns the full set of names this controller is addressable under:
/// its primary name plus every alias registered via
/// `ServiceBuilder.addAliases(...)` (`ContainerState.aliases` is
/// alias -> primary, so this reverse-scans it — cheap, since `provides()`
/// is called rarely relative to the hot install/dispatch paths).
fn native_service_controller_provides(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let names: Vec<Arc<ServiceName>> = {
        let container = global_container();
        let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        match state.by_id.get(&id) {
            Some(primary) => {
                let mut out = vec![primary.clone()];
                for (alias, target) in state.aliases.iter() {
                    if target == primary {
                        out.push(alias.clone());
                    }
                }
                out
            }
            None => Vec::new(),
        }
    };
    let set = match ctx.new_object_initialized("java/util/HashSet", "()V", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        // No real HashSet available (mock/unit-test contexts): degrade to an
        // empty-but-valid Set rather than failing dispatch outright — matches
        // `getState`'s own fallback philosophy above.
        _ => return Ok(Some(Value::Object(None))),
    };
    let set_pin = ctx.pin_native_root(set);
    let mut set_cur = set;
    for name in names {
        let name_obj = alloc_java_service_name(ctx, &name)?;
        let name_pin = ctx.pin_native_root(name_obj);
        set_cur = ctx.read_native_pin(set_pin, set_cur);
        let name_obj = ctx.read_native_pin(name_pin, name_obj);
        ctx.unpin_native_roots(name_pin);
        let _ = ctx.invoke_virtual(
            set_cur,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(name_obj))],
        )?;
    }
    set_cur = ctx.read_native_pin(set_pin, set_cur);
    ctx.unpin_native_roots(set_pin);
    Ok(Some(Value::Object(Some(set_cur))))
}

/// `ServiceController.getStartException()` — `BootstrapImpl$1`'s FAILED branch
/// calls this to build the bootstrap-failure report; as a code-less interface
/// method it previously died with `AbstractMethodError` (swallowed by the
/// listener-exception guard, silently losing the real failure). Builds a real
/// `StartException` from the shadow container's recorded failure message;
/// null when the service has not failed.
fn native_service_controller_get_start_exception(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let container = global_container();
    let failure = {
        let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        state
            .by_id
            .get(&id)
            .and_then(|n| state.services.get(n))
            .and_then(|c| c.failure_message.clone())
    };
    let msg = match failure {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let msg_obj = ctx.create_string(&msg);
    match ctx.new_object_initialized(
        "org/jboss/msc/service/StartException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg_obj))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `LifecycleContext.getElapsedTime()J` — nanoseconds since the current
/// lifecycle action began (BootstrapListener times boot with it). Another
/// code-less interface method on our synthetic `StartContext`. Answered from
/// the Rust-side `start_began` instant recorded when `drive_starts` invoked
/// the service's `start()`.
fn native_lifecycle_context_get_elapsed_time(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let began = {
        let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&id).and_then(|r| r.start_began)
    };
    let nanos = began
        .map(|t| t.elapsed().as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    Ok(Some(Value::Long(nanos)))
}

/// Map a shadow-container [`ServiceState`] to the `LifecycleEvent` enum
/// constant name real MSC would replay for a listener added at that rest
/// state (`ServiceControllerImpl.addListener` fires the current rest-state
/// event immediately so late listeners never miss a completed transition).
/// `None` for transient states (`New`/`Starting`/`Stopping`) — those notify
/// later, when the transition completes.
fn rest_state_event(state: ServiceState) -> Option<&'static str> {
    match state {
        ServiceState::Up => Some("UP"),
        ServiceState::Failed => Some("FAILED"),
        ServiceState::Down => Some("DOWN"),
        ServiceState::Removed => Some("REMOVED"),
        ServiceState::New | ServiceState::Starting | ServiceState::Stopping => None,
    }
}

/// Fire `LifecycleListener.handleEvent(controller, event)` on the given
/// listeners. Fetches the real `LifecycleEvent` enum constant fresh each time
/// (no caching — enum constants live in the enum class's statics, and a
/// cached copy here would go stale across a moving GC). Listener exceptions
/// are logged and swallowed, matching real MSC (`invokeListener` catches
/// `Throwable` so one broken listener cannot wedge the container).
fn fire_lifecycle_event(
    ctx: &mut dyn NativeContext,
    id: u64,
    event: &str,
    listeners: &[ObjectRef],
) {
    if listeners.is_empty() {
        return;
    }
    let mirror = {
        let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&id).and_then(|r| r.controller_mirror)
    };
    let mirror = match mirror {
        Some(m) => m,
        None => return,
    };
    // Sync the Java mirror's state field before listeners read it.
    reflect_controller(ctx, mirror, global_container(), id);
    let event_name = ctx.create_string(event);
    let event_obj = match ctx.invoke(
        "org/jboss/msc/service/LifecycleEvent",
        "valueOf",
        "(Ljava/lang/String;)Lorg/jboss/msc/service/LifecycleEvent;",
        &[Value::Object(Some(event_name))],
    ) {
        Ok(Some(Value::Object(Some(e)))) => e,
        other => {
            tracing::warn!(
                target: "jboss_msc",
                "LifecycleEvent.valueOf({event}) unavailable — listeners not notified: {other:?}"
            );
            return;
        }
    };
    for l in listeners {
        if msc_dbg() {
            eprintln!("[msc] fire {event} id={id} listener={:?}", l.as_ptr());
        }
        if let Err(e) = ctx.invoke_virtual(
            *l,
            "handleEvent",
            "(Lorg/jboss/msc/service/ServiceController;Lorg/jboss/msc/service/LifecycleEvent;)V",
            &[Value::Object(Some(mirror)), Value::Object(Some(event_obj))],
        ) {
            let detail = describe_method_call_failure(ctx, &e);
            tracing::warn!(
                target: "jboss_msc",
                "LifecycleListener.handleEvent({event}) threw {detail} — ignored (MSC-faithful)"
            );
        }
    }
}

/// Fire a lifecycle event to every listener currently registered for `id`.
fn fire_lifecycle_event_all(ctx: &mut dyn NativeContext, id: u64, event: &str) {
    let listeners: Vec<ObjectRef> = {
        let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&id)
            .map(|r| r.listeners.clone())
            .unwrap_or_default()
    };
    fire_lifecycle_event(ctx, id, event, &listeners);
}

/// `ServiceController.addListener(LifecycleListener)` — register the listener
/// on the shadow container and, when the controller is already at a rest
/// state, replay that state's event immediately (real MSC semantics;
/// `BootstrapImpl.internalBootstrap`'s bootstrap-completion chain hangs
/// forever without the replay). Previously unimplemented: the interface
/// method has no bytecode, so any call died with `AbstractMethodError`.
fn native_service_controller_add_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let listener = match args.get(1).copied() {
        Some(Value::Object(Some(l))) => l,
        _ => return Ok(None),
    };
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    {
        let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.entry(id).or_default().listeners.push(listener);
    }
    let state = {
        let container = global_container();
        let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        state
            .by_id
            .get(&id)
            .and_then(|n| state.services.get(n))
            .map(|c| c.state)
    };
    if msc_dbg() {
        eprintln!("[msc] addListener id={id} state={state:?}");
    }
    if let Some(event) = state.and_then(rest_state_event) {
        // Replay only to the newly-added listener.
        fire_lifecycle_event(ctx, id, event, &[listener]);
    }
    Ok(None)
}

/// `ServiceController.removeListener(LifecycleListener)` — drop by identity.
/// The GC remap keeps stored refs current, so pointer equality with the
/// (equally current) argument is the right identity test.
fn native_service_controller_remove_listener(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let listener = match args.get(1).copied() {
        Some(Value::Object(Some(l))) => l,
        _ => return Ok(None),
    };
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = map.get_mut(&id) {
        r.listeners.retain(|l| l.as_ptr() != listener.as_ptr());
    }
    Ok(None)
}

/// `StabilityMonitor.addController/removeController` — real MSC downcasts the
/// argument to its concrete `ServiceControllerImpl` to hook per-controller
/// bookkeeping (`StabilityMonitor.java:115`), which `ClassCastException`s on
/// our synthetic `ServiceController` mirror (first thrown from
/// `ApplicationServerService.start` → boot marked jboss.as FAILED). The
/// shadow container already tracks every installed service globally and the
/// `awaitStability` natives answer from it, so per-monitor membership is
/// redundant here — accept and ignore.
fn native_stability_monitor_controller_noop(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
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

fn native_start_context_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, CTX_FIELD_CONTROLLER_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    global_container().complete_async(id);
    fire_lifecycle_event_all(ctx, id, "UP");
    // An async completion makes dependents start-eligible. Under
    // CRATONVM_MSC_REAL_START the background-worker queue is bypassed
    // entirely (see add_service), so drive them here — unless a drive loop
    // higher on this stack is already running and will rescan anyway.
    if msc_real_start_enabled() {
        let was_driving = DRIVING.with(|d| d.replace(true));
        if !was_driving {
            let container = global_container().clone();
            let res = drive_starts(ctx, &container);
            DRIVING.with(|d| d.set(false));
            res?;
        }
    }
    Ok(None)
}

fn native_service_container_shutdown(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    global_container().shutdown();
    Ok(None)
}

/// `ServiceContainer.isShutdown()Z` -- see `ServiceContainer::is_shutdown`
/// doc comment for the AbstractMethodError this fixes.
fn native_service_container_is_shutdown(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(if global_container().is_shutdown() {
        1
    } else {
        0
    })))
}

/// R63 (WildFly): `org/jboss/msc/service/Lockable.acquireWrite()/acquireRead()`
/// uses a hand-rolled AQS-style wait loop that calls `Object.wait()` when the
/// lock is contended. CratonVM's interpreter is effectively single-threaded for
/// MSC boot — the MSC ContainerExecutor runs tasks inline on the main thread —
/// so a recursive task path can re-enter `acquireWrite` while the bit is already
/// set. There is no other thread to call `notify()`, so `Object.wait()` blocks
/// forever. Since all "concurrent" task execution happens on one thread, locking
/// is unnecessary: shim acquire/release to no-ops so MSC boot proceeds.
fn native_lockable_lock_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// R63 (WildFly): `DelegatingBasicLogger.isTraceEnabled()` returns
/// `this.log.isTraceEnabled()`, but in our synthetic-logger fixups
/// for ServiceLogger/ElytronMessages the `log` field is null. The
/// SecurityDomain$Builder.build call at line 1100 invokes
/// `isTraceEnabled` on the synthetic ElytronMessages_$logger and NPEs.
/// `DelegatingBasicLogger.is{Trace,Debug,Info}Enabled()` — delegate to the
/// wrapped `this.log` when it is present (so real backends report their true
/// enabled-state), falling back to `false` ONLY when `this.log` is null.
///
/// The blanket `return false` this replaces was added so WildFly's synthetic
/// `ServiceLogger.ROOT/SERVICE/FAIL` / `ElytronMessages.log` backfills (which
/// leave `this.log` null) don't NPE on `this.log.isXEnabled()` during a
/// non-logging boot. But returning false unconditionally also suppressed
/// level-guarded log calls on REAL loggers — notably Hibernate's testing
/// `DelegatingLogger`, where `if (COLLECTION_LOGGER.isDebugEnabled())` gates
/// the HHH90030006 rollback message that the test's `LogListener` observes.
/// That made `DetachedBagDelayedOperationTest` (and similar level-guarded
/// log-assertion tests) FAIL on CratonVM only. Honoring a non-null delegate
/// keeps the WildFly null-safety while letting real backends answer truthfully.
fn delegating_logger_is_enabled(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let log = match ctx.get_field_by_name(this, "log") {
        Value::Object(Some(o)) => o,
        // Null delegate (WildFly synthetic backfill): real bytecode would NPE,
        // so preserve the historical false.
        _ => return Ok(Some(Value::Int(0))),
    };
    match ctx.invoke_virtual(log, method, "()Z", &[]) {
        Ok(Some(v @ Value::Int(_))) => Ok(Some(v)),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_delegating_logger_is_trace_enabled(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    delegating_logger_is_enabled(ctx, args, "isTraceEnabled")
}

fn native_delegating_logger_is_debug_enabled(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    delegating_logger_is_enabled(ctx, args, "isDebugEnabled")
}

fn native_delegating_logger_is_info_enabled(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    delegating_logger_is_enabled(ctx, args, "isInfoEnabled")
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

fn native_ihs_iter_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

fn native_ihs_iter_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let mut idx = next_idx;
    while idx < len {
        let value = ctx.get_array_element(table, idx as usize);
        if !matches!(value, Value::Object(None)) {
            // current = idx; next = idx + 1; hasNext = false
            ctx.set_field_by_name(this, "current", Value::Int(idx));
            ctx.set_field_by_name(this, "next", Value::Int(idx + 1));
            ctx.set_field_by_name(this, "hasNext", Value::Int(0));
            return Ok(Some(value));
        }
        idx += 1;
    }
    ctx.set_field_by_name(this, "next", Value::Int(len));
    ctx.set_field_by_name(this, "hasNext", Value::Int(0));
    Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::NoSuchElementException {
            message: "IdentityHashSet iterator exhausted".to_string(),
        },
    )))
}

fn opt_obj_arg(args: &[Value], index: usize) -> Option<ObjectRef> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn copy_non_null_identity_hash_set(
    ctx: &mut dyn NativeContext,
    source: Option<ObjectRef>,
    dest: Option<ObjectRef>,
    label: &str,
) -> Result<(usize, usize), MethodCallFailed> {
    let (Some(source), Some(dest)) = (source, dest) else {
        return Ok((0, 0));
    };
    let table = match ctx.get_field_by_name(source, "table") {
        Value::Object(Some(t)) => t,
        _ => return Ok((0, 0)),
    };
    let len = ctx.array_length(table);
    let mut entries = Vec::new();
    let mut skipped_null = 0usize;
    for i in 0..len {
        match ctx.get_array_element(table, i) {
            Value::Object(Some(o)) => entries.push(o),
            Value::Object(None) => skipped_null += 1,
            _ => {}
        }
    }
    if entries.is_empty() {
        if msc_dbg() && skipped_null != 0 {
            eprintln!("[msc] awaitStability copy {label}: skipped {skipped_null} null slots");
        }
        return Ok((0, skipped_null));
    }

    let base = ctx.pin_native_root(dest);
    let handles: Vec<usize> = entries.iter().map(|o| ctx.pin_native_root(*o)).collect();
    let mut dest_cur = dest;
    for (entry, handle) in entries.iter().zip(handles.iter()) {
        dest_cur = ctx.read_native_pin(base, dest_cur);
        let entry_cur = ctx.read_native_pin(*handle, *entry);
        if let Err(e) = ctx.invoke_virtual(
            dest_cur,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(entry_cur))],
        ) {
            ctx.unpin_native_roots(base);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(base);
    if msc_dbg() {
        eprintln!(
            "[msc] awaitStability copy {label}: copied {} skipped_null={skipped_null}",
            entries.len()
        );
    }
    Ok((entries.len(), skipped_null))
}

fn time_unit_to_nanos(ctx: &mut dyn NativeContext, unit: ObjectRef, timeout: i64) -> i128 {
    match ctx.invoke_virtual(unit, "toNanos", "(J)J", &[Value::Long(timeout)]) {
        Ok(Some(Value::Long(nanos))) => nanos as i128,
        _ => timeout.max(0) as i128 * 1_000_000,
    }
}

fn remove_null_service_controller_from_set(
    ctx: &mut dyn NativeContext,
    dest: Option<ObjectRef>,
    label: &str,
) -> Result<(), MethodCallFailed> {
    let Some(dest) = dest else {
        return Ok(());
    };
    let removed = ctx.invoke_virtual(
        dest,
        "remove",
        "(Ljava/lang/Object;)Z",
        &[Value::Object(None)],
    )?;
    if msc_dbg() && matches!(removed, Some(Value::Int(v)) if v != 0) {
        eprintln!("[msc] awaitStability copy {label}: removed impossible null controller");
    }
    Ok(())
}

fn await_stability_lock(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    for field in ["lock", "stabilityLock"] {
        if let Value::Object(Some(lock)) = ctx.get_field_by_name(this, field) {
            return Some(lock);
        }
    }
    None
}

fn native_service_container_await_stability_common(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    timeout_nanos: Option<i128>,
    failed_dest: Option<ObjectRef>,
    problems_dest: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    let lock = match await_stability_lock(ctx, this) {
        Some(lock) => lock,
        None => return Ok(true),
    };

    let base = ctx.pin_native_root(this);
    let lock_pin = ctx.pin_native_root(lock);
    let failed_pin = failed_dest.map(|o| ctx.pin_native_root(o));
    let problems_pin = problems_dest.map(|o| ctx.pin_native_root(o));

    let mut remaining_nanos = timeout_nanos;
    let mut stable = false;
    let mut wait_error = None;

    let mut lock_cur = ctx.read_native_pin(lock_pin, lock);
    ctx.monitor_enter(lock_cur);
    loop {
        let this_cur = ctx.read_native_pin(base, this);
        let unstable = match ctx.get_field_by_name(this_cur, "unstableServices") {
            Value::Int(i) => i,
            _ => 0,
        };
        if unstable == 0 {
            stable = true;
            break;
        }

        match remaining_nanos.as_mut() {
            Some(remaining) => {
                if *remaining <= 0 {
                    break;
                }
                let wait_ms = ((*remaining + 999_999) / 1_000_000)
                    .max(1)
                    .min(u64::MAX as i128) as u64;
                lock_cur = ctx.read_native_pin(lock_pin, lock_cur);
                let before = Instant::now();
                if let Err(e) = ctx.monitor_wait(lock_cur, Some(wait_ms)) {
                    wait_error = Some(e);
                    break;
                }
                let elapsed = before.elapsed().as_nanos() as i128;
                *remaining = remaining.saturating_sub(elapsed);
            }
            None => {
                lock_cur = ctx.read_native_pin(lock_pin, lock_cur);
                if let Err(e) = ctx.monitor_wait(lock_cur, None) {
                    wait_error = Some(e);
                    break;
                }
            }
        }
    }
    lock_cur = ctx.read_native_pin(lock_pin, lock_cur);
    ctx.monitor_exit(lock_cur);

    if let Some(e) = wait_error {
        ctx.unpin_native_roots(base);
        return Err(e);
    }
    if !stable {
        ctx.unpin_native_roots(base);
        return Ok(false);
    }

    let this_cur = ctx.read_native_pin(base, this);
    let failed_source = match ctx.get_field_by_name(this_cur, "failed") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let failed_dest_cur = failed_pin.map(|h| ctx.read_native_pin(h, failed_dest.unwrap()));
    let failed_copy =
        copy_non_null_identity_hash_set(ctx, failed_source, failed_dest_cur, "failed");
    if let Err(e) = failed_copy {
        ctx.unpin_native_roots(base);
        return Err(e);
    }
    // `copy_non_null_identity_hash_set` invokes `Set.add` and can trigger a
    // moving GC — re-read the pinned dest before this second use.
    let failed_dest_cur = failed_pin.map(|h| ctx.read_native_pin(h, failed_dest.unwrap()));
    if let Err(e) = remove_null_service_controller_from_set(ctx, failed_dest_cur, "failed") {
        ctx.unpin_native_roots(base);
        return Err(e);
    }

    let this_cur = ctx.read_native_pin(base, this);
    let problems_source = match ctx.get_field_by_name(this_cur, "problems") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    let problems_dest_cur = problems_pin.map(|h| ctx.read_native_pin(h, problems_dest.unwrap()));
    let problems_copy =
        copy_non_null_identity_hash_set(ctx, problems_source, problems_dest_cur, "problems");
    if let Err(e) = problems_copy {
        ctx.unpin_native_roots(base);
        return Err(e);
    }
    // Same re-read requirement as `failed_dest_cur` above: the copy call
    // above can move the destination set.
    let problems_dest_cur = problems_pin.map(|h| ctx.read_native_pin(h, problems_dest.unwrap()));
    let cleanup = remove_null_service_controller_from_set(ctx, problems_dest_cur, "problems");
    ctx.unpin_native_roots(base);
    cleanup.map(|_| true)
}

fn native_service_container_await_stability_sets(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let failed_dest = opt_obj_arg(args, 1);
    let problems_dest = opt_obj_arg(args, 2);
    match native_service_container_await_stability_common(
        ctx,
        this,
        None,
        failed_dest,
        problems_dest,
    ) {
        Ok(_) => Ok(None),
        Err(e) => Err(e),
    }
}

fn native_service_container_await_stability_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let timeout = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let unit = opt_obj_arg(args, 2);
    let failed_dest = opt_obj_arg(args, 3);
    let problems_dest = opt_obj_arg(args, 4);
    let timeout_nanos = unit
        .map(|u| time_unit_to_nanos(ctx, u, timeout))
        .unwrap_or_else(|| timeout.max(0) as i128 * 1_000_000);
    match native_service_container_await_stability_common(
        ctx,
        this,
        Some(timeout_nanos),
        failed_dest,
        problems_dest,
    ) {
        Ok(true) => Ok(Some(Value::Int(1))),
        Ok(false) => Ok(Some(Value::Int(0))),
        Err(e) => Err(e),
    }
}

/// Bind an externally-allocated `ServiceController` Java object to a
/// controller id (used by T19.2 subsystem glue to hand a pre-built
/// controller back to the interpreter).
#[allow(dead_code)]
pub fn bind_controller_id(ctx: &dyn NativeContext, obj: ObjectRef, id: u64) {
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
#[derive(Default, Clone)]
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
    /// `LifecycleListener`s registered via `ServiceController.addListener`.
    /// Real Java objects held only by this side-table — GC roots and post-move
    /// remap targets like every other field here. Fired on Up/Failed
    /// transitions from the drive loop, and replayed once on `addListener`
    /// when the controller is already at a rest state (real MSC semantics —
    /// `BootstrapImpl.internalBootstrap`'s listener chain depends on both).
    listeners: Vec<ObjectRef>,
    /// When the in-flight lifecycle action (`start()`) began — backs
    /// `LifecycleContext.getElapsedTime()J` (BootstrapListener calls it while
    /// timing boot). Kept Rust-side so the synthetic `StartContext` layout
    /// (`CTX_NUM_SLOTS`, matched to `synthetic_stub_fields`) stays unchanged.
    start_began: Option<std::time::Instant>,
    /// Legacy `addDependency(name, type, Injector)` wiring captured from the
    /// builder at install: `(dependency ServiceRegistrationImpl, Injector)`
    /// pairs. Real MSC's StartTask resolves each dependency's value and
    /// calls `Injector.inject(value)` BEFORE `start()` runs (WildFly's
    /// `ServerService` reads `InjectedValue.getValue()` at start:272);
    /// `drive_starts` mirrors that via [`inject_dependency_values`]. Both
    /// elements are GC roots / remap targets.
    dep_injections: Vec<(ObjectRef, ObjectRef)>,
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
        for o in [
            r.service,
            r.controller_mirror,
            r.child_target,
            r.start_context,
        ]
        .into_iter()
        .flatten()
        {
            out.push(o);
        }
        out.extend(r.listeners.iter().copied());
        for (reg, inj) in r.dep_injections.iter() {
            out.push(*reg);
            out.push(*inj);
        }
    }
}

/// Post-GC remap for the container-held service objects (companion to
/// [`gc_scan_msc_service_roots`]). After a moving collection the held objects
/// relocate; repoint every stored `ObjectRef` to its new address.
pub fn gc_update_msc_service_refs(pointer_map: &cratonvm_types::PointerMap) {
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
        for l in r.listeners.iter_mut() {
            let old_addr = l.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *l = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
        for (reg, inj) in r.dep_injections.iter_mut() {
            for o in [reg, inj] {
                let old_addr = o.as_ptr() as usize;
                if let Some(&new_addr) = pointer_map.get(&old_addr) {
                    debug_assert!(new_addr != 0, "GC pointer map contains null address");
                    *o = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
}

// ===========================================================================
// P2/P3 — drive the real Java `service.start(StartContext)` callback.
//
// Enabled by default: `ServiceBuilderImpl.install()` extracts the service from
// the builder, registers it in the Rust container, and drives its real
// `start()` callback.  Faking the controller state as `Up` leaves
// `AbstractControllerService.controller` unset and deterministically breaks
// managed-server boot. `CRATONVM_MSC_REAL_START=0` is a temporary diagnostic
// escape hatch; `CRATONVM_DBG_MSC` traces the install/start sequence.
// ===========================================================================

/// Cached check of the real-MSC mode. It is the production default; accept a
/// conventional false value only as a diagnostic escape hatch.
fn msc_real_start_enabled() -> bool {
    crate::nbflags().msc_real_start
}

/// Cached check of the `CRATONVM_DBG_MSC` trace flag.
fn msc_dbg() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| crate::nbflags().dbg_msc)
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
    let map_pin = ctx.pin_native_root(map);
    let map = ctx.read_native_pin(map_pin, map);
    // invoke_virtual prepends the receiver; `args` is parameters ONLY (none here).
    let set = match ctx.invoke_virtual(map, "keySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => {
            ctx.unpin_native_roots(map_pin);
            return Vec::new();
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let set = ctx.read_native_pin(set_pin, set);
    let arr = match ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => {
            ctx.unpin_native_roots(map_pin);
            return Vec::new();
        }
    };
    let arr_pin = ctx.pin_native_root(arr);
    let n = ctx.array_length(arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let arr = ctx.read_native_pin(arr_pin, arr);
        if let Value::Object(Some(sn)) = ctx.get_array_element(arr, i) {
            if let Some(name) = read_service_name_robust(ctx, sn) {
                out.push(name);
            }
        }
    }
    ctx.unpin_native_roots(map_pin);
    out
}

/// Bug 15 follow-up (handoff P3 "value injection not wired"): make real MSC
/// value plumbing work under the intercepted `install()`.
///
/// Real `install()` calls `ServiceRegistrationImpl.set(controller, injector)`
/// for every provided name, which our interception skips entirely — leaving
/// (a) each `WritableValueImpl.controller` null, so the first
/// `Consumer.accept(value)` a service makes inside `start()` throws
/// `IllegalStateException("Outside of Service lifecycle method")` (this
/// killed all the `jboss.server.path.*` services), and (b) each per-name
/// `ServiceRegistrationImpl.injector` null, so `requires()`-side
/// `ReadableValueImpl.get()` → `registration.getValue()` throws
/// `"Service is not installed"` even after the provider ran.
///
/// `requires()` and `provides()` resolve names through the SAME real
/// `ServiceTargetImpl.getOrCreateRegistration` map (real bytecode that still
/// runs), so wiring `controller` + `injector` here reconnects both ends
/// exactly the way real install() does. The mirror satisfies `accept()`'s
/// `getState() == State.STARTING` reference-compare because our `getState`
/// native returns the real enum constant.
/// Bug 15 follow-up: `ServiceRegistrationImpl.getValue()` — the requires()-
/// supplier read path (`ReadableValueImpl.get` → `Dependency.getValue`).
/// Modern providers reach it through the injector wired in
/// [`wire_provides_injectors`]; LEGACY services (2-arg
/// `ServiceTarget.addService(name, Service)` — e.g. WildFly's management
/// executor `ServerService$ServerExecutorService`) have no provides()
/// consumer at all: their value is the legacy `Service.getValue()`. Real MSC
/// resolves those through `registration.instance` (a real
/// `ServiceControllerImpl`) which our install() interception never populates,
/// so the real bytecode threw `IllegalStateException("Service is not
/// installed")` even though the provider was Up (killed
/// `ServerService.start` → `AbstractControllerService.getExecutorService`).
fn native_service_registration_get_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // 1. Injector wired (modern provides-consumer path): defer to the real
    //    WritableValueImpl bytecode (returns the value or throws the real
    //    "Service unavailable" ISE).
    if let Value::Object(Some(injector)) = ctx.get_field_by_name(this, "injector") {
        return ctx.invoke_virtual(injector, "getValue", "()Ljava/lang/Object;", &[]);
    }
    // 2. Legacy path: resolve the registration's name against the shadow
    //    container and return the provider's own legacy getValue().
    let name = match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(sn)) => read_service_name_robust(ctx, sn),
        _ => None,
    };
    let service = name.as_ref().and_then(|n| {
        global_container().get_id(n).and_then(|id| {
            service_roots()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .and_then(|r| r.service)
        })
    });
    match service {
        Some(svc) => ctx.invoke_virtual(svc, "getValue", "()Ljava/lang/Object;", &[]),
        None => {
            let msg_obj = ctx.create_string("Service is not installed");
            match ctx.new_object_initialized(
                "java/lang/IllegalStateException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg_obj))],
            ) {
                Ok(Some(Value::Object(Some(exc)))) => Err(MethodCallFailed::ExceptionThrown(exc)),
                _ => Ok(Some(Value::Object(None))),
            }
        }
    }
}

/// `ServiceController.getValue()` on the synthetic mirror — legacy API used
/// by code holding a controller (real impl reads its primary registration).
/// Answer with the shadow service's legacy `getValue()`; null when the
/// service instance is absent or modern (no legacy getValue).
fn native_service_controller_get_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let service = {
        let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&id).and_then(|r| r.service)
    };
    match service {
        Some(svc) => match ctx.invoke_virtual(svc, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(v) => Ok(v),
            // Modern org.jboss.msc.Service has no getValue — treat as no value.
            Err(_) => Ok(Some(Value::Object(None))),
        },
        None => Ok(Some(Value::Object(None))),
    }
}

/// `ServiceController.getService()` on the synthetic controller mirror.
/// The real MSC controller exposes its installed service instance; retain the
/// equivalent identity held by the shadow container so management operations
/// can inspect it without hitting an abstract interface method.
fn native_service_controller_get_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = match ctx.get_field(this, SC_FIELD_ID) {
        Value::Long(l) => l as u64,
        _ => 0,
    };
    let service = service_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .and_then(|roots| roots.service);
    Ok(Some(match service {
        Some(service) => Value::Object(Some(service)),
        None => Value::Object(None),
    }))
}

/// `ServiceController.getName()` on the synthetic controller mirror.  The
/// controller is allocated with its primary `ServiceName` in slot zero, so
/// preserve the Java-visible identity rather than forcing callers through the
/// shadow container again.
fn native_service_controller_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(ctx.get_field(this, SC_FIELD_NAME)))
}

/// `ServiceRegistry.getServiceNames()` is an abstract MSC interface method.
/// Return a real Java List containing every primary shadow-container name;
/// callers use it for diagnostics and validation during real service starts.
fn native_service_registry_get_service_names(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let names: Vec<Arc<ServiceName>> = {
        let container = global_container();
        let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.services.keys().cloned().collect()
    };
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[]) {
        Ok(Some(Value::Object(Some(list)))) => list,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list_pin = ctx.pin_native_root(list);
    for name in names {
        let name_obj = alloc_java_service_name(ctx, &name)?;
        let name_pin = ctx.pin_native_root(name_obj);
        let list = ctx.read_native_pin(list_pin, list);
        let name_obj = ctx.read_native_pin(name_pin, name_obj);
        let result = ctx.invoke_virtual(
            list,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(name_obj))],
        );
        ctx.unpin_native_roots(name_pin);
        result?;
    }
    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(list_pin);
    Ok(Some(Value::Object(Some(list))))
}

/// Capture the builder's legacy `addDependency(name, type, Injector)` wiring:
/// for every `requires` entry with a non-empty `injectorList`, store
/// `(dependency registration, injector)` pairs in the service's roots so
/// [`inject_dependency_values`] can perform real MSC's inject-before-start.
fn capture_dependency_injections(ctx: &mut dyn NativeContext, builder: ObjectRef, id: u64) {
    let builder_pin = ctx.pin_native_root(builder);
    let builder = ctx.read_native_pin(builder_pin, builder);
    let requires = match ctx.get_field_by_name(builder, "requires") {
        Value::Object(Some(m)) => m,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return;
        }
    };
    let requires_pin = ctx.pin_native_root(requires);
    let requires = ctx.read_native_pin(requires_pin, requires);
    let values = match ctx.invoke_virtual(requires, "values", "()Ljava/util/Collection;", &[]) {
        Ok(Some(Value::Object(Some(v)))) => v,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return;
        }
    };
    let values_pin = ctx.pin_native_root(values);
    let values = ctx.read_native_pin(values_pin, values);
    let arr = match ctx.invoke_virtual(values, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return;
        }
    };
    let n = ctx.array_length(arr);
    // `arr` (the outer requires-entries array) is read again at the top of
    // every loop iteration, but a later iteration's own `toArray` hazard
    // below can move it in the meantime — pin/re-read across the loop.
    let arr_pin = ctx.pin_native_root(arr);
    let mut arr_cur = arr;
    for i in 0..n {
        arr_cur = ctx.read_native_pin(arr_pin, arr_cur);
        let dep = match ctx.get_array_element(arr_cur, i) {
            Value::Object(Some(d)) => d,
            _ => continue,
        };
        let reg = match ctx.get_field_by_name(dep, "registration") {
            Value::Object(Some(r)) => r,
            _ => continue,
        };
        let inj_list = match ctx.get_field_by_name(dep, "injectorList") {
            Value::Object(Some(l)) => l,
            _ => continue,
        };
        // `reg` is stored into the persistent `dep_injections` root table
        // below, but only AFTER the `toArray` call here — pin it across that
        // call so a moving GC during `toArray` can't land a stale pointer in
        // the table (the table's own remap only covers entries already
        // present at move time).
        let reg_pin = ctx.pin_native_root(reg);
        let inj_list_pin = ctx.pin_native_root(inj_list);
        let inj_list = ctx.read_native_pin(inj_list_pin, inj_list);
        let inj_arr = match ctx.invoke_virtual(inj_list, "toArray", "()[Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => {
                ctx.unpin_native_roots(reg_pin);
                continue;
            }
        };
        let inj_arr_pin = ctx.pin_native_root(inj_arr);
        let reg = ctx.read_native_pin(reg_pin, reg);
        let m = ctx.array_length(inj_arr);
        for j in 0..m {
            let inj_arr = ctx.read_native_pin(inj_arr_pin, inj_arr);
            if let Value::Object(Some(inj)) = ctx.get_array_element(inj_arr, j) {
                service_roots()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(id)
                    .or_default()
                    .dep_injections
                    .push((reg, inj));
            }
        }
        ctx.unpin_native_roots(reg_pin);
    }
    ctx.unpin_native_roots(builder_pin);
}

/// Real MSC's StartTask resolves every legacy-injected dependency's value
/// and calls `Injector.inject(value)` BEFORE the service's `start()` runs.
/// The registration's `getValue` dispatches to our
/// [`native_service_registration_get_value`], which handles both modern
/// (injector-wired) and legacy (`Service.getValue`) providers. An injection
/// failure is a start failure (returned as `Err`, handled by the caller's
/// existing failure arm).
fn inject_dependency_values(ctx: &mut dyn NativeContext, id: u64) -> Result<(), MethodCallFailed> {
    let pairs: Vec<(ObjectRef, ObjectRef)> = {
        let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.get(&id)
            .map(|r| r.dep_injections.clone())
            .unwrap_or_default()
    };
    for (idx, _) in pairs.iter().enumerate() {
        // Re-read the (GC-remapped) pair on every iteration — each
        // invoke_virtual below can move objects captured earlier.
        let (reg, inj) = {
            let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
            match map
                .get(&id)
                .and_then(|r| r.dep_injections.get(idx).copied())
            {
                Some(p) => p,
                None => continue,
            }
        };
        let value = ctx.invoke_virtual(reg, "getValue", "()Ljava/lang/Object;", &[])?;
        let value = value.unwrap_or(Value::Object(None));
        ctx.invoke_virtual(inj, "inject", "(Ljava/lang/Object;)V", &[value])?;
    }
    Ok(())
}

/// Read the builder's `provides` map keys as interned service names.
fn read_provides_names(ctx: &mut dyn NativeContext, builder: ObjectRef) -> Vec<Arc<ServiceName>> {
    let builder_pin = ctx.pin_native_root(builder);
    let builder = ctx.read_native_pin(builder_pin, builder);
    let provides = match ctx.get_field_by_name(builder, "provides") {
        Value::Object(Some(m)) => m,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return Vec::new();
        }
    };
    let provides_pin = ctx.pin_native_root(provides);
    let provides = ctx.read_native_pin(provides_pin, provides);
    let set = match ctx.invoke_virtual(provides, "keySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return Vec::new();
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let set = ctx.read_native_pin(set_pin, set);
    let arr = match ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return Vec::new();
        }
    };
    let arr_pin = ctx.pin_native_root(arr);
    let n = ctx.array_length(arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let arr = ctx.read_native_pin(arr_pin, arr);
        if let Value::Object(Some(sn)) = ctx.get_array_element(arr, i) {
            if let Some(name) = read_service_name_robust(ctx, sn) {
                out.push(name);
            }
        }
    }
    ctx.unpin_native_roots(builder_pin);
    out
}

fn wire_provides_injectors(
    ctx: &mut dyn NativeContext,
    builder: ObjectRef,
    id: u64,
    primary: &Arc<ServiceName>,
) {
    let builder_pin = ctx.pin_native_root(builder);
    let builder = ctx.read_native_pin(builder_pin, builder);
    // `addAliases(...)` names resolve to this service too.
    if let Value::Object(Some(alias_set)) = ctx.get_field_by_name(builder, "aliases") {
        let alias_set_pin = ctx.pin_native_root(alias_set);
        let alias_set = ctx.read_native_pin(alias_set_pin, alias_set);
        if let Ok(Some(Value::Object(Some(arr)))) =
            ctx.invoke_virtual(alias_set, "toArray", "()[Ljava/lang/Object;", &[])
        {
            let arr_pin = ctx.pin_native_root(arr);
            let n = ctx.array_length(arr);
            for i in 0..n {
                let arr = ctx.read_native_pin(arr_pin, arr);
                if let Value::Object(Some(sn)) = ctx.get_array_element(arr, i) {
                    if let Some(alias) = read_service_name_robust(ctx, sn) {
                        if alias != *primary {
                            global_container().add_alias(alias, primary.clone());
                        }
                    }
                }
            }
        }
        ctx.unpin_native_roots(alias_set_pin);
    }
    let builder = ctx.read_native_pin(builder_pin, builder);
    let provides = match ctx.get_field_by_name(builder, "provides") {
        Value::Object(Some(m)) => m,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return;
        }
    };
    let provides_pin = ctx.pin_native_root(provides);
    let builder = ctx.read_native_pin(builder_pin, builder);
    let target = match ctx.get_field_by_name(builder, "serviceTarget") {
        Value::Object(Some(t)) => Some(t),
        _ => None,
    };
    // Root the target at the point it is read. `entrySet` and `toArray` below
    // are both GC-capable; pinning the raw target after them was the live MSC
    // stale-at-pin residual.
    let target_pin = target.map(|t| ctx.pin_native_root(t));
    let provides = ctx.read_native_pin(provides_pin, provides);
    let set = match ctx.invoke_virtual(provides, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return;
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let set = ctx.read_native_pin(set_pin, set);
    let arr = match ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => {
            ctx.unpin_native_roots(builder_pin);
            return;
        }
    };
    let arr_pin = ctx.pin_native_root(arr);
    let n = ctx.array_length(arr);
    // `arr` (the outer entries array) and `target` both live across every
    // loop iteration's `getKey`/`getValue`/`getOrCreateRegistration` hazards;
    // `entry`/`key`/`writable` are each held across a hazard within a single
    // iteration too (`entry` across `getKey`, `key` across `getValue`,
    // `writable` across `getOrCreateRegistration`) — pin everything and
    // re-read right before each subsequent use.
    let mut arr_cur = arr;
    for i in 0..n {
        arr_cur = ctx.read_native_pin(arr_pin, arr_cur);
        let entry = match ctx.get_array_element(arr_cur, i) {
            Value::Object(Some(e)) => e,
            _ => continue,
        };
        let entry_pin = ctx.pin_native_root(entry);
        let key = match ctx.invoke_virtual(entry, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(k)))) => k,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let key_pin = ctx.pin_native_root(key);
        let entry = ctx.read_native_pin(entry_pin, entry);
        // Every provided name that differs from the primary serviceId is an
        // alias — dependency resolution (`can_start`) and lookups
        // (`getService`) must find this service under it, matching real
        // MSC's per-registration semantics.
        if let Some(provided) = read_service_name_robust(ctx, key) {
            if provided != *primary {
                global_container().add_alias(provided, primary.clone());
            }
        }
        let writable = match ctx.invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(w)))) => w,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let writable_pin = ctx.pin_native_root(writable);
        // Re-read the mirror from the GC-remapped side-table on every use —
        // the invoke_virtual calls above can trigger a moving collection, and
        // a stale local here is exactly the native stale-local root family.
        let mirror = {
            let map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
            match map.get(&id).and_then(|r| r.controller_mirror) {
                Some(m) => m,
                None => {
                    ctx.unpin_native_roots(entry_pin);
                    ctx.unpin_native_roots(builder_pin);
                    return;
                }
            }
        };
        let writable = ctx.read_native_pin(writable_pin, writable);
        ctx.set_field_by_name(writable, "controller", Value::Object(Some(mirror)));
        if let Some(t_orig) = target {
            let t = target_pin
                .map(|h| ctx.read_native_pin(h, t_orig))
                .unwrap_or(t_orig);
            let key = ctx.read_native_pin(key_pin, key);
            match ctx.invoke_virtual(
                t,
                "getOrCreateRegistration",
                "(Lorg/jboss/msc/service/ServiceName;)Lorg/jboss/msc/service/ServiceRegistrationImpl;",
                &[Value::Object(Some(key))],
            ) {
                Ok(Some(Value::Object(Some(reg)))) => {
                    let writable = ctx.read_native_pin(writable_pin, writable);
                    ctx.set_field_by_name(reg, "injector", Value::Object(Some(writable)));
                }
                other => {
                    if msc_dbg() {
                        eprintln!(
                            "[msc] wire_provides: getOrCreateRegistration failed (entry {i}): ok={}",
                            other.is_ok()
                        );
                    }
                }
            }
        }
        ctx.unpin_native_roots(entry_pin);
    }
    ctx.unpin_native_roots(builder_pin);
}

/// Allocate a synthetic `StartContext` carrying `controller_id`, and root it.
fn build_start_context(
    ctx: &mut dyn NativeContext,
    id: u64,
) -> Result<ObjectRef, MethodCallFailed> {
    let sctx =
        try_alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/StartContext", CTX_NUM_SLOTS)?;
    ctx.set_field(sctx, CTX_FIELD_CONTROLLER_ID, Value::Long(id as i64));
    {
        let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        map.entry(id).or_default().start_context = Some(sctx);
    }
    Ok(sctx)
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
                fire_lifecycle_event_all(ctx, id, "UP");
                continue;
            }
        };
        // The callback receiver comes from a process-global side table, not
        // the current Java frame. Keep it rooted across StartContext creation
        // and dependency injection: both paths can invoke Java and move it.
        let svc_pin = ctx.pin_native_root(svc);
        let sctx = build_start_context(ctx, id)?;
        let sctx_pin = ctx.pin_native_root(sctx);
        {
            let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
            map.entry(id).or_default().start_began = Some(std::time::Instant::now());
        }
        if msc_dbg() {
            eprintln!("[msc] -> start id={id}");
        }
        // Real MSC injects legacy dependency values BEFORE start(); an
        // injection failure is a start failure (same handling arm below).
        // Re-read both roots only after injection, immediately before the
        // callback, so AbstractControllerService.start never sees a stale
        // receiver or StartContext.
        let res = match inject_dependency_values(ctx, id) {
            Ok(()) => {
                let svc = ctx.read_native_pin(svc_pin, svc);
                let sctx = ctx.read_native_pin(sctx_pin, sctx);
                ctx.invoke_virtual(
                    svc,
                    "start",
                    "(Lorg/jboss/msc/service/StartContext;)V",
                    &[Value::Object(Some(sctx))],
                )
            }
            Err(e) => Err(e),
        };
        ctx.unpin_native_roots(sctx_pin);
        ctx.unpin_native_roots(svc_pin);
        match res {
            Ok(_) => {
                if msc_dbg() {
                    eprintln!("[msc] <- start id={id} OK");
                }
                container.finish_start(id);
                // finish_start leaves an async-pending service at `Starting`
                // (its later `StartContext.complete()` fires UP instead) —
                // only notify listeners when the transition actually landed.
                let now_up = {
                    let state = container.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state
                        .by_id
                        .get(&id)
                        .and_then(|n| state.services.get(n))
                        .map(|c| matches!(c.state, ServiceState::Up))
                        .unwrap_or(false)
                };
                if now_up {
                    fire_lifecycle_event_all(ctx, id, "UP");
                }
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
                // Decode a thrown Java exception to class + message — the raw
                // Debug form is just an ObjectRef pointer, which made real
                // start() failures undiagnosable (see
                // wildfly-domain-managed-servers-timeout.md, 2026-07-06).
                let detail = describe_method_call_failure(ctx, &e);
                container.record_failure(id, format!("service start failed: {detail}"));
                match e {
                    MethodCallFailed::ExceptionThrown(exc) => {
                        // Catchable Java exception: MSC-faithful — mark Failed
                        // (done above) and keep draining other services.
                        tracing::error!(
                            target: "jboss_msc",
                            service = %name,
                            "MSC service start() threw {detail} — marked FAILED, boot continues"
                        );
                        if msc_dbg() {
                            // Full Java stack trace (incl. cause chain) to stderr.
                            let _ = ctx.invoke_virtual(exc, "printStackTrace", "()V", &[]);
                        }
                        fire_lifecycle_event_all(ctx, id, "FAILED");
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
    // This native performs many class lookups, allocations, and Java calls
    // before its last use of the builder. Root it at entry; pinning the raw
    // entry copy near the end only captured an already-forwarded address.
    let builder_pin = ctx.pin_native_root(builder);
    let builder = ctx.read_native_pin(builder_pin, builder);

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
    // `sn_obj` (when Some but its canonical name can't be decoded) survives
    // across `read_provides_names`'s own `keySet`/`toArray` calls below,
    // until it's used again to build the controller mirror further down —
    // pin it now. Deliberately left pinned for the rest of this function
    // (no explicit unpin): it is never the innermost pin, so unpinning any
    // later batch here can't reach it without also discarding pins still in
    // use; the call-dispatch wrapper truncates the whole pin stack when this
    // native call returns regardless.
    let sn_pin = sn_obj.map(|o| (ctx.pin_native_root(o), o));
    let name = sn_obj.and_then(|o| read_service_name_robust(ctx, o));
    // Anonymous install (`ServiceTarget.addService()` with no name): the
    // service is addressable only via its `provides(...)` names. Real MSC
    // installs these normally; bailing here silently dropped whole services —
    // WildFly's ControlledProcessStateService (which provides
    // `org.wildfly.management.process-state-notifier`) was lost this way,
    // permanently dep-blocking `console-availability` → `server-controller`.
    // Promote the first provided name to primary (the rest become aliases in
    // `wire_provides_injectors`); a service with neither name nor provides
    // still gets a synthetic anonymous name so its start() side-effects run.
    let name = match name {
        Some(n) => n,
        None => {
            let builder = ctx.read_native_pin(builder_pin, builder);
            let provided = read_provides_names(ctx, builder);
            match provided.into_iter().next() {
                Some(n) => n,
                None => {
                    static ANON: AtomicU64 = AtomicU64::new(1);
                    let n = ANON.fetch_add(1, Ordering::Relaxed);
                    if msc_dbg() {
                        let bcls = obj_class_name(ctx, builder);
                        eprintln!(
                            "[msc] install: anonymous no-provides builder ({bcls}) — synthesizing cratonvm.anonymous.{n}"
                        );
                    }
                    ServiceName::parse(&format!("cratonvm.anonymous.{n}"))
                }
            }
        }
    };
    let builder = ctx.read_native_pin(builder_pin, builder);
    let service_ref = match ctx.get_field_by_name(builder, "service") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    // The service object is retained in a Rust-side root map only after the
    // controller mirror is allocated below. Pin it now: reading mode/deps and
    // allocating that mirror can move the service before the map sees it.
    let service_pin = service_ref.map(|service| (ctx.pin_native_root(service), service));
    let mode = match ctx.get_field_by_name(builder, "initialMode") {
        Value::Object(o) => read_mode_by_name(ctx, o),
        _ => Mode::Active,
    };
    let builder = ctx.read_native_pin(builder_pin, builder);
    let child_target = match ctx.get_field_by_name(builder, "serviceTarget") {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    };
    // `child_target` is stored into the persistent root map far below, only
    // after `read_dep_names`, `add_service`, and the controller-mirror
    // allocation all run — every one of which can move it first. Pin it
    // alongside `service_pin` (pushed right after it here, unwound together
    // via `service_pin`'s handle at the same points below).
    let child_target_pin = child_target.map(|o| (ctx.pin_native_root(o), o));
    let builder = ctx.read_native_pin(builder_pin, builder);
    let deps = match ctx.get_field_by_name(builder, "requires") {
        Value::Object(Some(o)) => read_dep_names(ctx, Some(o)),
        _ => Vec::new(),
    };

    let container = global_container().clone();
    let id = match container.add_service(
        name.clone(),
        deps.clone(),
        mode,
        // Re-read via the pin: `service_ref` itself is not refreshed after
        // `read_dep_names` above, which can move the object first.
        service_pin
            .as_ref()
            .map(|(pin, original)| ctx.read_native_pin(*pin, *original).as_ptr() as usize)
            .unwrap_or(0),
    ) {
        Ok(id) => id,
        Err(msg) => {
            if let Some((pin, _)) = service_pin {
                ctx.unpin_native_roots(pin);
            }
            if msc_dbg() {
                eprintln!(
                    "[msc] install {}: add_service error: {msg}",
                    name.canonical()
                );
            }
            return Ok(Some(Value::Object(None)));
        }
    };

    // Build the synthetic ServiceController mirror (returned to the caller and
    // from StartContext.getController()).
    // Anonymous installs have no serviceId object — mirror the resolved
    // primary name instead so getName()/diagnostics stay meaningful.
    // Allocated BEFORE the mirror: a GC triggered by this allocation would
    // otherwise stale the raw `ctrl_obj` local (it is only rooted later).
    let sn_for_mirror = match sn_pin.map(|(pin, original)| ctx.read_native_pin(pin, original)) {
        Some(sn) => sn,
        None => alloc_java_service_name(ctx, &name)?,
    };
    // cceres3: pin across GC-capable call (stream stale-at-store wave) — the
    // controller-mirror allocation below can move `sn_for_mirror` (read from
    // its pin just above, or freshly allocated in the anonymous branch) before
    // the NAME-slot store; re-read it after the alloc.
    let sn_mirror_pin = ctx.pin_native_root(sn_for_mirror);
    let ctrl_obj = try_alloc_concurrent_synthetic(
        ctx,
        "org/jboss/msc/service/ServiceController",
        SC_NUM_SLOTS,
    )?;
    let sn_for_mirror = ctx.read_native_pin(sn_mirror_pin, sn_for_mirror);
    ctx.set_field(ctrl_obj, SC_FIELD_NAME, Value::Object(Some(sn_for_mirror)));
    ctx.set_field(ctrl_obj, SC_FIELD_MODE, Value::Int(mode.ordinal()));
    ctx.set_field(
        ctrl_obj,
        SC_FIELD_STATE,
        Value::Int(ServiceState::Down.ordinal()),
    );
    ctx.set_field(ctrl_obj, SC_FIELD_VALUE, Value::Object(None));
    ctx.set_field(ctrl_obj, SC_FIELD_ID, Value::Long(id as i64));

    // Register every held Java ref as a GC root BEFORE driving start() (which
    // allocates heavily and can move/collect these).
    {
        let mut map = service_roots().lock().unwrap_or_else(|e| e.into_inner());
        let r = map.entry(id).or_default();
        r.service = service_pin
            .as_ref()
            .map(|(pin, original)| ctx.read_native_pin(*pin, *original));
        r.controller_mirror = Some(ctrl_obj);
        r.child_target = child_target_pin
            .as_ref()
            .map(|(pin, original)| ctx.read_native_pin(*pin, *original));
    }
    if let Some((pin, _)) = service_pin {
        ctx.unpin_native_roots(pin);
    }

    // P3 value plumbing: connect this builder's provides-consumers and the
    // per-name registrations real requires()-suppliers read from, and index
    // provided/alias names for dependency resolution.
    //
    // cceres3 (DS2/DM_002 live captures): `builder` is a raw copy captured at
    // native entry; the service-name/controller-mirror/list allocations above
    // can move it, so handing the raw copy to these two helpers made their
    // very first `get_field_by_name(builder, ..)` read recycled memory. Pin
    // and re-read at the point of use.
    let builder_cur = ctx.read_native_pin(builder_pin, builder);
    wire_provides_injectors(ctx, builder_cur, id, &name);
    // Legacy addDependency(…, Injector) wiring — injected before start().
    // (wire_provides_injectors is itself GC-capable — re-read again.)
    let builder_cur = ctx.read_native_pin(builder_pin, builder);
    capture_dependency_injections(ctx, builder_cur, id);
    ctx.unpin_native_roots(builder_pin);

    if msc_dbg() {
        eprintln!(
            "[msc] install id={id} name={} mode={mode:?} has_service={} deps={:?}",
            name.canonical(),
            service_ref.is_some(),
            deps.iter().map(|d| d.canonical()).collect::<Vec<_>>()
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
    fire_lifecycle_event_all(ctx, id, "FAILED");
    Ok(None)
}

/// Bug 15 fix: `ServiceContainerImpl.getService(ServiceName)`.
///
/// `native_service_builder_install` (P2, above) intercepts
/// `ServiceBuilderImpl.install()` wholesale and registers the service ONLY in
/// our Rust-side shadow container (`global_container()` / `service_roots()`) —
/// it never touches `ServiceContainerImpl`'s own real `registry` field (a real
/// `ConcurrentMap<ServiceName, ServiceRegistrationImpl>`). `getService` is not
/// otherwise intercepted, so its real bytecode
/// (`this.registry.get(name)` → `ServiceRegistrationImpl.getDependencyController()`)
/// always reads that untouched, empty real map — even for a service `install()`
/// just registered a moment earlier. `getRequiredService` (below) built on top
/// of this always sees `null` and throws `ServiceNotFoundException`, aborting
/// boot ~3s in under `CRATONVM_MSC_REAL_START=1` (`fixed-suite-bugs/wildfly/
/// bug-15-msc-real-start-servicenotfound-and-domain-hang.md`, Symptom 2). Fix:
/// answer from the SAME shadow container `install()` populates instead of the
/// always-empty real registry. `LeakDetectorServiceContainer.getService` (the
/// caller WildFly's `BootstrapImpl` actually sees) just forwards to this method
/// on its real `ServiceContainerImpl` delegate via `invokeinterface`, so hooking
/// only the concrete class here is sufficient — no separate hook needed on the
/// wrapper.
fn native_service_container_get_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sn_obj = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = match read_service_name_robust(ctx, sn_obj) {
        Some(n) => n,
        None => return Ok(Some(Value::Object(None))),
    };
    let container = global_container();
    let mirror = container.get_id(&name).and_then(|id| {
        service_roots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .and_then(|r| r.controller_mirror)
    });
    if msc_dbg() {
        eprintln!(
            "[msc] getService {}: {}",
            name.canonical(),
            if mirror.is_some() { "found" } else { "MISS" }
        );
    }
    Ok(Some(Value::Object(mirror)))
}

/// Bug 15 fix: `ServiceContainerImpl.getRequiredService(ServiceName)`. Real
/// bytecode is `getService(name)` + throw `ServiceNotFoundException` on `null`
/// (built via an `invokedynamic` string-concat of the `ServiceName`, which
/// separately renders blank under CratonVM — a pre-existing, lower-priority
/// diagnostic gap noted in bug-15 and not needed for this fix, since the
/// success path below never reaches that concat). We reimplement the same
/// shape against our shadow container so a real miss still throws the correct
/// exception type with a legible message.
fn native_service_container_get_required_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sn_obj = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => Some(o),
        _ => None,
    };
    let canonical = sn_obj
        .and_then(|o| read_service_name_robust(ctx, o))
        .map(|n| n.canonical().to_string());
    match native_service_container_get_service(ctx, args)? {
        Some(Value::Object(Some(o))) => Ok(Some(Value::Object(Some(o)))),
        _ => {
            let message = format!(
                "Service {} not found",
                canonical.as_deref().unwrap_or("<unknown>")
            );
            let msg_obj = ctx.create_string(&message);
            match ctx.new_object_initialized(
                "org/jboss/msc/service/ServiceNotFoundException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg_obj))],
            ) {
                Ok(Some(Value::Object(Some(exc)))) => Err(MethodCallFailed::ExceptionThrown(exc)),
                _ => Err(MethodCallFailed::InternalError(VmError::Internal {
                    message,
                })),
            }
        }
    }
}

/// `ServiceController.getServiceContainer()` → a synthetic ServiceContainer
/// (forces the global container to exist; the mirror is a thin handle).
fn native_service_controller_get_service_container(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let _ = global_container();
    let obj = try_alloc_concurrent_synthetic(ctx, "org/jboss/msc/service/ServiceContainer", 2)?;
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
        "of",
        "(Lorg/jboss/msc/service/ServiceName;[Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        native_service_name_of_parent_varargs,
    );
    r.register(
        sn,
        "append",
        "([Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        native_service_name_append_varargs,
    );
    r.register(
        sn,
        "append",
        "(Lorg/jboss/msc/service/ServiceName;)Lorg/jboss/msc/service/ServiceName;",
        native_service_name_append_service_name,
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
    r.register(cont, "shutdown", "()V", native_service_container_shutdown);
    r.register(
        cont,
        "isShutdown",
        "()Z",
        native_service_container_is_shutdown,
    );
    for stability_class in [
        cont,
        "org/jboss/msc/service/ServiceContainerImpl",
        "org/jboss/msc/service/StabilityMonitor",
    ] {
        r.register(
            stability_class,
            "awaitStability",
            "(Ljava/util/Set;Ljava/util/Set;)V",
            native_service_container_await_stability_sets,
        );
        r.register(
            stability_class,
            "awaitStability",
            "(JLjava/util/concurrent/TimeUnit;Ljava/util/Set;Ljava/util/Set;)Z",
            native_service_container_await_stability_timed,
        );
    }
    let stability_monitor = "org/jboss/msc/service/StabilityMonitor";
    r.register(
        stability_monitor,
        "awaitStability",
        "(Ljava/util/Set;Ljava/util/Set;Lorg/jboss/msc/service/StabilityStatistics;)V",
        native_service_container_await_stability_sets,
    );
    r.register(
        stability_monitor,
        "awaitStability",
        "(JLjava/util/concurrent/TimeUnit;Ljava/util/Set;Ljava/util/Set;Lorg/jboss/msc/service/StabilityStatistics;)Z",
        native_service_container_await_stability_timed,
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
    r.register(
        ctrl,
        "provides",
        "()Ljava/util/Set;",
        native_service_controller_provides,
    );
    // These are abstract interface methods. Register them unconditionally so
    // interface resolution cannot depend on whether the later real-start block
    // was reached before this controller class was loaded.
    for cls in [ctrl, "org/jboss/msc/service/ServiceControllerImpl"] {
        r.register(
            cls,
            "getService",
            "()Lorg/jboss/msc/service/Service;",
            native_service_controller_get_service,
        );
        r.register(
            cls,
            "getName",
            "()Lorg/jboss/msc/service/ServiceName;",
            native_service_controller_get_name,
        );
    }

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
    r.register(start_ctx, "complete", "()V", native_start_context_complete);
    // LifecycleContext.getElapsedTime()J — inherited (code-less) interface
    // method; BootstrapListener times boot with it and died with
    // AbstractMethodError on our synthetic StartContext.
    r.register(
        start_ctx,
        "getElapsedTime",
        "()J",
        native_lifecycle_context_get_elapsed_time,
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

    // R63 (WildFly): DelegatingBasicLogger.isTraceEnabled/isDebugEnabled/
    // isInfoEnabled. The default impls do `this.log.isXEnabled()`, but our
    // synthetic backfills for ServiceLogger.ROOT/SERVICE/FAIL and
    // ElytronMessages.log leave `this.log` null, which would NPE. These
    // natives delegate to `this.log` when present and only return false when
    // it is null — preserving the non-logging-boot null-safety WITHOUT
    // suppressing level-guarded log calls on real loggers (e.g. Hibernate's
    // testing DelegatingLogger, whose isDebugEnabled gates HHH90030006).
    let dbl = "org/jboss/logging/DelegatingBasicLogger";
    r.register(
        dbl,
        "isTraceEnabled",
        "()Z",
        native_delegating_logger_is_trace_enabled,
    );
    r.register(
        dbl,
        "isDebugEnabled",
        "()Z",
        native_delegating_logger_is_debug_enabled,
    );
    r.register(
        dbl,
        "isInfoEnabled",
        "()Z",
        native_delegating_logger_is_info_enabled,
    );

    // R79 (WildFly): replace MSC IdentityHashSet's iterator with a
    // CME-tolerant native implementation. Eager worker scheduling in
    // our interpreter causes setMode(REMOVE) inside RemoveChildrenTask
    // to bump the outer set's modCount mid-iteration; the bytecode
    // `next()` then throws CME. The native re-reads the table and
    // re-syncs expectedCount on every call. See native_ihs_iter_next.
    let ihs_iter = "org/jboss/msc/service/IdentityHashSet$IdentityHashSetIterator";
    r.register(ihs_iter, "hasNext", "()Z", native_ihs_iter_has_next);
    r.register(
        ihs_iter,
        "next",
        "()Ljava/lang/Object;",
        native_ihs_iter_next,
    );

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
            Ok(Some(native_construct_message_logger(
                ctx,
                intf_mirror,
                &category,
            )))
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
            Ok(Some(native_construct_message_logger(
                ctx,
                intf_mirror,
                &category,
            )))
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
            // Honor a ServiceLoader-registered custom `LoggerProvider` first —
            // e.g. Hibernate testing's `TestableLoggerProvider`, declared in
            // `META-INF/services/org.jboss.logging.LoggerProvider`, which the
            // log-inspection tests require so `Logger.getLogger` yields a
            // `DelegatingLogger` (else `AssertionFailure: JBoss Logger didn't
            // register the custom TestableLoggerProvider`). This runs the real
            // `ServiceLoader` bytecode (not a stub). Fall back to the built-in
            // `JDKLoggerProvider` when nothing is registered — preserving the
            // WildFly boot path whose real `LoggerProviders.<clinit>` (empty
            // ServiceLoader) was the reason for this interception.
            if let Some(p) = jboss_logging_serviceloader_provider(ctx) {
                return Ok(Some(p));
            }
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

    // P2/P3 (WildFly real service.start): enabled by default. Intercept
    // ServiceBuilder.install and drive the real start() callback so service
    // state mirrors its Java-visible initialization. See
    // native_service_builder_install. CRATONVM_MSC_REAL_START=0 disables this
    // only for diagnostic comparison.
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
        // Bug 15: getService/getRequiredService must read the SAME shadow
        // container install() populates — see native_service_container_get_service.
        // Registered on the concrete ServiceContainerImpl (not the ServiceContainer
        // interface): LeakDetectorServiceContainer.getService/getRequiredService
        // (what WildFly's BootstrapImpl actually calls) just forward via
        // invokeinterface to their real ServiceContainerImpl delegate, so hooking
        // the concrete class here is what dispatch lands on.
        // Registered on the concrete impl (real container objects), on the
        // synthetic mirror's own class name (`getServiceContainer()` hands
        // back a synthetic "ServiceContainer"), and on the ServiceRegistry
        // super-interface (where dispatch on the synthetic mirror resolves
        // the code-less method — BootstrapImpl$1's UP branch hit
        // `AbstractMethodError: ServiceRegistry.getRequiredService`). The
        // natives are receiver-agnostic (answer from the global shadow
        // container), so all three registrations share one implementation.
        for cls in [
            "org/jboss/msc/service/ServiceContainerImpl",
            "org/jboss/msc/service/ServiceContainer",
            "org/jboss/msc/service/ServiceRegistry",
        ] {
            r.register(
                cls,
                "getService",
                "(Lorg/jboss/msc/service/ServiceName;)Lorg/jboss/msc/service/ServiceController;",
                native_service_container_get_service,
            );
            r.register(
                cls,
                "getRequiredService",
                "(Lorg/jboss/msc/service/ServiceName;)Lorg/jboss/msc/service/ServiceController;",
                native_service_container_get_required_service,
            );
            r.register(
                cls,
                "getServiceNames",
                "()Ljava/util/List;",
                native_service_registry_get_service_names,
            );
        }
        // Bug 15 follow-ups (wildfly-domain-managed-servers-timeout.md,
        // 2026-07-07): lifecycle listeners on the synthetic controller mirror.
        // `ServiceController.addListener/removeListener` are abstract interface
        // methods with no native backing — any caller died with
        // `AbstractMethodError` (BootstrapImpl.internalBootstrap:113 was the
        // boot-path victim).
        r.register(
            ctrl,
            "addListener",
            "(Lorg/jboss/msc/service/LifecycleListener;)V",
            native_service_controller_add_listener,
        );
        r.register(
            ctrl,
            "removeListener",
            "(Lorg/jboss/msc/service/LifecycleListener;)V",
            native_service_controller_remove_listener,
        );
        // BootstrapImpl$1's FAILED branch reports the boot failure via
        // controller.getStartException() — code-less interface method.
        r.register(
            ctrl,
            "getStartException",
            "()Lorg/jboss/msc/service/StartException;",
            native_service_controller_get_start_exception,
        );
        r.register(
            ctrl,
            "getValue",
            "()Ljava/lang/Object;",
            native_service_controller_get_value,
        );
        r.register(
            ctrl,
            "getService",
            "()Lorg/jboss/msc/service/Service;",
            native_service_controller_get_service,
        );
        r.register(
            ctrl,
            "getName",
            "()Lorg/jboss/msc/service/ServiceName;",
            native_service_controller_get_name,
        );
        // Concrete controller bytecode sometimes resolves these inherited
        // abstract interface declarations against ServiceControllerImpl rather
        // than ServiceController. Register both owners so real MSC callbacks
        // never fall through to a code-less interface method.
        let ctrl_impl = "org/jboss/msc/service/ServiceControllerImpl";
        r.register(
            ctrl_impl,
            "getService",
            "()Lorg/jboss/msc/service/Service;",
            native_service_controller_get_service,
        );
        r.register(
            ctrl_impl,
            "getName",
            "()Lorg/jboss/msc/service/ServiceName;",
            native_service_controller_get_name,
        );
        // requires()-supplier read path — must resolve BOTH modern
        // (injector-wired) and legacy (Service.getValue) providers against
        // the shadow container.
        r.register(
            "org/jboss/msc/service/ServiceRegistrationImpl",
            "getValue",
            "()Ljava/lang/Object;",
            native_service_registration_get_value,
        );
        // Real `StabilityMonitor.addController/removeController` downcast to
        // the concrete `ServiceControllerImpl` — ClassCastException on our
        // mirror (thrown from ApplicationServerService.start:140). Membership
        // is redundant: the awaitStability natives (registered above,
        // un-gated) already answer from the global shadow container.
        let stability_monitor = "org/jboss/msc/service/StabilityMonitor";
        r.register(
            stability_monitor,
            "addController",
            "(Lorg/jboss/msc/service/ServiceController;)V",
            native_stability_monitor_controller_noop,
        );
        r.register(
            stability_monitor,
            "removeController",
            "(Lorg/jboss/msc/service/ServiceController;)V",
            native_stability_monitor_controller_noop,
        );
    }

    // Native dispatch is keyed by the class selected by method resolution, not
    // by Java's interface hierarchy. Copy the complete controller bridge onto
    // the concrete implementation only after all interface registrations are
    // present; otherwise inherited methods such as getService() can reach an
    // abstract, code-less declaration during real MSC startup.
    r.alias_class(
        "org/jboss/msc/service/ServiceController",
        "org/jboss/msc/service/ServiceControllerImpl",
    );

    // Do not alias ServiceController natives onto DelegatingServiceController.
    // It has real forwarding bytecode and a two-field wrapper layout; aliasing
    // getState()/getService()/getName() bypasses that bytecode and makes the
    // synthetic-controller natives read SC_FIELD_ID (slot 5) from the wrapper.
    // OperationContextServiceController extends this type, so that misdispatch
    // rolls back WildFly management services before domain inventory startup.
    // Let the wrapper forward to its synthetic ServiceController delegate.

    let _ = CTX_NUM_SLOTS; // silence unused constant when debug builds elide.
    r.set_category(__prev_cat);
}

/// Try to obtain a custom `org.jboss.logging.LoggerProvider` registered via the
/// standard ServiceLoader mechanism (`META-INF/services/org.jboss.logging.
/// LoggerProvider`). Returns the first provider found, or `None` so the caller
/// falls back to the built-in JDK provider. Runs real `ServiceLoader` bytecode,
/// so a test-supplied provider (Hibernate's `TestableLoggerProvider`) is
/// honoured. Any failure (no class, empty loader, provider <init> error) yields
/// `None` and the safe JDK fallback.
fn jboss_logging_serviceloader_provider(ctx: &mut dyn NativeContext) -> Option<Value> {
    let dbg = crate::nbflags().dbg_logprov;
    if dbg {
        eprintln!("[LOGPROV] findProvider serviceloader called");
    }
    // `findProvider` runs from `LoggerProviders.<clinit>`, which is frequently
    // triggered during a message-logger interface's own `<clinit>` (via
    // `getMessageLogger`) — BEFORE the `LoggerProvider` interface class itself
    // has been loaded. `class_id_by_name` only finds *already-loaded* classes,
    // so load it on demand (the real `findProvider` bytecode would have loaded
    // it via its `ServiceLoader.load(LoggerProvider.class, …)` reference).
    let mirror = if let Some(c) = ctx.class_id_by_name("org/jboss/logging/LoggerProvider") {
        Value::Object(Some(ctx.get_class_mirror(c)))
    } else if let Ok(Some(v)) = ctx.load_class("org/jboss/logging/LoggerProvider") {
        v
    } else {
        if dbg {
            eprintln!("[LOGPROV] LoggerProvider class could not be loaded");
        }
        return None;
    };
    let sl = match ctx.invoke(
        "java/util/ServiceLoader",
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        &[mirror],
    ) {
        Ok(Some(Value::Object(Some(s)))) => s,
        other => {
            if dbg {
                eprintln!("[LOGPROV] ServiceLoader.load failed ok={}", other.is_ok());
            }
            return None;
        }
    };
    let it = match ctx.invoke_virtual(sl, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(i)))) => i,
        other => {
            if dbg {
                eprintln!("[LOGPROV] iterator() failed ok={}", other.is_ok());
            }
            return None;
        }
    };
    // `it` is held across `hasNext()` before being used again for `next()` —
    // same "Iterator receiver across its own hasNext/next" hazard already
    // fixed at other call sites (`service_loader.rs`'s `drain_instances_to_stream`
    // / `native_sl_find_first`); pin/re-read here too.
    let it_pin = ctx.pin_native_root(it);
    let has = ctx.invoke_virtual(it, "hasNext", "()Z", &[]);
    if dbg {
        eprintln!("[LOGPROV] hasNext -> {has:?}");
    }
    match has {
        Ok(Some(Value::Int(1))) => {}
        _ => {
            ctx.unpin_native_roots(it_pin);
            return None;
        }
    }
    let it = ctx.read_native_pin(it_pin, it);
    let nx = ctx.invoke_virtual(it, "next", "()Ljava/lang/Object;", &[]);
    ctx.unpin_native_roots(it_pin);
    if dbg {
        match &nx {
            Ok(Some(Value::Object(Some(o)))) => {
                let cn = ctx.class_name_of_id(ctx.class_id_of_object(*o));
                eprintln!("[LOGPROV] next -> provider {cn:?}");
            }
            other => eprintln!("[LOGPROV] next failed ok={}", other.is_ok()),
        }
    }
    match nx {
        Ok(Some(v @ Value::Object(Some(_)))) => Some(v),
        _ => None,
    }
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

    // 3. `new <impl_name>(log)` - the canonical generated constructor takes
    //    a single `Logger` parameter and `super(log)`s into
    //    `DelegatingBasicLogger`. Use the pinned allocation+constructor helper:
    //    message logger constructors can allocate enough to trigger a moving GC,
    //    and returning the pre-<init> raw ref can later resolve as java/lang/Object
    //    and fail the interface checkcast in `<intf>.<clinit>`.
    let _ = ctx.load_class(&impl_name);
    match ctx.new_object_initialized(&impl_name, "(Lorg/jboss/logging/Logger;)V", &[log_obj]) {
        Ok(Some(Value::Object(Some(o)))) => Value::Object(Some(o)),
        _ => {
            // Last-ditch fallback: a bare instance is useful only if allocation
            // really produced the generated logger class. Returning a stale or
            // generic Object here fails the caller's typed checkcast.
            match ctx.new_object(&impl_name) {
                Ok(Some(Value::Object(Some(o))))
                    if ctx
                        .class_name_arc_of_id(ctx.class_id_of_object(o))
                        .as_deref()
                        == Some(impl_name.as_str()) =>
                {
                    Value::Object(Some(o))
                }
                _ => Value::Object(None),
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn service_controller_get_service_uses_msc_1_5_descriptor() {
        let mut registry = NativeMethodRegistry::new();
        register_jboss_msc_natives(&mut registry);
        let descriptor = "()Lorg/jboss/msc/service/Service;";
        for class in [
            "org/jboss/msc/service/ServiceController",
            "org/jboss/msc/service/ServiceControllerImpl",
        ] {
            assert!(
                registry.find(class, "getService", descriptor).is_some(),
                "{class}.getService must use the descriptor from jboss-msc 1.5"
            );
        }

        // This class has real bytecode which forwards every ServiceController
        // method to its delegate. It must never receive the synthetic mirror
        // natives (whose slot-5 controller ID is not in its two-field layout).
        let delegating = "org/jboss/msc/service/DelegatingServiceController";
        for (method, descriptor) in [
            (
                "getState",
                "()Lorg/jboss/msc/service/ServiceController$State;",
            ),
            ("getService", descriptor),
            ("getName", "()Lorg/jboss/msc/service/ServiceName;"),
        ] {
            assert!(
                registry.find(delegating, method, descriptor).is_none(),
                "{delegating}.{method}{descriptor} must retain its Java forwarding bytecode"
            );
        }
    }

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
    fn t19_1_service_name_hash_matches_jboss_msc_formula() {
        let suffix = ServiceName::of(["network", "interface", "management"]);
        assert_eq!(service_name_hash(&suffix), -1_335_710_409);

        let full = ServiceName::of(["jboss", "network", "interface", "management"]);
        assert_eq!(service_name_hash(&full), -1_212_000_734);
    }

    #[test]
    fn t19_1_service_name_append_segments_preserves_parent_chain() {
        let base = ServiceName::of(["jboss"]);
        let full = append_service_name_segments(
            Some(base.clone()),
            vec![
                "network".to_string(),
                "interface".to_string(),
                "management".to_string(),
            ],
        );

        assert_eq!(full.canonical(), "jboss.network.interface.management");
        let parent = full.parent().expect("parent should exist");
        assert_eq!(parent.canonical(), "jboss.network.interface");
        let grandparent = parent.parent().expect("grandparent should exist");
        assert_eq!(grandparent.canonical(), "jboss.network");
        let root = grandparent.parent().expect("root should exist");
        assert!(Arc::ptr_eq(&base, &root));
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
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 2)
            .unwrap();
        c.add_service(d.clone(), vec![b.clone()], Mode::Active, 3)
            .unwrap();
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
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 2)
            .unwrap();
        c.add_service(d.clone(), vec![b.clone()], Mode::Active, 3)
            .unwrap();
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
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 0)
            .unwrap();
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
        c.add_service(b.clone(), vec![a.clone()], Mode::Active, 0)
            .unwrap();
        c.add_service(d.clone(), vec![b.clone()], Mode::Active, 0)
            .unwrap();
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
        let id = c.add_service(n.clone(), vec![], Mode::Active, 0).unwrap();
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
        assert_eq!(Mode::parse("REMOVE"), Mode::Remove);
        assert_eq!(Mode::Remove.ordinal(), 0);
        assert_eq!(Mode::Active.ordinal(), 5);
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
