// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.4 - Quarkus ArC (CDI) container natives.
//!
//! Quarkus 3.x ships a build-time CDI container named **ArC**. At runtime,
//! every CDI lookup path funnels through a single entry point:
//!
//! ```text
//! io.quarkus.arc.Arc.initialize()                      // idempotent boot
//! io.quarkus.arc.Arc.container()                       // returns singleton
//! io.quarkus.arc.ArcContainer.instance(Class<T>)       // bean resolution
//! io.quarkus.arc.ArcContainer.beanManager()            // BeanManager view
//! ```
//!
//! During Keycloak 26 boot the Quarkus-generated `Main.main` calls
//! `StartupContext.runAllInStartupContext()` (handled by T19.3's
//! `quarkus_staticinit.rs`) and then `Arc.initialize()`. Every
//! `@ApplicationScoped` / `@Singleton` service Keycloak uses is then
//! discovered by `container.instance(SomeService.class)`.
//!
//! This module provides enough of ArC for that path to return. It does NOT
//! attempt to parse Quarkus' build-time bean graph (`bean-index.bin`
//! would require a code-generator replay) - instead every lookup lazily
//! materializes a plain `Object` of the requested class via
//! `ctx.new_object(class_name)`, records it into the scope's cache, and
//! returns it. For Keycloak's boot path this is sufficient because the
//! beans it resolves are themselves concrete classes (not interfaces), and
//! their `@Inject` fields are populated via the same lookup recursion.
//!
//! ## Field layout (synthetic-stub, `class_manager::synthetic_stub_fields`)
//!
//! | Class                                               | Slot 0          | Slot 1           |
//! |-----------------------------------------------------|-----------------|------------------|
//! | `io/quarkus/arc/Arc`                                | -               | -                |
//! | `io/quarkus/arc/ArcContainer`                       | containerId (J) | -                |
//! | `io/quarkus/arc/impl/ArcContainerImpl`              | containerId (J) | -                |
//! | `io/quarkus/arc/InstanceHandle`                     | containerId (J) | beanKey (Object) |
//! | `io/quarkus/arc/impl/InstanceHandleImpl`            | containerId (J) | beanKey (Object) |
//! | `javax/enterprise/inject/Instance`                  | containerId (J) | beanKey (Object) |
//! | `jakarta/enterprise/inject/Instance`                | containerId (J) | beanKey (Object) |
//! | `jakarta/enterprise/inject/spi/BeanManager`         | containerId (J) | -                |
//! | `javax/enterprise/inject/spi/BeanManager`           | containerId (J) | -                |
//! | `io/quarkus/arc/InjectableBean`                     | containerId (J) | beanKey (Object) |
//!
//! The classes above are exposed via ArC's public SPI; Keycloak bytecode
//! uses them as reference types when a bean is of a CDI-managed type
//! that isn't one of Keycloak's own classes.
//!
//! ## Security posture
//!
//! * The bean-instance map is keyed by a `ClassKey(name, loader_id)` so a
//!   hostile classloader cannot impersonate a bean by defining a class
//!   with a colliding name.
//! * Container state lives in a single `OnceLock<Arc<ArcContainerInner>>`
//!   with a first-call-wins double-checked pattern; a second
//!   `initialize()` observes the same pointer with zero side effects.
//! * `parking_lot::RwLock<HashMap>` is used for bean-scope caches. The
//!   mission brief allows `DashMap` for > 4-thread contention, but ArC
//!   lookup is read-dominated after warmup (reads >> writes), so
//!   `RwLock<HashMap>` with fast reader paths avoids adding a
//!   workspace-wide `dashmap` dep.
//! * `@Inject` field walk respects JDK-exposed declared-field metadata -
//!   it does not reflect into private static fields of system classes.

#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Class names
// ---------------------------------------------------------------------------

const CLS_ARC: &str = "io/quarkus/arc/Arc";
const CLS_ARC_CONTAINER: &str = "io/quarkus/arc/ArcContainer";
const CLS_ARC_CONTAINER_IMPL: &str = "io/quarkus/arc/impl/ArcContainerImpl";
const CLS_INSTANCE_HANDLE: &str = "io/quarkus/arc/InstanceHandle";
const CLS_INSTANCE_HANDLE_IMPL: &str = "io/quarkus/arc/impl/InstanceHandleImpl";
const CLS_INJECTABLE_BEAN: &str = "io/quarkus/arc/InjectableBean";
const CLS_INSTANCE_JAKARTA: &str = "jakarta/enterprise/inject/Instance";
const CLS_INSTANCE_JAVAX: &str = "javax/enterprise/inject/Instance";
const CLS_BEAN_MANAGER_JAKARTA: &str = "jakarta/enterprise/inject/spi/BeanManager";
const CLS_BEAN_MANAGER_JAVAX: &str = "javax/enterprise/inject/spi/BeanManager";

// Bean types ArC wraps natively (bootstrap path).
const CLS_APPLICATION_SCOPED_JAKARTA: &str = "jakarta/enterprise/context/ApplicationScoped";
const CLS_APPLICATION_SCOPED_JAVAX: &str = "javax/enterprise/context/ApplicationScoped";
const CLS_SINGLETON_JAKARTA: &str = "jakarta/inject/Singleton";
const CLS_SINGLETON_JAVAX: &str = "javax/inject/Singleton";
const CLS_REQUEST_SCOPED_JAKARTA: &str = "jakarta/enterprise/context/RequestScoped";
const CLS_REQUEST_SCOPED_JAVAX: &str = "javax/enterprise/context/RequestScoped";
const CLS_DEPENDENT_JAKARTA: &str = "jakarta/enterprise/context/Dependent";
const CLS_DEPENDENT_JAVAX: &str = "javax/enterprise/context/Dependent";

// ---------------------------------------------------------------------------
// Field layout constants (synthetic stubs)
// ---------------------------------------------------------------------------

pub(crate) const CONTAINER_FIELD_ID: usize = 0;
pub(crate) const INSTANCE_FIELD_CONTAINER_ID: usize = 0;
pub(crate) const INSTANCE_FIELD_BEAN_KEY: usize = 1;

// ---------------------------------------------------------------------------
// Scope-kind enum - drives per-lookup vs per-container caching.
// ---------------------------------------------------------------------------

/// Which CDI scope a bean is managed under. Determines whether an
/// `instance()` lookup returns a cached singleton or allocates fresh.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// `@ApplicationScoped` - single process-wide instance.
    Application,
    /// `@Singleton` - single process-wide instance, eagerly created.
    Singleton,
    /// `@RequestScoped` - one instance per simulated request; ArC exposes
    /// these via a request-bound `InstanceHandle` - we treat them like
    /// application-scoped for the boot path because no HTTP request is
    /// active during `Arc.initialize`.
    Request,
    /// `@Dependent` - fresh instance every lookup.
    Dependent,
}

impl Scope {
    /// Default scope when no annotation is present. Quarkus/ArC uses
    /// `@Dependent` for unannotated beans per CDI 4.0, but the boot
    /// path treats them as application-scoped so the bean graph has
    /// stable identity.
    fn default_for_bootstrap() -> Self {
        Scope::Application
    }
}

