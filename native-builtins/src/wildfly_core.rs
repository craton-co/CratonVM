// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.2.a — WildFly Core kernel: Deployment + Threads + Logging subsystems.
//!
//! After T19.1 MSC kernel (`jboss_msc.rs`) gets the service orchestrator
//! running, WildFly's boot sequence immediately pulls in three more kernel
//! subsystems that every other subsystem (naming / security / undertow /
//! datasources) depends on:
//!
//! 1. **Deployment** (`org.jboss.as.server.deployment.DeploymentUnit`) —
//!    the per-archive unit of work. Each deployed EAR/WAR/JAR becomes a
//!    `DeploymentUnit` tagged with an attachment map that phase processors
//!    use to communicate. Attachments are keyed by *identity* (Java
//!    `AttachmentKey` is an opaque sentinel object whose hashCode is its
//!    identityHashCode) — we mirror that by comparing `Arc::ptr_eq`.
//!
//! 2. **Threads** (`org.jboss.threads.EnhancedQueueExecutor`) — WildFly's
//!    bounded executor with a dedicated task queue; subsystems submit
//!    their async work to named pools (`ee`, `io`, `web`). We wrap each
//!    pool with a `std::thread::Builder` pool tagged `jboss-<kind>-N`
//!    so JFR / diagnostics can identify the origin.
//!
//! 3. **Logging** (`org.jboss.logmanager.LogManager` + `Logger`) —
//!    WildFly's drop-in JUL replacement.  Rather than duplicating the
//!    log machinery in Rust we thinly mirror the Java Logger object and
//!    route every `log()` / `info()` / `warning()` / etc. call through
//!    the existing `tracing` infrastructure.  Credential redaction runs
//!    at the mirror boundary so no subsystem can accidentally leak
//!    `password=…` into the logs regardless of what `tracing` subscriber
//!    is installed.
//!
//! ## Design invariants
//!
//! * **Pool safety**: thread pools are *bounded*.  Default core-size 4,
//!   max-size from the builder (clamped to 64).  Tasks run inside
//!   `catch_unwind(AssertUnwindSafe)` so a panic in one submitted task
//!   doesn't kill the worker.
//! * **Attachment-key identity**: `AttachmentKey` is never compared by
//!   content — it's an opaque marker.  The map uses `Arc<AttachmentKey>`
//!   as the key with pointer equality.
//! * **Log-level mapping**: JUL `Level.SEVERE → tracing::error!`,
//!   `WARNING → warn!`, `INFO → info!`, `CONFIG/FINE → debug!`, finer → trace!`.
//! * **Process state** (`ControlledProcessState`): a tiny state machine
//!   — the only two transitions our runtime actually drives are
//!   `Starting → Running` (after boot) and `Running → Stopping` (on
//!   shutdown).
//!
//! See `roadmap-100.md` T19.2.a.

#![allow(clippy::needless_pass_by_value)]

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::Value;

use crate::jboss_msc::{alloc_java_service_name, ServiceName};
use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ===========================================================================
// DeploymentUnit — the per-archive unit of work.
// ===========================================================================

/// Opaque `AttachmentKey<T>` marker.  Two keys are equal iff their `Arc`s
/// point at the same allocation — matches the JDK's identity-hashCode
/// behaviour for this class.
#[derive(Debug)]
pub struct AttachmentKey {
    /// Human-readable name used only in logging / diagnostics.  Never
    /// participates in equality.
    pub debug_name: String,
}

impl AttachmentKey {
    /// Allocate a fresh key with the given debug name.  Each call returns
    /// a distinct `Arc` so pointer equality is a true identity check.
    pub fn create(debug_name: impl Into<String>) -> Arc<AttachmentKey> {
        Arc::new(AttachmentKey {
            debug_name: debug_name.into(),
        })
    }
}

impl PartialEq for AttachmentKey {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for AttachmentKey {}

/// Wrapper `Arc<AttachmentKey>` that hashes / compares on identity so
/// `HashMap<AttachmentKeyIdentity, Value>` behaves like JDK's
/// identity-based `Attachments` map.
#[derive(Clone, Debug)]
struct AttachmentKeyIdentity(Arc<AttachmentKey>);

impl PartialEq for AttachmentKeyIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for AttachmentKeyIdentity {}
impl std::hash::Hash for AttachmentKeyIdentity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

fn native_path_address_from_elements(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = obj_arg(args, 0)?;
    // GC-SAFETY: `arr` (the source elements array) is read on every loop
    // iteration below, but `new_object_initialized` here and each
    // iteration's own `add` dispatch can all trigger a moving GC before
    // `arr` is read again. Pin it up front alongside `list` (already
    // protected) and re-read before each array access. Confirmed live via
    // CRATONVM_DBG_STALE_OBJREF (native_path_address_from_elements ->
    // ctx.invoke("PathAddress.pathAddress") -> invoke_on_class_shared_inner
    // dereferencing a stale receiver during WildFly parallel boot).
    let arr_pin = ctx.pin_native_root(arr);
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[])? {
        Some(Value::Object(Some(list))) => list,
        _ => try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?,
    };
    let arr = ctx.read_native_pin(arr_pin, arr);
    // `list` is a live ObjectRef that survives multiple GC-triggering calls
    // below (the `add` calls in the loop, then the `pathAddress`/ctor
    // dispatch); pin it across all of them and re-read before each use.
    let list_pin = ctx.pin_native_root(list);
    let len = ctx.array_length(arr);
    for i in 0..len {
        let arr = ctx.read_native_pin(arr_pin, arr);
        let elem = ctx.get_array_element(arr, i);
        let list = ctx.read_native_pin(list_pin, list);
        ctx.invoke_virtual(list, "add", "(Ljava/lang/Object;)Z", &[elem])?;
    }
    let list = ctx.read_native_pin(list_pin, list);
    let result = match ctx.invoke(
        "org/jboss/as/controller/PathAddress",
        "pathAddress",
        "(Ljava/util/List;)Lorg/jboss/as/controller/PathAddress;",
        &[Value::Object(Some(list))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        _ => {
            let list = ctx.read_native_pin(list_pin, list);
            ctx.new_object_initialized(
                "org/jboss/as/controller/PathAddress",
                "(Ljava/util/List;)V",
                &[Value::Object(Some(list))],
            )
        }
    };
    ctx.unpin_native_roots(arr_pin);
    result
}

/// One deployment in flight.  `name` is the archive name as the WildFly
/// server sees it (`keycloak-server.war`, etc.).  The service name
/// derived from it follows the WildFly convention
/// `jboss.deployment.unit.<name>`.
#[derive(Debug)]
pub struct DeploymentUnit {
    pub name: Arc<str>,
    pub service_name: Arc<ServiceName>,
    attachments: Mutex<HashMap<AttachmentKeyIdentity, AttachmentValue>>,
}

/// Values stored in the attachments map.  `Object(usize)` carries a
/// Java `ObjectRef`'s pointer — safe because the deployment unit owns
/// a strong reference that outlives any attachment lookup.
#[derive(Debug, Clone)]
pub enum AttachmentValue {
    Bool(bool),
    Int(i32),
    Long(i64),
    Str(Arc<str>),
    Object(usize),
    None,
}

impl DeploymentUnit {
    /// Build a unit from its archive name.  The resulting `ServiceName`
    /// uses the WildFly convention `jboss.deployment.unit.<name>`.
    pub fn new(name: &str) -> Arc<DeploymentUnit> {
        let service_name = wildfly_deployment_unit_name(name);
        Arc::new(DeploymentUnit {
            name: Arc::<str>::from(name),
            service_name,
            attachments: Mutex::new(HashMap::new()),
        })
    }

    /// Return the archive name as an interned `Arc<str>` (identity-safe
    /// for fast map lookups).
    pub fn get_name(&self) -> Arc<str> {
        self.name.clone()
    }

    /// Return the MSC service name for this unit
    /// (`jboss.deployment.unit.<name>`).
    pub fn get_service_name(&self) -> Arc<ServiceName> {
        self.service_name.clone()
    }

    /// Retrieve an attachment; returns `AttachmentValue::None` when the
    /// key is absent.  The lookup is by identity per JDK semantics.
    pub fn get_attachment(&self, key: &Arc<AttachmentKey>) -> AttachmentValue {
        let map = self.attachments.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&AttachmentKeyIdentity(key.clone()))
            .cloned()
            .unwrap_or(AttachmentValue::None)
    }

    /// Install an attachment; replaces any prior value under the same
    /// identity and returns the old value (if any).
    pub fn put_attachment(
        &self,
        key: &Arc<AttachmentKey>,
        value: AttachmentValue,
    ) -> AttachmentValue {
        let mut map = self.attachments.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(AttachmentKeyIdentity(key.clone()), value)
            .unwrap_or(AttachmentValue::None)
    }

    /// Remove an attachment; returns the prior value (or `None` if the
    /// key was absent).
    pub fn remove_attachment(&self, key: &Arc<AttachmentKey>) -> AttachmentValue {
        let mut map = self.attachments.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&AttachmentKeyIdentity(key.clone()))
            .unwrap_or(AttachmentValue::None)
    }

    /// Number of attachments currently installed.
    pub fn attachment_count(&self) -> usize {
        self.attachments
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

/// Convert a deployment archive name to its canonical MSC service name
/// (`jboss.deployment.unit.<archive>`).  Matches
/// `Services.deploymentUnitName(String)` in WildFly 27+.
pub fn wildfly_deployment_unit_name(archive: &str) -> Arc<ServiceName> {
    ServiceName::of(["jboss", "deployment", "unit", archive])
}

/// The base `jboss.deployment.unit` service name — used as the common
/// prefix constant (`Services.JBOSS_DEPLOYMENT_UNIT`).
pub fn jboss_deployment_unit_base() -> Arc<ServiceName> {
    ServiceName::of(["jboss", "deployment", "unit"])
}

fn read_string_arg(ctx: &mut dyn NativeContext, value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    }
}

fn capability_service_name(base_name: &str, dynamic_parts: &[String]) -> Arc<ServiceName> {
    let mut name = ServiceName::parse(base_name);
    for part in dynamic_parts {
        if !part.is_empty() {
            name = name.append(part);
        }
    }
    name
}

fn capability_service_name_value(
    ctx: &mut dyn NativeContext,
    base_name: &str,
    dynamic_parts: &[String],
) -> Result<Value, MethodCallFailed> {
    let name = capability_service_name(base_name, dynamic_parts);
    Ok(Value::Object(Some(alloc_java_service_name(ctx, &name)?)))
}

/// `OperationContext.getCapabilityServiceName(String, Class)` fallback.
///
/// Real WildFly first consults the runtime capability registry and, when that
/// cannot parse a capability as a registry-backed capability, falls back to
/// `ServiceNameFactory.parseServiceName(capabilityName)`. Under CratonVM the
/// under-modeled registry path can produce null instead of throwing the
/// `IllegalStateException` that triggers that fallback, which later becomes
/// `ServiceBuilderImpl.requires(null)`. Mirror the fallback at the boundary.
fn native_operation_context_get_capability_service_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let base = read_string_arg(ctx, args.get(1)).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::NullPointerException {
            message: Some("capabilityName must not be null".to_string()),
        })
    })?;
    Ok(Some(capability_service_name_value(ctx, &base, &[])?))
}

/// `OperationContext.getCapabilityServiceName(String, String, Class)`.
fn native_operation_context_get_capability_service_name_dynamic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let base = read_string_arg(ctx, args.get(1)).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::NullPointerException {
            message: Some("capabilityBaseName must not be null".to_string()),
        })
    })?;
    let mut parts = Vec::new();
    if let Some(part) = read_string_arg(ctx, args.get(2)) {
        parts.push(part);
    }
    Ok(Some(capability_service_name_value(ctx, &base, &parts)?))
}

/// `OperationContext.getCapabilityServiceName(String, Class, String...)`.
fn native_operation_context_get_capability_service_name_varargs(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let base = read_string_arg(ctx, args.get(1)).ok_or_else(|| {
        MethodCallFailed::from(RuntimeError::NullPointerException {
            message: Some("capabilityBaseName must not be null".to_string()),
        })
    })?;
    let mut parts = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(3) {
        for i in 0..ctx.array_length(*arr) {
            if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                if let Some(part) = ctx.read_string(s) {
                    parts.push(part);
                }
            }
        }
    }
    Ok(Some(capability_service_name_value(ctx, &base, &parts)?))
}