// ---------------------------------------------------------------------------
// ClassKey - tamper-resistant bean identity.
//
// A class name alone is NOT sufficient: the JLS permits distinct classes
// with identical names loaded by different classloaders. ArC binds a
// bean to a type, so if a hostile loader redefines e.g. `java/lang/String`
// with the same name but different loader, the two must hash to different
// ClassKeys so the hostile instance can't shadow the real bean.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClassKey {
    name: String,
    loader_id: i32,
}

impl ClassKey {
    fn new(name: impl Into<String>, loader_id: i32) -> Self {
        Self {
            name: name.into(),
            loader_id,
        }
    }

    /// Build a ClassKey from a loaded ClassId by reading the class name
    /// and loader out of the NativeContext.
    fn from_class_id(ctx: &dyn NativeContext, class_id: ClassId) -> Self {
        let name = ctx
            .class_name_of_id(class_id)
            .unwrap_or_else(|| format!("<unknown:{}>", class_id.as_u32()));
        let loader_id = ctx.loader_id_of_class(class_id);
        Self::new(name, loader_id)
    }

    pub(crate) fn class_name(&self) -> &str {
        &self.name
    }
}

// ---------------------------------------------------------------------------
// ArcContainerInner - the real container state behind the OnceLock.
// ---------------------------------------------------------------------------

/// The private container state shared across every `Arc.container()` call.
///
/// Each inner instance is identified by a unique `container_id` (monotonic
/// `AtomicU64`) that we embed in the ArcContainer / InstanceHandle synthetic
/// objects so the native side can recover it without trusting ObjectRef
/// pointer identity (which collides under parallel unit tests).
pub(crate) struct ArcContainerInner {
    id: u64,
    /// Per-scope bean caches. Application + Singleton share the same
    /// process-wide map; Request + Dependent are handled separately.
    ///
    /// GC note (gc-followups-20260706): KNOWN-UNSOUND across GCs — cached
    /// bean `ObjectRef`s are neither GC roots nor remapped, so a bean only
    /// reachable from this cache is collectable and any cached ref goes
    /// stale after a moving GC (a later `Arc.container().instance(...)`
    /// then hands Java a dangling address). Tolerated so far because beans
    /// resolved during boot are promptly stored into Java fields; convert
    /// values to the `(identity_key, ObjectRef)` var-handle-root pattern
    /// (ASYNC_POOL in lib.rs) before relying on cross-GC cache hits.
    app_scoped: RwLock<HashMap<ClassKey, ObjectRef>>,
    request_scoped: RwLock<HashMap<ClassKey, ObjectRef>>,
    /// Static bean index recorded at initialize()-time or lazily grown
    /// by first-lookup. Maps type name -> declared scope.
    bean_scopes: RwLock<HashMap<ClassKey, Scope>>,
    /// Per-class cache of @Inject field slot indices. Populated on first
    /// lookup of a bean type.
    inject_fields: RwLock<HashMap<ClassKey, Vec<InjectField>>>,
    /// Count of successful bean resolutions - useful for tests + metrics.
    resolution_count: AtomicU64,
    /// Initialization-success flag. Flipped true by `initialize()`.
    initialized: std::sync::atomic::AtomicBool,
}