// ===========================================================================
// EnhancedQueueExecutor + JBossThreadFactory — bounded thread pool.
// ===========================================================================

/// Configuration for an `EnhancedQueueExecutor`.  Values come from the
/// Java-side `Builder` calls; we clamp `max_size` to 64 to prevent
/// runaway resource allocation.
#[derive(Clone, Debug)]
pub struct QueueExecutorConfig {
    pub name: String,
    pub core_size: usize,
    pub max_size: usize,
    pub thread_prefix: String,
}

impl QueueExecutorConfig {
    /// Build a new config with sensible defaults (core=4, max=16) and
    /// the given pool name (influences the thread name pattern).
    pub fn new(name: impl Into<String>) -> Self {
        let nm = name.into();
        Self {
            thread_prefix: match nm.as_str() {
                "ee" => "jboss-ee".to_string(),
                "io" => "jboss-io".to_string(),
                "web" => "jboss-web".to_string(),
                other => format!("jboss-{other}"),
            },
            name: nm,
            core_size: 4,
            max_size: 16,
        }
    }

    /// Override `max_size`, clamped to `[1, 64]`.
    pub fn with_max_size(mut self, max: usize) -> Self {
        self.max_size = max.clamp(1, 64);
        if self.core_size > self.max_size {
            self.core_size = self.max_size;
        }
        self
    }

    /// Override `core_size`, clamped to `[0, max_size]`.
    pub fn with_core_size(mut self, core: usize) -> Self {
        self.core_size = core.min(self.max_size);
        self
    }
}

/// A single task submitted to the executor.  Wrapping in a boxed
/// `FnOnce` lets us type-erase the closure while still running it on a
/// worker thread.
type PoolTask = Box<dyn FnOnce() + Send + 'static>;

struct PoolState {
    queue: VecDeque<PoolTask>,
    shutdown: bool,
    /// Number of tasks that have completed (success or panic).
    completed: u64,
    /// Number of tasks dropped because the pool was full.
    rejected: u64,
}

/// `EnhancedQueueExecutor` — bounded, named thread pool.
///
/// Tasks are dispatched round-robin to a fixed-size worker set created
/// eagerly at `new()`.  `submit()` blocks briefly (via condvar) if the
/// queue is at `max_size`; the caller can use `try_submit()` for a
/// non-blocking rejection path.
pub struct EnhancedQueueExecutor {
    config: QueueExecutorConfig,
    inner: Mutex<PoolState>,
    cv: Condvar,
    /// Monotonic thread counter used for the name suffix.
    next_thread_id: AtomicUsize,
}

impl EnhancedQueueExecutor {
    /// Build a new executor and spawn `core_size` workers immediately.
    pub fn new(config: QueueExecutorConfig) -> Arc<EnhancedQueueExecutor> {
        let exec = Arc::new(EnhancedQueueExecutor {
            config: config.clone(),
            inner: Mutex::new(PoolState {
                queue: VecDeque::new(),
                shutdown: false,
                completed: 0,
                rejected: 0,
            }),
            cv: Condvar::new(),
            next_thread_id: AtomicUsize::new(0),
        });
        for _ in 0..config.core_size {
            // B9: a failed worker spawn (OS thread-limit / ENOMEM) is logged
            // and tolerated rather than panicking the process. The pool still
            // functions with whatever workers did start; if none start, callers
            // fall back to `drain_locally` / `drain_all_pending_runnables`.
            if let Err(e) = spawn_worker(exec.clone()) {
                tracing::warn!(
                    target: "wildfly_core",
                    pool = %config.name,
                    error = %e,
                    "failed to spawn EnhancedQueueExecutor worker — continuing with fewer workers"
                );
            }
        }
        exec
    }

    /// Submit a task.  Returns `Err` if the pool is shut down or the
    /// queue has reached `max_size`.
    pub fn submit<F: FnOnce() + Send + 'static>(&self, task: F) -> Result<(), String> {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown {
            return Err("EnhancedQueueExecutor: already shut down".to_string());
        }
        if state.queue.len() >= self.config.max_size {
            state.rejected += 1;
            return Err(format!(
                "EnhancedQueueExecutor[{}]: queue full (max={})",
                self.config.name, self.config.max_size
            ));
        }
        state.queue.push_back(Box::new(task));
        self.cv.notify_one();
        Ok(())
    }

    /// Drain the queue synchronously on the caller's thread.  Intended
    /// for tests that want deterministic completion without waiting on
    /// worker threads.
    pub fn drain_locally(&self) {
        loop {
            let task = {
                let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.pop_front()
            };
            match task {
                Some(t) => {
                    let _ = catch_unwind(AssertUnwindSafe(move || t()));
                    let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    st.completed += 1;
                }
                None => break,
            }
        }
    }

    /// Set the shutdown flag; worker threads drain pending tasks and
    /// exit on their next wakeup.
    pub fn shutdown(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.shutdown = true;
        self.cv.notify_all();
    }

    /// Count of completed tasks since pool creation.
    pub fn completed_count(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed
    }

    /// Count of tasks rejected because the queue was full.
    pub fn rejected_count(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rejected
    }

    /// Current queue depth (pending tasks not yet picked up).
    pub fn queue_depth(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .len()
    }

    /// Executor's configured name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Max queue size (also bound on concurrent outstanding work).
    pub fn max_size(&self) -> usize {
        self.config.max_size
    }

    /// Build a `std::thread::Builder` with the canonical WildFly thread
    /// name pattern — each invocation allocates a fresh id so the name
    /// is unique.  Exposed as `JBossThreadFactory.newThread(Runnable)`.
    pub fn new_thread_builder(&self) -> std::thread::Builder {
        let id = self.next_thread_id.fetch_add(1, Ordering::SeqCst);
        let name = format!("{}-{}", self.config.thread_prefix, id);
        std::thread::Builder::new().name(name)
    }
}

/// Spawn one worker thread for `exec`.
///
/// B9: an OS thread-limit / ENOMEM failure must not abort the whole process.
/// We return the spawn error so the caller can decide what to do (the pool
/// simply runs with fewer workers; tasks can still drain via the surviving
/// workers, `drain_locally`, or `drain_all_pending_runnables`).
fn spawn_worker(exec: Arc<EnhancedQueueExecutor>) -> std::io::Result<()> {
    let b = exec.new_thread_builder();
    let e2 = exec.clone();
    b.spawn(move || worker_loop(e2)).map(|_handle| ())
}

fn worker_loop(exec: Arc<EnhancedQueueExecutor>) {
    loop {
        let task = {
            let mut state = exec.inner.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.shutdown && state.queue.is_empty() {
                    return;
                }
                if let Some(t) = state.queue.pop_front() {
                    break t;
                }
                let res = exec
                    .cv
                    .wait_timeout(state, std::time::Duration::from_millis(500))
                    .unwrap_or_else(|e| e.into_inner());
                state = res.0;
            }
        };
        // Wrap every task in catch_unwind so one panic doesn't kill
        // the whole worker thread.
        let outcome = catch_unwind(AssertUnwindSafe(move || task()));
        let mut state = exec.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.completed += 1;
        if let Err(payload) = outcome {
            let msg = panic_payload_to_string(&payload);
            tracing::warn!(
                target: "wildfly_core",
                pool = %exec.config.name,
                error = %msg,
                "task panicked — worker survived, task dropped"
            );
        }
    }
}

fn panic_payload_to_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "task panicked (unknown payload)".to_string()
    }
}

/// `JBossThread.run()` is mostly a logging/exit-handler wrapper around
/// `super.run()`.  In CratonVM's current real-JDK layout, the Runnable target
/// is reliably stored in `Thread$FieldHolder.task`, while the JDK bytecode
/// reached by JBossThread's `invokespecial Thread.run()` can still read a
/// layout-variant direct `Thread.target` slot and silently return.  Bridge the
/// core behavior by invoking the resolved Runnable directly.
/// `JBossThread.onExit` hooks awaiting their thread's termination, keyed by VM
/// thread id. Each hook is held as a global root so the moving collector remaps
/// it while it waits (a bare `ObjectRef` in a process-global map would go
/// stale; `pin_native_root` is a per-thread stack and cannot outlive the call
/// that registered the hook).
fn jboss_exit_hooks() -> &'static Mutex<HashMap<u64, Vec<usize>>> {
    static HOOKS: OnceLock<Mutex<HashMap<u64, Vec<usize>>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run and discard the current thread's exit hooks. jboss-threads runs them in
/// reverse registration order (they are pushed onto a stack), and a throwing
/// hook must not stop the rest.
fn jboss_run_exit_hooks(ctx: &mut dyn NativeContext) {
    let tid = ctx.thread_id();
    let hooks = jboss_exit_hooks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&tid);
    let hooks = match hooks {
        Some(h) => h,
        None => return,
    };
    for root in hooks.into_iter().rev() {
        if let Some(hook) = ctx.resolve_global_root(root) {
            let _ = ctx.invoke_virtual(hook, "run", "()V", &[]);
        }
        ctx.remove_global_root(root);
    }
}

fn native_jboss_thread_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut target = ctx.get_field_by_name(this, "target");
    if matches!(target, Value::Object(None)) {
        if let Value::Object(Some(holder)) = ctx.get_field_by_name(this, "holder") {
            target = ctx.get_field_by_name(holder, "task");
        }
    }
    if matches!(target, Value::Object(None)) && ctx.object_num_fields(this) >= 4 {
        target = ctx.get_field(this, 3);
    }
    let result = if let Value::Object(Some(runnable)) = target {
        ctx.invoke_virtual(runnable, "run", "()V", &[]).map(|_| ())
    } else {
        Ok(())
    };
    // The real `JBossThread.run()` drains its exit handlers in a finally block,
    // so they run whether or not the task threw.
    jboss_run_exit_hooks(ctx);
    result?;
    Ok(None)
}

fn native_jboss_thread_factory_new_thread(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let runnable = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(Value::Object(Some(thread))) = ctx.new_object_initialized(
        "org/jboss/threads/JBossThread",
        "(Ljava/lang/Runnable;)V",
        &[runnable],
    )? {
        return Ok(Some(Value::Object(Some(thread))));
    }

    let thread = try_alloc_concurrent_synthetic(ctx, "org/jboss/threads/JBossThread", 5)?;
    let name = ctx.create_string("jboss-thread");
    ctx.set_field(thread, 0, Value::Object(Some(name)));
    ctx.set_field(thread, 1, Value::Int(5));
    ctx.set_field(thread, 3, runnable);
    ctx.set_field(thread, 4, Value::Int(0));
    Ok(Some(Value::Object(Some(thread))))
}

/// `JBossExecutors.protectedCallable(Callable)` — wraps a callable so
/// the user's thread-context-classloader is preserved.  In our runtime
/// classloader swapping is a no-op, so this simply returns the identity
/// wrapper; the production JBossExecutors code additionally installs a
/// TCCL restore in the happy path.  We document the limitation here so
/// the caller doesn't rely on semantics we don't implement.
pub fn protected_callable_identity<F, R>(f: F) -> impl FnOnce() -> R
where
    F: FnOnce() -> R,
{
    // Intentionally a no-op: TCCL is process-wide in our runtime and we
    // don't currently support per-thread class-loader override, so the
    // "protected" part is a nop.
    f
}

// ===========================================================================
// Global executor registry — the three subsystem-scoped pools WildFly
// needs during boot.
// ===========================================================================

/// Registry of the standard pool names + their executor handles.
struct ExecutorRegistry {
    pools: Mutex<HashMap<String, Arc<EnhancedQueueExecutor>>>,
}

fn executor_registry() -> &'static ExecutorRegistry {
    static REG: OnceLock<ExecutorRegistry> = OnceLock::new();
    REG.get_or_init(|| ExecutorRegistry {
        pools: Mutex::new(HashMap::new()),
    })
}

/// Look up (or create) the pool with `name`.  Returns a clone of the
/// shared executor handle.
pub fn get_or_create_pool(name: &str) -> Arc<EnhancedQueueExecutor> {
    let mut pools = executor_registry()
        .pools
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(p) = pools.get(name) {
        return p.clone();
    }
    let exec = EnhancedQueueExecutor::new(QueueExecutorConfig::new(name));
    pools.insert(name.to_string(), exec.clone());
    exec
}

/// Shut down every pool.  Idempotent; safe to call at process shutdown.
pub fn shutdown_all_pools() {
    let pools = executor_registry()
        .pools
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for p in pools.values() {
        p.shutdown();
    }
}