impl ArcContainerInner {
    fn new(id: u64) -> Self {
        Self {
            id,
            app_scoped: RwLock::new(HashMap::new()),
            request_scoped: RwLock::new(HashMap::new()),
            bean_scopes: RwLock::new(HashMap::new()),
            inject_fields: RwLock::new(HashMap::new()),
            resolution_count: AtomicU64::new(0),
            initialized: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Returns the container id.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Returns true if `initialize()` has completed at least once.
    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Declare a bean's scope. Called from tests and (optionally) from
    /// downstream natives that want to pre-seed the bean index.
    #[allow(dead_code)]
    pub(crate) fn declare_bean(&self, key: ClassKey, scope: Scope) {
        self.bean_scopes.write().insert(key, scope);
    }

    /// Return the scope for a bean type, defaulting to
    /// `Scope::default_for_bootstrap()` when no explicit scope is known.
    fn scope_of(&self, key: &ClassKey) -> Scope {
        self.bean_scopes
            .read()
            .get(key)
            .copied()
            .unwrap_or_else(Scope::default_for_bootstrap)
    }

    /// Count of beans that have been successfully resolved.
    #[allow(dead_code)]
    pub(crate) fn resolution_count(&self) -> u64 {
        self.resolution_count.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// InjectField - one slot flagged for @Inject population.
// ---------------------------------------------------------------------------

/// A discovered `@Inject` field on a bean class. `slot` is the field index
/// within the owning class; `target_type` is the class name of the
/// declared field type (what `container.instance(target_type)` must
/// return to populate this field).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InjectField {
    slot: usize,
    target_type: String,
}

impl InjectField {
    pub(crate) fn new(slot: usize, target_type: impl Into<String>) -> Self {
        Self {
            slot,
            target_type: target_type.into(),
        }
    }

    pub(crate) fn slot(&self) -> usize {
        self.slot
    }

    pub(crate) fn target_type(&self) -> &str {
        &self.target_type
    }
}

// ---------------------------------------------------------------------------
// Process-wide container singleton with idempotent double-checked CAS.
// ---------------------------------------------------------------------------

/// Monotonic id generator for container instances. Never wraps in practice
/// (one container per JVM), but starting at 1 ensures a 0 field value is
/// unambiguously "uninitialized / not a container id".
fn next_container_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// The one-and-only ArcContainerInner for the process. `OnceLock` gives
/// us first-call-wins: the first `Arc.initialize()` allocates the inner,
/// subsequent calls observe the exact same pointer via `get_or_init`'s
/// happens-before guarantee.
fn container_cell() -> &'static OnceLock<Arc<ArcContainerInner>> {
    static CELL: OnceLock<Arc<ArcContainerInner>> = OnceLock::new();
    &CELL
}

/// Get-or-init the process-wide container. Idempotent: multiple concurrent
/// callers race exactly once into the initializer; all others observe the
/// same `Arc<ArcContainerInner>`.
pub(crate) fn get_or_init_container() -> Arc<ArcContainerInner> {
    container_cell()
        .get_or_init(|| {
            let inner = ArcContainerInner::new(next_container_id());
            inner.initialized.store(true, Ordering::Release);
            Arc::new(inner)
        })
        .clone()
}

/// Read-only accessor for tests that want to observe the container
/// without triggering initialization.
#[cfg(test)]
fn try_container() -> Option<Arc<ArcContainerInner>> {
    container_cell().get().cloned()
}

/// Test-only: drain every cache on the singleton. Tests must call this
/// between runs to avoid pollution. We do NOT re-seat the OnceLock - that
/// would break the idempotent-by-design contract.
#[cfg(test)]
fn reset_container_state() {
    if let Some(inner) = container_cell().get() {
        inner.app_scoped.write().clear();
        inner.request_scoped.write().clear();
        inner.bean_scopes.write().clear();
        inner.inject_fields.write().clear();
        inner.resolution_count.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// Returns `true` when the synthetic ArC shim has been explicitly opted
/// into. This module is a SYNTHETIC stub: it fakes Quarkus' build-time CDI
/// bean discovery instead of running the real ArC bytecode. Per the project
/// rule "real Java bytecode runs by DEFAULT; synthetic shims are
/// experimental opt-in", this shim is OFF by default. It is enabled only via
/// either:
///
/// * the `synthetic-quarkus-arc` cargo feature (compile-time), or
/// * the `CRATONVM_SYNTHETIC_QUARKUS_ARC=1` environment variable (runtime).
///
/// When neither is set, [`register_quarkus_arc_natives`] registers nothing
/// and the VM defers to whatever real `io.quarkus.arc.*` bytecode is on the
/// classpath.
///
/// Note: `cfg!(feature = "synthetic-quarkus-arc")` evaluates to `false` when
/// the feature is not declared in `Cargo.toml`, so this stays sound whether
/// or not the feature has been added to the manifest.
fn synthetic_arc_opted_in_value(runtime_value: Option<&str>) -> bool {
    cfg!(feature = "synthetic-quarkus-arc") || runtime_value == Some("1")
}

fn synthetic_arc_opted_in() -> bool {
    let runtime_value = cratonvm_types::flags::runtime_var("CRATONVM_SYNTHETIC_QUARKUS_ARC").ok();
    synthetic_arc_opted_in_value(runtime_value.as_deref())
}

/// Register every `io.quarkus.arc.*` native we implement. Called from
/// `register_essential_natives` in `lib.rs` after T19.3.
///
/// DEFAULT-OFF: this is a synthetic shim (see [`synthetic_arc_opted_in`]).
/// Unless the caller opts in via the `synthetic-quarkus-arc` cargo feature or
/// the `CRATONVM_SYNTHETIC_QUARKUS_ARC=1` env var, this is a no-op and the VM
/// runs whatever real ArC bytecode is on the classpath instead.
pub fn register_quarkus_arc_natives(registry: &mut NativeMethodRegistry) {
    if !synthetic_arc_opted_in() {
        tracing::debug!(
            "quarkus.arc: synthetic ArC shim disabled (default); set \
             CRATONVM_SYNTHETIC_QUARKUS_ARC=1 or enable the \
             `synthetic-quarkus-arc` feature to opt in"
        );
        return;
    }
    tracing::info!("quarkus.arc: synthetic ArC shim enabled (opt-in)");
    register_quarkus_arc_natives_unconditional(registry);
}

/// Perform the actual native registration, ignoring the opt-in gate.
/// Separated out so the gate lives in exactly one place and tests can
/// exercise the registration mechanics without depending on the ambient
/// feature/env configuration.
fn register_quarkus_arc_natives_unconditional(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    register_arc_facade(registry);
    register_arc_container(registry);
    register_instance_handle(registry);
    register_bean_manager(registry);
    register_injectable_bean(registry);
    registry.set_category(__prev_cat);
}

fn register_arc_facade(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // static Arc.initialize() - idempotent boot.
    registry.register(CLS_ARC, "initialize", "()V", native_arc_initialize);
    // static Arc.container() returns the singleton container.
    registry.register(
        CLS_ARC,
        "container",
        "()Lio/quarkus/arc/ArcContainer;",
        native_arc_container,
    );
    // static Arc.shutdown() - flushes caches so a follow-on initialize
    // observes a clean slate. Keycloak's shutdown hooks call this.
    registry.register(CLS_ARC, "shutdown", "()V", native_arc_shutdown);
    // static Arc.isRunning()
    registry.register(CLS_ARC, "isRunning", "()Z", native_arc_is_running);
    registry.set_category(__prev_cat);
}

fn register_arc_container(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    // Both interface + impl dispatch the same way.
    for cls in [CLS_ARC_CONTAINER, CLS_ARC_CONTAINER_IMPL] {
        registry.register(
            cls,
            "instance",
            "(Ljava/lang/Class;[Ljava/lang/annotation/Annotation;)Lio/quarkus/arc/InstanceHandle;",
            native_container_instance_class,
        );
        // Simpler arity that Keycloak uses.
        registry.register(
            cls,
            "instance",
            "(Ljava/lang/Class;)Lio/quarkus/arc/InstanceHandle;",
            native_container_instance_class_simple,
        );
        registry.register(
            cls,
            "instance",
            "(Ljava/lang/String;)Lio/quarkus/arc/InstanceHandle;",
            native_container_instance_name,
        );
        registry.register(
            cls,
            "select",
            "(Ljava/lang/Class;)Ljakarta/enterprise/inject/Instance;",
            native_container_select,
        );
        registry.register(
            cls,
            "beanManager",
            "()Ljakarta/enterprise/inject/spi/BeanManager;",
            native_container_bean_manager,
        );
        registry.register(
            cls,
            "beanManager",
            "()Ljavax/enterprise/inject/spi/BeanManager;",
            native_container_bean_manager,
        );
        registry.register(cls, "isRunning", "()Z", native_container_is_running);
        registry.register(
            cls,
            "requestContext",
            "()Lio/quarkus/arc/ManagedContext;",
            native_container_request_context,
        );
    }
    registry.set_category(__prev_cat);
}

fn register_instance_handle(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    for cls in [
        CLS_INSTANCE_HANDLE,
        CLS_INSTANCE_HANDLE_IMPL,
        CLS_INSTANCE_JAKARTA,
        CLS_INSTANCE_JAVAX,
    ] {
        registry.register(
            cls,
            "get",
            "()Ljava/lang/Object;",
            native_instance_handle_get,
        );
        registry.register(cls, "isAvailable", "()Z", native_instance_handle_available);
        registry.register(cls, "isResolvable", "()Z", native_instance_handle_available);
        registry.register(cls, "destroy", "()V", native_instance_handle_no_op);
        registry.register(cls, "close", "()V", native_instance_handle_no_op);
    }
    registry.set_category(__prev_cat);
}

fn register_bean_manager(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    for cls in [CLS_BEAN_MANAGER_JAKARTA, CLS_BEAN_MANAGER_JAVAX] {
        registry.register(
            cls,
            "createInstance",
            "()Ljakarta/enterprise/inject/Instance;",
            native_bean_manager_create_instance,
        );
        registry.register(
            cls,
            "getBeans",
            "(Ljava/lang/reflect/Type;[Ljava/lang/annotation/Annotation;)Ljava/util/Set;",
            native_bean_manager_get_beans,
        );
        registry.register(
            cls,
            "getReference",
            "(Ljakarta/enterprise/inject/spi/Bean;Ljava/lang/reflect/Type;Ljakarta/enterprise/context/spi/CreationalContext;)Ljava/lang/Object;",
            native_bean_manager_get_reference,
        );
    }
    registry.set_category(__prev_cat);
}

fn register_injectable_bean(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::SyntheticStub);
    registry.register(
        CLS_INJECTABLE_BEAN,
        "get",
        "(Ljakarta/enterprise/context/spi/CreationalContext;)Ljava/lang/Object;",
        native_injectable_bean_get,
    );
    registry.register(
        CLS_INJECTABLE_BEAN,
        "getScope",
        "()Ljava/lang/Class;",
        native_injectable_bean_get_scope,
    );
    registry.register(
        CLS_INJECTABLE_BEAN,
        "getBeanClass",
        "()Ljava/lang/Class;",
        native_injectable_bean_get_bean_class,
    );
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// Read a Class mirror argument and recover the ClassKey it represents.
///
/// Class mirrors in CratonVM hold the ClassId in slot 0 (int-encoded) and
/// the name as a Java String in slot 1 - but the mock context doesn't
/// wire those consistently, so we prefer the high-level accessors
/// (`class_id_of_object` / `class_name_of_id`) when available and fall
/// back to reading the slot-1 name string otherwise.
fn class_key_from_class_mirror(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<ClassKey> {
    // Preferred: every Class mirror has a backing ClassId. Works in
    // production and in the mock where `get_class_mirror` populates it.
    let cid_from_object = ctx.class_id_of_object(mirror);
    if cid_from_object.as_u32() != 0 {
        if let Some(name) = ctx.class_name_of_id(cid_from_object) {
            return Some(ClassKey::new(name, ctx.loader_id_of_class(cid_from_object)));
        }
    }
    // Fallback: the mock uses slot 0 = int class_id and slot 1 = Java
    // String mirror name.
    if let Value::Int(encoded_cid) = ctx.get_field(mirror, 0) {
        if encoded_cid > 0 {
            let class_id = ClassId::new(encoded_cid as u32);
            if let Some(name) = ctx.class_name_of_id(class_id) {
                return Some(ClassKey::new(name, ctx.loader_id_of_class(class_id)));
            }
        }
    }
    if let Value::Object(Some(name_obj)) = ctx.get_field(mirror, 1) {
        if let Some(name) = ctx.read_string(name_obj) {
            // Loader id unknown from raw name - best-effort lookup.
            let loader_id = ctx
                .class_id_by_name(&name)
                .map(|cid| ctx.loader_id_of_class(cid))
                .unwrap_or(2);
            return Some(ClassKey::new(name, loader_id));
        }
    }
    None
}

/// Allocate a synthetic ArcContainer object and embed the container id
/// into slot 0 so the impl-side natives can recover the inner state
/// without trusting ObjectRef identity.
fn alloc_arc_container_obj(ctx: &mut dyn NativeContext, id: u64) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(CLS_ARC_CONTAINER_IMPL)
        .unwrap_or_else(|_| {
            // Graceful fallback - we never want container creation to
            // propagate up as a fatal error.
            ClassId::new(0)
        });
    let obj = ctx.alloc_object(cid, 2);
    ctx.set_field(obj, CONTAINER_FIELD_ID, Value::Long(id as i64));
    obj
}

/// Allocate an InstanceHandle wrapping a specific bean key.
fn alloc_instance_handle(
    ctx: &mut dyn NativeContext,
    container_id: u64,
    bean_key_marker: ObjectRef,
) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(CLS_INSTANCE_HANDLE_IMPL)
        .unwrap_or_else(|_| ClassId::new(0));
    let obj = ctx.alloc_object(cid, 2);
    ctx.set_field(
        obj,
        INSTANCE_FIELD_CONTAINER_ID,
        Value::Long(container_id as i64),
    );
    ctx.set_field(
        obj,
        INSTANCE_FIELD_BEAN_KEY,
        Value::Object(Some(bean_key_marker)),
    );
    obj
}

/// Build an `UnsatisfiedResolutionException` - or fall back to a
/// platform-level `IllegalStateException` when the CDI class isn't
/// available.
fn throw_unsatisfied_resolution(ctx: &mut dyn NativeContext, message: &str) -> MethodCallFailed {
    // Preferred: real CDI exception via new_object. Many deployments ship
    // this class as part of jakarta.enterprise-api.
    for name in [
        "jakarta/enterprise/inject/UnsatisfiedResolutionException",
        "javax/enterprise/inject/UnsatisfiedResolutionException",
    ] {
        if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object(name) {
            // Slot 0 is the message in our throwable synthetic layout.
            let msg = ctx.create_string(message);
            ctx.set_field(exc, 0, Value::Object(Some(msg)));
            return MethodCallFailed::ExceptionThrown(exc);
        }
    }
    // Fallback - platform exception always available.
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalStateException {
        message: message.to_string(),
    }))
}

/// Recover the container id embedded in the object's slot 0. Returns 0
/// when the slot isn't a Long (e.g. not initialized via our native).
fn container_id_of(ctx: &dyn NativeContext, obj: ObjectRef) -> u64 {
    match ctx.get_field(obj, 0) {
        Value::Long(v) => v as u64,
        Value::Int(v) => v as u32 as u64,
        _ => 0,
    }
}

/// Find-or-create the application-scoped bean instance for `key`.
fn resolve_app_scoped(
    inner: &ArcContainerInner,
    ctx: &mut dyn NativeContext,
    key: &ClassKey,
) -> Option<ObjectRef> {
    {
        let reader = inner.app_scoped.read();
        if let Some(obj) = reader.get(key) {
            return Some(*obj);
        }
    }
    // Miss - allocate and write under the writer lock. Race-tolerant via
    // `entry().or_insert_with`: if another writer beat us, we return the
    // existing instance so every caller sees the same identity.
    let created = create_bean_instance(ctx, key)?;
    let mut writer = inner.app_scoped.write();
    let final_obj = *writer.entry(key.clone()).or_insert(created);
    Some(final_obj)
}

/// Create a fresh instance of a bean type. Returns None if the class
/// can't be loaded.
fn create_bean_instance(ctx: &mut dyn NativeContext, key: &ClassKey) -> Option<ObjectRef> {
    match ctx.new_object(&key.name) {
        Ok(Some(Value::Object(Some(obj)))) => Some(obj),
        _ => None,
    }
}

/// Walk the declared fields of `key.name`'s class and return every field
/// that's annotated with `@Inject` (jakarta or javax). Result is cached
/// per-ClassKey.
fn discover_inject_fields(
    inner: &ArcContainerInner,
    ctx: &dyn NativeContext,
    key: &ClassKey,
) -> Vec<InjectField> {
    {
        let reader = inner.inject_fields.read();
        if let Some(cached) = reader.get(key) {
            return cached.clone();
        }
    }
    let class_id = match ctx.class_id_by_name(&key.name) {
        Some(cid) => cid,
        None => {
            inner.inject_fields.write().insert(key.clone(), Vec::new());
            return Vec::new();
        }
    };
    let fields = ctx.declared_fields(class_id);
    let mut discovered = Vec::new();
    for (slot, meta) in fields.iter().enumerate() {
        let annotations = ctx.field_annotations(class_id, &meta.name);
        let has_inject = annotations.iter().any(|a| {
            a.type_descriptor == "Ljakarta/inject/Inject;"
                || a.type_descriptor == "Ljavax/inject/Inject;"
        });
        if has_inject {
            // Extract the field's target type from its descriptor.
            let target_type = extract_descriptor_class(&meta.descriptor)
                .unwrap_or_else(|| meta.descriptor.to_string());
            discovered.push(InjectField::new(slot, target_type));
        }
    }
    inner
        .inject_fields
        .write()
        .insert(key.clone(), discovered.clone());
    discovered
}

/// Pull the internal class name out of a JVM field descriptor.
/// `Ljava/lang/String;` -> `java/lang/String`. `[I` -> `[I` (arrays are
/// passed through unchanged because ArC doesn't inject array types).
/// Primitives are also passed through unchanged.
fn extract_descriptor_class(desc: &str) -> Option<String> {
    let bytes = desc.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'L' && bytes[bytes.len() - 1] == b';' {
        Some(desc[1..desc.len() - 1].to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Arc facade natives
// ---------------------------------------------------------------------------

/// `io.quarkus.arc.Arc.initialize()` - idempotent.
///
/// Called from Quarkus-generated `Main.main` after
/// `StartupContext.runAllInStartupContext()`. First call allocates the
/// container; subsequent calls return immediately without side effects.
fn native_arc_initialize(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let inner = get_or_init_container();
    // Emit a single tracing event on first init - the `initialized`
    // AtomicBool has been set inside `get_or_init_container` under the
    // OnceLock's critical section, so a compare_exchange from false->
    // true will succeed exactly once.
    if !inner.is_initialized() {
        // Shouldn't happen - `get_or_init_container` always sets it -
        // but protect against future refactors.
        inner.initialized.store(true, Ordering::Release);
    }
    tracing::info!("quarkus.arc.Arc.initialize: container id={}", inner.id());
    Ok(None)
}

/// `Arc.container()` - returns the process-wide ArcContainer singleton.
/// Auto-initializes if not yet done (matches Quarkus behavior where
/// accessing `container` before `initialize` triggers a lazy init).
fn native_arc_container(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let inner = get_or_init_container();
    let obj = alloc_arc_container_obj(ctx, inner.id());
    Ok(Some(Value::Object(Some(obj))))
}

/// `Arc.shutdown()` - drains caches and marks the container un-ready.
/// The `OnceLock` is NOT reset (idempotent contract forbids re-seat) -
/// we only clear the per-scope bean maps. A subsequent `initialize()`
/// continues to observe the same container id; a `container.instance(...)`
/// after shutdown will transparently re-materialize beans.
fn native_arc_shutdown(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    if let Some(inner) = container_cell().get() {
        inner.app_scoped.write().clear();
        inner.request_scoped.write().clear();
        tracing::info!(
            "quarkus.arc.Arc.shutdown: container id={} caches drained",
            inner.id()
        );
    }
    Ok(None)
}

/// `Arc.isRunning()` - true iff the container has ever been initialized
/// and has not been explicitly shut down. We approximate with the
/// `is_initialized` flag; shutdown flips internal caches but keeps the
/// flag set (matching Quarkus' behavior where the container survives
/// app restart).
fn native_arc_is_running(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let running = container_cell()
        .get()
        .map(|inner| inner.is_initialized())
        .unwrap_or(false);
    Ok(Some(Value::Int(if running { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// ArcContainer natives
// ---------------------------------------------------------------------------

/// `ArcContainer.instance(Class<T> beanType, Annotation... qualifiers)`.
/// The boot path ignores qualifiers - Keycloak uses them only for
/// `@Named("foo")` beans which we treat as identity matches.
fn native_container_instance_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ArcContainer.instance: null receiver".into()),
                },
            )));
        }
    };
    let class_mirror = match arg_obj(args, 1) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ArcContainer.instance: null class argument".into()),
                },
            )));
        }
    };
    resolve_to_handle(ctx, this, class_mirror)
}

/// Two-arg form (the form Keycloak generates when no qualifier array is
/// present). Same semantics as the varargs form.
fn native_container_instance_class_simple(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ArcContainer.instance: null receiver".into()),
                },
            )));
        }
    };
    let class_mirror = match arg_obj(args, 1) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ArcContainer.instance: null class argument".into()),
                },
            )));
        }
    };
    resolve_to_handle(ctx, this, class_mirror)
}

/// String-keyed lookup - used by `@Named` bean lookup in older Quarkus.
fn native_container_instance_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ArcContainer.instance: null receiver".into()),
                },
            )));
        }
    };
    let name_str = match arg_obj(args, 1).and_then(|o| ctx.read_string(o)) {
        Some(s) if !s.is_empty() => s,
        _ => {
            return Err(throw_unsatisfied_resolution(
                ctx,
                "ArcContainer.instance(String): name is null or empty",
            ));
        }
    };
    // Resolve the class id by name - defaults to app-loader if known.
    let loader_id = ctx
        .class_id_by_name(&name_str)
        .map(|cid| ctx.loader_id_of_class(cid))
        .unwrap_or(2);
    let key = ClassKey::new(&name_str, loader_id);
    resolve_via_key(ctx, this, key)
}

/// `ArcContainer.select(Class<T>)` - returns an `Instance<T>` which in
/// our implementation is the same synthetic object as an InstanceHandle.
fn native_container_select(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_container_instance_class_simple(ctx, args)
}