// ===========================================================================
// LogManager / Logger / Level — thin mirror routed through `tracing`.
// ===========================================================================

/// JUL log-level enum translated to our `tracing` target.  Values match
/// the JDK `java.util.logging.Level` `intValue()` constants so Java code
/// that compares levels by int observes the right ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JulLevel {
    Off,
    Severe,
    Warning,
    Info,
    Config,
    Fine,
    Finer,
    Finest,
    All,
}

impl JulLevel {
    pub fn name(self) -> &'static str {
        match self {
            JulLevel::Off => "OFF",
            JulLevel::Severe => "SEVERE",
            JulLevel::Warning => "WARNING",
            JulLevel::Info => "INFO",
            JulLevel::Config => "CONFIG",
            JulLevel::Fine => "FINE",
            JulLevel::Finer => "FINER",
            JulLevel::Finest => "FINEST",
            JulLevel::All => "ALL",
        }
    }

    /// Match the JDK `java.util.logging.Level.intValue()` constants so
    /// Java code that tests `level.intValue() >= Level.INFO.intValue()`
    /// sees the right ordering.
    pub fn int_value(self) -> i32 {
        match self {
            JulLevel::Off => i32::MAX,
            JulLevel::Severe => 1000,
            JulLevel::Warning => 900,
            JulLevel::Info => 800,
            JulLevel::Config => 700,
            JulLevel::Fine => 500,
            JulLevel::Finer => 400,
            JulLevel::Finest => 300,
            JulLevel::All => i32::MIN,
        }
    }

    pub fn parse(s: &str) -> JulLevel {
        match s {
            "OFF" => JulLevel::Off,
            "SEVERE" | "ERROR" => JulLevel::Severe,
            "WARNING" | "WARN" => JulLevel::Warning,
            "INFO" => JulLevel::Info,
            "CONFIG" => JulLevel::Config,
            "FINE" | "DEBUG" => JulLevel::Fine,
            "FINER" => JulLevel::Finer,
            "FINEST" | "TRACE" => JulLevel::Finest,
            "ALL" => JulLevel::All,
            _ => JulLevel::Info,
        }
    }
}

/// A Java-visible `Logger` mirror.  The real state (effective level,
/// handlers, filter) lives in the global `tracing` subscriber; this
/// mirror only retains the JDK-facing name + level override so
/// `getName()` / `isLoggable()` give the right answers.
pub struct LoggerMirror {
    pub name: Arc<str>,
    /// Per-logger level override; `None` means inherit from root.
    level: Mutex<Option<JulLevel>>,
}

impl LoggerMirror {
    pub fn new(name: &str) -> Arc<LoggerMirror> {
        Arc::new(LoggerMirror {
            name: Arc::<str>::from(name),
            level: Mutex::new(None),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn get_level(&self) -> Option<JulLevel> {
        *self.level.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_level(&self, level: Option<JulLevel>) {
        *self.level.lock().unwrap_or_else(|e| e.into_inner()) = level;
    }

    /// Route a log record to the global `tracing` subscriber.  The
    /// message is redacted first so credentials never reach the
    /// subscriber regardless of what formatter is installed.
    pub fn log(&self, level: JulLevel, message: &str) {
        let redacted = redact_credentials(message);
        match level {
            JulLevel::Severe => {
                tracing::error!(target: "wildfly_core::logger", logger = %self.name, "{}", redacted);
            }
            JulLevel::Warning => {
                tracing::warn!(target: "wildfly_core::logger", logger = %self.name, "{}", redacted);
            }
            JulLevel::Info | JulLevel::Config => {
                tracing::info!(target: "wildfly_core::logger", logger = %self.name, "{}", redacted);
            }
            JulLevel::Fine => {
                tracing::debug!(target: "wildfly_core::logger", logger = %self.name, "{}", redacted);
            }
            JulLevel::Finer | JulLevel::Finest | JulLevel::All => {
                tracing::trace!(target: "wildfly_core::logger", logger = %self.name, "{}", redacted);
            }
            JulLevel::Off => {
                // Do nothing; OFF suppresses output.
            }
        }
    }
}

/// Process-wide registry of named Loggers.  WildFly code calls
/// `LogManager.getLogger(String)` repeatedly for the same name and
/// relies on getting the same instance — we back that with this map.
struct LogManagerRegistry {
    loggers: Mutex<HashMap<String, Arc<LoggerMirror>>>,
}

fn log_manager() -> &'static LogManagerRegistry {
    static MGR: OnceLock<LogManagerRegistry> = OnceLock::new();
    MGR.get_or_init(|| LogManagerRegistry {
        loggers: Mutex::new(HashMap::new()),
    })
}

/// `LogManager.getLogger(name)` — returns the process-wide mirror for
/// this logger name, creating it on first access.
pub fn get_logger(name: &str) -> Arc<LoggerMirror> {
    let mut loggers = log_manager()
        .loggers
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(l) = loggers.get(name) {
        return l.clone();
    }
    let l = LoggerMirror::new(name);
    loggers.insert(name.to_string(), l.clone());
    l
}

/// Count of currently-registered loggers; test-only helper.
#[cfg(test)]
pub fn logger_count() -> usize {
    log_manager()
        .loggers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// Redact credentials in log messages.  Matches `password=...`,
/// `secret=...`, `token=...` (case-insensitive) and replaces the value
/// portion with `<redacted>`.  This runs on every log emission so
/// subsystems can't accidentally leak credentials regardless of what
/// `tracing` subscriber is installed.
pub fn redact_credentials(msg: &str) -> String {
    use regex::Regex;
    // Two passes:
    //   1) key=value / key: value forms — password=xxx, secret: abc.
    //   2) `Authorization: Bearer <token>` — the HTTP header pattern where
    //      the token follows a scheme word.
    static RE_KV: OnceLock<Regex> = OnceLock::new();
    static RE_BEARER: OnceLock<Regex> = OnceLock::new();
    let re_kv = RE_KV.get_or_init(|| {
        Regex::new(r"(?i)\b(password|passwd|pwd|secret|token|api[_-]?key)\s*[=:]\s*[^\s,;&]+")
            .expect("credential-redaction regex compiles")
    });
    let re_bearer = RE_BEARER.get_or_init(|| {
        // Match "Authorization: <scheme> <token>" and "Bearer <token>" /
        // "Basic <token>" forms.  Walks the header form separately from
        // kv pairs so "Authorization: Bearer tok123" collapses to a
        // single redaction rather than leaving the token on the right.
        Regex::new(r"(?i)\bauthorization\s*[=:]\s*\S+(\s+\S+)?|\b(bearer|basic)\s+\S+")
            .expect("bearer redaction regex compiles")
    });
    let pass1 = re_bearer
        .replace_all(msg, |_caps: &regex::Captures| "authorization=<redacted>")
        .into_owned();
    re_kv
        .replace_all(&pass1, |caps: &regex::Captures| {
            format!("{}=<redacted>", &caps[1])
        })
        .into_owned()
}

// ===========================================================================
// ControlledProcessState — the domain-level state machine.
// ===========================================================================

/// WildFly's `ControlledProcessState.State` values.  We track only the
/// states the runtime actually cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Stopped,
    Starting,
    Running,
    Reloading,
    Restarting,
    Stopping,
}

impl ProcessState {
    pub fn ordinal(self) -> i32 {
        match self {
            ProcessState::Stopped => 0,
            ProcessState::Starting => 1,
            ProcessState::Running => 2,
            ProcessState::Reloading => 3,
            ProcessState::Restarting => 4,
            ProcessState::Stopping => 5,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProcessState::Stopped => "STOPPED",
            ProcessState::Starting => "STARTING",
            ProcessState::Running => "RUNNING",
            ProcessState::Reloading => "RELOADING",
            ProcessState::Restarting => "RESTARTING",
            ProcessState::Stopping => "STOPPING",
        }
    }
}

/// Process-wide handle for the `ModelController` / `ControlledProcessState`.
/// Most methods return success; the real state machine lives here so
/// Java code inspecting `getState()` sees the right value.
pub struct ModelController {
    state: Mutex<ProcessState>,
    boot_generation: AtomicU64,
}

impl ModelController {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProcessState::Starting),
            boot_generation: AtomicU64::new(0),
        }
    }

    pub fn get_state(&self) -> ProcessState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mark the process as fully booted.  Called once at the end of
    /// `Main.main()` after all subsystems reported `Up`.
    pub fn mark_running(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *st = ProcessState::Running;
        self.boot_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Begin graceful shutdown; subsystems should drain in reverse
    /// dependency order.
    pub fn mark_stopping(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *st = ProcessState::Stopping;
    }

    /// Current boot generation — incremented on every `mark_running`
    /// transition so `reload()` handlers can detect a fresh boot.
    pub fn boot_generation(&self) -> u64 {
        self.boot_generation.load(Ordering::SeqCst)
    }
}

/// Process-wide singleton for the model controller.
pub fn global_model_controller() -> &'static Arc<ModelController> {
    static INSTANCE: OnceLock<Arc<ModelController>> = OnceLock::new();
    INSTANCE.get_or_init(|| Arc::new(ModelController::new()))
}

// ===========================================================================
// Java ↔ Rust glue layer.
// ===========================================================================
//
// Field layout (matches `synthetic_stub_fields` in class_manager.rs):
//
//   DeploymentUnit            (3): name (String), attachments_map (Object),
//                                   service_name (ServiceName).
//   Services                  (0): pure-static, no instance fields.
//   EnhancedQueueExecutor     (4): name (String), core_size (Int),
//                                   max_size (Int), tasks_queue (Object).
//   Logger                    (2): name (String), parent_handle (Object).
//   Level                     (2): name (String), value_int (Int).
//   ModelController           (1): state (Int ordinal).
//   ControlledProcessState    (1): state_enum_ordinal (Int).

const DU_FIELD_NAME: usize = 0;
#[allow(dead_code)]
const DU_FIELD_ATTACHMENTS: usize = 1;
const DU_FIELD_SERVICE_NAME: usize = 2;
const DU_NUM_FIELDS: usize = 3;

// Canonical-name slot for an `org.jboss.msc.service.ServiceName` mirror.
// Mirrors the (private) `SN_FIELD_CANONICAL` in jboss_msc.rs: slot 1 holds the
// full dotted name, which is the slot `read_java_service_name` reads back.
const SN_FIELD_CANONICAL: usize = 1;

const EXEC_FIELD_NAME: usize = 0;
const EXEC_FIELD_CORE: usize = 1;
const EXEC_FIELD_MAX: usize = 2;
#[allow(dead_code)]
const EXEC_FIELD_QUEUE: usize = 3;
const EXEC_NUM_FIELDS: usize = 4;

const LOG_FIELD_NAME: usize = 0;
#[allow(dead_code)]
const LOG_FIELD_PARENT: usize = 1;
const LOG_NUM_FIELDS: usize = 2;

const LVL_FIELD_NAME: usize = 0;
const LVL_FIELD_VALUE: usize = 1;
const LVL_NUM_FIELDS: usize = 2;

const MC_FIELD_STATE: usize = 0;
const MC_NUM_FIELDS: usize = 1;

fn native_services_deployment_unit_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Services.deploymentUnitName(String) — static; no `this`.
    let name = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let sn = wildfly_deployment_unit_name(&name);
    // R80: must populate `name` + `hashCode` (not just `canonicalName`) so
    // JDK `ServiceName.equals` does not NPE on `this.name == null`.
    let obj = crate::jboss_msc::alloc_java_service_name(ctx, &sn)?;
    // FIX: `alloc_java_service_name` writes the full dotted name via
    // `set_field_by_name(obj, "canonicalName", ..)` and the leaf segment via
    // `set_field_by_name(obj, "name", ..)`. The canonical-state slot for a
    // `ServiceName` mirror is slot 1 (`SN_FIELD_CANONICAL`), which is also the
    // slot `read_java_service_name` reads back. When the `"canonicalName"`
    // field name does not resolve to slot 1, the by-name write is dropped while
    // the `"name"` write lands the *leaf* segment ("war") into slot 1 —
    // truncating the mirror to the trailing dot-separated component instead of
    // the full `jboss.deployment.unit.<archive>` name. Anchor the canonical
    // dotted name into the canonical slot explicitly so the mirror always holds
    // the complete hierarchical service name.
    let canonical = ctx.create_string(sn.canonical());
    ctx.set_field(obj, SN_FIELD_CANONICAL, Value::Object(Some(canonical)));
    Ok(Some(Value::Object(Some(obj))))
}