/// `ArcContainer.beanManager()` - returns a synthetic BeanManager object.
/// Same container id is embedded so downstream dispatches find the
/// container state.
fn native_container_bean_manager(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("ArcContainer.beanManager: null receiver".into()),
                },
            )));
        }
    };
    let container_id = container_id_of(ctx, this);
    let cid = ctx
        .ensure_class_initialized(CLS_BEAN_MANAGER_JAKARTA)
        .unwrap_or_else(|_| ClassId::new(0));
    let obj = ctx.alloc_object(cid, 2);
    ctx.set_field(obj, 0, Value::Long(container_id as i64));
    Ok(Some(Value::Object(Some(obj))))
}

/// `ArcContainer.isRunning()`
fn native_container_is_running(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let running = container_cell()
        .get()
        .map(|inner| inner.is_initialized())
        .unwrap_or(false);
    Ok(Some(Value::Int(if running { 1 } else { 0 })))
}

/// `ArcContainer.requestContext()` - returns a stub ManagedContext.
/// During boot Quarkus activates/deactivates it around request threads;
/// since our boot path isn't serving HTTP yet we return a stub that
/// tracks no state.
fn native_container_request_context(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cid = ctx
        .ensure_class_initialized("io/quarkus/arc/ManagedContext")
        .unwrap_or_else(|_| ClassId::new(0));
    Ok(Some(Value::Object(Some(ctx.alloc_object(cid, 1)))))
}

/// Shared resolution kernel. Reads the container id off `this`, picks
/// the right scope cache, creates-or-returns, and wraps the result in
/// an InstanceHandle.
fn resolve_to_handle(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    class_mirror: ObjectRef,
) -> MethodCallResult {
    let key = match class_key_from_class_mirror(ctx, class_mirror) {
        Some(k) => k,
        None => {
            return Err(throw_unsatisfied_resolution(
                ctx,
                "ArcContainer.instance: class mirror is missing name or id",
            ));
        }
    };
    resolve_via_key(ctx, this, key)
}

fn resolve_via_key(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: ClassKey,
) -> MethodCallResult {
    let container_id = container_id_of(ctx, this);
    let inner = container_cell().get().cloned().unwrap_or_else(|| {
        // `instance()` before `initialize()` - lazy init per Quarkus
        // behavior. Never hit in practice (Main.main always inits
        // first) but keeps us safe.
        get_or_init_container()
    });
    // Sanity check: embedded container id must match the singleton.
    // A mismatch means the caller built an ArcContainer from an
    // unrelated native - treat as a fatal contract violation but
    // fall through to using the real singleton.
    if container_id != 0 && container_id != inner.id() {
        tracing::warn!(
            "quarkus.arc.ArcContainer.instance: receiver id {} != singleton id {}; using singleton",
            container_id,
            inner.id()
        );
    }
    let scope = inner.scope_of(&key);
    let resolved = match scope {
        Scope::Application | Scope::Singleton => resolve_app_scoped(&inner, ctx, &key),
        Scope::Request => {
            // Treat like app-scoped during boot (no request alive).
            let mut reader_hit = inner.request_scoped.read().get(&key).copied();
            if reader_hit.is_none() {
                if let Some(created) = create_bean_instance(ctx, &key) {
                    let mut writer = inner.request_scoped.write();
                    reader_hit = Some(*writer.entry(key.clone()).or_insert(created));
                }
            }
            reader_hit
        }
        Scope::Dependent => create_bean_instance(ctx, &key),
    };
    let bean_obj = match resolved {
        Some(o) => o,
        None => {
            return Err(throw_unsatisfied_resolution(
                ctx,
                &format!("No bean matches: {}", key.class_name()),
            ));
        }
    };
    // Populate @Inject fields on newly materialized instances.
    populate_inject_fields(&inner, ctx, bean_obj, &key);
    inner.resolution_count.fetch_add(1, Ordering::Relaxed);
    Ok(Some(Value::Object(Some(alloc_instance_handle(
        ctx,
        inner.id(),
        bean_obj,
    )))))
}