fn native_deployment_unit_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name_val = ctx.get_field(this, DU_FIELD_NAME);
    match name_val {
        Value::Object(Some(_)) => Ok(Some(name_val)),
        _ => {
            let s = ctx.create_string("");
            Ok(Some(Value::Object(Some(s))))
        }
    }
}

fn native_deployment_unit_get_service_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sn_val = ctx.get_field(this, DU_FIELD_SERVICE_NAME);
    match sn_val {
        Value::Object(Some(_)) => Ok(Some(sn_val)),
        _ => {
            // Fall back to building a ServiceName from the archive name.
            let name = match ctx.get_field(this, DU_FIELD_NAME) {
                Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
                _ => String::new(),
            };
            let sn = wildfly_deployment_unit_name(&name);
            let obj = crate::jboss_msc::alloc_java_service_name(ctx, &sn)?;
            // FIX: same canonical-slot truncation as in
            // `native_services_deployment_unit_name` — anchor the full dotted
            // name into the canonical slot so readers of slot 1 see
            // `jboss.deployment.unit.<archive>` rather than just the leaf
            // segment.
            let canonical = ctx.create_string(sn.canonical());
            ctx.set_field(obj, SN_FIELD_CANONICAL, Value::Object(Some(canonical)));
            Ok(Some(Value::Object(Some(obj))))
        }
    }
}

fn native_log_manager_get_logger(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let _mirror = get_logger(&name); // Ensure the mirror is registered.
    let obj = try_alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/Logger", LOG_NUM_FIELDS)?;
    let name_obj = ctx.create_string(&name);
    ctx.set_field(obj, LOG_FIELD_NAME, Value::Object(Some(name_obj)));
    Ok(Some(Value::Object(Some(obj))))
}

/// `Logger.log(Level, String)` — honours the credential-redaction
/// pass and routes through `tracing`.
fn native_logger_log(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if args.len() < 3 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "Logger.log: expected 3 args (this, level, msg), got {}",
                args.len()
            ),
        }));
    }
    let this = obj_arg(args, 0)?;
    let level_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let msg = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let level = level_obj
        .and_then(|lvl_obj| match ctx.get_field(lvl_obj, LVL_FIELD_NAME) {
            Value::Object(Some(n)) => ctx.read_string(n).map(|s| JulLevel::parse(&s)),
            _ => None,
        })
        .unwrap_or(JulLevel::Info);
    let name = match ctx.get_field(this, LOG_FIELD_NAME) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    get_logger(&name).log(level, &msg);
    Ok(None)
}

fn native_logger_info(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_logger_level_helper(ctx, args, JulLevel::Info)
}
fn native_logger_warning(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_logger_level_helper(ctx, args, JulLevel::Warning)
}
fn native_logger_severe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_logger_level_helper(ctx, args, JulLevel::Severe)
}
fn native_logger_fine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_logger_level_helper(ctx, args, JulLevel::Fine)
}

fn native_logger_level_helper(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    level: JulLevel,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let msg = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let name = match ctx.get_field(this, LOG_FIELD_NAME) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        _ => String::new(),
    };
    get_logger(&name).log(level, &msg);
    Ok(None)
}

fn native_level_get(ctx: &mut dyn NativeContext, level: JulLevel) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "org/jboss/logmanager/Level", LVL_NUM_FIELDS)?;
    let name = ctx.create_string(level.name());
    ctx.set_field(obj, LVL_FIELD_NAME, Value::Object(Some(name)));
    ctx.set_field(obj, LVL_FIELD_VALUE, Value::Int(level.int_value()));
    Ok(Some(Value::Object(Some(obj))))
}

fn native_level_info(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    native_level_get(ctx, JulLevel::Info)
}
fn native_level_warning(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    native_level_get(ctx, JulLevel::Warning)
}
fn native_level_severe(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    native_level_get(ctx, JulLevel::Severe)
}
fn native_level_fine(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    native_level_get(ctx, JulLevel::Fine)
}
fn native_level_all(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    native_level_get(ctx, JulLevel::All)
}

fn native_process_state_get_state(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // getState() returns `ControlledProcessState$State` — the ENUM — not the
    // outer `ControlledProcessState`. The previous impl allocated a
    // `ControlledProcessState`, so any `x.getState().ordinal()` / switch-map
    // (e.g. `ModelControllerImpl$4`) threw
    // `NoSuchMethodError: ControlledProcessState.ordinal()I`. Return the REAL
    // enum-constant singleton matching our Rust-side process state (correct
    // type + identity + ordinal, so `==`, `ordinal()` and enum switches work).
    // Note: our `ProcessState` ordinals differ from the JDK `State` enum order,
    // so map by NAME, not ordinal. `Reloading`/`Restarting` map to the JDK's
    // `RELOAD_REQUIRED`/`RESTART_REQUIRED`.
    let name = match global_model_controller().get_state() {
        ProcessState::Stopped => "STOPPED",
        ProcessState::Starting => "STARTING",
        ProcessState::Running => "RUNNING",
        ProcessState::Reloading => "RELOAD_REQUIRED",
        ProcessState::Restarting => "RESTART_REQUIRED",
        ProcessState::Stopping => "STOPPING",
    };
    let cls = "org/jboss/as/controller/ControlledProcessState$State";
    let cid = match ctx.ensure_class_initialized(cls) {
        Ok(cid) => cid,
        Err(_) => return Ok(Some(Value::Object(None))),
    };
    if let Some(idx) = ctx.static_field_index_by_name(cid, name) {
        return Ok(Some(ctx.get_static_field(cid, idx)));
    }
    Ok(Some(Value::Object(None)))
}

// ===========================================================================
// ControlledProcessState state-transition shims.
//
// WildFly 39's `ControlledProcessState` stores its state in a JDK
// `AtomicStampedReference`. CratonVM intrinsifies that class with a 1-field
// layout (ref slot only), while the JDK bytecode for ASR routes writes
// through a VarHandle that we don't fully model. The combination leaves
// the `state` field of `ControlledProcessState` reading back as `null` on
// the `setStarting()` boot path, producing:
//
//   java.lang.NullPointerException: Cannot invoke set on null
//     at org.jboss.as.controller.ControlledProcessState.setStarting(...)
//     at org.jboss.as.controller.AbstractControllerService.start(...)
//
// which surfaces as `JBTHR00005: Operation failed` and aborts the server
// with `WFLYSRV0239`. Since the canonical state lives in our Rust-side
// `global_model_controller()` (and is what user code observing
// `getState()` already sees via the intrinsic above), we can safely
// short-circuit the bytecode state-transition methods to a no-op /
// state-update pair. The receiver may be `null` (interpreter dispatches
// the native even on null this), which matches the JDK contract because
// these methods only mutate per-instance bookkeeping that is otherwise
// unobserved.
// B6: these state-transition shims are only registered under the default-OFF
// `app-stubs` feature (see `register_wildfly_core_natives`). Suppress dead-code
// warnings in the DEFAULT build where they are intentionally not wired up.
#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]
fn native_process_state_set_starting(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    *global_model_controller()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = ProcessState::Starting;
    Ok(None)
}

#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]
fn native_process_state_set_running(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    global_model_controller().mark_running();
    Ok(None)
}

#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]
fn native_process_state_set_stopping(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    global_model_controller().mark_stopping();
    Ok(None)
}

#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]
fn native_process_state_set_stopped(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    *global_model_controller()
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = ProcessState::Stopped;
    Ok(None)
}

#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]
fn native_process_state_noop_object(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // setRestartRequired() / setReloadRequired() return a stamp token used
    // by their revert counterparts. We don't model RESTART_REQUIRED /
    // RELOAD_REQUIRED transitions; return null which `revert*` then ignores.
    Ok(Some(Value::Object(None)))
}

#[cfg_attr(not(feature = "app-stubs"), allow(dead_code))]
fn native_process_state_noop_void(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn native_exec_builder_build(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Signature: Builder.build() -> EnhancedQueueExecutor. `this` carries
    // the configured name / sizes in its mirror fields.  Missing values
    // fall back to defaults.
    let this = obj_arg(args, 0)?;
    let name = match ctx.get_field(this, EXEC_FIELD_NAME) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_else(|| "default".to_string()),
        _ => "default".to_string(),
    };
    let core = match ctx.get_field(this, EXEC_FIELD_CORE) {
        Value::Int(i) if i >= 0 => i as usize,
        _ => 4,
    };
    let max = match ctx.get_field(this, EXEC_FIELD_MAX) {
        Value::Int(i) if i >= 1 => i as usize,
        _ => 16,
    };
    let cfg = QueueExecutorConfig::new(name.clone())
        .with_max_size(max)
        .with_core_size(core);
    let _exec = get_or_create_pool(&name);
    // Build the Java-side mirror object; we don't expose the Rust
    // handle — subsequent calls rely on the pool registry keyed by name.
    let obj = try_alloc_concurrent_synthetic(
        ctx,
        "org/jboss/threads/EnhancedQueueExecutor",
        EXEC_NUM_FIELDS,
    )?;
    let name_obj = ctx.create_string(&cfg.name);
    ctx.set_field(obj, EXEC_FIELD_NAME, Value::Object(Some(name_obj)));
    ctx.set_field(obj, EXEC_FIELD_CORE, Value::Int(cfg.core_size as i32));
    ctx.set_field(obj, EXEC_FIELD_MAX, Value::Int(cfg.max_size as i32));
    Ok(Some(Value::Object(Some(obj))))
}

/// `EnhancedQueueExecutor$Builder.setKeepAliveTime(Duration)` — no-op shim.
///
/// The real setter calls `Assert.checkNotNullParam` and then checks
/// `duration.compareTo(Duration.ZERO) > 0`, throwing `JBTHR00109` otherwise.
/// In CratonVM the `Builder` instance fields are sometimes left at their
/// default (e.g. when initialized via reflection paths that bypass `<init>`),
/// so this validation fires spuriously. The pool's keep-alive policy isn't
/// observed on the Rust side (workers live for the executor's lifetime), so
/// dropping the value is safe — return `this` to keep the builder chain.
fn native_exec_builder_set_keep_alive_duration(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Object(Some(this))))
}

/// `EnhancedQueueExecutor$Builder.setKeepAliveTime(long, TimeUnit)` — no-op.
fn native_exec_builder_set_keep_alive_long(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Object(Some(this))))
}

// ---------------------------------------------------------------------------
// Round 89: Deferred Runnable queue for EnhancedQueueExecutor.execute.
//
// Round 69's "run synchronously on the calling thread" choice causes a subtle
// ordering bug at WildFly boot: `MSC$Service.addListener(...)` is invoked
// from `BootstrapImpl.bootstrap()` AFTER the MSC service controller has been
// `execute()`-d on EQE. With synchronous-on-caller execute, the listener
// chain fires before any subsystem starts, `AsyncFutureTask.setResult(...)`
// runs, the boot future flips to COMPLETE, and `Main.main` returns instantly
// (~18s of clinit work, then silent exit, no banner).
//
// Fix: ENQUEUE Runnables (per-EQE FIFO) and DRAIN them at the natural
// "I am about to wait" point — `AsyncFutureTask.await()`. The caller
// thread itself runs the drained tasks, so we keep single-thread semantics
// (no cross-thread interpreter dispatch) but the boot listener fires only
// AFTER the bootstrap task body has run end-to-end.
// ---------------------------------------------------------------------------

use cratonvm_types::ObjectRef;

/// Per-EQE pending-Runnable queue. Keyed by the EQE `this` ObjectRef.
/// `CRATONVM_EQE_SYNC_EXECUTE=1` reverts to the Round-69 sync-on-caller
/// behaviour (escape hatch for Keycloak in case the deferral regresses it).
///
/// GC note (gc-followups-20260706, fixed 2026-07-19): was KNOWN-UNSOUND
/// across GCs — the queued Runnable refs were neither rooted nor remapped,
/// so a moving GC landing in the enqueue→drain window (`execute` to the
/// drain in `AsyncFutureTask.await()`) left `drain_all_pending_runnables`
/// calling `invoke_virtual` on a stale `ObjectRef`, dispatching into
/// whatever the collector had since placed at that address. Under WildFly's
/// highly concurrent `parallel-extension-add` boot step — every extension's
/// activation goes through exactly this queue, with ~30 extensions
/// allocating/classloading simultaneously — this is the live mechanism
/// behind the `ClassCastException: java.lang.Object cannot be cast to X`
/// family (X being whatever class the stale address's reused object
/// happened to report): see
/// `wildfly-remoting-classcastexception-parallel-extension-add-FIXED.md`.
/// Fixed by rooting each queued Runnable at enqueue time
/// (`register_var_handle_root`, keyed by identity hash) and re-resolving to
/// the current address at drain time (`read_var_handle_root`), the same
/// pattern already used by `classloader_value_sidetable.rs`'s
/// `rooted_entry`/`resolve_entry`. The EQE map *key* (`this`) is still a raw
/// address and can go stale across a move too, but that only splits a given
/// EQE's tasks across two map buckets after the object relocates — every
/// bucket is still drained unconditionally by `drain_all_pending_runnables`,
/// so no task is lost or misdispatched; left as a documented, lower-severity
/// follow-up rather than folded into this fix.
static EQE_PENDING: OnceLock<Mutex<HashMap<ObjectRef, VecDeque<(i32, ObjectRef)>>>> =
    OnceLock::new();

fn eqe_pending() -> &'static Mutex<HashMap<ObjectRef, VecDeque<(i32, ObjectRef)>>> {
    EQE_PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drain ALL pending Runnables across every known EQE, running each
/// `run()` on the calling thread. Called from `AsyncFutureTask.await()`
/// just before we'd otherwise short-circuit to COMPLETE — this ensures
/// the bootstrap task body actually runs (subsystem start, listener
/// callbacks, log output) before the future is reported complete.
///
/// We iterate until no more tasks are produced (running task N may enqueue
/// task N+1), bounded by a generous safety cap to prevent infinite loops
/// from a misbehaving service.
fn drain_all_pending_runnables(ctx: &mut dyn NativeContext) {
    const MAX_ITERATIONS: usize = 4096;
    let mut iterations = 0usize;
    loop {
        if iterations >= MAX_ITERATIONS {
            break;
        }
        // Snapshot one runnable from any queue. Lock briefly to avoid
        // holding it while re-entering the interpreter.
        let next = {
            let mut map = match eqe_pending().lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let mut found: Option<(i32, ObjectRef)> = None;
            let mut empty_keys: Vec<ObjectRef> = Vec::new();
            for (k, q) in map.iter_mut() {
                if let Some(r) = q.pop_front() {
                    found = Some(r);
                    break;
                }
                empty_keys.push(*k);
            }
            for k in empty_keys {
                if let Some(q) = map.get(&k) {
                    if q.is_empty() {
                        map.remove(&k);
                    }
                }
            }
            found
        };
        match next {
            Some((identity_key, stale_ref)) => {
                // Re-resolve to the CURRENT address: any GC between this
                // Runnable's `execute()` enqueue and this drain may have
                // moved it (see the GC note on `EQE_PENDING`). Falls back to
                // the cached ref only if the root was never registered
                // (identity_key == 0, i.e. malformed enqueue) or the
                // registry lookup misses.
                let r = if identity_key != 0 {
                    ctx.read_var_handle_root(identity_key).unwrap_or(stale_ref)
                } else {
                    stale_ref
                };
                let _ = ctx.invoke_virtual(r, "run", "()V", &[]);
                iterations += 1;
            }
            None => break,
        }
    }
    if crate::nbflags().dbg_eqe {
        eprintln!("[eqe] drained {} runnables", iterations);
    }
    let _ = iterations;
}

fn async_future_status_is(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(Some(a)), Value::Object(Some(b))) => a == b,
        _ => false,
    }
}

fn async_future_status_is_named(
    ctx: &dyn NativeContext,
    status: &Value,
    singleton: &Value,
    expected_name: &str,
) -> bool {
    if async_future_status_is(status, singleton) {
        return true;
    }
    let status_obj = match status {
        Value::Object(Some(obj)) => *obj,
        _ => return false,
    };
    match ctx.get_field_by_name(status_obj, "name") {
        Value::Object(Some(name)) => ctx.read_string(name).as_deref() == Some(expected_name),
        _ => false,
    }
}

fn async_future_payload_result_matters(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    matches!(
        class_name.as_str(),
        "org/jboss/as/protocol/mgmt/ActiveOperationImpl"
            | "org/jboss/as/server/mgmt/domain/ServerBootOperationsService$FutureBootUpdates"
    )
}

fn async_future_wait_keepalive(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    timeout_ms: Option<u64>,
) -> Result<ObjectRef, MethodCallFailed> {
    let pin = ctx.pin_native_root(obj);
    ctx.monitor_enter(obj);
    let obj = ctx.read_native_pin(pin, obj);
    let wr = ctx.monitor_wait(obj, timeout_ms);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.monitor_exit(obj);
    ctx.unpin_native_roots(pin);
    wr?;
    Ok(obj)
}

fn async_future_await_payload_result(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
    waiting: &Value,
) -> MethodCallResult {
    loop {
        let status = ctx.get_field_by_name(this, "status");
        if !async_future_status_is_named(ctx, &status, waiting, "WAITING") {
            return Ok(Some(status));
        }
        this = async_future_wait_keepalive(ctx, this, Some(5))?;
    }
}

fn native_exec_execute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if args.len() < 2 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("execute: expected (this, runnable), got {}", args.len()),
        }));
    }
    let this = obj_arg(args, 0)?;
    let runnable_ref = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("execute: null Runnable".into()),
                },
            )));
        }
    };
    let name = match ctx.get_field(this, EXEC_FIELD_NAME) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_else(|| "default".to_string()),
        _ => "default".to_string(),
    };
    let pool = get_or_create_pool(&name);
    // Submit a bookkeeping marker so pool stats reflect activity.
    let _ = pool.submit(|| {});

    if crate::nbflags().eqe_sync_execute {
        // Round-69 behaviour (sync on caller). Escape hatch.
        let _ = ctx.invoke_virtual(runnable_ref, "run", "()V", &[]);
        return Ok(None);
    }

    // Round 89: enqueue for later drain in AsyncFutureTask.await().
    //
    // Root the Runnable BEFORE releasing it into the queue: `execute()` can
    // return well before the drain runs, and any GC in that window (highly
    // likely under `parallel-extension-add`'s concurrent allocation load)
    // would otherwise leave the queued `ObjectRef` dangling — see the GC
    // note on `EQE_PENDING`.
    ctx.register_var_handle_root(runnable_ref);
    let identity_key = ctx.identity_hash_code(runnable_ref);
    {
        let mut map = match eqe_pending().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        map.entry(this)
            .or_insert_with(VecDeque::new)
            .push_back((identity_key, runnable_ref));
        if crate::nbflags().dbg_eqe {
            eprintln!("[eqe] enqueue pool={} pending_keys={}", name, map.len());
        }
    }
    Ok(None)
}

/// `org.jboss.threads.AsyncFutureTask.await()` — short-circuit native that
/// never blocks. The pure-Java implementation reads `this.status` and, if
/// it's still `WAITING`, calls `Object.wait()` until `setResult/setFailed/
/// setCancelled` flips it. In real WildFly that transition happens when
/// the MSC service container completes startup of `jboss.as` and the
/// `BootstrapImpl$1` LifecycleListener fires.
///
/// Under CratonVM we don't drive the MSC service graph to RUNNING (the
/// service container's worker threads don't reliably advance through the
/// graph in real-JDK mode), so the bootstrap future stays in WAITING and
/// the main thread parks forever in `Object.wait()` — the canonical
/// Keycloak boot hang at:
///
/// ```text
///   org/jboss/modules/Main.main pc=2917
///   org/jboss/threads/AsyncFutureTask.get pc=8
///   org/jboss/threads/AsyncFutureTask.await pc=18
///   java/lang/Object.wait pc=5
/// ```
///
/// This native replaces the Java `await()` body entirely: if status is
/// already terminal we return it untouched (preserves COMPLETE/FAILED/
/// CANCELLED semantics for tasks the runtime DID drive to completion);
/// if it's still WAITING we transition it to COMPLETE in place. The
/// `result` field stays null — `AsyncFutureTask.get()` returns it, and
/// the only live caller in the Keycloak boot path (`Main.main`) discards
/// the result with `pop`. WildFly's `BootstrapImpl.startup()` is NOT on
/// the active boot path (Main calls `bootstrap().get()` directly, never
/// `startup()`).
fn native_async_future_task_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Round 89: drain any pending Runnables enqueued via EQE.execute first.
    // This is the natural "I am about to wait" point — running queued work
    // on the calling thread here means BootstrapImpl's LifecycleListener
    // (which sets the future result) fires only AFTER the bootstrap task
    // body has actually executed, not before MSC's addListener call returns.
    drain_all_pending_runnables(ctx);
    // Re-read status — it may have flipped to COMPLETE during the drain.
    let status = ctx.get_field_by_name(this, "status");
    // Resolve Status enum class and its WAITING/COMPLETE static fields.
    let status_cid = match ctx.ensure_class_initialized("org/jboss/threads/AsyncFuture$Status") {
        Ok(cid) => cid,
        Err(_) => {
            // Class wasn't loadable — return the current status untouched.
            return Ok(Some(status));
        }
    };
    let waiting = match ctx.static_field_index_by_name(status_cid, "WAITING") {
        Some(idx) => ctx.get_static_field(status_cid, idx),
        None => return Ok(Some(status)),
    };
    let complete = match ctx.static_field_index_by_name(status_cid, "COMPLETE") {
        Some(idx) => ctx.get_static_field(status_cid, idx),
        None => return Ok(Some(status)),
    };
    // Identity-compare against WAITING. Status is a singleton enum so
    // pointer equality is the right check.
    let is_waiting = async_future_status_is_named(ctx, &status, &waiting, "WAITING");
    if is_waiting {
        if async_future_payload_result_matters(ctx, this) {
            return async_future_await_payload_result(ctx, this, &waiting);
        }
        // Round 88: scope this bypass more narrowly. The unconditional flip
        // to COMPLETE was added for Keycloak's `org/jboss/modules/Main.main`
        // boot path (which discards the future result). But WildFly's
        // `org/jboss/as/server/Main.main` ALSO awaits via this native, and
        // returning COMPLETE with a null result causes WildFly's main thread
        // to return immediately — no subsystems start, no listening ports
        // come up, the JVM exits silently with no output. Round 87 fixes
        // got us past clinits but boot now no-ops.
        //
        // Heuristic: only short-circuit when the future's `result` field is
        // non-null (i.e. some producer DID call setResult on it; flipping
        // status is a benign nudge to wake the waiter). When `result` is
        // still null AND we're called from the WildFly boot path, this
        // means the MSC service container hasn't reached STABLE yet — flip
        // to FAILED so WildFly logs a real error instead of silent exit.
        let result_field = ctx.get_field_by_name(this, "result");
        let has_result = !matches!(result_field, Value::Object(None));
        if has_result {
            // HONEST path: a producer DID call setResult on this future, so the
            // result is genuinely available — flipping status to COMPLETE is a
            // benign nudge to wake the waiter, not a fabricated boot. Always on.
            ctx.set_field_by_name(this, "status", complete.clone());
            return Ok(Some(complete));
        }
        // result is still null AND status is still WAITING: the MSC service
        // container never reached STABLE, so there is NO real boot result.
        //
        // B2 (SyntheticStub): flipping status to COMPLETE here FAKES a
        // successful WildFly/Keycloak boot when the service graph never
        // actually started. The underlying gap is that CratonVM does not drive
        // the MSC service container to RUNNING in real-JDK mode (see
        // `jboss_msc.rs` worker/`drive_starts`). Per the no-synthetic-stub
        // policy this fake is gated behind the default-OFF `app-stubs` feature
        // so the DEFAULT build no longer fakes the boot; it instead returns the
        // real WAITING status, surfacing the hang/gap for diagnosis.
        #[cfg(feature = "app-stubs")]
        {
            // Keycloak compatibility: `org/jboss/modules/Main.main` discards
            // the future result, so the COMPLETE flip lets its boot proceed.
            // `CRATONVM_AWAIT_NO_SHORTCIRCUIT=1` opts out even under app-stubs
            // to surface the real WildFly hang (Object.wait) for diagnosis.
            if crate::nbflags().await_no_shortcircuit {
                std::thread::yield_now();
                return Ok(Some(status));
            }
            ctx.set_field_by_name(this, "status", complete.clone());
            return Ok(Some(complete));
        }
        #[cfg(not(feature = "app-stubs"))]
        {
            // DEFAULT build: do not fake the boot. Return the real WAITING
            // status untouched so the unmet MSC-startup gap is visible rather
            // than masked by a fabricated COMPLETE.
            let _ = complete;
            std::thread::yield_now();
            return Ok(Some(status));
        }
    }
    Ok(Some(status))
}