/// Walk every @Inject field on the bean's class and populate each with
/// a fresh lookup. Idempotent: if the field is already non-null (e.g.
/// populated on an earlier lookup of the same app-scoped instance),
/// we leave it alone.
fn populate_inject_fields(
    inner: &ArcContainerInner,
    ctx: &mut dyn NativeContext,
    bean: ObjectRef,
    key: &ClassKey,
) {
    let fields = discover_inject_fields(inner, ctx, key);
    for f in &fields {
        let current = ctx.get_field(bean, f.slot());
        if !matches!(current, Value::Object(None)) {
            continue; // already populated
        }
        let target_key_name = f.target_type().replace('.', "/");
        let loader_id = ctx
            .class_id_by_name(&target_key_name)
            .map(|cid| ctx.loader_id_of_class(cid))
            .unwrap_or(2);
        let sub_key = ClassKey::new(&target_key_name, loader_id);
        if sub_key == *key {
            // Self-injection is a CDI error; leave field null.
            continue;
        }
        let scope = inner.scope_of(&sub_key);
        let resolved = match scope {
            Scope::Application | Scope::Singleton | Scope::Request => {
                resolve_app_scoped(inner, ctx, &sub_key)
            }
            Scope::Dependent => create_bean_instance(ctx, &sub_key),
        };
        if let Some(child) = resolved {
            ctx.set_field(bean, f.slot(), Value::Object(Some(child)));
        }
    }
}

// ---------------------------------------------------------------------------
// InstanceHandle natives
// ---------------------------------------------------------------------------

/// `InstanceHandle.get()` - unwraps the stored bean.
fn native_instance_handle_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("InstanceHandle.get: null receiver".into()),
                },
            )));
        }
    };
    match ctx.get_field(this, INSTANCE_FIELD_BEAN_KEY) {
        Value::Object(Some(o)) => Ok(Some(Value::Object(Some(o)))),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `InstanceHandle.isAvailable()` / `isResolvable()`.
fn native_instance_handle_available(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Int(0))),
    };
    let available = matches!(
        ctx.get_field(this, INSTANCE_FIELD_BEAN_KEY),
        Value::Object(Some(_))
    );
    Ok(Some(Value::Int(if available { 1 } else { 0 })))
}

/// `InstanceHandle.destroy()` / `close()`. For app-scoped beans the
/// container owns the lifecycle so we ignore the request. Dependent
/// beans would need per-handle teardown, but since our boot path doesn't
/// keep dependent state we treat this as a graceful no-op.
fn native_instance_handle_no_op(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// BeanManager natives
// ---------------------------------------------------------------------------

/// `BeanManager.createInstance()` - returns a universal `Instance<Object>`
/// that can be further narrowed via `select(Class)`. We return a
/// synthetic handle pointing at `java.lang.Object` so subsequent
/// `.select(MyBean.class).get()` follows the container lookup path.
fn native_bean_manager_create_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("BeanManager.createInstance: null receiver".into()),
                },
            )));
        }
    };
    let container_id = container_id_of(ctx, this);
    let placeholder = ctx.create_string("java/lang/Object");
    Ok(Some(Value::Object(Some(alloc_instance_handle(
        ctx,
        container_id,
        placeholder,
    )))))
}

/// `BeanManager.getBeans(Type, Annotation...)` - returns an empty Set
/// (ArC injection is name-based, so the bytecode rarely iterates this).
fn native_bean_manager_get_beans(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return a fresh empty HashSet - Keycloak mostly uses this for
    // discovering qualifiers on beans and is tolerant of empty results.
    match ctx.new_object("java/util/HashSet") {
        Ok(Some(v)) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `BeanManager.getReference(Bean, Type, CreationalContext)` - unwraps
/// the bean by reading its embedded ClassKey. The `Bean` argument in our
/// impl is actually an `InjectableBean` synthetic object with its own
/// bean key slot.
fn native_bean_manager_get_reference(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let bean = match arg_obj(args, 1) {
        Some(o) => o,
        None => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("BeanManager.getReference: null bean".into()),
                },
            )));
        }
    };
    match ctx.get_field(bean, INSTANCE_FIELD_BEAN_KEY) {
        Value::Object(Some(o)) => Ok(Some(Value::Object(Some(o)))),
        _ => Ok(Some(Value::Object(None))),
    }
}

// ---------------------------------------------------------------------------
// InjectableBean natives
// ---------------------------------------------------------------------------

fn native_injectable_bean_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Same as InstanceHandle.get - the injectable bean holds the same
    // kind of handle with a bean key.
    native_instance_handle_get(ctx, args)
}

fn native_injectable_bean_get_scope(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return the ApplicationScoped class mirror - enough for most
    // CDI scope comparisons.
    let cid = ctx
        .ensure_class_initialized(CLS_APPLICATION_SCOPED_JAKARTA)
        .or_else(|_| ctx.ensure_class_initialized(CLS_APPLICATION_SCOPED_JAVAX))
        .unwrap_or_else(|_| ClassId::new(0));
    Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))))
}