/// Identity hashes of synthetic `EnhancedQueueExecutor`s whose `shutdown()` has
/// been called. Only reachable with `CRATONVM_SYNTHETIC_EQE` set — the default
/// build runs the real jboss-threads bytecode, which maintains `threadStatus`.
fn eqe_shutdown_flags() -> &'static Mutex<std::collections::HashSet<i32>> {
    static FLAGS: OnceLock<Mutex<std::collections::HashSet<i32>>> = OnceLock::new();
    FLAGS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// `EnhancedQueueExecutor.shutdown()` / `shutdown(boolean)`. The synthetic
/// executor drains inline, so there is nothing to interrupt — recording the
/// request is the whole of the state transition.
fn native_eqe_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let key = ctx.identity_hash_code(*this);
        eqe_shutdown_flags()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
    }
    Ok(None)
}

/// Shared body of `isShutdown()`, `isTerminated()` and `awaitTermination(..)`
/// for the synthetic executor — see the comment at their registration.
fn native_eqe_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let down = match args.first() {
        Some(Value::Object(Some(this))) => {
            let key = ctx.identity_hash_code(*this);
            eqe_shutdown_flags()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&key)
        }
        _ => false,
    };
    Ok(Some(Value::Int(i32::from(down))))
}

/// Register all WildFly Core kernel natives with the method registry.
pub fn register_wildfly_core_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "org/jboss/as/controller/PathAddress",
        "pathAddress",
        "([Lorg/jboss/as/controller/PathElement;)Lorg/jboss/as/controller/PathAddress;",
        native_path_address_from_elements,
    );
    r.register(
        "org/jboss/threads/JBossThread",
        "run",
        "()V",
        native_jboss_thread_run,
    );
    // A JBossThread whose task throws routes the Throwable through here; the
    // real body hands it to the thread's UncaughtExceptionHandler, whose JDK
    // default prints "Exception in thread ..." plus the stack trace to
    // System.err. The no-op this replaced (added in be6055605 as part of a
    // bulk registration, with no rationale of its own) swallowed every
    // uncaught exception raised on a WildFly worker thread — a failed boot
    // task simply vanished. Reproduce the JDK default: print the Throwable.
    r.register(
        "org/jboss/threads/JBossThread",
        "dispatchUncaughtException",
        "(Ljava/lang/Throwable;)V",
        |ctx, args| {
            // Instance shape is [this, throwable]; if the receiver is ever
            // elided the arg list is just [throwable]. The Throwable is the
            // last argument either way.
            if let Some(Value::Object(Some(t))) = args.last() {
                let _ = ctx.invoke_virtual(*t, "printStackTrace", "()V", &[]);
            }
            Ok(None)
        },
    );
    // `onExit(hook)` registers a Runnable to be run once the calling thread
    // terminates, and returns whether it WAS registered. W3 kept `false`
    // because there was no per-thread exit-hook table; W4 adds one
    // (`jboss_exit_hooks`), drained by `native_jboss_thread_run` in the same
    // finally position the real `JBossThread.run()` uses.
    //
    // The real method returns false when the current thread is not a
    // JBossThread — its exit is not something jboss-threads can observe. We
    // keep that condition, and it is also exactly the condition under which our
    // drain runs: only a JBossThread's `run()` dispatches to the native above.
    r.register(
        "org/jboss/threads/JBossThread",
        "onExit",
        "(Ljava/lang/Runnable;)Z",
        |ctx, args| {
            // Static in jboss-threads, so `args` is `[hook]`; if it is ever
            // dispatched with a receiver the hook is still the last argument.
            let hook = match args.last() {
                Some(Value::Object(Some(h))) => *h,
                _ => return Ok(Some(Value::Int(0))),
            };
            let current = ctx.current_thread_object();
            let cls = ctx
                .class_name_of_id(ctx.class_id_of_object(current))
                .unwrap_or_default();
            // Accept JBossThread and its jboss-threads subclasses.
            if !cls.starts_with("org/jboss/threads/") {
                return Ok(Some(Value::Int(0)));
            }
            let tid = ctx.thread_id();
            let root = ctx.add_global_root(hook);
            jboss_exit_hooks()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(tid)
                .or_default()
                .push(root);
            Ok(Some(Value::Int(1)))
        },
    );
    r.register(
        "org/jboss/threads/JBossThreadFactory",
        "newThread",
        "(Ljava/lang/Runnable;)Ljava/lang/Thread;",
        native_jboss_thread_factory_new_thread,
    );
    r.register(
        "org/jboss/threads/JBossThreadFactory",
        "access$100",
        "(Lorg/jboss/threads/JBossThreadFactory;Ljava/lang/Runnable;)Ljava/lang/Thread;",
        native_jboss_thread_factory_new_thread,
    );

    // --- DIAGNOSTIC (CRATONVM_DBG_CAPVAL): trace the two inputs of
    // OperationContextImpl.validateCapabilities' `tolerant` flag
    // (`getRunningMode() == ADMIN_ONLY && (capabilitiesAlreadyBroken ||
    // isBooting())`). On HotSpot the WildFly subsystem-test boots with
    // valid=false but tolerant=true (WFLYCTL0362 logged, boot continues);
    // under CratonVM tolerant evaluates false and boot fails. Both
    // methods are trivial field getters — the overrides read the same
    // field by name and log, so behavior is preserved.
    if crate::nbflags().dbg_capval {
        let aoc = "org/jboss/as/controller/AbstractOperationContext";
        r.register(aoc, "isBooting", "()Z", |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let v = ctx.get_field_by_name(this, "booting");
            let b = matches!(v, Value::Int(n) if n != 0);
            eprintln!("[CAPVAL] isBooting -> {b} (raw={v:?})");
            Ok(Some(Value::Int(if b { 1 } else { 0 })))
        });
        r.register(
            aoc,
            "getRunningMode",
            "()Lorg/jboss/as/controller/RunningMode;",
            |ctx, args| {
                let this = match args.first() {
                    Some(Value::Object(Some(o))) => *o,
                    _ => return Ok(Some(Value::Object(None))),
                };
                let v = ctx.get_field_by_name(this, "runningMode");
                let name = if let Value::Object(Some(m)) = v {
                    match ctx.get_field_by_name(m, "name") {
                        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                        _ => "<no-name>".to_string(),
                    }
                } else {
                    "<null>".to_string()
                };
                // Identity check against the live RunningMode.ADMIN_ONLY static —
                // validateCapabilities compares with if_acmpne, so a name match
                // with a pointer mismatch IS the bug.
                let static_admin = ctx
                    .class_id_by_name("org/jboss/as/controller/RunningMode")
                    .and_then(|cid| {
                        ctx.static_field_index_by_name(cid, "ADMIN_ONLY")
                            .map(|idx| ctx.get_static_field(cid, idx))
                    });
                let same = matches!((v, static_admin), (Value::Object(Some(a)), Some(Value::Object(Some(b)))) if a == b);
                eprintln!(
                    "[CAPVAL] getRunningMode -> {name} ({v:?}) static_ADMIN_ONLY={static_admin:?} identical={same}"
                );
                Ok(Some(v))
            },
        );
    }

    // --- Services ---
    let services = "org/jboss/as/server/deployment/Services";
    r.register(
        services,
        "deploymentUnitName",
        "(Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
        native_services_deployment_unit_name,
    );

    for operation_context in [
        "org/jboss/as/controller/OperationContext",
        "org/jboss/as/controller/OperationContextImpl",
        "org/jboss/as/controller/AbstractOperationContext",
    ] {
        r.register(
            operation_context,
            "getCapabilityServiceName",
            "(Ljava/lang/String;Ljava/lang/Class;)Lorg/jboss/msc/service/ServiceName;",
            native_operation_context_get_capability_service_name,
        );
        r.register(
            operation_context,
            "getCapabilityServiceName",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Class;)Lorg/jboss/msc/service/ServiceName;",
            native_operation_context_get_capability_service_name_dynamic,
        );
        r.register(
            operation_context,
            "getCapabilityServiceName",
            "(Ljava/lang/String;Ljava/lang/Class;[Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
            native_operation_context_get_capability_service_name_varargs,
        );
    }

    // --- DeploymentUnit ---
    let du = "org/jboss/as/server/deployment/DeploymentUnit";
    r.register(
        du,
        "getName",
        "()Ljava/lang/String;",
        native_deployment_unit_get_name,
    );
    r.register(
        du,
        "getServiceName",
        "()Lorg/jboss/msc/service/ServiceName;",
        native_deployment_unit_get_service_name,
    );

    // --- LogManager / Logger ---
    let lm = "org/jboss/logmanager/LogManager";
    r.register(
        lm,
        "getLogger",
        "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
        native_log_manager_get_logger,
    );
    let logger = "org/jboss/logmanager/Logger";
    r.register(
        logger,
        "log",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        native_logger_log,
    );
    r.register(logger, "info", "(Ljava/lang/String;)V", native_logger_info);
    r.register(
        logger,
        "warning",
        "(Ljava/lang/String;)V",
        native_logger_warning,
    );
    r.register(
        logger,
        "severe",
        "(Ljava/lang/String;)V",
        native_logger_severe,
    );
    r.register(logger, "fine", "(Ljava/lang/String;)V", native_logger_fine);

    // --- Level ---
    let level = "org/jboss/logmanager/Level";
    r.register(
        level,
        "INFO",
        "()Lorg/jboss/logmanager/Level;",
        native_level_info,
    );
    r.register(
        level,
        "WARNING",
        "()Lorg/jboss/logmanager/Level;",
        native_level_warning,
    );
    r.register(
        level,
        "SEVERE",
        "()Lorg/jboss/logmanager/Level;",
        native_level_severe,
    );
    r.register(
        level,
        "FINE",
        "()Lorg/jboss/logmanager/Level;",
        native_level_fine,
    );
    r.register(
        level,
        "ALL",
        "()Lorg/jboss/logmanager/Level;",
        native_level_all,
    );

    // --- ControlledProcessState / ModelController ---
    r.register(
        "org/jboss/as/controller/ControlledProcessState",
        "getState",
        "()Lorg/jboss/as/controller/ControlledProcessState$State;",
        native_process_state_get_state,
    );
    r.register(
        "org/jboss/as/controller/ModelController",
        "getState",
        "()Lorg/jboss/as/controller/ControlledProcessState$State;",
        native_process_state_get_state,
    );
    // B6 (SyntheticStub): state-transition shims that BYPASS the real
    // `ControlledProcessState` bytecode to hide an AtomicStampedReference
    // VarHandle modeling gap (the ASR-backed `state` field reads back null on
    // the `setStarting()` boot path → NPE → WFLYSRV0239; see the comment on
    // the implementations above). These no-op the transition and update only
    // the Rust-side `global_model_controller()`, which FAKES the WildFly
    // process-state machine instead of fixing the underlying VarHandle defect.
    //
    // Per the no-synthetic-stub policy these are gated behind the default-OFF
    // `app-stubs` feature: in the DEFAULT build they are NOT installed, so the
    // real bytecode runs and the VarHandle gap surfaces honestly (rather than
    // being masked). `getState()` above stays registered unconditionally — it
    // is an honest bridge returning the real enum-constant singleton, not a
    // fake.
    #[cfg(feature = "app-stubs")]
    r.with_category(cratonvm_native_api::NativeKind::SyntheticStub, |r| {
        let cps = "org/jboss/as/controller/ControlledProcessState";
        r.register(cps, "setStarting", "()V", native_process_state_set_starting);
        r.register(cps, "setRunning", "()V", native_process_state_set_running);
        r.register(cps, "setStopping", "()V", native_process_state_set_stopping);
        r.register(cps, "setStopped", "()V", native_process_state_set_stopped);
        r.register(
            cps,
            "setRestartRequired",
            "()Ljava/lang/Object;",
            native_process_state_noop_object,
        );
        r.register(
            cps,
            "setReloadRequired",
            "()Ljava/lang/Object;",
            native_process_state_noop_object,
        );
        r.register(
            cps,
            "revertRestartRequired",
            "(Ljava/lang/Object;)V",
            native_process_state_noop_void,
        );
        r.register(
            cps,
            "revertReloadRequired",
            "(Ljava/lang/Object;)V",
            native_process_state_noop_void,
        );
        r.register(
            cps,
            "checkRestartRequired",
            "()V",
            native_process_state_noop_void,
        );
    });

    // --- EnhancedQueueExecutor ---
    // The synthetic Rust-backed executor below is a *partial* shadow and is
    // BROKEN as a whole: `build()` allocates a 4-field synthetic stub and
    // never runs the real `<init>(Builder)`, so the real `threadStatus` long
    // (packing core/max pool size) stays 0. `getCorePoolSize()` /
    // `getMaximumPoolSize()` are NOT shimmed → real bytecode reads 0 → the
    // executor believes its max pool size is 0 and never spawns a worker, so
    // tasks submitted via jboss-msc (every WildFly service lifecycle / boot
    // operation) are never run → `ModelControllerService` never starts → the
    // subsystem-test `waitForSetup` CountDownLatch is never counted down →
    // testSubsystem hangs 300s. Default to the REAL jboss-threads bytecode,
    // which initializes `threadStatus` and spawns real worker `Thread`s
    // (verified working in isolation). Opt back into the synthetic shim with
    // `CRATONVM_SYNTHETIC_EQE=1`. Same partial-shadow fix pattern as Phaser /
    // BlockingQueue. See gap-phaser-real-bytecode-state.md + the WildFly
    // testSubsystem write-up.
    if crate::nbflags().synthetic_eqe {
        r.register(
            "org/jboss/threads/EnhancedQueueExecutor$Builder",
            "build",
            "()Lorg/jboss/threads/EnhancedQueueExecutor;",
            native_exec_builder_build,
        );
        // Bypass `Builder.setKeepAliveTime` validation — WildFly 39's bootstrap
        // path constructs Builder defaults that under CratonVM end up with a
        // `null` (or non-positive) `keepAliveTime` Duration, which the real
        // setter rejects with `JBTHR00109`. We don't actually use the
        // keep-alive value (the pool is driven from our Rust-side
        // `EnhancedQueueExecutor`), so swallow the arg and return `this`.
        r.register(
            "org/jboss/threads/EnhancedQueueExecutor$Builder",
            "setKeepAliveTime",
            "(Ljava/time/Duration;)Lorg/jboss/threads/EnhancedQueueExecutor$Builder;",
            native_exec_builder_set_keep_alive_duration,
        );
        r.register(
            "org/jboss/threads/EnhancedQueueExecutor$Builder",
            "setKeepAliveTime",
            "(JLjava/util/concurrent/TimeUnit;)Lorg/jboss/threads/EnhancedQueueExecutor$Builder;",
            native_exec_builder_set_keep_alive_long,
        );
        r.register(
            "org/jboss/threads/EnhancedQueueExecutor",
            "execute",
            "(Ljava/lang/Runnable;)V",
            native_exec_execute,
        );

        // EnhancedQueueExecutor shutdown lifecycle. The synthetic executor is
        // Rust-backed (inline task drain — see native_exec_execute), so its
        // `threadStatus` long field is never maintained. The stock
        // `shutdown()` bytecode spins forever in `compareAndSetThreadStatus`
        // (an `AtomicLongFieldUpdater.compareAndSet` on that field, which can
        // never succeed against an unmaintained field). Shim the lifecycle to
        // clean terminal values so synthetic-mode cleanup completes.
        //
        // W4: these used to report terminal state UNCONDITIONALLY — `true` even
        // before `shutdown()` was ever called — on the grounds that the
        // synthetic executor drains tasks inline in `native_exec_execute` and
        // so never has in-flight work. The second half of that is right; the
        // first half is not: `ExecutorService.isShutdown()` is defined as "this
        // executor has been shut down", so answering true up front broke the
        // ordinary `if (!exec.isShutdown()) exec.execute(task)` guard, which
        // silently dropped every task. Track the one bit that actually exists.
        //
        // `isTerminated` == `isShutdown` here (inline drain ⇒ nothing is ever
        // pending once shutdown is requested), and `awaitTermination` returns
        // it immediately rather than sleeping: no work is outstanding, so no
        // amount of waiting on this thread can change the answer.
        //
        // Reachability: this whole block is gated on `nbflags().synthetic_eqe`,
        // which is PRESENCE-parsed from `CRATONVM_SYNTHETIC_EQE` — the default
        // build has it unset and runs the real jboss-threads bytecode, and note
        // that `CRATONVM_SYNTHETIC_EQE=0` still turns it ON.
        let eqe = "org/jboss/threads/EnhancedQueueExecutor";
        r.register(eqe, "shutdown", "()V", native_eqe_shutdown);
        r.register(eqe, "shutdown", "(Z)V", native_eqe_shutdown);
        r.register(eqe, "isShutdown", "()Z", native_eqe_is_shutdown);
        r.register(eqe, "isTerminated", "()Z", native_eqe_is_shutdown);
        r.register(
            eqe,
            "awaitTermination",
            "(JLjava/util/concurrent/TimeUnit;)Z",
            native_eqe_is_shutdown,
        );
    }

    // --- AsyncFutureTask ---
    // Short-circuit `await()` so the Keycloak boot path (`Main.main` ->
    // `Bootstrap.bootstrap().get()`) doesn't park forever in
    // `Object.wait()` waiting for an MSC service graph that we don't
    // fully drive to RUNNING.
    //
    // B2: the null-result WAITING->COMPLETE flip inside this native fakes a
    // successful boot and is now gated behind the default-OFF `app-stubs`
    // feature (see `native_async_future_task_await`). Tagged SyntheticStub so
    // the audit / `--dump-native-registry` census flags it; the honest paths
    // (already-terminal status, real has_result flip, queued-Runnable drain)
    // remain active in the default build.
    r.with_category(cratonvm_native_api::NativeKind::SyntheticStub, |r| {
        r.register(
            "org/jboss/threads/AsyncFutureTask",
            "await",
            "()Lorg/jboss/threads/AsyncFuture$Status;",
            native_async_future_task_await,
        );
    });
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn t19_2_a_deployment_unit_get_name_returns_interned_arc_str() {
        let du = DeploymentUnit::new("keycloak-server.war");
        let n1 = du.get_name();
        let n2 = du.get_name();
        // Interned `Arc<str>` — same pointer each time.
        assert!(
            Arc::ptr_eq(&n1, &n2),
            "get_name() should return identical Arc<str> refs"
        );
        assert_eq!(&*n1, "keycloak-server.war");
    }

    #[test]
    fn t19_2_a_services_deployment_unit_name_matches_convention() {
        let sn = wildfly_deployment_unit_name("my-app.war");
        assert_eq!(sn.canonical(), "jboss.deployment.unit.my-app.war");
        // The base constant — `Services.JBOSS_DEPLOYMENT_UNIT`.
        let base = jboss_deployment_unit_base();
        assert_eq!(base.canonical(), "jboss.deployment.unit");
        // And a derived unit must be a child of the base.
        let base2 = sn.parent().and_then(|p| p.parent()).unwrap();
        assert_eq!(base2.canonical(), "jboss.deployment");
    }

    /// B2/B6 gating contract:
    ///   * `ControlledProcessState` getState is an honest bridge → always
    ///     registered.
    ///   * The state-transition *setter* no-op shims (B6) that mask the
    ///     AtomicStampedReference VarHandle gap are only registered under the
    ///     default-OFF `app-stubs` feature; in the default build they are
    ///     absent so real bytecode runs.
    ///   * `AsyncFutureTask.await` (B2) is always registered (it has honest
    ///     paths) but tagged `SyntheticStub` so the audit census flags it.
    #[test]
    fn b2_b6_wildfly_stub_gating_contract() {
        let mut r = NativeMethodRegistry::new();
        // Deterministic regardless of the CRATONVM_NO_STUBS env: keep
        // SyntheticStub registrations so we can assert the tag + presence.
        r.set_drop_synthetic_stubs(false);
        register_wildfly_core_natives(&mut r);

        let cps = "org/jboss/as/controller/ControlledProcessState";
        // getState is an honest bridge — present in every build.
        assert!(
            r.find(
                cps,
                "getState",
                "()Lorg/jboss/as/controller/ControlledProcessState$State;",
            )
            .is_some(),
            "ControlledProcessState.getState must always be registered"
        );

        // B6: the setStarting no-op shim is gated on `app-stubs`.
        let set_starting = r.find(cps, "setStarting", "()V");
        if cfg!(feature = "app-stubs") {
            assert!(
                set_starting.is_some(),
                "with app-stubs, setStarting shim should be registered"
            );
            assert_eq!(
                r.kind_of(cps, "setStarting", "()V"),
                Some(cratonvm_native_api::NativeKind::SyntheticStub),
                "setStarting shim must be tagged SyntheticStub"
            );
        } else {
            assert!(
                set_starting.is_none(),
                "default build must NOT register the setStarting shim (real bytecode runs)"
            );
        }

        // B2: await is always registered but tagged SyntheticStub.
        assert_eq!(
            r.kind_of(
                "org/jboss/threads/AsyncFutureTask",
                "await",
                "()Lorg/jboss/threads/AsyncFuture$Status;",
            ),
            Some(cratonvm_native_api::NativeKind::SyntheticStub),
            "AsyncFutureTask.await must be tagged SyntheticStub for the audit"
        );
    }

    #[test]
    fn t19_2_a_thread_factory_builds_jboss_named_thread() {
        let cfg = QueueExecutorConfig::new("ee");
        let exec = EnhancedQueueExecutor::new(cfg);
        let b = exec.new_thread_builder();
        // `std::thread::Builder` doesn't expose its name publicly — the
        // only way to verify is to spawn and query inside. So do that:
        // the thread stashes its name into a channel.
        let (tx, rx) = std::sync::mpsc::channel();
        b.spawn(move || {
            let name = std::thread::current()
                .name()
                .unwrap_or("<anon>")
                .to_string();
            tx.send(name).unwrap();
        })
        .unwrap();
        let observed = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(
            observed.starts_with("jboss-ee-"),
            "expected jboss-ee-N, got {observed}"
        );
        exec.shutdown();
    }

    #[test]
    fn t19_2_a_enhanced_queue_executor_submit_runs_runnable() {
        let cfg = QueueExecutorConfig::new("t19_2_a_submit")
            .with_max_size(4)
            .with_core_size(0);
        let exec = EnhancedQueueExecutor::new(cfg);
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c2 = counter.clone();
        exec.submit(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        })
        .expect("submit succeeds");
        // No live workers because core_size=0 — drain synchronously.
        exec.drain_locally();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(exec.completed_count(), 1);
        exec.shutdown();
    }

    #[test]
    fn t19_2_a_enhanced_queue_executor_respects_max_size() {
        let cfg = QueueExecutorConfig::new("t19_2_a_max")
            .with_max_size(3)
            .with_core_size(0);
        let exec = EnhancedQueueExecutor::new(cfg);
        // Fill the queue exactly to max.
        for _ in 0..3 {
            exec.submit(|| std::thread::sleep(std::time::Duration::from_millis(1)))
                .expect("submit under capacity");
        }
        // Fourth submit must be rejected.
        let err = exec
            .submit(|| {})
            .expect_err("submit beyond max must reject");
        assert!(err.contains("queue full"), "got: {err}");
        assert_eq!(exec.rejected_count(), 1);
        // Also: max-size clamp protects against runaway.
        let huge = QueueExecutorConfig::new("huge").with_max_size(10_000);
        assert_eq!(huge.max_size, 64, "max_size clamped to 64");
        exec.shutdown();
    }

    #[test]
    fn t19_2_a_log_manager_get_logger_returns_mirror() {
        let a = get_logger("org.jboss.as.server.deployment");
        let b = get_logger("org.jboss.as.server.deployment");
        // Same name => same Arc handle.
        assert!(Arc::ptr_eq(&a, &b), "get_logger should intern by name");
        assert_eq!(a.name(), "org.jboss.as.server.deployment");
        // Level override round-trips.
        a.set_level(Some(JulLevel::Warning));
        assert_eq!(b.get_level(), Some(JulLevel::Warning));
        a.set_level(None);
        assert_eq!(b.get_level(), None);
    }

    #[test]
    fn t19_2_a_logger_info_routes_to_tracing() {
        // Structural test: calling info() must redact credentials before
        // delegating to the `tracing` subscriber.  We can't observe the
        // subscriber directly without dragging in a test subscriber, so
        // verify the redaction function (which runs inside `.log()`)
        // produces the right output.
        let redacted = redact_credentials("user=alice password=hunter2 other=val");
        assert!(!redacted.contains("hunter2"), "password must be stripped");
        assert!(redacted.contains("password=<redacted>"));
        // Also: secret, token get redacted.
        assert_eq!(redact_credentials("secret=abc"), "secret=<redacted>");
        assert_eq!(redact_credentials("token=xyz"), "token=<redacted>");
        // But non-credential substrings like `passwordLength=12` are untouched.
        let safe = redact_credentials("passwordLength=12");
        assert_eq!(safe, "passwordLength=12");
        // And the actual log call must not panic.
        let lg = get_logger("t19_2_a.logger.info");
        lg.log(JulLevel::Info, "hello world password=topsecret");
    }

    #[test]
    fn t19_2_a_level_info_value_matches_jdk() {
        // JDK `java.util.logging.Level.INFO.intValue() == 800`.
        assert_eq!(JulLevel::Info.int_value(), 800);
        assert_eq!(JulLevel::Warning.int_value(), 900);
        assert_eq!(JulLevel::Severe.int_value(), 1000);
        assert_eq!(JulLevel::Fine.int_value(), 500);
        assert_eq!(JulLevel::Config.int_value(), 700);
        // Level names round-trip.
        assert_eq!(JulLevel::parse("INFO"), JulLevel::Info);
        assert_eq!(JulLevel::parse("SEVERE"), JulLevel::Severe);
        assert_eq!(JulLevel::parse("GARBAGE"), JulLevel::Info);
        // Ordering: SEVERE > WARNING > INFO.
        assert!(JulLevel::Severe.int_value() > JulLevel::Warning.int_value());
        assert!(JulLevel::Warning.int_value() > JulLevel::Info.int_value());
    }

    #[test]
    fn t19_2_a_model_controller_state_after_boot_is_running() {
        let mc = ModelController::new();
        assert_eq!(mc.get_state(), ProcessState::Starting);
        assert_eq!(mc.boot_generation(), 0);
        mc.mark_running();
        assert_eq!(mc.get_state(), ProcessState::Running);
        assert_eq!(mc.boot_generation(), 1);
        // Stopping transition.
        mc.mark_stopping();
        assert_eq!(mc.get_state(), ProcessState::Stopping);
        // Ordinal stays consistent with enum order.
        assert_eq!(ProcessState::Starting.ordinal(), 1);
        assert_eq!(ProcessState::Running.ordinal(), 2);
    }

    #[test]
    fn t19_2_a_deployment_attachment_get_put_round_trip() {
        let du = DeploymentUnit::new("ear-test.ear");
        let key_a = AttachmentKey::create("moduleSpec");
        let key_b = AttachmentKey::create("annotationIndex");
        // Missing key -> None.
        assert!(matches!(du.get_attachment(&key_a), AttachmentValue::None));
        // Put a value.
        let prev = du.put_attachment(&key_a, AttachmentValue::Int(42));
        assert!(matches!(prev, AttachmentValue::None));
        match du.get_attachment(&key_a) {
            AttachmentValue::Int(i) => assert_eq!(i, 42),
            other => panic!("expected Int(42), got {:?}", other),
        }
        assert_eq!(du.attachment_count(), 1);
        // Different key => independent slot (identity-keyed).
        assert!(matches!(du.get_attachment(&key_b), AttachmentValue::None));
        // Overwrite returns prior value.
        let prev = du.put_attachment(&key_a, AttachmentValue::Str(Arc::<str>::from("hi")));
        match prev {
            AttachmentValue::Int(42) => {}
            other => panic!("expected old Int(42), got {:?}", other),
        }
        // Two distinct Arcs to the same conceptual "moduleSpec" name
        // must be *different* keys (identity, not name-based).
        let key_a2 = AttachmentKey::create("moduleSpec");
        assert!(matches!(du.get_attachment(&key_a2), AttachmentValue::None));
        // Remove releases the slot.
        du.remove_attachment(&key_a);
        assert!(matches!(du.get_attachment(&key_a), AttachmentValue::None));
        assert_eq!(du.attachment_count(), 0);
    }

    // Bonus smoke tests — registration, glue, and end-to-end flow.

    #[test]
    fn t19_2_a_wildfly_core_natives_registered() {
        let mut r = NativeMethodRegistry::new();
        register_wildfly_core_natives(&mut r);
        assert!(r
            .find(
                "org/jboss/as/server/deployment/Services",
                "deploymentUnitName",
                "(Ljava/lang/String;)Lorg/jboss/msc/service/ServiceName;",
            )
            .is_some());
        assert!(r
            .find(
                "org/jboss/as/controller/OperationContextImpl",
                "getCapabilityServiceName",
                "(Ljava/lang/String;Ljava/lang/Class;)Lorg/jboss/msc/service/ServiceName;",
            )
            .is_some());
        assert!(r
            .find(
                "org/jboss/logmanager/LogManager",
                "getLogger",
                "(Ljava/lang/String;)Lorg/jboss/logmanager/Logger;",
            )
            .is_some());
        // NOTE: `EnhancedQueueExecutor$Builder.build` is now opt-in behind
        // `CRATONVM_SYNTHETIC_EQE` (default = real jboss-threads bytecode), so
        // it is intentionally NOT registered here.
        assert!(r
            .find(
                "org/jboss/as/controller/ControlledProcessState",
                "getState",
                "()Lorg/jboss/as/controller/ControlledProcessState$State;",
            )
            .is_some());
        assert!(r
            .find(
                "org/jboss/logmanager/Logger",
                "info",
                "(Ljava/lang/String;)V",
            )
            .is_some());
    }

    #[test]
    fn t19_2_a_native_get_logger_builds_mirror_object() {
        let mut ctx = mock_ctx();
        let name_obj = ctx.create_string("com.example.App");
        let res = native_log_manager_get_logger(&mut ctx, &[Value::Object(Some(name_obj))])
            .expect("getLogger should not fail");
        let obj = match res {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected logger object, got {:?}", other),
        };
        // Field 0 should hold the name back.
        let stored = match ctx.get_field(obj, LOG_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert_eq!(stored, "com.example.App");
    }

    #[test]
    fn t19_2_a_get_or_create_pool_reuses_by_name() {
        let p1 = get_or_create_pool("t19_2_a_pool_reuse");
        let p2 = get_or_create_pool("t19_2_a_pool_reuse");
        assert!(Arc::ptr_eq(&p1, &p2));
        p1.shutdown();
    }

    #[test]
    fn t19_2_a_panic_in_task_does_not_crash_worker() {
        let cfg = QueueExecutorConfig::new("t19_2_a_panic")
            .with_max_size(4)
            .with_core_size(0);
        let exec = EnhancedQueueExecutor::new(cfg);
        exec.submit(|| panic!("boom in task")).expect("submit");
        exec.submit(|| {}).expect("submit again");
        exec.drain_locally();
        // Both tasks counted — panic didn't prevent completion accounting.
        assert_eq!(exec.completed_count(), 2);
        exec.shutdown();
    }

    #[test]
    fn t19_2_a_services_native_glue_produces_service_name_mirror() {
        let mut ctx = mock_ctx();
        let arg = ctx.create_string("my-archive.war");
        let res = native_services_deployment_unit_name(&mut ctx, &[Value::Object(Some(arg))])
            .expect("deploymentUnitName should succeed");
        let sn_obj = match res {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected ServiceName object, got {:?}", other),
        };
        // Canonical is stored in slot 1 per jboss_msc.rs constants.
        let canonical = match ctx.get_field(sn_obj, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        assert_eq!(canonical, "jboss.deployment.unit.my-archive.war");
    }

    #[test]
    fn t19_2_a_operation_context_capability_name_falls_back_to_service_name() {
        let mut ctx = mock_ctx();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let cap = ctx.create_string("org.wildfly.transactions.xa-resource-recovery-registry");
        let res = native_operation_context_get_capability_service_name(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(cap)),
                Value::Object(None),
            ],
        )
        .expect("capability fallback should succeed");
        match res {
            Some(Value::Object(Some(_))) => {}
            other => panic!("expected ServiceName object, got {:?}", other),
        }
        let name = capability_service_name(
            "org.wildfly.transactions.xa-resource-recovery-registry",
            &[],
        );
        assert_eq!(
            name.canonical(),
            "org.wildfly.transactions.xa-resource-recovery-registry"
        );
    }

    #[test]
    fn t19_2_a_operation_context_capability_name_appends_dynamic_parts() {
        let mut ctx = mock_ctx();
        let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
        let cap = ctx.create_string("org.wildfly.clustering.infinispan.cache");
        let p0 = ctx.create_string("hibernate");
        let p1 = ctx.create_string("entity");
        let parts = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
        ctx.set_array_element(parts, 0, Value::Object(Some(p0)));
        ctx.set_array_element(parts, 1, Value::Object(Some(p1)));
        let res = native_operation_context_get_capability_service_name_varargs(
            &mut ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(cap)),
                Value::Object(None),
                Value::Object(Some(parts)),
            ],
        )
        .expect("capability varargs fallback should succeed");
        match res {
            Some(Value::Object(Some(_))) => {}
            other => panic!("expected ServiceName object, got {:?}", other),
        }
        let name = capability_service_name(
            "org.wildfly.clustering.infinispan.cache",
            &["hibernate".to_string(), "entity".to_string()],
        );
        assert_eq!(
            name.canonical(),
            "org.wildfly.clustering.infinispan.cache.hibernate.entity"
        );
    }

    #[test]
    fn t19_2_a_level_native_glue_builds_mirror_with_correct_int_value() {
        let mut ctx = mock_ctx();
        let res = native_level_info(&mut ctx, &[]).expect("Level.INFO should succeed");
        let obj = match res {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected Level object, got {:?}", other),
        };
        let name = match ctx.get_field(obj, LVL_FIELD_NAME) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let int_val = match ctx.get_field(obj, LVL_FIELD_VALUE) {
            Value::Int(i) => i,
            _ => 0,
        };
        assert_eq!(name, "INFO");
        assert_eq!(int_val, 800);
    }

    #[test]
    fn t19_2_a_redact_credentials_handles_various_cases() {
        assert_eq!(redact_credentials("Password=xyz"), "Password=<redacted>");
        // Case-insensitive matches still pick the canonical lower/upper name.
        let redacted = redact_credentials("Authorization: Bearer tok123");
        assert!(!redacted.contains("tok123"));
        // No credential substrings => no changes.
        let unchanged = "just a normal message with no secrets";
        assert_eq!(redact_credentials(unchanged), unchanged);
        // Multiple occurrences all stripped.
        let multi = redact_credentials("password=a&token=b&ok=1");
        assert!(!multi.contains("=a") && !multi.contains("=b"));
        // Field named passwordLength is NOT a credential.
        let kept = redact_credentials("passwordLength=12");
        assert_eq!(kept, "passwordLength=12");
    }

    #[test]
    fn t19_2_a_deployment_unit_service_name_is_child_of_base() {
        let du = DeploymentUnit::new("app.ear");
        let sn = du.get_service_name();
        assert_eq!(sn.canonical(), "jboss.deployment.unit.app.ear");
        // Parent chain: app.ear -> unit -> deployment -> jboss.
        let p1 = sn.parent().unwrap();
        assert_eq!(p1.canonical(), "jboss.deployment.unit");
        let p2 = p1.parent().unwrap();
        assert_eq!(p2.canonical(), "jboss.deployment");
    }

    #[test]
    fn t19_2_a_attachment_identity_not_value_equality() {
        let du = DeploymentUnit::new("x.war");
        // Two keys with the same debug name are still distinct.
        let k1 = AttachmentKey::create("same");
        let k2 = AttachmentKey::create("same");
        du.put_attachment(&k1, AttachmentValue::Bool(true));
        // k2 is not equal to k1 (different Arc allocation).
        assert!(matches!(du.get_attachment(&k2), AttachmentValue::None));
        // But the same Arc reused finds the value.
        let k1_again = k1.clone();
        match du.get_attachment(&k1_again) {
            AttachmentValue::Bool(true) => {}
            other => panic!("expected Bool(true), got {:?}", other),
        }
    }
}