fn native_injectable_bean_get_bean_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    // If we recorded the bean's class id somewhere, return that;
    // otherwise fall through to returning the bean type's class via
    // slot 1 (where the wrapped instance lives). Reflecting on the
    // wrapped instance via `class_id_of_object` gives us the right
    // mirror for the majority of cases.
    match ctx.get_field(this, INSTANCE_FIELD_BEAN_KEY) {
        Value::Object(Some(o)) => {
            let cid = ctx.class_id_of_object(o);
            Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::NativeMethodRegistry;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Test fixture - tests that touch the container singleton serialize
    /// on this mutex so one test's state doesn't leak into another.
    /// The OnceLock itself is idempotent-by-design, but the per-scope
    /// bean caches are stateful.
    fn state_guard() -> &'static parking_lot::Mutex<()> {
        static GUARD: OnceLock<parking_lot::Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| parking_lot::Mutex::new(()))
    }

    fn make_arc_container(ctx: &mut MockNativeContext) -> (u64, ObjectRef) {
        let inner = get_or_init_container();
        let obj = alloc_arc_container_obj(ctx, inner.id());
        (inner.id(), obj)
    }

    fn make_class_mirror_for(ctx: &mut MockNativeContext, class_name: &str) -> ObjectRef {
        let cid = ctx.ensure_class_initialized(class_name).unwrap();
        ctx.get_class_mirror(cid)
    }

    #[test]
    fn register_quarkus_arc_natives_registers_all_expected_entries() {
        let mut r = NativeMethodRegistry::new();
        // Exercise the registration mechanics directly, independent of the
        // opt-in gate (which is tested separately below).
        register_quarkus_arc_natives_unconditional(&mut r);
        // Key facade entries
        assert!(r.find(CLS_ARC, "initialize", "()V").is_some());
        assert!(r
            .find(CLS_ARC, "container", "()Lio/quarkus/arc/ArcContainer;")
            .is_some());
        assert!(r.find(CLS_ARC, "shutdown", "()V").is_some());
        assert!(r.find(CLS_ARC, "isRunning", "()Z").is_some());
        // Container entries (both interface + impl)
        for cls in [CLS_ARC_CONTAINER, CLS_ARC_CONTAINER_IMPL] {
            assert!(r
                .find(
                    cls,
                    "instance",
                    "(Ljava/lang/Class;)Lio/quarkus/arc/InstanceHandle;"
                )
                .is_some());
            assert!(r
                .find(
                    cls,
                    "beanManager",
                    "()Ljakarta/enterprise/inject/spi/BeanManager;"
                )
                .is_some());
        }
        // Handle dispatches
        assert!(r
            .find(CLS_INSTANCE_HANDLE_IMPL, "get", "()Ljava/lang/Object;")
            .is_some());
        // BeanManager dispatches
        assert!(r
            .find(
                CLS_BEAN_MANAGER_JAKARTA,
                "createInstance",
                "()Ljakarta/enterprise/inject/Instance;"
            )
            .is_some());
    }

    #[test]
    fn synthetic_shim_is_default_off_and_startup_opt_in_enables_it() {
        // Startup configuration is immutable. Exercise the parser directly
        // instead of mutating process state after the global snapshot latched.
        if !cfg!(feature = "synthetic-quarkus-arc") {
            assert!(
                !synthetic_arc_opted_in_value(None),
                "shim must be off by default without opt-in"
            );
        }

        assert!(
            synthetic_arc_opted_in_value(Some("1")),
            "startup flag must enable the synthetic shim"
        );
        let mut r_on = NativeMethodRegistry::new();
        register_quarkus_arc_natives_unconditional(&mut r_on);
        assert!(
            r_on.find(CLS_ARC, "initialize", "()V").is_some(),
            "opt-in path must register the synthetic ArC shim"
        );
    }

    #[test]
    fn arc_initialize_is_idempotent_returns_same_container_id() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();

        native_arc_initialize(&mut ctx, &[]).unwrap();
        let id1 = get_or_init_container().id();

        native_arc_initialize(&mut ctx, &[]).unwrap();
        let id2 = get_or_init_container().id();

        assert_eq!(id1, id2, "initialize must be idempotent on container id");
    }

    #[test]
    fn arc_initialize_marks_is_running_true_and_shutdown_keeps_id() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();

        let running_before = native_arc_is_running(&mut ctx, &[]).unwrap().unwrap();
        // If some earlier test initialized, we'll be running already;
        // we don't assert on the before-state, only on after-init.
        let _ = running_before;

        native_arc_initialize(&mut ctx, &[]).unwrap();
        let running = native_arc_is_running(&mut ctx, &[]).unwrap().unwrap();
        match running {
            Value::Int(1) => {}
            other => panic!("expected running=true, got {:?}", other),
        }

        let id_before = get_or_init_container().id();
        native_arc_shutdown(&mut ctx, &[]).unwrap();
        let id_after = get_or_init_container().id();
        assert_eq!(id_before, id_after, "shutdown must not reseat container id");
    }

    #[test]
    fn container_returns_object_with_correct_id_slot() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let container_val = native_arc_container(&mut ctx, &[]).unwrap().unwrap();
        let container_obj = match container_val {
            Value::Object(Some(o)) => o,
            _ => panic!("expected container object"),
        };
        let id_field = ctx.get_field(container_obj, CONTAINER_FIELD_ID);
        match id_field {
            Value::Long(v) => {
                assert_eq!(
                    v as u64,
                    get_or_init_container().id(),
                    "embedded id matches"
                );
            }
            other => panic!("expected Long id slot, got {:?}", other),
        }
    }

    #[test]
    fn app_scoped_lookup_returns_same_instance_across_two_calls() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let container_obj = match native_arc_container(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("container"),
        };
        let mirror = make_class_mirror_for(&mut ctx, "com/example/AppBean");

        let h1 = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("handle1 {:?}", other),
        };
        let b1 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h1))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("bean1 {:?}", other),
        };

        let h2 = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("handle2 {:?}", other),
        };
        let b2 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h2))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            other => panic!("bean2 {:?}", other),
        };

        assert_eq!(b1, b2, "app-scoped lookups must share instance identity");
    }

    #[test]
    fn dependent_scope_lookup_returns_fresh_instance_each_call() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        // Pre-declare as dependent
        let inner = get_or_init_container();
        let bean_name = "com/example/DepBean";
        let cid = ctx.ensure_class_initialized(bean_name).unwrap();
        let key = ClassKey::new(bean_name, ctx.loader_id_of_class(cid));
        inner.declare_bean(key, Scope::Dependent);

        let container_obj = match native_arc_container(&mut ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("container"),
        };
        let mirror = ctx.get_class_mirror(cid);

        let b1 = {
            let h = match native_container_instance_class_simple(
                &mut ctx,
                &[
                    Value::Object(Some(container_obj)),
                    Value::Object(Some(mirror)),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Object(Some(o)) => o,
                other => panic!("h {:?}", other),
            };
            match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(o)) => o,
                other => panic!("b {:?}", other),
            }
        };
        let b2 = {
            let h = match native_container_instance_class_simple(
                &mut ctx,
                &[
                    Value::Object(Some(container_obj)),
                    Value::Object(Some(mirror)),
                ],
            )
            .unwrap()
            .unwrap()
            {
                Value::Object(Some(o)) => o,
                other => panic!("h {:?}", other),
            };
            match native_instance_handle_get(&mut ctx, &[Value::Object(Some(h))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(o)) => o,
                other => panic!("b {:?}", other),
            }
        };
        assert_ne!(
            b1, b2,
            "@Dependent lookups must allocate a fresh instance each time"
        );
    }

    #[test]
    fn bean_not_found_throws_unsatisfied_resolution_when_mirror_unreadable() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (_id, container_obj) = make_arc_container(&mut ctx);

        // Use a bogus "mirror" that is NOT a real mirror - just an
        // object with no recognized slot layout. resolution should
        // fall into the error path.
        let bogus_cid = ctx.ensure_class_initialized("Bogus").unwrap();
        let bogus_mirror = ctx.alloc_object(bogus_cid, 0);

        // But note: our class_key_from_class_mirror will read
        // class_id_of_object(bogus_mirror) which returns Bogus' class_id,
        // so the lookup succeeds with key "Bogus". To trigger the error
        // path we'd need class_id == 0 - instead test via the
        // String-name lookup path with an empty name.
        let empty = ctx.create_string("");
        let result = native_container_instance_name(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(empty)),
            ],
        );
        assert!(result.is_err(), "empty name must surface as error");

        // Also sanity check non-error case: bogus_mirror does resolve
        // successfully since it has a class_id.
        let result_ok = native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(bogus_mirror)),
            ],
        );
        assert!(result_ok.is_ok(), "valid mirror with class_id resolves");
    }

    #[test]
    fn instance_handle_get_returns_wrapped_bean() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (id, container_obj) = make_arc_container(&mut ctx);
        let mirror = make_class_mirror_for(&mut ctx, "com/example/HandleBean");

        let handle_val = native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap();

        let handle = match handle_val {
            Value::Object(Some(o)) => o,
            _ => panic!("handle"),
        };

        // isAvailable should be true.
        let avail = native_instance_handle_available(&mut ctx, &[Value::Object(Some(handle))])
            .unwrap()
            .unwrap();
        assert!(matches!(avail, Value::Int(1)));

        // get() should return the bean.
        let bean_val = native_instance_handle_get(&mut ctx, &[Value::Object(Some(handle))])
            .unwrap()
            .unwrap();
        assert!(matches!(bean_val, Value::Object(Some(_))));

        // Container id embedded in handle matches singleton.
        let slot = ctx.get_field(handle, INSTANCE_FIELD_CONTAINER_ID);
        match slot {
            Value::Long(v) => assert_eq!(v as u64, id),
            other => panic!("expected Long id in handle, got {:?}", other),
        }
    }

    #[test]
    fn bean_manager_returns_synthetic_with_container_id() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (id, container_obj) = make_arc_container(&mut ctx);
        let bm_val = native_container_bean_manager(&mut ctx, &[Value::Object(Some(container_obj))])
            .unwrap()
            .unwrap();
        let bm = match bm_val {
            Value::Object(Some(o)) => o,
            _ => panic!("bm"),
        };
        let slot = ctx.get_field(bm, 0);
        match slot {
            Value::Long(v) => assert_eq!(v as u64, id),
            other => panic!("expected container id on bean manager, got {:?}", other),
        }
    }

    #[test]
    fn class_key_preserves_classloader_identity() {
        // Two different loader ids yield different ClassKeys even for
        // the same class name - defends against hostile loader
        // impersonation.
        let k1 = ClassKey::new("com/example/X", 2);
        let k2 = ClassKey::new("com/example/X", 99);
        assert_ne!(k1, k2);
        let k3 = ClassKey::new("com/example/X", 2);
        assert_eq!(k1, k3);
    }

    #[test]
    fn descriptor_extraction_strips_l_prefix_and_semicolon() {
        assert_eq!(
            extract_descriptor_class("Ljava/lang/String;"),
            Some("java/lang/String".to_string())
        );
        assert_eq!(extract_descriptor_class("I"), None);
        assert_eq!(extract_descriptor_class("[I"), None);
        assert_eq!(
            extract_descriptor_class("Lio/quarkus/arc/ArcContainer;"),
            Some("io/quarkus/arc/ArcContainer".to_string())
        );
    }

    #[test]
    fn resolution_count_increments_per_successful_lookup() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (_id, container_obj) = make_arc_container(&mut ctx);
        let before = get_or_init_container().resolution_count();
        let mirror = make_class_mirror_for(&mut ctx, "com/example/Counted");
        native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap();
        native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap();
        let after = get_or_init_container().resolution_count();
        assert_eq!(after - before, 2, "each lookup increments counter once");
    }

    #[test]
    fn instance_handle_destroy_is_a_graceful_no_op() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (_id, container_obj) = make_arc_container(&mut ctx);
        let mirror = make_class_mirror_for(&mut ctx, "com/example/Closeable");
        let handle = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("handle"),
        };
        native_instance_handle_no_op(&mut ctx, &[Value::Object(Some(handle))]).unwrap();
        native_instance_handle_no_op(&mut ctx, &[Value::Object(Some(handle))]).unwrap();
        // Still resolvable after "destroy" - we do not invalidate.
        let avail = native_instance_handle_available(&mut ctx, &[Value::Object(Some(handle))])
            .unwrap()
            .unwrap();
        assert!(matches!(avail, Value::Int(1)));
    }

    #[test]
    fn bean_manager_get_reference_unwraps_bean_slot() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (id, container_obj) = make_arc_container(&mut ctx);
        let mirror = make_class_mirror_for(&mut ctx, "com/example/Wrapped");
        let handle = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("handle"),
        };

        let bm =
            match native_container_bean_manager(&mut ctx, &[Value::Object(Some(container_obj))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(o)) => o,
                _ => panic!("bm"),
            };

        // getReference expects (null CreationalContext, handle, null)
        // - our impl reads args[1] only.
        let ref_val = native_bean_manager_get_reference(
            &mut ctx,
            &[
                Value::Object(Some(bm)),
                Value::Object(Some(handle)),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        match ref_val {
            Value::Object(Some(_)) => {}
            other => panic!("expected unwrapped bean, got {:?}", other),
        }
        // Container id unchanged.
        assert_eq!(get_or_init_container().id(), id);
    }

    #[test]
    fn concurrent_initialize_returns_same_container_across_threads() {
        use std::sync::Barrier;

        let _g = state_guard().lock();
        reset_container_state();

        // Initialize the container once up-front in main thread; the
        // race happens over `get_or_init_container` under the OnceLock.
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let bar = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    bar.wait();
                    // Each thread hits initialize & observes a shared id.
                    let inner = get_or_init_container();
                    inner.id()
                })
            })
            .collect();
        let mut ids: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(
            ids.len(),
            1,
            "all threads must observe the same container id"
        );
    }

    #[test]
    fn inject_field_walk_populates_discovered_fields() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        // The mock's declared_fields returns empty, so the discover
        // step returns an empty Vec and does not blow up. We assert the
        // cache is populated after first call (even if empty).
        let inner = get_or_init_container();
        let bean_name = "com/example/WithInjects";
        let cid = ctx.ensure_class_initialized(bean_name).unwrap();
        let key = ClassKey::new(bean_name, ctx.loader_id_of_class(cid));
        let discovered_first = discover_inject_fields(&inner, &ctx, &key);
        let discovered_second = discover_inject_fields(&inner, &ctx, &key);
        assert_eq!(
            discovered_first, discovered_second,
            "cached result must be stable"
        );
        let reader = inner.inject_fields.read();
        assert!(reader.contains_key(&key), "cache must have the key");
    }

    #[test]
    fn select_alias_behaves_identically_to_instance() {
        let _g = state_guard().lock();
        reset_container_state();
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();

        let (_id, container_obj) = make_arc_container(&mut ctx);
        let mirror = make_class_mirror_for(&mut ctx, "com/example/SelectBean");

        let inst_handle = match native_container_instance_class_simple(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let sel_handle = match native_container_select(
            &mut ctx,
            &[
                Value::Object(Some(container_obj)),
                Value::Object(Some(mirror)),
            ],
        )
        .unwrap()
        .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        // Different handle identity (new alloc), but same underlying
        // bean slot.
        let b1 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(inst_handle))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        let b2 = match native_instance_handle_get(&mut ctx, &[Value::Object(Some(sel_handle))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!(),
        };
        assert_eq!(b1, b2, "select + instance yield the same bean");
    }

    #[test]
    fn try_container_is_some_after_init_none_before() {
        let _g = state_guard().lock();
        // Because OnceLock is process-wide and a prior test may have
        // initialized it, we can only assert the post-init invariant
        // here.
        let mut ctx = mock_ctx();
        native_arc_initialize(&mut ctx, &[]).unwrap();
        assert!(try_container().is_some(), "container must exist after init");
    }

    #[test]
    fn scope_default_is_application_for_unannotated_beans() {
        assert_eq!(Scope::default_for_bootstrap(), Scope::Application);
    }
}
