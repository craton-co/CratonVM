// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Registry for native method implementations.
//!
//! Maps (class, method, descriptor) triples to Rust function callbacks
//! that implement the native method behavior.

// AUDIT 2026-05-16: std::collections::HashMap is unused — the registry
// migrated to rustc_hash::FxHashMap (T10.9.B). Import removed.
use std::sync::{Arc, OnceLock};

// Call-site memoization handles. `NativeMethodId` indexes this file's
// `NativeSlot` table; `NativeMethodKey` is the precomputable digest. Both live
// in `native_id.rs` so the memo cell (`NativeCallSite`) and its documentation
// sit together, away from this file's 3,000-line `NativeContext` trait.
use crate::native_id::{NativeMethodId, NativeMethodKey};

/// NIO-SERVER-SOCKET (route 1): cached check of the `CRATONVM_REAL_NET_SOCKETS`
/// env var. When set, the native registry drops all synthetic
/// `java/net/Socket` / `java/net/ServerSocket` registrations so real JDK
/// bytecode drives the `sun/nio/ch/Net` path. Cached in a `OnceLock` because
/// `register()` is called thousands of times at startup.
fn real_net_sockets_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_REAL_NET_SOCKETS").is_some())
}

/// REAL-FORKJOINPOOL (opt-in `CRATONVM_REAL_FORKJOINPOOL`): when set, the
/// registry drops the synthetic `java/util/concurrent/ForkJoinPool` natives
/// **except `execute`** so the real JDK pool bytecode runs (proper init,
/// parallelism = cpus-1, real `ForkJoinWorkerThread`s; work-stealing degrades to
/// caller-runs). The synthetic pool's `commonPool()` returns an uninitialised
/// real-class instance (no queues), so real `invokeAll`/`submit` throws
/// `RejectedExecutionException` — dropping the natives fixes that and enables
/// Weld's real concurrent CDI bootstrap (`ConcurrentBeanDeployer`).
///
/// `execute` is KEPT synthetic (eager-inline, run on the caller) so that
/// `CompletableFuture.*Async` (which schedules every stage via `execute`) keeps
/// working under the real pool — see below.
///
/// **Opt-in, NOT a safe global default.** CratonVM's cross-worker memory
/// ordering doesn't reliably publish an object-reference field written by one
/// worker to a task on another worker. The eager-inline `execute` masks this for
/// `CompletableFuture` (everything runs caller-side), but `parallel-stream` /
/// fork-join work uses `ForkJoinTask.fork`/`invoke` (not `execute`), runs on
/// real workers, and can read stale-null cross-worker state — observed as a
/// regression in `PersistenceXmlParserTest` (4/4 → 2/4) when this was forced on
/// globally. So the synthetic pool stays the default; this gate is for
/// concurrent-CDI workloads that don't lean on parallel-stream result passing.
/// HIB-CV-20.
fn real_forkjoinpool_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_REAL_FORKJOINPOOL").is_some())
}

use rustc_hash::{FxHashMap, FxHashSet};

use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::ClassId;
use cratonvm_types::{ArrayElementType, ObjectKind};
use cratonvm_types::{ObjectRef, Value};

/// VM-owned data needed to materialize a truthful JMX `ThreadInfo` object.
///
/// The object references are strong, GC-remapped registry roots for the short
/// interval in which a thread owns, waits on, or contends for a lock.  Native
/// JMX code pins them before doing any allocating work.
#[derive(Clone, Debug, Default)]
pub struct ThreadJmxSnapshot {
    pub thread_object: Option<ObjectRef>,
    pub thread_id: i64,
    pub thread_name: String,
    /// JMM/JVMTI thread-status bits reserved for consumers that use the
    /// encoded state rather than the JDK 25 `Thread.State` field.
    pub thread_status: i32,
    pub stack_trace: Vec<StackTraceEntry>,
    pub lock: Option<ObjectRef>,
    /// Logical JMM class name for `lock` when a VM shim deliberately models
    /// the backing synchronizer without materializing its private JDK object.
    pub lock_class_name: Option<String>,
    pub lock_owner_id: i64,
    pub lock_owner_name: Option<String>,
    pub locked_monitors: Vec<ObjectRef>,
    pub locked_synchronizers: Vec<ObjectRef>,
}

fn value_matches_primitive_array(element_type: ArrayElementType, value: Value) -> bool {
    match element_type {
        ArrayElementType::Boolean
        | ArrayElementType::Byte
        | ArrayElementType::Char
        | ArrayElementType::Short
        | ArrayElementType::Int => matches!(value, Value::Int(_)),
        ArrayElementType::Long => matches!(value, Value::Long(_)),
        ArrayElementType::Float => matches!(value, Value::Float(_)),
        ArrayElementType::Double => matches!(value, Value::Double(_)),
        ArrayElementType::Reference => false,
    }
}

// ---------------------------------------------------------------------------
// Reflection metadata types
// ---------------------------------------------------------------------------

/// Metadata for a field declared in a class, used by reflection.
pub struct FieldMetadata {
    pub name: String,
    pub descriptor: String,
    pub access_flags: u16,
    /// Absolute heap field index (accounts for inherited instance fields).
    pub slot_index: usize,
    pub declaring_class_id: ClassId,
    pub is_static: bool,
}

/// Metadata for a method declared in a class, used by reflection.
pub struct MethodMetadata {
    pub name: String,
    pub descriptor: String,
    pub access_flags: u16,
    pub declaring_class_id: ClassId,
    /// Internal names of exception classes the method declares to
    /// throw (JVMS §4.7.5 `Exceptions` attribute). Empty for methods
    /// without a `throws` clause. Populated by the VM's
    /// `declared_methods` impl from `Attribute::Exceptions`.
    ///
    /// Used by `native_builtins::build_proxy_spec_for` (WP2.5 v3 item 3)
    /// to thread the declared exception set through to the generated
    /// proxy class's `<clinit>` so that `wrap_undeclared_throwable`
    /// (WP2.5 v3 item 6) can match thrown exceptions against it.
    pub exceptions: Vec<String>,
}

/// WP2.3 — defineClass options carried through `NativeContext::define_class_full`.
///
/// Mirrors `cratonvm_classloading::DefineClassOptions` but without the
/// crate dependency, so native-builtins can construct it without
/// importing classloading directly. The VM's `NativeContext`
/// implementation translates this into a `DefineClassOptions` and
/// delegates to `ClassManager::define_class_with_options`.
#[derive(Debug, Clone, Default)]
pub struct DefineClassFull {
    /// Optional override of the registered name (for hidden / mangled).
    pub override_name: Option<String>,
    /// Mark the new class as hidden (JEP 371).
    pub hidden: bool,
    /// Skip bytecode verification on these bytes.
    pub skip_verification: bool,
    /// Optional URL for the resulting `CodeSource` (e.g. `"file:/foo.jar"`).
    pub code_source_url: Option<String>,
    /// DER-encoded signer certificates to attach as the `CodeSource`.
    pub code_source_certificates: Vec<Vec<u8>>,
    /// Allow redefinition (replaces in place if class exists). Used by WP2.4.
    pub allow_redefine: bool,
    /// Nest-host attribution (NESTMATE). Internal class name.
    pub nest_host_class_name: Option<String>,
    /// If `true`, run `<clinit>` on the new class before returning.
    pub initialize: bool,
    /// Privileged define: the bytes come from a trusted JVM-internal
    /// code-generation path (`sun.misc.Unsafe.defineClass` /
    /// `jdk.internal.misc.Unsafe.defineClass0`) that, on HotSpot, bypasses
    /// `ClassLoader.preDefineClass`'s "Prohibited package name: java.*"
    /// guard. Set by the Unsafe.defineClass natives so toolchains like
    /// ByteBuddy can inject a privileged accessor (e.g.
    /// `java.lang.ClassLoader$ByteBuddyAccessor$V1`) into a protected
    /// platform package, exactly as they do on a real JVM. Off by default;
    /// the ordinary `ClassLoader.defineClass` path leaves it false so the
    /// spoofing guard still applies there.
    pub privileged_define: bool,
    /// Force loader-faithful supertype/interface linking for this define —
    /// mirrors `cratonvm_classloading::DefineClassOptions::force_loader_faithful_linking`.
    /// Set by generated `$ProxyN` class definitions
    /// (`native_builtins::define_or_get_proxy_class`) so the proxy links
    /// against the EXACT interface `ClassId` it was generated for, rather
    /// than the loader-agnostic global `load_class(name)` fallback that
    /// `CRATONVM_LOADER_AWARE_RESOLUTION` (default off) otherwise gates.
    pub force_loader_faithful_linking: bool,
}

/// The lambda-call-site metadata needed to round-trip a serializable lambda.
///
/// Mirrors the fields of `java.lang.invoke.SerializedLambda` that matter for
/// reconstruction: the functional-interface SAM, the implementation method
/// handle, the instantiated (specialized) descriptor, and the capture-value
/// types. Produced by [`NativeContext::lambda_proxy_serial_metadata`] for a
/// synthetic lambda-proxy class and consumed (alongside the captured field
/// values) by the deserialization path, which feeds it straight into
/// [`NativeContext::register_lambda_proxy`].
#[derive(Clone, Debug)]
pub struct LambdaSerialMetadata {
    pub functional_interface: String,
    pub sam_method_name: String,
    pub sam_descriptor: String,
    pub impl_class: String,
    pub impl_member: String,
    pub impl_descriptor: String,
    /// JVMS `reference_kind` byte (1..=9) of the implementation method handle.
    pub impl_ref_kind: u8,
    pub instantiated_descriptor: String,
    /// One type char per captured value (`'L'`, `'I'`, `'J'`, ...), in
    /// factory-argument / proxy-field order.
    pub capture_types: String,
}

/// Compute a fast 128-bit hash key for a native method triple.
///
/// Returns a `(u64, u64)` pair. The two halves are produced by **two
/// genuinely different hash functions**, not the same FNV-1a function
/// re-seeded:
///
///  * the first half uses the standard 64-bit FNV-1a prime
///    (`0x100000001b3`), and
///  * the second half uses a *different* odd multiplier
///    (`0x880355f21e6d1965`, the well-known `fasthash`/`xxhash`-family
///    mixing constant) so the two passes are not just affine variants
///    of one another.
///
/// Both halves then get a splitmix64-style finalization avalanche, so
/// any residual structural correlation between the two passes is
/// destroyed before the values are used as a key.
///
/// With two effectively-independent 64-bit hashes the composite key is
/// ~128-bit; the birthday-collision probability for the few thousand
/// native methods we register is negligibly small (and `register`
/// still carries a `debug_assert!` collision check as a backstop).
#[inline]
pub(crate) fn native_method_hash(class: &str, method: &str, descriptor: &str) -> (u64, u64) {
    native_method_hash_from(native_class_hash(class), method, descriptor)
}

/// FNV prime for the first pass of the native-method digest.
const FNV_PRIME: u64 = 0x100000001b3;
/// A distinct odd multiplier for the second pass — different bit pattern
/// *and* different magnitude, so the second hash is not a re-seeded copy of
/// the first.
const ALT_PRIME: u64 = 0x880355f21e6d1965;

/// The **unfinalized** accumulator pair after hashing only the class-name
/// component of a native-method triple.
///
/// This is the prefix state [`native_method_hash`] would hold halfway through,
/// exposed so a lookup can (a) test whether the class registers ANY native at
/// all before paying for the rest of the digest, and (b) finish the digest
/// from here without re-walking the class name. Deliberately NOT passed
/// through [`fmix64`]: it is a resumable state, not a key.
///
/// PERF (H2 `TestFileSystem.testConcurrent`, 2026-07-26). `slot_for_exact` is
/// on the interpreter's every-invoke path via `invoke_or_native`, and it was
/// the single largest entry in a live 30s/999Hz profile of that test —
/// **8.75%** of CPU across its two real threads, plus a large share of the
/// `__memcmp_evex_movbe` time spent name-verifying the digest hit. The digest
/// is a byte-at-a-time walk of all three strings (~60-100 bytes, two
/// accumulators), and the overwhelming majority of the calls come from
/// application classes — `org/h2/mvstore/...`, `org/h2/store/...` — that
/// register no natives whatsoever, so all of that work produced a miss. The
/// class name alone is ~25 of those bytes and answers "miss" for every one of
/// them.
#[inline]
fn native_class_hash(class: &str) -> (u64, u64) {
    let mut h1 = 0xcbf29ce484222325;
    let mut h2 = 0x9e3779b97f4a7c15;
    hash_component_pair(&mut h1, &mut h2, class, FNV_PRIME, ALT_PRIME);
    (h1, h2)
}

/// Finish a native-method digest from a [`native_class_hash`] prefix state.
#[inline]
fn native_method_hash_from(class_state: (u64, u64), method: &str, descriptor: &str) -> (u64, u64) {
    let (mut h1, mut h2) = class_state;
    hash_byte_pair(&mut h1, &mut h2, b'.', FNV_PRIME, ALT_PRIME);
    hash_component_pair(&mut h1, &mut h2, method, FNV_PRIME, ALT_PRIME);
    hash_byte_pair(&mut h1, &mut h2, b'.', FNV_PRIME, ALT_PRIME);
    hash_component_pair(&mut h1, &mut h2, descriptor, FNV_PRIME, ALT_PRIME);
    (fmix64(h1), fmix64(h2))
}

/// Update both independent native-method hash accumulators for one byte.
#[inline]
fn hash_byte_pair(h1: &mut u64, h2: &mut u64, byte: u8, prime1: u64, prime2: u64) {
    *h1 ^= byte as u64;
    *h1 = h1.wrapping_mul(prime1);
    *h2 ^= byte as u64;
    *h2 = h2.wrapping_mul(prime2);
}

/// One scan over a component, updating the two statistically independent
/// FNV-style accumulators in parallel. This preserves the previous key format
/// while avoiding two full byte walks for every hot-path registry lookup.
#[inline]
fn hash_component_pair(h1: &mut u64, h2: &mut u64, component: &str, prime1: u64, prime2: u64) {
    for byte in component.bytes() {
        hash_byte_pair(h1, h2, byte, prime1, prime2);
    }
}

/// splitmix64 finalizer — an avalanche mix that spreads every input bit
/// across the whole 64-bit output. Applied to each hash half so the two
/// halves of the composite key have no shared low-order structure.
#[inline]
fn fmix64(mut h: u64) -> u64 {
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58476d1ce4e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d049bb133111eb);
    h ^= h >> 31;
    h
}

/// GC/STW hooks for VM-registered native carrier threads.
///
/// Some native subsystems spawn host OS threads that are registered in the VM
/// thread list, but do not own a full [`NativeContext`] while they sit in an
/// OS wait primitive. Those threads still need to publish their OS tid for
/// cross-thread diagnostics/takeover and must enter the GC-blocked population
/// before parking so a stop-the-world pause does not wait for a thread that
/// cannot reach an interpreter safepoint.
pub trait NativeThreadBlocker: Send + Sync {
    /// Publish the current OS thread id for this VM thread.
    fn publish_os_tid(&self);

    /// Enter a GC-blocked native wait region.
    fn enter_blocked(&self);

    /// Leave a GC-blocked native wait region.
    fn leave_blocked(&self);
}

/// Transport-only mirror of `vm::runtime::offload::SerializedResult`'s
/// scalar variants, returned by [`NativeContext::gpu_future_take_result`].
///
/// `native-api` cannot depend on `vm` (the dependency runs the other
/// way — `vm`'s `NativeContextImpl` implements this crate's
/// `NativeContext` trait), so a completed GPU submission's
/// `SerializedResult` can't cross the trait boundary as-is. This enum is
/// the narrow subset `gpu_future_take_result` needs to hand back: the
/// four scalar-reduction shapes a `)I`/`)J`/`)F`/`)D`-returning kernel
/// produces, plus `Void` for a kernel with no return value (or one that
/// wrote its result into a caller-owned array instead — see the trait
/// method's doc comment). Primitive-array *future results* are
/// deliberately not represented here: today `finalize_submission` never
/// stamps a `SerializedResult::PrimitiveArray*` into a completed
/// submission (array outputs are delivered via writeback into the
/// caller's own array), so there is nothing for this enum to carry for
/// that case.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GpuFutureResult {
    /// The kernel had a void return, or wrote its result into a
    /// caller-owned output array via writeback — nothing new to hand
    /// back through the future's result slot.
    Void,
    /// Scalar-return accumulator readback (`)I` descriptor).
    ScalarI32(i32),
    /// Scalar-return accumulator readback (`)J` descriptor).
    ScalarI64(i64),
    /// Scalar-return accumulator readback (`)F` descriptor).
    ScalarF32(f32),
    /// Scalar-return accumulator readback (`)D` descriptor).
    ScalarF64(f64),
}

/// Opaque reference to a GC-updated slot owned by a [`NativeHandleScope`].
///
/// The fallback is intentionally private. Lightweight mock contexts do not
/// implement a moving heap and therefore use it when their default
/// [`NativeContext::handle_get`] returns `None`; production VM contexts always
/// read the current address through `slot`. Native implementations cannot
/// extract either value, so they cannot accidentally keep using a pre-GC raw
/// reference or reinterpret a slot index as a heap address.
#[derive(Debug)]
pub struct NativeHandle {
    slot: u32,
    fallback: ObjectRef,
}

/// Early-return- and panic-safe native root scope.
///
/// Construct this before retaining any object across an allocating or
/// re-entrant VM call. Every object rooted through [`Self::root`] remains in
/// the executing thread's collector-visible handle table until this guard is
/// dropped. [`Drop`] closes the scope on every Rust exit path, eliminating the
/// manually paired `handle_scope_push`/`handle_scope_pop` discipline.
///
/// `DerefMut<Target = dyn NativeContext>` lets existing native code call VM
/// capabilities through the scope while the roots are active:
///
/// ```ignore
/// let mut scope = NativeHandleScope::new(ctx);
/// let receiver = scope.root(receiver);
/// let array = scope.new_array(ArrayElementType::Char, len); // may collect
/// let receiver = scope.get(&receiver); // always the current address
/// scope.set_field(receiver, 0, Value::Object(Some(array)));
/// // scope closes automatically, including on `?` or `return`.
/// ```
pub struct NativeHandleScope<'a> {
    context: &'a mut dyn NativeContext,
}

impl<'a> NativeHandleScope<'a> {
    /// Open a nested scope on `context`.
    pub fn new(context: &'a mut dyn NativeContext) -> Self {
        context.handle_scope_push();
        Self { context }
    }

    /// Root `object` and return an opaque handle that can only be resolved
    /// through this scope.
    pub fn root(&mut self, object: ObjectRef) -> NativeHandle {
        NativeHandle {
            slot: self.context.handle_root(object),
            fallback: object,
        }
    }

    /// Resolve `handle` to its current post-GC address.
    pub fn get(&self, handle: &NativeHandle) -> ObjectRef {
        self.context
            .handle_get(handle.slot)
            .unwrap_or(handle.fallback)
    }
}

impl<'a> std::ops::Deref for NativeHandleScope<'a> {
    type Target = dyn NativeContext + 'a;

    fn deref(&self) -> &Self::Target {
        self.context
    }
}

impl<'a> std::ops::DerefMut for NativeHandleScope<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.context
    }
}

impl Drop for NativeHandleScope<'_> {
    fn drop(&mut self) {
        self.context.handle_scope_pop();
    }
}

/// Trait providing VM capabilities needed by native method implementations.
///
/// The `Vm` struct implements this trait. Using a trait here avoids circular
/// module dependencies between `native` and `vm`.
pub trait NativeClassAccess {
    /// Capability boundary: Class identity, metadata, loading, resources, and modules.


    /// Load a class by name. Returns the ClassId.
    fn load_class(&mut self, name: &str) -> MethodCallResult;

    /// Get the class name for a ClassId.
    fn class_name_of_id(&self, class_id: ClassId) -> Option<String>;

    /// Get the class id of a heap object.
    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId;

    /// True when the named class is loaded as a synthetic stub (no real
    /// `.class` bytes). Used to branch native helpers that must mirror JDK
    /// behaviour without registering natives that would override real JDK
    /// bytecode once the stub upgrades.
    fn is_class_synthetic_stub(&self, class_name: &str) -> bool {
        false
    }

    /// Check if a method exists in a class (searches the class hierarchy).
    /// Returns `true` if the method is found.
    fn method_exists(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool;

    /// True iff `class_id` ITSELF declares (not merely inherits) a method with
    /// the given name+descriptor. Unlike [`method_exists`], this does NOT walk
    /// superclasses — it answers "does this exact class override the method?".
    /// Used by the `ClassLoader.getResources` native to distinguish a custom
    /// loader that overrides `findResources` (delegate to it) from one that
    /// merely inherits the default (fall back to parent-delegated scan).
    /// Default returns `false` so mock/test contexts compile unchanged.
    fn class_declares_method(&self, _class_id: ClassId, _name: &str, _descriptor: &str) -> bool {
        false
    }

    /// Ensure a class is loaded and initialized. Returns the ClassId.
    fn ensure_class_initialized(
        &mut self,
        name: &str,
    ) -> Result<ClassId, cratonvm_types::error::MethodCallFailed>;

    /// Ensure the exact already-resolved class id is initialized without
    /// re-resolving its binary name through the global loader map.
    fn ensure_class_initialized_with_class_id(
        &mut self,
        class_id: ClassId,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        if let Some(name) = self.class_name_of_id(class_id) {
            self.ensure_class_initialized(&name)?;
        }
        Ok(())
    }

    /// Register a lambda proxy synthesized from a *reflective*
    /// `LambdaMetafactory.metafactory` / `altMetafactory` call (as opposed to
    /// the `invokedynamic` opcode, which is handled inline in the interpreter).
    ///
    /// Returns the raw `u32` of a freshly-allocated synthetic proxy `ClassId`
    /// whose lambda call-site metadata is registered in the VM's
    /// `lambda_proxies` table — identical in shape to the metadata produced by
    /// the `invokedynamic` lambda bootstrap, so the interpreter's SAM-dispatch
    /// path (`try_lambda_dispatch`) handles instances of it without any further
    /// special-casing. A factory `MethodHandle` (kind `MH_KIND_LAMBDA_FACTORY`)
    /// later allocates proxy instances of this class once the captured values
    /// are known.
    ///
    /// `impl_ref_kind` is the JVMS `reference_kind` byte (1..=9) of the
    /// implementation method handle. `capture_types` is one type char per
    /// captured value (`'L'`, `'I'`, `'J'`, ...), in factory-argument order.
    ///
    /// Returns `0` when the host cannot register a proxy (e.g. test mocks, or
    /// the proxy table is full); callers treat `0` as "unsupported" and fall
    /// back to a non-null no-op `CallSite`.
    #[allow(clippy::too_many_arguments)]
    fn register_lambda_proxy(
        &mut self,
        functional_interface: &str,
        sam_method_name: &str,
        sam_descriptor: &str,
        impl_class: &str,
        impl_member: &str,
        impl_descriptor: &str,
        impl_ref_kind: u8,
        instantiated_descriptor: &str,
        capture_types: &str,
    ) -> u32 {
        let _ = (
            functional_interface,
            sam_method_name,
            sam_descriptor,
            impl_class,
            impl_member,
            impl_descriptor,
            impl_ref_kind,
            instantiated_descriptor,
            capture_types,
        );
        0
    }

    /// Check if child_class is a subclass of parent_class.
    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool;

    /// Get the superclass ClassId. Returns None for java/lang/Object.
    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId>;

    /// Get the ClassId for a loaded class by name. Returns None if not loaded.
    fn class_id_by_name(&self, name: &str) -> Option<ClassId>;

    /// Resolve `name` to a `ClassId`, preferring whichever loaded class is
    /// registered under the SAME classloader as `near`'s own declaring
    /// context, falling back to the normal global (bootstrap-first) search
    /// used by [`Self::class_id_by_name`].
    ///
    /// A plain by-name lookup can silently resolve to an unrelated
    /// same-named class loaded under a DIFFERENT classloader when the JVM
    /// spec's (loader, name) identity legitimately produces two distinct
    /// classes with the same name -- e.g. Hibernate ORM's bytecode
    /// enhancement reloads an `@EmbeddedId` class under its own private
    /// ByteBuddy classloader. Resolving a field/parameter's declared type
    /// via plain name search can then find the FIRST-loaded (often stale)
    /// variant instead of the one the caller's own class actually sees,
    /// causing a real, correctly-typed value to be rejected as an
    /// assignability mismatch. Use this instead of `class_id_by_name`
    /// whenever `name` is a symbolic reference that must match the specific
    /// class variant visible to a known class (`near`) -- e.g. a
    /// `Field`/`Method`/`Constructor`'s own declaring class.
    fn class_id_by_name_near(&self, name: &str, _near: ClassId) -> Option<ClassId> {
        self.class_id_by_name(name)
    }

    /// Resolve `name` the way a requester-less parent-delegation loader
    /// would: bootstrap, then extension, then application. Do not collapse
    /// onto an unrelated user-defined loader's private same-named class unless
    /// it is the only possible answer.
    ///
    /// Use this for lookups performed on behalf of ordinary application code
    /// when no precise requesting class is available. Prefer
    /// [`Self::class_id_by_name_near`] whenever a requester is known.
    fn class_id_by_name_delegated(&self, name: &str) -> Option<ClassId> {
        self.class_id_by_name(name)
    }

    /// Resolve `name` to a `ClassId`, LOADING it through
    /// `referencing_class_id`'s own defining classloader if it isn't loaded
    /// yet -- exactly as a bytecode instruction (`new`/`checkcast`/
    /// `invokestatic`/...) referencing `name` FROM `referencing_class_id`
    /// would (JVMS SS5.4.3 initiating-loader semantics).
    ///
    /// Unlike [`Self::class_id_by_name_near`]/[`Self::class_id_by_name`] --
    /// pure lookups that only succeed once `name` has already been
    /// resolved/indexed under that loader -- this drives the loader's own
    /// `loadClass`/`defineClass` on a miss, so it also answers correctly the
    /// very first time a class is needed under a given loader (the gap that
    /// made two prior lookup-based fix attempts for the H2 `Parser`
    /// loader-collapse bug regress on a fresh session -- see
    /// docs/known-issues/h2/bug-h2-suite-residual-fail-triage.md's
    /// eighth-pass section).
    ///
    /// Native overrides that construct or invoke-special a DIFFERENT class
    /// than their own receiver's declaring class (an app/H2 native bridging
    /// into the receiver's own package -- e.g. `SessionLocal.prepareLocal`'s
    /// `new Parser(this)`) MUST use this instead of
    /// `new_object_initialized`/`invoke_special` with a bare name: those
    /// collapse to whichever loader defined `name` FIRST process-wide,
    /// silently constructing/invoking the WRONG loader's copy of the class
    /// whenever the receiver's own defining loader is a user-defined one
    /// distinct from the first-loaded (usually Application) copy.
    ///
    /// The default implementation ignores `referencing_class_id` and falls
    /// back to the name-only [`Self::ensure_class_initialized`] -- sufficient
    /// for test mocks and any context with a single (global) loader
    /// namespace; the real VM implementation honours per-loader identity.
    fn class_id_by_name_via_referencing_class(
        &mut self,
        _referencing_class_id: ClassId,
        name: &str,
    ) -> Result<ClassId, cratonvm_types::error::MethodCallFailed> {
        self.ensure_class_initialized(name)
    }

    /// For a synthetic lambda-proxy `ClassId` (created by `register_lambda_proxy`,
    /// class id `>= 0x8000_0000`, not in the class store), return the internal
    /// name of its functional (SAM) interface. Returns `None` for any non-lambda
    /// class. Used by reflection natives so a lambda's mirror reports a sane
    /// hierarchy (`getSuperclass()` = Object, `getInterfaces()` = [SAM]) instead
    /// of null/empty — Gradle's listener type-walk calls
    /// `concreteClass.getSuperclass().isInterface()` and a null superclass NPEs.
    fn lambda_functional_interface(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    /// For a synthetic lambda-proxy `ClassId`, return the internal name of the
    /// lambda's *defining* class (the class that owns the implementation method,
    /// e.g. `Refl6` for a `() -> {}` whose body compiles to `Refl6.lambda$..`).
    /// Returns `None` for any non-lambda class. Used by the reflection name
    /// natives (`getName`/`getSimpleName`/`getNestHost`) to synthesize the
    /// HotSpot-style `<host>$$Lambda/0x<id>` name instead of `unknown_<id>`.
    fn lambda_proxy_host(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    /// For a synthetic lambda-proxy `ClassId`, return `(sam_method_name,
    /// sam_erased_descriptor, instantiated_descriptor)` from the
    /// `LambdaMetafactory` bootstrap that created it. `sam_erased_descriptor`
    /// is the functional interface's own (type-erased) SAM descriptor — the
    /// key needed to look up that method's generic `Signature` attribute via
    /// `method_signature`. `instantiated_descriptor` is the call-site-specific,
    /// concrete-typed descriptor (e.g. `(Lcom/foo/Bar;)V` for a
    /// `Consumer<Bar>` lambda) — the source of truth for substituting the
    /// functional interface's type variable(s) with concrete types. Returns
    /// `None` for any non-lambda class. Used to synthesize a real
    /// `ParameterizedType` for `Class.getGenericInterfaces()` on a lambda
    /// proxy instead of falling back to the raw (non-generic) interface
    /// `Class` — Spring's `GenericTypeResolver` (and similar reflection-based
    /// generic-argument resolvers) require an actual `ParameterizedType` and
    /// throw when only a raw `Class` is available.
    fn lambda_call_site_descriptors(&self, _class_id: ClassId) -> Option<(String, String, String)> {
        None
    }

    /// For a synthetic lambda-proxy `ClassId`, return the full lambda
    /// call-site metadata required to serialize and later reconstruct the
    /// lambda (see [`LambdaSerialMetadata`]). Returns `None` for any
    /// non-lambda class. Used by the object-serialization natives to emit a
    /// `SerializedLambda`-equivalent record instead of attempting to serialize
    /// the (un-loadable) synthetic `$$Lambda` proxy class by name.
    fn lambda_proxy_serial_metadata(&self, _class_id: ClassId) -> Option<LambdaSerialMetadata> {
        None
    }

    /// Get the ClassLoaderId for a loaded class.
    /// Returns 0 = Bootstrap, 1 = Extension, 2 = Application, 3+ = UserDefined(id).
    fn loader_id_of_class(&self, class_id: ClassId) -> i32;

    /// Check if a class is a record (has Record attribute, JEP 395).
    fn is_record_class(&self, class_id: ClassId) -> bool;

    /// Get the record components (name, descriptor) for a record class.
    fn record_components(&self, class_id: ClassId) -> Vec<(String, String)>;

    /// Check if a class is sealed (has PermittedSubclasses attribute, JEP 409).
    fn is_sealed_class(&self, class_id: ClassId) -> bool;

    /// Get the permitted subclass names for a sealed class.
    fn permitted_subclasses(&self, class_id: ClassId) -> Vec<String>;

    /// Get the total number of instance fields (including inherited) for a
    /// loaded class.  Returns 0 if the class isn't loaded.  Used by native
    /// allocators that otherwise hard-code a synthetic field count — in
    /// real-JDK mode the hard-coded count often underestimates the real
    /// layout, and allocating with too few slots causes out-of-bounds
    /// `get_field` / `set_field` later when bytecode accesses an inherited
    /// field at `first_field_index + local_offset`.
    fn class_num_total_fields(&self, class_id: ClassId) -> usize {
        let _ = class_id;
        0
    }

    // -- Reflection metadata methods --

    /// Get metadata for all fields declared in this class (not inherited).
    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata>;

    /// Get metadata for all methods declared in this class (not inherited).
    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata>;

    /// Get the ClassIds of directly implemented/extended interfaces.
    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId>;

    /// Get the raw access_flags bits for a class.
    fn class_access_flags(&self, class_id: ClassId) -> u16;

    /// Get or create a Class mirror for a primitive type (e.g. "int", "boolean").
    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef;

    // -- Annotation support --

    /// Get runtime-visible annotation type descriptors for a class.
    /// Returns a list of (type_descriptor, element_value_pairs) tuples.
    fn class_annotations(&self, class_id: ClassId) -> Vec<AnnotationData>;

    /// Get runtime-visible annotation type descriptors for a method.
    /// `method_name` and `method_desc` identify the method within the class.
    fn method_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<AnnotationData>;

    /// Get runtime-visible annotation type descriptors for a field.
    /// `field_name` identifies the field within the class.
    fn field_annotations(&self, class_id: ClassId, field_name: &str) -> Vec<AnnotationData>;

    /// Get parameter annotations for a method.
    /// Returns a Vec of Vec<AnnotationData>, one per parameter.
    fn method_parameter_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<AnnotationData>>;

    /// Get the runtime-visible TYPE_USE annotations that target a method's
    /// return type (JVMS 4.7.20 `target_type` 0x14, METHOD_RETURN) with an
    /// empty `type_path` (i.e. annotations placed directly on the top-level
    /// return type rather than a nested array/type-argument component).
    ///
    /// Backs `Method.getAnnotatedReturnType().getDeclaredAnnotations()` so
    /// JSpecify-style `@Nullable`/`@NonNull` (which are TYPE_USE-only and thus
    /// live in `RuntimeVisibleTypeAnnotations`, not `RuntimeVisibleAnnotations`)
    /// are surfaced to reflection. Default impl returns an empty `Vec` so mock
    /// `NativeContext` implementations don't need to plumb the attribute store.
    fn method_return_type_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<AnnotationData> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a method return
    /// type's TYPE ARGUMENTS, at any nesting depth (JVMS 4.7.20 `target_type`
    /// 0x14, METHOD_RETURN, with a `type_path` made entirely of TYPE_ARGUMENT
    /// entries) -- e.g. `List<@NotBlank String> getNames()`, or nested generics
    /// like `ValueExtractor<Wrapper<@Foo ?>>`.
    ///
    /// The outer `Vec` is indexed by the top-level `type_argument_index`
    /// (0-based, per JVMS 4.7.20.2); each entry's own `children` carries the
    /// next nesting level. Default impl returns an empty `Vec`.
    fn method_return_type_argument_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<TypeArgAnnotations> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a method's
    /// formal parameters (JVMS 4.7.20 `target_type` 0x16,
    /// METHOD_FORMAL_PARAMETER) with an empty `type_path`. The outer `Vec` is
    /// indexed by `formal_parameter_index`; entries with no annotations are
    /// empty inner `Vec`s.
    ///
    /// Backs `Parameter.getAnnotatedType().getDeclaredAnnotations()`. Default
    /// impl returns an empty `Vec`.
    fn method_parameter_type_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<Vec<AnnotationData>> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a field's type
    /// (JVMS 4.7.20 `target_type` 0x13, FIELD) with an empty `type_path`.
    /// Backs `Field.getAnnotatedType().getDeclaredAnnotations()`. Default impl
    /// returns an empty `Vec`.
    fn field_type_annotations(&self, _class_id: ClassId, _field_name: &str) -> Vec<AnnotationData> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a field type's
    /// TYPE ARGUMENTS, at any nesting depth (JVMS 4.7.20 `target_type` 0x13,
    /// FIELD, with a `type_path` made entirely of TYPE_ARGUMENT entries) --
    /// e.g. `List<@NotBlank String> names`.
    ///
    /// The outer `Vec` is indexed by the top-level `type_argument_index`
    /// (0-based, per JVMS 4.7.20.2); each entry's own `children` carries the
    /// next nesting level. Default impl returns an empty `Vec`.
    fn field_type_argument_annotations(
        &self,
        _class_id: ClassId,
        _field_name: &str,
    ) -> Vec<TypeArgAnnotations> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations that target a method
    /// formal parameter's type ARGUMENTS, at any nesting depth (JVMS 4.7.20
    /// `target_type` 0x16, METHOD_FORMAL_PARAMETER, with a `type_path` made
    /// entirely of TYPE_ARGUMENT entries) -- e.g. the `@Valid` in
    /// `List<@Valid Person> persons`, which annotates the type argument
    /// `Person`, not the top-level `List` parameter type.
    ///
    /// The outer `Vec` is indexed by `formal_parameter_index`; the inner
    /// `Vec` is indexed by the top-level `type_argument_index` (0-based, per
    /// JVMS 4.7.20.2), and each entry's own `children` carries the next
    /// nesting level. Backs
    /// `((AnnotatedParameterizedType) method.getAnnotatedParameterTypes()[i])
    /// .getAnnotatedActualTypeArguments()[j].getDeclaredAnnotations()`, which
    /// Spring's `HandlerMethod.MethodValidationInitializer
    /// .getContainerElementAnnotations` walks to decide whether e.g.
    /// `addPeople(List<@Valid Person> persons)` needs method validation.
    /// Default impl returns an empty `Vec`.
    fn method_parameter_type_argument_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<Vec<TypeArgAnnotations>> {
        Vec::new()
    }

    /// Get the generic Signature attribute for a class (if present).
    fn class_signature(&self, class_id: ClassId) -> Option<String>;

    /// Get the generic Signature attribute for a method (if present).
    fn method_signature(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<String>;

    /// Get the generic Signature attribute for a field (if present).
    fn field_signature(&self, class_id: ClassId, field_name: &str) -> Option<String>;

    /// WP2.1 — Get the parsed `MethodParameters` attribute (JVMS 4.7.24)
    /// for a method. Each entry is `(name, access_flags)`. The name is the
    /// resolved Utf8 from the constant pool, or empty string if
    /// `name_index == 0` (synthetic / unnamed parameter).
    ///
    /// Returns an empty `Vec` if the method has no `MethodParameters`
    /// attribute (the common case for code not compiled with `-parameters`),
    /// or if the class / method cannot be located. Callers should fall
    /// back to synthesizing `arg0`, `arg1`, … names in that case.
    ///
    /// Default implementation returns an empty `Vec` so that mock
    /// `NativeContext` implementations don't need to plumb through
    /// the class-file attribute store.
    fn method_parameters(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<(String, u16)> {
        Vec::new()
    }

    /// Get annotation default values for annotation type methods.
    /// Returns the default ElementValue for the given method, if any.
    fn method_annotation_default(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Option<AnnotationElementValue>;

    /// WP2.1 — Get the list of checked-exception class internal names from
    /// a method's `Exceptions` attribute (JVMS 4.7.5).
    ///
    /// Returns an empty `Vec` if the method has no `Exceptions` attribute
    /// (no `throws` clause), or if the class / method cannot be located.
    /// Each entry is a binary internal class name like
    /// `"java/io/IOException"`.
    ///
    /// Default implementation returns an empty `Vec` so that mock
    /// `NativeContext` implementations don't need to plumb through the
    /// class-file attribute store.
    fn method_exceptions(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<String> {
        Vec::new()
    }

    // -- JPMS Module support (N3) --

    /// Return the module name (JPMS) of the class with the given ClassId.
    ///
    /// Returns `None` for classes in the unnamed module.
    fn module_name_of_class(&self, class_id: ClassId) -> Option<String>;

    /// Find a classpath resource by name. Returns raw bytes or None if not found.
    ///
    /// Searches bootstrap → extension → application classpaths.
    /// The name should be a forward-slash-separated path (leading `/` is stripped).
    fn find_resource(&self, name: &str) -> Option<Vec<u8>>;

    /// Return the raw bytes of the class file from which `class_id` was
    /// loaded (or last redefined). Reads from `ClassManager::class_bytes_cache`,
    /// which is populated by every `define_class_with_options` call.
    /// Used by `Instrumentation.retransformClasses` to seed the transformer
    /// chain with the true source bytes — `find_resource` would only find
    /// classpath-resident classes, missing dynamically-defined and hidden
    /// classes. Default impl returns `None` so test mocks compile.
    fn class_bytes(&self, _class_id: ClassId) -> Option<Vec<u8>> {
        None
    }

    /// Return a URL string (e.g. `file:/...` or `jar:file:/...!/...`) for
    /// every classpath entry that contains a resource with the given name.
    /// Used by `ClassLoader.getResources` / `getSystemResources`.
    /// Default implementation returns an empty vector so test mocks compile.
    fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        let _ = name;
        Vec::new()
    }

    /// Return the raw bytes of every classpath entry that contains a
    /// resource with the given name. Parallel to [`find_all_resource_urls`]
    /// but returns content rather than URLs — used by Rust-native resource
    /// enumeration paths (e.g. `ServiceLoader` provider discovery in
    /// `native-builtins/src/service_loader.rs`) that bypass the JDK's
    /// `URL.openStream` / `BufferedReader` chain.
    /// Default implementation returns an empty vector so test mocks compile.
    fn find_all_resource_bytes(&self, name: &str) -> Vec<Vec<u8>> {
        let _ = name;
        Vec::new()
    }

    /// Find the filesystem path of the classpath entry that holds a given
    /// class (for `Class.getProtectionDomain().getCodeSource().getLocation()`).
    /// Returns a `file:`-scheme-ready absolute path (directory has trailing
    /// slash, JAR is a plain path).  Returns `None` for classes loaded from
    /// jimage (bootstrap JDK) or if the class cannot be found on any path.
    fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        let _ = class_name;
        None
    }

    /// Return the CodeSource URL attached to a loaded class — what
    /// `Class.getProtectionDomain().getCodeSource().getLocation()` returns.
    /// This is populated at class-load time from the classpath entry that
    /// produced the class, and surfaces real JAR/dir URLs (e.g.
    /// `file:/opt/app.jar`) rather than the synthetic `class:` placeholder.
    /// Returns `None` for synthetic stubs and JDK internals.
    fn class_code_base(&self, class_id: ClassId) -> Option<String> {
        let _ = class_id;
        None
    }

    /// Return the SHA-256 hex digests of every signer certificate block on
    /// the class's CodeSource (one per JAR-signer). Empty vector means an
    /// unsigned source; used by `security_manager` policy enforcement to
    /// match `grant signedBy "..."` entries.
    fn class_code_source_cert_digests(&self, class_id: ClassId) -> Vec<String> {
        let _ = class_id;
        Vec::new()
    }

    /// Return the raw DER-encoded signer certificate blocks (PKCS#7 / CMS
    /// SignedData) attached to the class's CodeSource.  Parallel to
    /// `class_code_source_cert_digests` — one block per JAR-signer — but
    /// exposes the original bytes so the policy engine can parse each
    /// signer's X.509 subject DN for `grant signedBy "CN=..."` matching.
    /// Empty vector means an unsigned source.
    fn class_code_source_certs(&self, class_id: ClassId) -> Vec<Vec<u8>> {
        let _ = class_id;
        Vec::new()
    }

    /// Reverse-lookup: given a `java.lang.Class` mirror object, return the
    /// backing `ClassId` (the class the mirror reflects).  Returns `None`
    /// for primitive-type mirrors and for non-mirror objects.
    ///
    /// Implemented by consulting the VM's `class_mirrors_reverse` map;
    /// avoids encoding the class_id in the mirror's Java-visible fields,
    /// which would clash with real-JDK `java/lang/Class` layout.
    fn class_id_from_mirror(&self, mirror: ObjectRef) -> Option<ClassId> {
        let _ = mirror;
        None
    }

    /// List all class names on the application classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`).
    fn list_application_class_names(&self) -> Vec<String>;

    /// Dynamically extend the application classpath (for URLClassLoader).
    ///
    /// Each string in `paths` is a filesystem path to either a directory or a
    /// JAR/ZIP file. Paths are appended to the application class finder so that
    /// subsequent `ensure_class_initialized` and `find_resource` calls search them.
    fn register_dynamic_classpath(&mut self, paths: &[String]);

    /// Append paths to the BOOTSTRAP class search path so their classes load
    /// with the bootstrap loader (null `Class.getClassLoader()`). Drives
    /// `Instrumentation.appendToBootstrapClassLoaderSearch`. The default
    /// implementation falls back to the application classpath; the VM's
    /// `NativeContext` overrides it to target the bootstrap loader.
    fn register_bootstrap_classpath(&mut self, paths: &[String]) {
        self.register_dynamic_classpath(paths);
    }

    /// Define a new class from raw bytecode (Phase 23.2).
    ///
    /// Parses the class bytes, registers the class with the ClassManager under
    /// the application loader, and returns the ClassId of the newly defined class.
    /// Returns `None` if parsing fails.
    fn define_class_from_bytes(&mut self, name: &str, bytes: &[u8]) -> Option<ClassId>;

    /// NEW-8: Define a hidden class (JEP 371 / JEP 429) from raw bytecode.
    ///
    /// `stored_name` is the mangled name under which the class is
    /// registered in the class store — typically
    /// `"<original>/0x<counter>"` so multiple hidden classes derived
    /// from the same template get distinct names. The class's
    /// `hidden` flag is set atomically with registration so the class
    /// is never visible to `find_class_by_name` / `Class.forName`.
    ///
    /// Returns a typed error string on parse / link failure so the
    /// caller can surface a proper `ClassFormatError` or
    /// `LinkageError` to Java code. The default implementation
    /// delegates to [`define_class_from_bytes`] for backward
    /// compatibility — implementations that want real JEP 371
    /// semantics should override this.
    fn define_hidden_class_from_bytes(
        &mut self,
        stored_name: &str,
        bytes: &[u8],
    ) -> Result<ClassId, String> {
        match self.define_class_from_bytes(stored_name, bytes) {
            Some(cid) => {
                self.set_class_hidden(cid);
                Ok(cid)
            }
            None => Err(format!("failed to define hidden class {stored_name}")),
        }
    }

    /// Define a new class under a specific user-defined classloader namespace.
    ///
    /// `loader_id` is the unique integer ID of the user-defined classloader.
    /// Classes defined with different loader IDs are isolated (same class name
    /// can exist in multiple loader namespaces per JVM spec §5.3).
    fn define_class_with_loader(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
    ) -> Option<ClassId>;

    /// WP2.3 — full-options defineClass. Returns
    /// `Ok(class_id)` on success or `Err(error_message)` describing
    /// the LinkageError / ClassFormatError. Used by all four entry
    /// points (Unsafe.defineClass, jdk.internal.misc.Unsafe.defineClass,
    /// MethodHandles.Lookup.defineClass, ClassLoader.defineClass1/2)
    /// so they all dispatch to the same backend.
    ///
    /// `loader_id == 0` means use the application loader; non-zero
    /// values are user-defined loader namespaces.
    fn define_class_full(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
        opts: DefineClassFull,
    ) -> Result<ClassId, String> {
        // Default: degrade to define_class_with_loader / define_class_from_bytes.
        let result = if loader_id > 0 {
            self.define_class_with_loader(name, bytes, loader_id)
        } else {
            self.define_class_from_bytes(name, bytes)
        };
        match result {
            Some(cid) => {
                if opts.hidden {
                    self.set_class_hidden(cid);
                }
                Ok(cid)
            }
            None => Err(format!("define_class_full failed for {name}")),
        }
    }

    /// WP2.4 — redefine the bytecode of an already-loaded class.
    ///
    /// Used by `Instrumentation.redefineClasses` /
    /// `retransformClasses`. The class must already exist; otherwise
    /// returns `Err("class not loaded")`. On success, the JIT cache
    /// for the old class is invalidated and any new method lookups
    /// resolve through the new bytecode.
    fn redefine_class(&mut self, class_id: ClassId, new_bytes: &[u8]) -> Result<(), String> {
        let _ = (class_id, new_bytes);
        Err("redefine_class not implemented".to_string())
    }

    /// Like [`Self::redefine_class`] but for
    /// `Instrumentation.retransformClasses`: preserves the class's original
    /// cached bytes so each retransformation re-runs the transformer chain
    /// from the ORIGINAL bytes (not the previously-woven ones). Without this,
    /// a second retransform of the same class — e.g. `mockStatic(X)` then
    /// `mock(X)` — double-instruments it. The default delegates to
    /// `redefine_class` (correct for impls that don't cache original bytes).
    fn retransform_class(&mut self, class_id: ClassId, new_bytes: &[u8]) -> Result<(), String> {
        self.redefine_class(class_id, new_bytes)
    }

    /// WP2.4 — list all loaded classes.
    fn list_loaded_class_ids(&self) -> Vec<ClassId> {
        Vec::new()
    }

    /// WP2.4 — list all classes whose initiating loader was the
    /// application loader (or, if `loader_id != 0`, the user-defined
    /// loader with that id).
    fn list_initiated_class_ids(&self, _loader_id: u32) -> Vec<ClassId> {
        Vec::new()
    }

    /// Look up a class by name within a specific user-defined loader's namespace.
    /// Falls back to the standard delegation chain if not found.
    fn class_id_by_name_and_loader(&self, name: &str, loader_id: u32) -> Option<ClassId>;

    /// Exact `(loader, name)` lookup with **no** delegation/global fallback —
    /// only a class the loader with `loader_id` has itself defined. Used by
    /// loader-faithful `findLoadedClass` so it never returns another loader's
    /// class. Default impl falls back to the (fallback-prone)
    /// [`Self::class_id_by_name_and_loader`] for contexts that do not override
    /// it (e.g. test mocks).
    fn class_id_defined_by_loader_exact(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        self.class_id_by_name_and_loader(name, loader_id)
    }

    /// Allocate a unique classloader ID for a new user-defined classloader instance.
    fn allocate_loader_id(&mut self) -> u32;

    // -- JPMS module queries (Phase B) --

    /// Check if module `reader` reads module `provider`.
    fn reads_module(&self, reader: &str, provider: &str) -> bool {
        // Default: all modules can read each other (classpath-only mode).
        let _ = (reader, provider);
        true
    }

    /// Check if `module_name` exports `pkg` unconditionally (to all modules).
    ///
    /// AUDIT 2026-05-19: this method has **no default** and is REQUIRED. A
    /// fail-open default (`true`) silently grants arbitrary cross-module
    /// access for any implementor that forgets to override it. Forcing every
    /// `NativeContext` impl to provide a body makes the security decision
    /// explicit. Classpath-only / mock contexts should return `true`
    /// deliberately; a JPMS-enabled VM must perform a real readability check.
    fn is_package_exported_unqualified(&self, module_name: &str, pkg: &str) -> bool;

    /// Check if `module_name` exports `pkg` to `to_module`.
    ///
    /// AUDIT 2026-05-19: REQUIRED, no default — see
    /// `is_package_exported_unqualified` for the fail-open rationale.
    fn is_package_exported_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool;

    /// Check if `module_name` opens `pkg` unconditionally.
    ///
    /// AUDIT 2026-05-19: REQUIRED, no default — see
    /// `is_package_exported_unqualified` for the fail-open rationale.
    fn is_package_open_unqualified(&self, module_name: &str, pkg: &str) -> bool;

    /// Check if `module_name` opens `pkg` to `to_module`.
    ///
    /// AUDIT 2026-05-19: REQUIRED, no default — see
    /// `is_package_exported_unqualified` for the fail-open rationale.
    fn is_package_open_to(&self, module_name: &str, pkg: &str, to_module: &str) -> bool;

    /// Add a dynamic read edge: `reader` reads `provider`.
    fn module_add_reads(&mut self, reader: &str, provider: &str) {
        let _ = (reader, provider);
    }

    /// Add a dynamic export: `module_name` exports `pkg` to `target`.
    fn module_add_exports(&mut self, module_name: &str, pkg: &str, target: &str) {
        let _ = (module_name, pkg, target);
    }

    /// Add a dynamic open: `module_name` opens `pkg` to `target`.
    fn module_add_opens(&mut self, module_name: &str, pkg: &str, target: &str) {
        let _ = (module_name, pkg, target);
    }

    /// Return all packages owned by `module_name`.
    fn module_packages(&self, module_name: &str) -> Vec<String> {
        let _ = module_name;
        vec![]
    }

    /// Return the `uses` service-type binary names (slash format) declared by
    /// `module_name`'s `module-info.class` `uses` directives. Empty for the
    /// unnamed module, an unregistered module, or a module whose descriptor
    /// declares no `uses`.
    fn module_uses(&self, module_name: &str) -> Vec<String> {
        let _ = module_name;
        vec![]
    }

    /// True if `module_name`'s `module-info.class` declared `open module ...`
    /// (the real `ACC_MODULE_OPEN` flag). `false` for the unnamed module, an
    /// unregistered module, or a module that isn't open.
    fn module_is_open(&self, module_name: &str) -> bool {
        let _ = module_name;
        false
    }

    /// Return all registered module names.
    fn all_module_names(&self) -> Vec<String> {
        vec![]
    }

    /// Find which module owns a given package (slash format).
    fn module_for_package(&self, pkg: &str) -> Option<String> {
        let _ = pkg;
        None
    }

    /// Mark a class as hidden (JEP 371). Hidden classes are not discoverable via
    /// `Class.forName` or `ClassLoader.findLoadedClass`.
    fn set_class_hidden(&mut self, class_id: ClassId) {
        let _ = class_id;
    }

    /// Query whether a class is hidden (JEP 371). Used by the
    /// `Class.isHidden()` native. The default returns `false` for every
    /// class so implementations that do not support hidden classes
    /// continue to work.
    fn is_class_hidden(&self, class_id: ClassId) -> bool {
        let _ = class_id;
        false
    }

    /// Copy nest-host and nest-members information from `source_class` to
    /// `target_class`. Used by `defineHiddenClass` when the `NESTMATE`
    /// class option is specified: the hidden class joins the lookup
    /// class's nest rather than being a standalone nest of its own.
    /// The default is a no-op for implementations that do not track
    /// nest membership.
    fn copy_nest_info(&mut self, source_class: ClassId, target_class: ClassId) {
        let _ = (source_class, target_class);
    }

    /// Force a class to complete its `<clinit>` immediately. Used by
    /// `defineHiddenClass` when the `initialize` flag is `true`, and by
    /// `Class.forName`/`Constructor.newInstance`/`Lookup.ensureInitialized`.
    /// The default is a no-op — callers that care about deterministic init
    /// must override this in their NativeContext impl.
    ///
    /// HIB-CV-26 fix (2026-07-16): the error type is `MethodCallFailed`
    /// (not a flattened `String`) so a `<clinit>` failure keeps its
    /// two-layer identity all the way to the caller: a genuine Java
    /// exception from a static initializer comes back as
    /// `MethodCallFailed::ExceptionThrown` (already correctly wrapped as a
    /// catchable `ExceptionInInitializerError`/`NoClassDefFoundError` by
    /// `ensure_class_initialized_shared` per JVMS §5.5) and only a true
    /// VM-level bug comes back as `MethodCallFailed::InternalError`.
    /// Collapsing both into a `String` here previously forced every call
    /// site to treat ordinary `<clinit>` exceptions as unrecoverable
    /// internal errors, aborting the VM instead of letting Java code catch
    /// them.
    fn initialize_class(&mut self, class_id: ClassId) -> Result<(), MethodCallFailed> {
        let _ = class_id;
        Ok(())
    }

    /// Return all JPMS `provides` implementation class names for a given service
    /// interface (binary class name, e.g. `"com/example/MyService"`).
    /// Walks all registered module descriptors' `provides` entries.
    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        let _ = service_class;
        vec![]
    }

    // -- JPMS deep reflection access (Phase B) --

    /// Check whether `accessor_class_id` has deep (reflective) access to
    /// `target_class_id` via JPMS `opens` directives.
    ///
    /// Returns `Ok(())` if allowed (same module, unnamed module, target module
    /// opens the package, or a dynamic `addOpens` edge exists).
    /// Returns `Err(message)` if the access is denied.
    ///
    /// Called by reflection natives (`Method.invoke`, `Field.get/set`,
    /// `Constructor.newInstance`) when `setAccessible(true)` is used on a
    /// member in a different module.
    ///
    /// AUDIT 2026-05-19: this method has **no default** and is REQUIRED. The
    /// previous `Ok(())` default silently allowed arbitrary cross-module
    /// `setAccessible` for any implementor that forgot to override it.
    /// Forcing every `NativeContext` impl to provide a body makes the
    /// security decision explicit — classpath-only / mock contexts may
    /// return `Ok(())` deliberately; a JPMS-enabled VM must perform a real
    /// module-readability + opens check.
    fn check_deep_reflection_access(
        &self,
        accessor_class_id: ClassId,
        target_class_id: ClassId,
    ) -> Result<(), String>;

    // -- T13 java/lang/Class reflection metadata --

    /// Get the class file version (major number) for a class.
    fn class_file_version(&self, _class_id: ClassId) -> u16 {
        65 // Default: Java 21
    }

    /// Get the inner classes of a class.
    /// Returns vec of (inner_class_name, outer_class_name, inner_name, access_flags).
    fn inner_classes(&self, _class_id: ClassId) -> Vec<(String, String, String, u16)> {
        Vec::new()
    }

    /// Get the enclosing method info for a class.
    /// Returns (enclosing_class, method_name, method_descriptor) or None.
    fn enclosing_method(&self, _class_id: ClassId) -> Option<(String, String, String)> {
        None
    }

    /// Get the declaring class of this class (from InnerClasses attribute).
    /// Returns the class ID of the outer class, or None if not an inner class.
    fn declaring_class(&self, _class_id: ClassId) -> Option<ClassId> {
        None
    }

    /// Get the raw annotation bytes for a class.
    /// Returns the bytes of the RuntimeVisibleAnnotations attribute, or empty.
    fn raw_annotations(&self, _class_id: ClassId) -> Vec<u8> {
        Vec::new()
    }

    /// Get the raw type annotation bytes for a class.
    fn raw_type_annotations(&self, _class_id: ClassId) -> Vec<u8> {
        Vec::new()
    }

    /// Get the runtime-visible TYPE_USE annotations targeting one of this
    /// class's declared supertypes (JVMS 4.7.20 `target_type` 0x10,
    /// CLASS_EXTENDS). `supertype_index` is the JVMS-defined index: `0xFFFF`
    /// (65535) selects the superclass, `0..n` selects the n-th entry of
    /// `getInterfaces()`.
    ///
    /// Returns a [`TypeArgAnnotations`] tree: `.anns` holds annotations with
    /// an empty `type_path` (directly on the supertype itself, e.g.
    /// `implements @Foo Bar`); `.children[i]` holds the subtree for the
    /// supertype's i-th type argument (recursively, for arbitrarily nested
    /// generics, e.g. `implements ValueExtractor<ArgumentValue<@ExtractedValue
    /// ?>>`). Backs `Class.getAnnotatedSuperclass()` /
    /// `Class.getAnnotatedInterfaces()` and their
    /// `getAnnotatedActualTypeArguments()` chains. Default impl returns an
    /// empty tree so mock `NativeContext` implementations don't need to plumb
    /// the attribute store.
    fn class_extends_type_annotations(
        &self,
        _class_id: ClassId,
        _supertype_index: u16,
    ) -> TypeArgAnnotations {
        TypeArgAnnotations::default()
    }

    /// Get the nest host class name for a class.
    /// Returns None if the class is its own nest host.
    fn nest_host_name(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    /// Get the nest member class names for a class.
    fn nest_member_names(&self, _class_id: ClassId) -> Vec<String> {
        Vec::new()
    }
}

pub trait NativeInvokeAccess: NativeClassAccess {
    /// Capability boundary: Java method invocation and linkage.


    /// Invoke a method by class name, method name, descriptor, and arguments.
    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult;

    /// Invoke a method on an ALREADY-RESOLVED declaring class, bypassing
    /// name-based class resolution entirely.
    ///
    /// `Method.invoke()` (reflection) on a static method already has an
    /// unambiguous declaring `ClassId` in hand (from the `Method` object's
    /// own `clazz` mirror) — it must not re-resolve the class by NAME, which
    /// goes through the loader-blind global lookup (`load_class`/
    /// `get_loaded_class_id`). That lookup deliberately returns "not found"
    /// (not a guess) whenever 2+ *different* user-defined loaders each
    /// register their own distinct class under the identical simple name —
    /// an intentional, documented anti-ambiguity guard (see
    /// `ClassManager::get_loaded_class_id`), but it means ANY name-based
    /// re-resolution after the fact is unsound the moment a second same-named
    /// class from a different loader exists anywhere in the process — a
    /// completely ordinary pattern for repeatedly-invoked test/codegen
    /// harnesses that mint a fresh ClassLoader + identically-named generated
    /// class each time (e.g. Spring's `TestCompiler`/
    /// `@CompileWithForkedClassLoader`, which produced the exact
    /// `GroupsMetadataValueDelegateTests` "class file error: class not
    /// found" VM abort this fixes). Default implementation falls back to the
    /// name-based [`Self::invoke`] for callers/mocks that have no ClassId
    /// fast path; the real VM overrides this to skip re-resolution.
    fn invoke_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let _ = class_id;
        self.invoke(class_name, method_name, descriptor, args)
    }

    // -- LinkResolver wiring for Java-side reflection natives -----------
    //
    // Round 9 audit fix (HIGH #7): Java-side reflection
    // (`Class.getDeclaredMethod`, `Class.getMethod`,
    // `Class.getDeclaredField`, `Class.getField`) re-walks the entire
    // metadata + hierarchy on every probe. Spring / Hibernate /
    // ByteBuddy run thousands of identical `(class_id, name, desc)`
    // probes during cold start. Round-8 wired JNI's `GetMethodID` /
    // `GetFieldID` into the per-VM `LinkResolver`; these helpers
    // extend the same cache to the Java-side natives.
    //
    // The trait methods are intentionally narrow: probe + insert.
    // The native code keeps owning the actual hierarchy walk (it has
    // synthetic-method awareness, ByteBuddy reentrancy guards, and
    // shim short-circuits that the LinkResolver layer doesn't model).
    // The default implementations are no-ops so the existing
    // `MockNativeContext` and other test contexts compile unchanged
    // and behave as if the cache is permanently empty (correct but
    // slow — no caching).
    //
    // The `index_or_slot` payload is overloaded:
    //   * for **method** lookups it is the position within the
    //     declaring class's `methods` vec (matches the JNI shape);
    //   * for **field** lookups it is the absolute heap slot index
    //     (matches `LinkResolver`'s `absolute_index`).
    // The boolean is `is_static` (only meaningful for fields; ignored
    // for methods).

    /// Probe the per-VM `LinkResolver` for a previously-resolved
    /// reflective method `(class_id, name, descriptor)` triple. Returns
    /// `Some((declaring_class_id, method_index))` on cache hit,
    /// `None` on cold miss (caller should walk the hierarchy and call
    /// [`Self::link_resolver_insert_method`] with the result).
    fn link_resolver_get_method(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
    ) -> Option<(ClassId, u32)> {
        None
    }

    /// Populate the LinkResolver method cache. `index` is the position
    /// of the resolved method inside `declaring`'s `methods` vec.
    fn link_resolver_insert_method(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
        _declaring: ClassId,
        _index: u32,
    ) {
    }

    /// Probe the LinkResolver field cache. Returns
    /// `Some((declaring_class_id, absolute_index, is_static))` on hit.
    fn link_resolver_get_field(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
    ) -> Option<(ClassId, u32, bool)> {
        None
    }

    /// Populate the LinkResolver field cache.
    fn link_resolver_insert_field(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
        _declaring: ClassId,
        _absolute_index: u32,
        _is_static: bool,
    ) {
    }

    /// Invoke a virtual method on a receiver, with lambda-proxy awareness.
    ///
    /// If the receiver's ClassId is registered as a lambda proxy, this performs
    /// lambda dispatch (reading captures, dispatching by MethodHandle kind).
    /// Otherwise, it resolves the receiver's class name and performs normal
    /// method invocation.
    ///
    /// `args` does NOT include the receiver — the implementation prepends it.
    fn invoke_virtual(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult;

    /// Invoke a virtual method whose declaring class is known by the caller.
    ///
    /// Most callers should use [`Self::invoke_virtual`]. Method-handle dispatch
    /// has one extra piece of information, though: the owner class stored in the
    /// handle. VM contexts can use that as a recovery target when receiver-based
    /// dispatch collapses to bare `java/lang/Object` for a non-Object member.
    /// Mock/test contexts keep the simple virtual behavior by default.
    fn invoke_virtual_declared(
        &mut self,
        _declared_class: &str,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual(receiver, method_name, descriptor, args)
    }

    /// Invoke a virtual method on `receiver`, skipping the native-override
    /// check entirely so a registered Rust native for this exact
    /// (class, method, descriptor) triple is NOT re-entered — dispatch goes
    /// straight to the receiver's real JDK bytecode.
    ///
    /// This exists for natives that must distinguish a genuinely-real object
    /// from a same-named synthetic one by *instance* state rather than by
    /// class name (registration is static/global and can't make that call).
    /// The canonical example is `ThreadPoolExecutor.execute`/`submit`/
    /// `shutdown`: CratonVM's synthetic `Executors.newSingleThreadExecutor()`
    /// et al. stamp their 2-field placeholder with the REAL `ThreadPoolExecutor`
    /// class name, so a real executor (e.g. the internal async worker pool in
    /// `native-builtins`) and a synthetic one are indistinguishable by class
    /// name alone. The native override checks `executor_has_real_workers`
    /// per-instance and calls this method for a real receiver instead of
    /// recursing back into itself via [`Self::invoke_virtual`] (which would
    /// hit the same native registration again and loop forever).
    ///
    /// Default implementation falls back to [`Self::invoke_virtual`] — safe
    /// for any context that has no such real/synthetic ambiguity to resolve
    /// (mocks, tests, other native contexts).
    fn invoke_virtual_bytecode_only(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_virtual(receiver, method_name, descriptor, args)
    }

    /// Invoke a method with invokespecial semantics — *exactly* the resolved
    /// method on `class_name`, with no virtual dispatch and no interface
    /// retarget to the receiver's concrete class.
    ///
    /// This is the dispatch contract behind `MethodHandles.Lookup.findSpecial`
    /// and the JLS `super.m()` call sequence. Use cases:
    ///
    /// - private-to-private calls within the same class
    /// - default-method super calls: `Lookup.findSpecial(I.class, "m", mt, C.class)`
    ///   on an `I.super.m()` pattern must invoke `I.m()`, NOT C's overriding
    ///   `m()` — even though the receiver is a concrete `C`.
    ///
    /// `args[0]` MUST be the receiver. Parameters follow.
    ///
    /// The default implementation falls back to [`Self::invoke`] for
    /// implementations that do not need super-call semantics. The `Vm`
    /// override bypasses the iface/abstract retarget that `invoke_on_class_shared`
    /// applies, so it is correct for super-call invocation.
    fn invoke_special(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke(class_name, method_name, descriptor, args)
    }

    /// Loader-aware form of [`Self::invoke_special`].
    ///
    /// Reflective invocation already resolved the declaring class to
    /// `class_id`; preserving that identity avoids accidentally selecting a
    /// same-named class from another loader namespace. Mocks may ignore the id.
    fn invoke_special_by_class_id(
        &mut self,
        class_id: ClassId,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        let _ = class_id;
        self.invoke_special(class_name, method_name, descriptor, args)
    }

    /// Like [`Self::invoke_special`] but for a native that IS ITSELF the
    /// native registered for `(class_name, method_name, descriptor)` and
    /// must run that class's own real bytecode body directly.
    ///
    /// [`Self::invoke_special`] re-finds ANY registered native FIRST (see its
    /// own contract) -- calling it from inside that same native re-enters it
    /// (unbounded Rust-stack recursion). This variant skips that check
    /// entirely and resolves straight to `class_name`'s own bytecode, still
    /// with true invokespecial semantics: static binding on `class_name`'s
    /// hierarchy, never virtual dispatch to a receiver's overriding
    /// subclass. That distinction is the whole point -- the bug this exists
    /// to fix was a native registered on `ThreadPoolExecutor.shutdown()`,
    /// reached via `ScheduledThreadPoolExecutor.shutdown()`'s
    /// `super.shutdown()`, which used `invoke_virtual_bytecode_only` (dynamic
    /// receiver class) and so re-dispatched straight back into the STPE
    /// override that called it -- `StackOverflowError` from infinite
    /// self-recursion.
    ///
    /// `args[0]` MUST be the receiver, same contract as [`Self::invoke_special`].
    ///
    /// Default implementation falls back to [`Self::invoke_special`] -- safe
    /// for any context with no such native-reentrancy hazard (mocks, tests).
    fn invoke_special_bytecode_only(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        self.invoke_special(class_name, method_name, descriptor, args)
    }
}

pub trait NativeHeapAccess: NativeInvokeAccess {
    /// Capability boundary: Allocation, roots, object fields, arrays, and strings.


    /// Create a new object of the given class.
    /// Returns an ObjectRef wrapped as `Value::Object(Some(ref))`.
    fn new_object(&mut self, class_name: &str) -> MethodCallResult;

    /// Allocate an object of `class_name` and run its `<init>` constructor
    /// (`init_desc`, with `init_args` as the post-`this` arguments), returning
    /// the freshly-constructed object.
    ///
    /// **GC-safety:** unlike `new_object` followed by a separate `invoke(...,
    /// "<init>", ...)`, this keeps the new object pinned as a GC root across the
    /// `<init>` call and returns the *forwarded* reference. Under the moving
    /// collector, a heavy constructor (e.g. BouncyCastle provider setup that
    /// allocates enough to trigger a collection) relocates the object; a native
    /// that held the pre-`<init>` raw `ObjectRef` would otherwise return a stale
    /// pointer that resolves to a reused `java.lang.Object`. Reflective
    /// construction paths (`Constructor.newInstance`, `Class.newInstance`,
    /// `Provider$Service.newInstance`) must use this instead of `new_object` +
    /// `invoke`.
    ///
    /// The default implementation is the non-GC-safe `new_object` + `invoke`
    /// pair (sufficient for test mocks with no moving GC); the real VM overrides
    /// it with the pinned variant.
    fn new_object_initialized(
        &mut self,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        let obj_val = self.new_object(class_name)?;
        if let Some(Value::Object(Some(obj))) = obj_val {
            let mut full = Vec::with_capacity(init_args.len() + 1);
            full.push(Value::Object(Some(obj)));
            full.extend_from_slice(init_args);
            self.invoke(class_name, "<init>", init_desc, &full)?;
        }
        Ok(obj_val)
    }

    /// Allocate an object of the EXACT `class_id` and run its `<init>`
    /// (`init_desc`, with `init_args` as the post-`this` arguments),
    /// returning the freshly-constructed (GC-forwarded) object.
    ///
    /// Unlike [`Self::new_object_initialized`], which resolves the class by
    /// *name* (and therefore collapses to whichever loader defined that name
    /// first — the global Application/bootstrap copy), this honours per-loader
    /// class identity per JVMS §5.3: `(defining loader, name)`. Reflective
    /// construction (`Constructor.newInstance`, `Class.newInstance`) must use
    /// this when it already holds the declaring class's mirror, so a class a
    /// custom loader defined (load-time weaving, webapp/OSGi isolation) is
    /// instantiated as ITSELF rather than as a same-named class some other
    /// loader happens to have defined first.
    ///
    /// The default implementation resolves the class id back to a name and
    /// delegates to the name-based path — sufficient for test mocks and any
    /// context with a single (global) loader namespace.
    fn new_object_initialized_with_class_id(
        &mut self,
        class_id: ClassId,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        match self.class_name_of_id(class_id) {
            Some(name) => self.new_object_initialized(&name, init_desc, init_args),
            None => Ok(Some(Value::Object(None))),
        }
    }

    /// Pin a heap object as a GC root and return an opaque handle index.
    ///
    /// Native code that holds an `ObjectRef` across a re-entrant call
    /// (`invoke` / `new_object_initialized` / any operation that can allocate)
    /// MUST pin it first: under the moving collector the object may relocate,
    /// leaving the raw `ObjectRef` stale (it then resolves to a reused, usually
    /// `java.lang.Object`, slot). After the call, read the forwarded reference
    /// back with [`read_native_pin`] before using it. Unpin the whole batch with
    /// [`unpin_native_roots`] passing the index returned by the *first* pin.
    ///
    /// Forgot to pin somewhere? Run with `CRATONVM_DBG_STALE_OBJREF=1` (the
    /// `Generational` GC backend only) to turn a stale read into an immediate,
    /// deterministic panic instead of silent corruption — see
    /// `gc/src/stale_objref_debug.rs` and
    /// docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.
    ///
    /// Default impl is a no-op (handle 0) for test mocks with no moving GC.
    fn pin_native_root(&mut self, _obj: ObjectRef) -> usize {
        0
    }

    /// Read back a pinned object by handle, returning its current (post-GC,
    /// possibly forwarded) reference. `fallback` is returned when the handle is
    /// out of range. Default impl returns `fallback` (mocks don't relocate).
    fn read_native_pin(&self, _handle: usize, fallback: ObjectRef) -> ObjectRef {
        fallback
    }

    /// Release all native pin roots from `base` (a handle returned by
    /// [`pin_native_root`]) onward. Default impl is a no-op.
    fn unpin_native_roots(&mut self, _base: usize) {}

    // ---- rooted handle scope (arch/handles) ----
    //
    // `pin_native_root`/`read_native_pin`/`unpin_native_roots` above fix
    // staleness but not the discipline: a native method still holds the
    // SAME `ObjectRef` type before and after pinning, so reading the
    // pre-pin local by mistake type-checks fine and silently reintroduces
    // the bug. The handle-scope quartet below is `cratonvm_types::handle`'s
    // rooted-handle design (`RootedHandle`/`HandleStorage`) adapted to this
    // trait-object boundary: a handle is an opaque `u32` slot, not an
    // `ObjectRef`, so there is no raw pointer left to accidentally read.
    //
    // **Discipline — READ BEFORE holding any `ObjectRef` across a call that
    // can allocate:** any native code that holds a reference across
    // `invoke`/`new_object_initialized`/`new_array`/anything that can run
    // Java (and therefore can trigger a GC) MUST root it with
    // [`handle_root`] first and read it back with [`handle_get`] afterward
    // — never keep using the pre-call local. The usual shape:
    //
    // ```ignore
    // let mut scope = NativeHandleScope::new(ctx);
    // let this_h = scope.root(this);
    // let arr = scope.new_array(ArrayElementType::Char, len); // may GC-move `this`
    // let this = scope.get(&this_h);                          // current address
    // ```
    //
    // `NativeHandleScope` closes itself on normal, early-return, and unwind
    // paths. Scopes nest and each guard releases only its own handles. See
    // `docs/feature-designs/native-handle-discipline.md` for the full
    // design and `native_builtins::lang_string` for worked examples
    // (`native_string_init_abstract_string_builder`,
    // `native_sb_init_default`, `native_sb_init_string`,
    // `native_sb_init_charsequence`, `native_sb_init_capacity`).
    //
    // Default impls below mirror the no-op/pass-through convention already
    // used by `pin_native_root` & co. for mock/test contexts with no moving
    // GC: pushing/popping a scope is a no-op, and `handle_root` delegates to
    // the already-default-implemented `pin_native_root` (truncated to
    // `u32`) as the closest existing stand-in. `handle_get`'s default
    // returns `None` rather than trying to read back through that
    // delegation — `pin_native_root`'s own default doesn't retain anything
    // to read, so there is nothing genuine to hand back. The VM's
    // `NativeContextImpl` overrides all four with a real per-thread slot
    // table (`vm/src/vm/vm_exec.rs`, "handle scope support").

    /// Push a new handle scope. Every [`handle_root`] call until the
    /// matching [`handle_scope_pop`] is released together when that pop
    /// runs. Default impl is a no-op.
    fn handle_scope_push(&mut self) {}

    /// Pop the current handle scope, releasing every handle rooted since the
    /// matching [`handle_scope_push`]. Default impl is a no-op.
    fn handle_scope_pop(&mut self) {}

    /// Root `r` in the current handle scope and return its slot id. Reading
    /// through the slot (via [`handle_get`]) always returns `r`'s current,
    /// possibly-GC-forwarded address — a handle can never go stale while its
    /// scope is open, unlike a raw `ObjectRef` copy.
    ///
    /// Default impl delegates to [`pin_native_root`] (see the block doc
    /// above for why); real GC-safety comes from the VM's override.
    fn handle_root(&mut self, r: ObjectRef) -> u32 {
        self.pin_native_root(r) as u32
    }

    /// Read back the current reference for `slot` (from [`handle_root`]), or
    /// `None` if `slot` is out of range or its scope already popped. Default
    /// impl returns `None`.
    fn handle_get(&self, _slot: u32) -> Option<ObjectRef> {
        None
    }

    /// Create a *persistent* global GC root for `obj`, returning an opaque handle.
    ///
    /// Unlike [`pin_native_root`] (which is per-thread and unwound when the
    /// current native call returns), a global root survives across calls and
    /// across threads, and is remapped by the moving collector. It lives until
    /// [`remove_global_root`] is called. Use it to hold an `ObjectRef` that a
    /// *different* thread will consume later — e.g. an asynchronous-I/O
    /// `CompletionHandler` / attachment / target `ByteBuffer` parked while a
    /// worker thread performs a blocking read, then delivered on a dispatcher
    /// thread. Backed by the same table as JNI `NewGlobalRef`.
    ///
    /// Default impl returns `0` (no-op) for mock contexts with no moving GC.
    fn add_global_root(&mut self, _obj: ObjectRef) -> usize {
        0
    }

    /// Resolve a global root handle (from [`add_global_root`]) to its current
    /// (post-GC, possibly relocated) reference. Returns `None` for handle `0` or
    /// an unknown handle. Default impl returns `None`.
    fn resolve_global_root(&self, _handle: usize) -> Option<ObjectRef> {
        None
    }

    /// Release a global root created by [`add_global_root`]. Returns `true` if the
    /// handle was found and removed. Default impl returns `false`.
    fn remove_global_root(&mut self, _handle: usize) -> bool {
        false
    }

    /// Look up a bounded, GC-rooted exact-HashMap cache using a Java String
    /// object directly. VM implementations can compare compact payloads without
    /// allocating a host String; the default leaves mock contexts unchanged.
    fn hashmap_string_node_cache_get_object(
        &mut self,
        _map: ObjectRef,
        _key: ObjectRef,
    ) -> Option<Value> {
        None
    }

    /// Look up a bounded, GC-rooted exact-HashMap String node cache. Native
    /// implementations may use this to avoid rediscovering immutable keys;
    /// the default keeps lightweight test contexts independent of VM layout.
    fn hashmap_string_node_cache_get(&mut self, _map: ObjectRef, _key: &str) -> Option<Value> {
        None
    }

    /// Publish an exact HashMap node for [`Self::hashmap_string_node_cache_get`].
    /// Implementations must preserve normal map mutation semantics.
    fn hashmap_string_node_cache_put(&mut self, _map: ObjectRef, _key: &str, _node: ObjectRef) {}

    /// Get the identity hash code of an ObjectRef.
    fn identity_hash_code(&self, obj: ObjectRef) -> i32;

    /// B-J: register a `java.lang.invoke.VarHandle` as a permanent GC root.
    /// VarHandles live in `static final` fields and are used for lock-free CAS;
    /// without an explicit root a moving GC reclaimed them and left their static
    /// holder slots stale (all-zero header → misdispatch). Default no-op for
    /// non-VM contexts (tests/mocks); the interpreter overrides it to insert
    /// into `SharedVm::var_handle_roots`.
    fn register_var_handle_root(&mut self, _vh: ObjectRef) {}

    /// Read back the CURRENT address of a persistent native root previously
    /// registered with [`register_var_handle_root`], keyed by the value
    /// [`identity_hash_code`] returned for it at registration time.
    ///
    /// Rationale: `register_var_handle_root` keeps the object alive and the
    /// GC remaps the registry entry after a move — but it cannot rewrite raw
    /// `ObjectRef` copies cached in native `static`s (`ASYNC_POOL`,
    /// `SYSTEM_CL`, `SECURITY_MANAGER`). Such long-lived-native-singleton
    /// caches must store the identity key alongside the raw ref and re-read
    /// through this method at every use; using only the cached raw ref is a
    /// use-after-move once a moving young GC or a promotion relocates the
    /// object. Default `None` for non-VM contexts (callers fall back to the
    /// cached ref, matching the mock heaps that never move objects).
    fn read_var_handle_root(&self, _identity_key: i32) -> Option<ObjectRef> {
        None
    }

    // -- Heap access methods (for native method implementations) --
    //
    // # Security contract (M4a — unvalidated slot indices)
    //
    // Every accessor in this section (and the volatile / CAS / static
    // variants further down) takes a bare `usize` slot `index` with no type
    // carried bound and returns an infallible `Value`. The `index` is NOT
    // validated by the trait: it is the *caller's* obligation to pass a slot
    // that is in range for `obj`'s class layout (for fields) or for the
    // array's length (for elements). Callers typically derive the index from
    // trusted reflection metadata (`FieldMetadata::slot_index`,
    // `resolve_field_index`, `array_length`) and must NOT pass an index
    // sourced from untrusted Java/native input without first bounds-checking
    // it against `array_length` / the resolved field count.
    //
    // Implementations are the enforcement point: an implementation MUST
    // bounds-check `index` and MUST NOT read or write memory outside the
    // object's field block / the array's element range. On an out-of-range
    // index an implementation must fail safe (e.g. return
    // `Value::Object(None)` / a default for reads, no-op for writes, or
    // raise a VM error) — it must NEVER perform an out-of-bounds heap access.
    // The production `NativeContextImpl` in the `vm` crate performs this
    // validation; mock/test impls that elide it must only ever be fed
    // trusted indices.

    /// Read an object field by slot index.
    ///
    /// `index` is an absolute heap field slot (see [`FieldMetadata::slot_index`]).
    /// The caller must ensure it is in range for `obj`'s class; the
    /// implementation MUST bounds-check and MUST NOT read out of range (M4a).
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an object field by slot index.
    ///
    /// `index` is an absolute heap field slot (see [`FieldMetadata::slot_index`]).
    /// The caller must ensure it is in range for `obj`'s class; the
    /// implementation MUST bounds-check and MUST NOT write out of range (M4a).
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value);

    /// Read an object field by name. Resolves the field name to a slot index
    /// by searching the object's class hierarchy. Returns `Value::Object(None)`
    /// if the field is not found.
    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value;

    /// Write an object field by name. Resolves the field name to a slot index
    /// by searching the object's class hierarchy. No-op if the field is not found.
    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value);

    /// Resolve a field name to its slot index for a given class.
    /// Returns `None` if the field is not found in the class hierarchy.
    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize>;

    /// Resolve a field name to its slot index by `ClassId` directly --
    /// no class-name round-trip. Returns `None` if the field is not found
    /// in the class hierarchy.
    ///
    /// Prefer this over `resolve_field_index` whenever the caller already
    /// holds the object (and so its exact `ClassId` via
    /// `class_id_of_object`): `resolve_field_index`'s name-based lookup
    /// re-resolves the class GLOBALLY by name, which returns `None`
    /// whenever 2+ distinct loaders each define their own class under the
    /// same simple name (a legitimate "ambiguous" answer for a bare name,
    /// but a needless loss when the caller already holds the exact,
    /// unambiguous `ClassId` -- e.g. a native shim reading a field off a
    /// third-party object whose class gets redefined under a fresh loader
    /// each time, such as ByteBuddy classes under
    /// `@CompileWithForkedClassLoader`).
    fn resolve_field_index_by_class_id(&self, class_id: ClassId, field_name: &str)
        -> Option<usize>;

    /// Allocate a primitive array (element_type: Boolean=4..Long=11).
    fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef;

    /// Allocate a reference array for the given component class.
    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef;

    /// Fallible reference-array allocator: returns `None` when the request
    /// cannot be satisfied (the backing array is too large for the heap) so the
    /// caller can throw a *catchable* `OutOfMemoryError` instead of the VM
    /// hard-aborting in the infallible [`new_ref_array`](Self::new_ref_array).
    /// The default delegates to `new_ref_array` so non-VM contexts (mocks)
    /// compile unchanged; the VM impl overrides it with the no-GC
    /// young→old-gen spill path that reports OOM rather than aborting.
    fn try_new_ref_array(&mut self, class_id: ClassId, length: usize) -> Option<ObjectRef> {
        Some(self.new_ref_array(class_id, length))
    }

    /// Fallible primitive-array allocator — the `new_array` counterpart of
    /// [`try_new_ref_array`](Self::try_new_ref_array). Returns `None` when the
    /// request is too large for the heap so a native (e.g. `StringBuilder(int)`)
    /// can raise a catchable `OutOfMemoryError` instead of aborting. Default
    /// delegates to the infallible `new_array`.
    fn try_new_array(
        &mut self,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        Some(self.new_array(element_type, length))
    }

    /// Component (element) class id of an array class `class_id`, or `None` if
    /// it is not an array class. Lets natives allocate a typed array matching a
    /// given array `Class` — e.g. `Arrays.copyOf(T[], n, a.getClass())` /
    /// `Collection.toArray(T[])`, where the result must be `a.getClass()` (e.g.
    /// `String[][]`), not a bare `Object[]`. The result is the `component_class_id`
    /// the caller feeds back into `new_ref_array`. Default `None` (callers fall
    /// back to `Object[]`).
    fn array_component_class_id(&self, _class_id: ClassId) -> Option<ClassId> {
        None
    }

    /// Get the length of an array object.
    fn array_length(&self, obj: ObjectRef) -> usize;

    /// Whether `obj` is an array object (as opposed to an ordinary instance).
    ///
    /// This is a heap object-kind check — it does NOT go through
    /// `class_id_of_object`/`class_name_of_id`, which for a heap-allocated
    /// reference array report the *component* class (arrays store their element
    /// class id + an array kind flag rather than a distinct `[L…;` class id), so
    /// a class-name prefix test cannot reliably detect arrays. Default `false`
    /// (mock contexts without a heap); the VM overrides it.
    fn object_is_array(&self, _obj: ObjectRef) -> bool {
        false
    }

    /// Read an array element by index.
    ///
    /// `index` must be in `0..array_length(obj)`. The trait does NOT validate
    /// it (M4a): the caller is responsible for range-checking against
    /// [`array_length`](Self::array_length), and the implementation MUST
    /// bounds-check and MUST NOT read out of range (fail safe — e.g. default
    /// value or VM error — never an out-of-bounds heap read).
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an array element by index.
    ///
    /// `index` must be in `0..array_length(obj)`. The trait does NOT validate
    /// it (M4a): the caller is responsible for range-checking against
    /// [`array_length`](Self::array_length), and the implementation MUST
    /// bounds-check and MUST NOT write out of range (fail safe — no-op or VM
    /// error — never an out-of-bounds heap write).
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value);

    // -- Bulk primitive-array intrinsics (perf path) -----------------------
    //
    // These default to the per-element loop so existing mock contexts compile
    // unchanged. The VM override (`NativeContextImpl`) replaces the loop with
    // a single `ptr::copy_nonoverlapping` against the compact array payload,
    // eliminating ~N virtual dispatches + Value boxes per byte/char copy.
    //
    // Callers in native-io / native-builtins / native-collections (e.g.
    // ZipFile entry reads, FileInputStream/OutputStream, String construction
    // from char[]) currently loop element-by-element through
    // `set_array_element` / `get_array_element`; migrating those callers to
    // these intrinsics is the round-2 perf win.

    /// Bulk copy from a host byte slice into a Java `byte[]` array at the given offset.
    /// Returns `true` on success, `false` on bounds error / wrong array kind.
    ///
    /// The default impl loops via `set_array_element` (correct but slow). The
    /// VM override uses `ptr::copy_nonoverlapping` against the array's raw
    /// payload, which is ~50-100x faster for multi-KB copies.
    fn write_byte_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[u8]) -> bool {
        // CRIT fix: previously the per-element fallback wrote unconditionally,
        // so a too-large `src` would silently overflow past the array end
        // (or panic on the underlying `set_array_element`, depending on impl)
        // while the function reported `true` to the caller. Guard the entry
        // with the same bounds check the VM override performs.
        let dst_len = self.array_length(arr);
        if dst_off
            .checked_add(src.len())
            .map_or(true, |end| end > dst_len)
        {
            return false;
        }
        for (i, b) in src.iter().enumerate() {
            self.set_array_element(arr, dst_off + i, Value::Int(*b as i8 as i32));
        }
        true
    }

    /// Bulk read from a Java `byte[]` array into a host buffer at the given offset.
    /// Returns the number of bytes actually copied (0 on bounds error / wrong array kind).
    ///
    /// The default impl loops via `get_array_element`; the VM override
    /// `memcpy`s from the array's raw payload.
    fn read_byte_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [u8]) -> usize {
        // CRIT fix: clamp to the array length up front so we never call
        // `get_array_element` out of bounds (which can panic in real
        // contexts) and so the return value honours the documented
        // "0 on bounds error" contract when `src_off` itself is past end.
        let src_len = self.array_length(arr);
        if src_off > src_len {
            return 0;
        }
        let available = src_len - src_off;
        let n = available.min(dst.len());
        for i in 0..n {
            match self.get_array_element(arr, src_off + i) {
                Value::Int(v) => dst[i] = v as u8,
                _ => return i,
            }
        }
        n
    }

    /// Bulk read from a Java `char[]` array into a host `u16` buffer.
    ///
    /// Default: per-element loop. VM override: single `copy_nonoverlapping`
    /// of `len * 2` bytes from the compact char-array payload (chars are
    /// stored as little-endian `u16` matching host order on supported
    /// targets — same convention `read_char_array_bulk` in `vm_heap`).
    fn read_char_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [u16]) -> usize {
        // CRIT fix: same out-of-bounds guard as `read_byte_array_into`.
        let src_len = self.array_length(arr);
        if src_off > src_len {
            return 0;
        }
        let available = src_len - src_off;
        let n = available.min(dst.len());
        for i in 0..n {
            match self.get_array_element(arr, src_off + i) {
                Value::Int(v) => dst[i] = v as u16,
                _ => return i,
            }
        }
        n
    }

    /// Bulk write from a host `u16` buffer into a Java `char[]` array at
    /// the given destination offset. Returns `true` on success, `false` on
    /// bounds error / wrong array kind.
    ///
    /// AUDIT 2026-05-17: symmetric to `read_char_array_into`. Used by
    /// `stream_decoder::refill` to populate the read-ahead char buffer
    /// in one shot instead of N `set_array_element` round-trips. The
    /// VM override `memcpy`s into the compact char-array payload.
    fn write_char_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[u16]) -> bool {
        // CRIT fix: bounds-check the destination before any writes so we
        // never silently overflow past the array end while reporting `true`.
        let dst_len = self.array_length(arr);
        if dst_off
            .checked_add(src.len())
            .map_or(true, |end| end > dst_len)
        {
            return false;
        }
        for (i, c) in src.iter().enumerate() {
            self.set_array_element(arr, dst_off + i, Value::Int(*c as i32));
        }
        true
    }

    /// Bulk copy of array elements (primitive arrays only — for ref arrays
    /// the caller must do per-element typecheck). `src` and `dst` may alias
    /// (the VM override uses `copy_within` for same-array overlap, falling
    /// back to `copy_nonoverlapping` for distinct backings).
    ///
    /// Returns `true` on success, `false` on bounds / element-type mismatch.
    /// Even a zero-length copy only succeeds after the array kind, primitive
    /// element type, and offset bounds have been validated.
    fn bulk_array_copy(
        &mut self,
        src: ObjectRef,
        src_off: usize,
        dst: ObjectRef,
        dst_off: usize,
        len: usize,
    ) -> bool {
        if self.heap_kind_of(src) != ObjectKind::Array
            || self.heap_kind_of(dst) != ObjectKind::Array
        {
            return false;
        }
        let src_type = self.heap_element_type_of(src);
        let dst_type = self.heap_element_type_of(dst);
        if src_type != dst_type || src_type == ArrayElementType::Reference {
            return false;
        }
        let src_end = match src_off.checked_add(len) {
            Some(end) => end,
            None => return false,
        };
        let dst_end = match dst_off.checked_add(len) {
            Some(end) => end,
            None => return false,
        };
        if src_end > self.array_length(src) || dst_end > self.array_length(dst) {
            return false;
        }
        if len == 0 {
            return true;
        }

        for i in 0..len {
            if !value_matches_primitive_array(src_type, self.get_array_element(src, src_off + i)) {
                return false;
            }
        }

        let same_array = src.as_ptr() == dst.as_ptr();
        if same_array && dst_off > src_off {
            for i in (0..len).rev() {
                let v = self.get_array_element(src, src_off + i);
                self.set_array_element(dst, dst_off + i, v);
            }
        } else {
            for i in 0..len {
                let v = self.get_array_element(src, src_off + i);
                self.set_array_element(dst, dst_off + i, v);
            }
        }
        true
    }

    /// Get the ObjectKind (Object or Array) of a heap object.
    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind;

    /// Get the ArrayElementType of an array object.
    /// Returns `ArrayElementType::Reference` for non-array objects or reference arrays.
    fn heap_element_type_of(&self, obj: ObjectRef) -> ArrayElementType;

    /// Create a Java String object from a Rust &str. Returns the ObjectRef.
    ///
    /// Consults and populates the VM's interned-string pool: equal text yields
    /// the *same* ObjectRef. Use only for content that should behave like a
    /// string literal. For dynamically produced strings — `StringBuilder
    /// .toString()`, `substring`, etc. — use [`create_string_uninterned`]
    /// (Self::create_string_uninterned) so `==` reports them as distinct.
    fn create_string(&mut self, text: &str) -> ObjectRef;

    /// Create a Java String object from a Rust &str **without** interning.
    ///
    /// Always allocates a fresh, distinct String object — the correct
    /// constructor for dynamically produced strings, matching the JVM spec
    /// requirement that only literals and `String.intern()` participate in
    /// the constant pool. Defaults to [`create_string`](Self::create_string)
    /// for mock/test contexts.
    fn create_string_uninterned(&mut self, text: &str) -> ObjectRef {
        self.create_string(text)
    }

    /// Create a dynamic String at a native-call safepoint when the caller has
    /// no unpinned Java references.  The VM implementation may collect before
    /// allocating; the default keeps mock contexts and legacy implementations
    /// on the ordinary uninterned path.
    fn create_string_uninterned_gc_safe(&mut self, text: &str) -> ObjectRef {
        self.create_string_uninterned(text)
    }

    /// Probe a per-thread cache for an ASCII case-conversion result. The
    /// cache alternates two immutable values so consecutive calls stay
    /// observably distinct.
    fn get_ascii_case_string_cached(
        &mut self,
        _source: ObjectRef,
        _upper: bool,
    ) -> Option<ObjectRef> {
        None
    }

    /// Create and retain the alternating pair for an ASCII case conversion.
    fn create_ascii_case_string_cached(
        &mut self,
        _source: ObjectRef,
        text: &str,
        _upper: bool,
    ) -> ObjectRef {
        self.create_string_uninterned_gc_safe(text)
    }

    /// Populate an *already-allocated* `java/lang/String` object's backing
    /// fields directly from raw UTF-16 code `units`, using the same
    /// Latin1-fits-in-a-byte bulk scan + little-endian compact-string layout
    /// as [`create_string`](Self::create_string). Unlike `create_string`,
    /// this does NOT allocate the `String` object itself and does NOT touch
    /// the intern pool — it exists for native `<init>` overrides
    /// (`String(char[])`, `String(char[], int, int)`) that intercept
    /// construction *after* `new` has already allocated `this`: a
    /// constructor native must mutate `this` in place, not return a
    /// different object identity.
    ///
    /// Preserves raw code units byte-for-byte (including unpaired
    /// surrogates), unlike routing through a Rust `&str`, which cannot
    /// represent those. Returns `false` only on backing-array allocation
    /// failure (heap exhaustion) — the caller should surface a catchable
    /// `OutOfMemoryError`.
    ///
    /// Default impl (mock/test contexts, which treat strings as opaque
    /// objects): stores the units in a plain `char[]` at field 0 via the
    /// generic array/field primitives. The VM override replaces this with
    /// the exact compact-string layout used by every other String natively.
    fn init_string_from_units(&mut self, this: ObjectRef, units: &[u16]) -> bool {
        let arr = self.new_array(ArrayElementType::Char, units.len());
        for (i, &u) in units.iter().enumerate() {
            self.set_array_element(arr, i, Value::Int(u as i32));
        }
        self.set_field(this, 0, Value::Object(Some(arr)));
        true
    }

    /// Read a Java String object back to a Rust String.
    fn read_string(&self, obj: ObjectRef) -> Option<String>;

    /// Return the raw Java `String.hashCode()` for a confirmed String object.
    /// `None` means that `obj` is not a String. Implementations may override
    /// this to inspect compact storage without allocating a host String.
    fn java_string_hash_code(&self, obj: ObjectRef) -> Option<i32> {
        self.read_string(obj).map(|text| {
            text.encode_utf16().fold(0i32, |hash, unit| {
                hash.wrapping_mul(31).wrapping_add(unit as i32)
            })
        })
    }

    /// Compare two confirmed Java Strings without routing through Java
    /// dispatch. `None` means at least one operand is not a String.
    fn java_strings_equal(&self, a: ObjectRef, b: ObjectRef) -> Option<bool> {
        Some(self.read_string(a)? == self.read_string(b)?)
    }

    /// Get or create the java.lang.Class mirror for the given ClassId.
    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef;

    /// Allocate an object with the given class_id and number of fields,
    /// without loading a class (for synthetic objects).
    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef;

    /// Fallible twin of [`alloc_object`](Self::alloc_object) for a native-call
    /// safepoint where the caller holds no unpinned Java references (same
    /// contract as `create_string_uninterned_gc_safe`). Returns `None`
    /// instead of hard-aborting the process when the heap is exhausted, so
    /// the caller can surface a catchable `java.lang.OutOfMemoryError`.
    /// Defaults to the aborting `alloc_object` (wrapped in `Some`) for
    /// mock/test contexts; the real VM implementation overrides this with
    /// the actual fallible allocator.
    fn try_alloc_object_gc_safe(
        &mut self,
        class_id: ClassId,
        num_fields: usize,
    ) -> Option<ObjectRef> {
        Some(self.alloc_object(class_id, num_fields))
    }

    /// Get the number of fields (slots) of a heap object.
    fn object_num_fields(&self, obj: ObjectRef) -> usize;

    /// Returns the total number of bytes allocated on the heap.
    fn heap_allocated_bytes(&self) -> usize;

    // -- ObjectStreamClass descriptor cache (WP0.2) --
    //
    // Backs `java.io.ObjectStreamClass.lookup(Class)`. See
    // `vm::runtime::serialization::oscache` for the rationale.

    /// Look up a previously-built `ObjectStreamClass` descriptor for
    /// `class_id`. Returns `None` if `lookup` has never been called for
    /// this class yet (the native then allocates a fresh descriptor and
    /// installs it via `osc_cache_put`).
    ///
    /// Default implementation returns `None` — mock contexts and other
    /// simple impls just never cache.
    fn osc_cache_get(&self, _class_id: ClassId) -> Option<ObjectRef> {
        None
    }

    /// Install a freshly-built `ObjectStreamClass` descriptor in the
    /// cache. Returns the `ObjectRef` that ends up cached (either
    /// `desc` on a successful insert, or the pre-existing entry if
    /// another thread raced us). Callers MUST use the returned ref as
    /// the result of `lookup` — discarding it would break the
    /// identity contract.
    ///
    /// Default implementation ignores the descriptor and returns `desc`
    /// unchanged — non-caching contexts behave as if every lookup
    /// builds a fresh descriptor.
    fn osc_cache_put(&self, _class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        desc
    }

    // -- Volatile field access (for sun.misc.Unsafe / Atomics) --

    /// Read an object field with volatile (sequentially consistent) semantics.
    ///
    /// Same slot-index contract as [`get_field`](Self::get_field): `index`
    /// must be in range for `obj`'s class, the trait does NOT validate it
    /// (M4a), and the implementation MUST bounds-check and MUST NOT read out
    /// of range. Note `Unsafe` callers may pass an index derived from a
    /// Java-supplied field *offset* — implementations must treat such input as
    /// untrusted and validate it.
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an object field with volatile (sequentially consistent) semantics.
    ///
    /// Same slot-index contract as [`set_field`](Self::set_field): `index`
    /// must be in range for `obj`'s class, the trait does NOT validate it
    /// (M4a), and the implementation MUST bounds-check and MUST NOT write out
    /// of range. `Unsafe`-sourced offsets are untrusted and must be validated
    /// by the implementation.
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value);

    // -- Compare-and-swap --

    /// Compare-and-swap on an object field. Returns true if field contained
    /// `expected` and was updated to `new_val`.
    ///
    /// Same slot-index contract as [`set_field`](Self::set_field): `index`
    /// must be in range for `obj`'s class, the trait does NOT validate it
    /// (M4a), and the implementation MUST bounds-check and MUST NOT
    /// read/write out of range. On an out-of-range index the implementation
    /// must fail safe (return `false`), never touch out-of-bounds memory.
    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool;

    /// audit-round5 fix #9 (HIGH): atomic `fetch_add` on an `int` instance
    /// field. Returns the *previous* value (matching `AtomicInteger.getAndAdd`
    /// / `AtomicI32::fetch_add` semantics).
    ///
    /// Default implementation is a `compare_and_swap_field` retry loop. The
    /// VM override should map this to a single `LOCK XADD` (one trait
    /// dispatch, no CAS spin under contention).
    ///
    /// On a field-type mismatch this returns `Err(MethodCallFailed)` wrapping
    /// an `IllegalArgumentException` rather than panicking: a panic crossing
    /// the native/Java boundary is unsound (it may unwind through JIT-compiled
    /// frames that are not unwind-safe).
    fn atomic_fetch_add_int(
        &mut self,
        obj: ObjectRef,
        index: usize,
        delta: i32,
    ) -> Result<i32, MethodCallFailed> {
        loop {
            let current = self.get_field_volatile(obj, index);
            // Bug 3 (CRIT type corruption): the previous default impl
            // silently fell back to old=0 for non-Int slots, then
            // CAS-wrote `Value::Int(delta)` over the existing slot —
            // corrupting both the numeric value and the field's type
            // tag (a Long field would become Int). Surface the
            // caller's mis-dispatch as a catchable Java exception
            // instead of panicking across the native boundary. The
            // Long → atomic_fetch_add_long delegation is intentionally
            // NOT done here because the int-variant's i32 return type
            // cannot losslessly carry a Long previous value; callers
            // must route via the correct accessor.
            let old = match current {
                Value::Int(v) => v,
                other => {
                    return Err(MethodCallFailed::InternalError(
                        RuntimeError::IllegalArgumentException {
                            message: format!(
                                "atomic_fetch_add_int: field {} on object is not Int: {:?}",
                                index, other
                            ),
                        }
                        .into(),
                    ));
                }
            };
            let new_val = Value::Int(old.wrapping_add(delta));
            if self.compare_and_swap_field(obj, index, current, new_val) {
                return Ok(old);
            }
        }
    }

    /// audit-round5 fix #9 (HIGH): atomic `fetch_add` on a `long` instance
    /// field — `AtomicLong.getAndAdd` / `AtomicI64::fetch_add` analogue.
    /// See `atomic_fetch_add_int` for the default-impl rationale.
    ///
    /// As with `atomic_fetch_add_int`, a field-type mismatch yields
    /// `Err(MethodCallFailed)` (`IllegalArgumentException`) instead of a
    /// panic that could unwind through JIT frames.
    fn atomic_fetch_add_long(
        &mut self,
        obj: ObjectRef,
        index: usize,
        delta: i64,
    ) -> Result<i64, MethodCallFailed> {
        loop {
            let current = self.get_field_volatile(obj, index);
            // Bug 3 (CRIT type corruption): refuse to silently rewrite a
            // non-Long slot. The previous default impl's `_ => 0` arm
            // turned a wrong-typed field (Int, Reference, …) into
            // `Value::Long(delta)`, permanently corrupting the slot's
            // type tag. A `panic!` here would unwind across the native
            // boundary and abort the VM, so instead `debug_assert!`
            // (loud in debug builds) and return the current value
            // unchanged in release — a type mismatch indicates a native
            // dispatch bug (caller used the wrong accessor).
            let old = match current {
                Value::Long(v) => v,
                other => {
                    return Err(MethodCallFailed::InternalError(
                        RuntimeError::IllegalArgumentException {
                            message: format!(
                                "atomic_fetch_add_long: field {} on object is not Long: {:?}",
                                index, other
                            ),
                        }
                        .into(),
                    ));
                }
            };
            let new_val = Value::Long(old.wrapping_add(delta));
            if self.compare_and_swap_field(obj, index, current, new_val) {
                return Ok(old);
            }
        }
    }

    // -- Object allocation without constructor --

    /// Allocate an uninitialized object instance (for Unsafe.allocateInstance).
    /// Returns None if the class cannot be loaded.
    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef>;

    /// Register a discovered weak/soft/phantom reference with the GC's ReferenceProcessor.
    /// `ref_type`: 0=Weak, 1=Soft, 2=Phantom
    /// `reference_obj`: the Reference object itself
    /// `referent`: the referred-to object
    /// `queue`: optional ReferenceQueue object
    fn discover_reference(
        &mut self,
        ref_type: u8,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    );

    /// Notify the GC's reference processor that a `SoftReference.get()` just
    /// observed its referent, refreshing the LRU timestamp used by
    /// soft-reference clearing heuristics on the next major GC.
    ///
    /// Round-5 fix (HIGH): without this hook, the LRU index sees
    /// `last_access_time_ms == 0` forever and every SoftReference looks
    /// infinitely stale — clearing on the first low-memory cycle and
    /// defeating soft-ref-backed caches. The VM overrides this with a
    /// call into `ReferenceProcessor::touch_soft_reference`. The default
    /// no-op keeps mock/test contexts compiling.
    fn touch_soft_reference(&mut self, _reference_obj: ObjectRef) {}

    /// INT-8: GC keep-alive for a referent a `Reference.get()` just handed to
    /// the mutator — the HotSpot `G1ReferenceGet` intrinsic barrier
    /// equivalent. While a G1 concurrent mark cycle is active, the marker
    /// deliberately does NOT trace through referent slots (referent-slot
    /// hiding); a mutator that reads a referent and stores it into an
    /// already-scanned (black) object would create the only strong path via
    /// an edge the snapshot cannot see, and the remark-time reference
    /// processor could then clear the weak ref and free the referent while
    /// strongly reachable (use-after-free). The VM overrides this with the
    /// heap's SATB pre-barrier (`VmHeap::write_barrier_pre`), which logs the
    /// value as a mark root when marking is active and is a no-op otherwise.
    /// `refersTo` intentionally does NOT call this — its JDK contract is to
    /// test the referent WITHOUT keeping it alive. The default no-op keeps
    /// mock/test contexts compiling.
    fn gc_reference_keep_alive(&mut self, _referent: ObjectRef) {}
}

pub trait NativeThreadAccess: NativeHeapAccess {
    /// Capability boundary: Threads, monitors, parking, blocking, and scoped values.


    // -- Threading methods --

    /// Get the current thread's ThreadId (as a u64).
    fn thread_id(&self) -> u64;

    /// Acquire the monitor (synchronized) on the given object.
    fn monitor_enter(&mut self, obj: ObjectRef);

    /// GC-safe variant of [`monitor_enter`], for the rare native whose
    /// contended wait needs to be excused from an in-flight STW barrier
    /// pause instead of leaving the calling thread counted in its `expected`
    /// set for the whole wait (see
    /// `docs/internal/fixed-suite-bugs/wildfly-standalone-boot-stw-jit-takeover-hang.md`).
    ///
    /// Deliberately NARROW: `monitor_enter` itself stays on its original,
    /// non-GC-blocked path for the other ~80 native call sites that use
    /// it (Semaphore/Phaser/Exchanger/blocking-queue/ConcurrentHashMap/
    /// ReentrantLock/Condition/etc.) — a from-scratch audit of every one of
    /// those (2026-07-13) found the overwhelming majority keep reading
    /// fields off the SAME `obj`/`this` after the call without any
    /// pin-and-refresh, so blanket-switching `monitor_enter`'s contended
    /// path to span a completing (possibly moving) GC pause would expose
    /// all of them to the stale-`ObjectRef`-across-GC bug class this
    /// codebase has repeatedly hit (see
    /// `docs/internal/wildfly-parallel-boot-stale-objectref-residual.md`)
    /// — an unaudited-at-scale regression risk far worse than the original
    /// hang. This method exists so the ONE call site with live-gdb-confirmed
    /// evidence of the deadlock (`CountDownLatch`'s `native_cdl_await` /
    /// `native_cdl_await_timeout` / `native_cdl_count_down` polling loop,
    /// contending a shared handshake latch under WildFly's
    /// `parallel-extension-add`) can opt in individually, and MUST use the
    /// returned reference for anything after the call — the object may have
    /// moved if the wait spanned a GC. Default implementation is a no-op
    /// pass-through to `monitor_enter` (correct for every mock/test context
    /// in this workspace, none of which move objects mid-wait).
    fn monitor_enter_gc_safe(&mut self, obj: ObjectRef) -> ObjectRef {
        self.monitor_enter(obj);
        obj
    }

    /// Release the monitor (synchronized) on the given object.
    fn monitor_exit(&mut self, obj: ObjectRef);

    /// Perform Object.wait() on the given object's monitor.
    fn monitor_wait(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
    ) -> cratonvm_types::error::MethodCallResult;

    /// Perform Object.notify() on the given object's monitor.
    fn monitor_notify(&mut self, obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Perform Object.notifyAll() on the given object's monitor.
    fn monitor_notify_all(&mut self, obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Spawn a new OS thread to run Thread.run() on the given Java Thread object.
    fn thread_start(&mut self, thread_obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Block until the target thread (identified by Java Thread object) finishes.
    fn thread_join(&mut self, thread_obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Check if the target thread (identified by Java Thread object) is alive.
    fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool;

    /// Coarse run-state of the target thread, derived from the VM thread
    /// registry (the authoritative liveness source). Returns:
    ///   * `0` — NEW: the thread was never started (no registry entry).
    ///   * `1` — RUNNABLE: started and still alive.
    ///   * `2` — TERMINATED: started and has since finished.
    ///
    /// Value `3` represents an alive thread parked in a blocking region
    /// (`WAITING`) and value `4` an alive thread acquiring a contended
    /// monitor (`BLOCKED`); the default returns `0`.
    ///
    /// Used to back `Thread.getState()` in real-JDK mode, where the JDK
    /// bytecode reads `holder.threadStatus` — a field the VM does not keep
    /// updated, so `getState()` would otherwise always report `NEW` (even for
    /// finished threads), tripping strict thread-leak detectors. The default
    /// returns `0`.
    fn thread_run_state(&self, _thread_obj: ObjectRef) -> u8 {
        0
    }

    /// The Java call stack of the target thread (identified by its Thread
    /// object), innermost frame first. For the *current* thread this is the live
    /// stack; for another thread it is the snapshot published at its last
    /// blocking deposit point (so for a parked thread it shows where it is
    /// stuck). Empty if unavailable. Backs cross-thread `Thread.getStackTrace()`
    /// / `Thread.dumpThreads()`. The default returns empty.
    fn thread_stack_trace(&self, _thread_obj: ObjectRef) -> Vec<StackTraceEntry> {
        Vec::new()
    }

    /// Atomically snapshot the thread state and lock relationships needed by
    /// `ThreadMXBean`. The default leaves lightweight/mock contexts source
    /// compatible; production VMs must return GC-safe registry-backed refs.
    fn thread_jmx_snapshot(&self, _thread_obj: ObjectRef) -> Option<ThreadJmxSnapshot> {
        None
    }

    /// Record the current ownership of an `AbstractOwnableSynchronizer`.
    /// Implementations retain/remap the synchronizer while it is owned so a
    /// later JMX dump can report `lockedSynchronizers` without heap walking.
    fn record_jmx_owned_synchronizer(
        &mut self,
        _synchronizer: ObjectRef,
        _owner: Option<ObjectRef>,
    ) {
    }

    /// Get the Java Thread object for the current thread.
    fn current_thread_object(&mut self) -> ObjectRef;

    /// Interrupt the target thread (identified by Java Thread object).
    fn thread_interrupt(&mut self, thread_obj: ObjectRef);

    /// T1.5.1 — post an asynchronous `Throwable` to the target
    /// thread (identified by its Java `Thread` object). The target
    /// will raise the exception at its next safepoint.
    ///
    /// Returns `true` if the post succeeded (target found, slot
    /// written), `false` if the target is not alive or not
    /// registered. Default impl is a no-op for mock contexts.
    fn thread_post_async_exception(
        &mut self,
        _thread_obj: ObjectRef,
        _throwable: ObjectRef,
    ) -> bool {
        false
    }

    /// Check and optionally clear the current thread's interrupted status.
    fn is_interrupted(&self, clear: bool) -> bool;

    /// Check the interrupted status of the thread identified by `thread_obj`
    /// — which may be a thread *other* than the current one (e.g.
    /// `ThreadPoolExecutor.interruptIdleWorkers` calls `worker.isInterrupted()`
    /// from the pool-management thread). Never clears the flag. The default
    /// impl falls back to the current thread's status for mock contexts that
    /// don't track per-thread state.
    fn thread_is_interrupted(&self, _thread_obj: ObjectRef) -> bool {
        self.is_interrupted(false)
    }

    // -- Virtual-thread / Loom (JEP 444/491) --
    //
    // Default implementations make these no-ops so platform native code (and
    // test mocks) don't need to implement them. The VM overrides them in
    // `vm_exec.rs` to drive the carrier-thread semaphore and pin tracking.

    /// Returns `true` if the current thread is a virtual thread.
    fn is_current_virtual(&self) -> bool {
        false
    }

    /// Return the current thread's pin depth (0 = not pinned).
    fn vt_pin_count(&self) -> u32 {
        0
    }

    /// Increment the current thread's pin count with the given reason.
    /// Called from `monitor_enter` / JNI entry. No-op for platform threads.
    fn vt_pin(&mut self, _reason: &'static str) {}

    /// Decrement the current thread's pin count.
    /// No-op for platform threads or when pin_count is already zero.
    fn vt_unpin(&mut self) {}

    /// Release the carrier-thread permit so another virtual thread can run.
    /// Called before a blocking syscall (sleep, park, NIO wait) in a VT.
    /// No-op for platform threads.
    fn vt_release_carrier(&mut self) {}

    /// Reacquire a carrier-thread permit after a blocking operation completes.
    /// Must be paired with `vt_release_carrier`. No-op for platform threads.
    fn vt_acquire_carrier(&mut self) {}

    /// Request a continuation-backed timed park. Returns `true` only for an
    /// unpinned virtual thread whose interpreter frames can be frozen by the
    /// VM. The native must then return `ContinuationYield` without blocking.
    fn vt_park_for(&mut self, _duration: std::time::Duration) -> bool {
        false
    }

    /// Register the current unpinned virtual thread as an asynchronous waiter
    /// on a VM-local stable key. The native must recheck its condition after
    /// registration and return `ContinuationYield` only while it remains false.
    fn vt_wait_on_key(&mut self, _key: u64) -> bool {
        false
    }

    /// Cancel a waiter registration made by [`Self::vt_wait_on_key`].
    fn vt_cancel_wait_on_key(&mut self, _key: u64) {}

    /// Wake and resubmit all virtual threads waiting on a stable key.
    fn vt_wake_waiters(&mut self, _key: u64) {}

    /// Get the number of alive threads in the VM.
    fn active_thread_count(&self) -> i32;

    /// Get the Java Thread objects for all alive threads (up to `max` entries).
    /// Returns the number of thread objects written.
    fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef>;

    /// T19.H1 — mark the start of a *blocking region* inside a native
    /// method (a spin/poll loop or an OS wait that may run for a long
    /// time, e.g. `ReferenceQueue.remove`, a selector `select`, a socket
    /// `accept`).
    ///
    /// While inside a blocking region the calling thread is treated as
    /// GC-safe: its frame roots are published to the registry snapshot
    /// and a concurrent stop-the-world collector will NOT wait for it to
    /// reach an interpreter safepoint. Every `begin_blocking_region` MUST
    /// be paired with exactly one `end_blocking_region`.
    ///
    /// The default impl is a no-op so out-of-tree `NativeContext`
    /// implementors (tests) need not change.
    fn begin_blocking_region(&mut self) {}

    /// Same GC-safety contract as `begin_blocking_region`, for a region with
    /// a bounded/known wait duration (`Thread.sleep`, a timed `Object.wait`,
    /// `LockSupport.parkNanos`, …). `Thread.getState()` reports
    /// `TIMED_WAITING` for a thread inside one of these vs. plain `WAITING`
    /// for an unbounded `begin_blocking_region` — real JDK's
    /// `Thread.State` makes exactly this distinction, and callers such as
    /// Spring Boot's `SpringApplicationShutdownHookTests` assert on it via
    /// `Awaitility.await().until(thread::getState, State.TIMED_WAITING::equals)`.
    ///
    /// The default impl just delegates to `begin_blocking_region` (reported
    /// as plain `WAITING`) so out-of-tree `NativeContext` implementors need
    /// not change; must still be paired with exactly one `end_blocking_region`.
    fn begin_timed_blocking_region(&mut self) {
        self.begin_blocking_region();
    }

    /// T19.H1 — end a blocking region opened by `begin_blocking_region`.
    /// Re-syncs the thread with any GC that ran while it was blocked.
    fn end_blocking_region(&mut self) {}

    /// End a blocking region AND re-sync caller-held raw `Value` refs.
    ///
    /// A native poll loop captures its arguments as raw `Value`s before
    /// entering the region; a moving GC that completes while the thread is
    /// blocked relocates the referenced objects, and the thread-side
    /// re-sync (`end_blocking_region`) only repairs the *frames* — the
    /// native-local copies would keep their stale pre-GC addresses (the
    /// `ReferenceQueue.remove` stale-receiver writer). Pass those locals
    /// here so they are rewritten through the same accumulated GC fixup.
    ///
    /// The default impl ends the region without touching `refs` (matches
    /// VMs/tests whose collector never moves objects under natives).
    fn end_blocking_region_refs(&mut self, refs: &mut [Value]) {
        let _ = &refs;
        self.end_blocking_region();
    }

    // -- Park/Unpark (LockSupport) --

    /// Park the current thread (block until unparked or timeout).
    fn park(&mut self, timeout: Option<std::time::Duration>);

    /// Unpark a thread identified by its Java Thread object.
    fn unpark(&self, thread_obj: ObjectRef);

    // -- Scoped Values (JEP 446, Java 25) --

    /// Look up a scoped value binding by key_id on the current thread's stack.
    fn get_scoped_value(&self, key_id: u64) -> Option<Value>;

    /// Push a scoped value binding onto the current thread's stack.
    fn push_scoped_value(&mut self, key_id: u64, value: Value);

    /// Round-9 GC fix: push a scoped value binding AND remember the
    /// ScopedValue key object so the GC keeps it live for the duration of
    /// the binding. Default impl forwards to the legacy
    /// `push_scoped_value` for backward compatibility — callers that have
    /// the key ObjectRef (e.g. `Carrier.run`) should call this overload
    /// instead so the key cannot be reclaimed while bindings exist.
    fn push_scoped_value_with_key(
        &mut self,
        key_id: u64,
        _key_ref: Option<ObjectRef>,
        value: Value,
    ) {
        self.push_scoped_value(key_id, value);
    }

    /// Pop the most recent scoped value binding from the current thread's stack.
    fn pop_scoped_value(&mut self);

    /// Return the current depth (number of entries) of the scoped value binding stack.
    fn scoped_value_depth(&self) -> usize;
}

pub trait NativeExceptionAccess: NativeHeapAccess {
    /// Capability boundary: Throwable and stack-trace capture.


    /// Capture the current Java call stack without retaining it. Used by
    /// StackWalker and caller-sensitive helpers.
    fn capture_stack_trace(&mut self, throwable_hash: i32) -> Vec<StackTraceEntry>;

    /// Capture and retain a stack trace for `Throwable.fillInStackTrace`.
    ///
    /// The default keeps lightweight/mock contexts source-compatible. The VM
    /// implementation overrides it so retained frames are owned by the VM,
    /// rather than by the Java thread that happened to construct the throwable.
    fn capture_throwable_stack_trace(&mut self, throwable: ObjectRef) -> Vec<StackTraceEntry> {
        self.capture_stack_trace(self.identity_hash_code(throwable))
    }

    /// Retrieve a previously captured stack trace as an owned snapshot.
    ///
    /// An owned value deliberately avoids lending a reference through a
    /// VM-shared lock while another Java thread may replace or discard a trace.
    fn get_stack_trace(&self, throwable_hash: i32) -> Option<Vec<StackTraceEntry>>;

    /// The exact `ClassId` each live frame is currently executing in,
    /// innermost (most recent call) first.
    ///
    /// Unlike `capture_stack_trace`'s `StackTraceEntry`s (which carry only a
    /// display `class_name: Arc<str>`, for `Throwable`/`StackWalker` output),
    /// this exposes each frame's precise, already-resolved `ClassId` — no
    /// re-resolution by name needed. That distinction matters whenever two
    /// *different* classes share one name (a common shape for custom
    /// classloaders, e.g. Hibernate bytecode-enhancement's per-test-class
    /// `EnhancingClassLoader`, or a class reloaded under a fresh loader
    /// between JUnit tests sharing one process): re-resolving a frame's name
    /// via `class_id_by_name` collapses to whichever definition the global
    /// class table associates with that name (typically the first one ever
    /// registered), which is not necessarily the one actually executing on
    /// that frame. Used by `latest_user_defined_loader_class` to correctly
    /// mirror `jdk.internal.misc.VM.latestUserDefinedLoader()`.
    fn frame_class_ids(&self) -> Vec<ClassId> {
        Vec::new()
    }
}

pub trait NativeGpuAccess: NativeInvokeAccess {
    /// Capability boundary: Optional GPU submission and result services.


    /// Phase 5 escape hatch for GPU offload — dispatch the named method
    /// asynchronously on the GPU and return the submission handle. The
    /// default impl returns `None` (no GPU offload). The VM's
    /// `NativeContextImpl` overrides under `#[cfg(feature = "gpu-offload")]`
    /// to resolve `class_name`/`method_name`/`descriptor` against the
    /// class manager, marshal `java_args` into `KernelArgs`, and call
    /// `OffloadCache::dispatch_async`. The returned handle is what the
    /// Java `GpuFutureImpl` wraps; pass it back to
    /// `Native.futureSynchronize` / `Native.futureGetResult` to drive
    /// the future.
    ///
    /// `java_args` follows the same convention as the JVM stack: each
    /// `Value::Object(Some(...))` is a Java array reference, each
    /// `Value::Int/Long/Float/Double` is a primitive scalar.
    fn gpu_dispatch_method(
        &mut self,
        _class_name: &str,
        _method_name: &str,
        _descriptor: &str,
        _java_args: &[Value],
    ) -> Option<u64> {
        None
    }

    /// GpuStream affinity — mint a new Java-visible CUDA stream on the
    /// per-VM default-ordinal `OffloadCache`.
    ///
    /// Called by `Native.newStream` (wraps the returned handle in a
    /// `GpuStreamImpl`) and, lazily, by every `submit`/`launch`/
    /// `submitMethod`/`submitWithArg(s)` handler the first time a given
    /// `GpuExecutor` handle is used — see
    /// `native-builtins/src/craton_gpu.rs::resolve_or_create_default_stream`.
    /// That laziness is what gives an executor a real *default* stream:
    /// every dispatch through the same executor handle reuses the one
    /// stream minted on its first submit, instead of each call getting
    /// its own private one-shot stream (the gap
    /// `docs/gpu/async-api.md` describes under "GpuStream affinity is
    /// not wired up").
    ///
    /// Returns `None` when there is no device (no driver / `--gpu` off
    /// / `gpu-offload` compiled off on the VM side) — the default impl
    /// here, matching every other no-driver fallback in this trait.
    /// The VM's `NativeContextImpl` overrides under
    /// `#[cfg(feature = "gpu-offload")]` to call
    /// `runtime::offload::OffloadCache::stream_create`.
    fn gpu_stream_create(&mut self) -> Option<u64> {
        None
    }

    /// Release a stream minted by [`gpu_stream_create`](Self::gpu_stream_create).
    /// Safe to call on an unknown or already-released `handle`
    /// (no-op) — same idempotent-release convention as
    /// [`gpu_release_array_cache`](Self::gpu_release_array_cache).
    ///
    /// Default impl is a no-op (no GPU offload). The VM override calls
    /// `runtime::offload::OffloadCache::stream_release`.
    fn gpu_stream_release(&mut self, _handle: u64) {}

    /// Stream-affine sibling of [`gpu_dispatch_method`](Self::gpu_dispatch_method):
    /// identical contract, plus `stream_handle`.
    ///
    /// * `Some(h)` — pin this dispatch onto the CUDA stream previously
    ///   minted by [`gpu_stream_create`](Self::gpu_stream_create) under
    ///   handle `h`. Two dispatches pinned to the SAME `h` serialize in
    ///   submission order (the ordering guarantee a CUDA stream gives
    ///   for free). An `h` that was never minted, or was already
    ///   released via [`gpu_stream_release`](Self::gpu_stream_release),
    ///   is a hard failure (a `Failed` submission), not a silent
    ///   fresh-stream fallback.
    /// * `None` — identical to calling
    ///   [`gpu_dispatch_method`](Self::gpu_dispatch_method) directly: a
    ///   fresh, private, one-shot stream for this dispatch alone.
    ///
    /// Default impl delegates to `gpu_dispatch_method` and ignores
    /// `stream_handle` — correct for every mock/test context (no GPU
    /// offload at all) and for a VM build with `gpu-offload` off. The
    /// VM's `NativeContextImpl` overrides under
    /// `#[cfg(feature = "gpu-offload")]` to call
    /// `runtime::offload::dispatch_method_from_native_on_stream`.
    fn gpu_dispatch_method_on_stream(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        java_args: &[Value],
        _stream_handle: Option<u64>,
    ) -> Option<u64> {
        self.gpu_dispatch_method(class_name, method_name, descriptor, java_args)
    }

    /// Phase 6 #4 — query the real GPU submission registry for the
    /// future at `handle`. Returns:
    ///   * `Some(0)` — Running
    ///   * `Some(1)` — Completed
    ///   * `Some(2)` — Failed
    ///   * `None`    — the handle is not in the real registry
    ///                 (caller should fall back to the synthetic
    ///                 future state in native-builtins).
    /// Default impl returns None (no GPU offload).
    fn gpu_future_status(&self, _handle: u64) -> Option<i32> {
        None
    }

    /// 2026-07-11 — take the real GPU submission's completed result for
    /// `handle`, without blocking. This is the read half `gpu_future_status`
    /// was missing: `gpu_future_status` (now backed by
    /// `runtime::offload::poll_submission_status`) tells a caller *that*
    /// a submission finished; this method hands back *what it produced*.
    ///
    /// Returns:
    ///   * `Some(GpuFutureResult::Scalar*)` — the submission is complete
    ///     and its kernel returned a scalar (`)I`/`)J`/`)F`/`)D`).
    ///   * `Some(GpuFutureResult::Void)` — the submission is complete and
    ///     either the kernel had a void return, or it wrote its result
    ///     into a caller-owned primitive array rather than the future's
    ///     result slot (array results are delivered via writeback into
    ///     the caller's own arrays, not through the future — see
    ///     [`GpuFutureResult`]'s doc comment).
    ///   * `None` — the handle is not in the real registry, the
    ///     submission is still `Running`, or it failed. This method
    ///     never blocks and never finalizes-and-waits on the caller's
    ///     behalf beyond what an already-observed completion allows: a
    ///     caller that hasn't first seen `gpu_future_status`/
    ///     `futureIsDone` report completion should treat `None` here as
    ///     "not ready yet" and fall back to the blocking
    ///     `gpu_future_synchronize` path, not as a permanent failure.
    ///
    /// Default impl returns `None` (no GPU offload).
    fn gpu_future_take_result(&self, _handle: u64) -> Option<GpuFutureResult> {
        None
    }

    /// Phase 6 #4 — block until the real GPU submission at `handle`
    /// completes (via its recorded event). Returns:
    ///   * `Some(Ok(()))`    — completed
    ///   * `Some(Err(msg))`  — submission failed; `msg` carries the reason
    ///   * `None`            — handle not in the real registry
    /// Default impl returns None.
    fn gpu_future_synchronize(&self, _handle: u64) -> Option<Result<(), String>> {
        None
    }

    /// Phase 8 #1 — evict the device-side buffer cache entry for
    /// the given `GpuArray` handle. Called by
    /// `Native.releaseArray` so a long-running Java program that
    /// churns through GpuArrays doesn't accumulate device memory.
    ///
    /// Default impl is a no-op (no GPU offload). The VM override
    /// calls `runtime::offload::device_cache::release(handle)`.
    fn gpu_release_array_cache(&mut self, _handle: u64) {}

    /// Phase 10 #1 — wipe the explicit-submit input-residency cache.
    /// Called by `Native.releaseExecutor` so the device buffers
    /// cached by plain `int[]` / `long[]` / `float[]` / `double[]`
    /// args to `submitMethod` are freed when the Java
    /// `GpuExecutor` is closed.
    ///
    /// Default impl is a no-op (no GPU offload). The VM override
    /// calls `runtime::offload::input_cache::clear_all()`.
    fn gpu_clear_input_cache(&mut self) {}

    /// Phase 9 #1 — materialise the device-side buffer's contents
    /// into host bytes if (and only if) the cache entry is dirty
    /// from a prior kernel's writes. Returns
    /// `Some(little-endian-bytes)` when a download happened (and
    /// the caller should write them into the resident store
    /// before reading the Java array), or `None` when the entry
    /// is unknown or clean (host bytes are already current).
    ///
    /// Default impl returns None (no GPU offload). The VM
    /// override calls
    /// `runtime::offload::device_cache::download_into_bytes_if_dirty(handle)`.
    fn gpu_array_download_if_dirty(&self, _handle: u64) -> Option<Vec<u8>> {
        None
    }

    /// GPU device enumeration escape hatch.
    ///
    /// Returns one entry per attached CUDA device, in ordinal order:
    /// `(name, compute_major, compute_minor, total_global_mem_bytes)`.
    ///
    /// The default impl returns an empty `Vec` — the truthful answer on
    /// a build without the `gpu-offload` feature, or on a host with no
    /// CUDA driver. The VM's `NativeContextImpl` overrides this under
    /// `#[cfg(feature = "gpu-offload")]` to call [`cuda_bridge::probe`].
    /// Because `probe()` itself returns `Err(NoDriver)` on a driverless
    /// host (or when `cuda-bridge` was built in stub mode), the override
    /// likewise yields an empty `Vec` there — `deviceCount()` honestly
    /// reports `0` rather than pretending a device exists.
    ///
    /// `cuda_bridge::probe` currently reports only the primary device
    /// (ordinal 0); the contract here is general (a `Vec`) so a future
    /// multi-device probe needs no signature change.
    fn gpu_device_info(&self) -> Vec<(String, u32, u32, u64)> {
        Vec::new()
    }

    /// Phase 6 #5 — resolve a `GpuCallable` / `GpuRunnable` /
    /// `GpuFunction` lambda's target method.
    ///
    /// When the user writes
    ///
    /// ```ignore
    /// executor.submit(() -> Pipeline.vectorAdd(a, b, out));
    /// ```
    ///
    /// the lambda is materialised as a proxy object whose class is
    /// recorded in `shared.classes.lambda_proxies`. This method looks the
    /// proxy up and returns
    /// `Some((target_class, target_method, target_descriptor,
    ///        captured_values))` so the dispatcher can route through
    /// `gpu_dispatch_method`.
    ///
    /// Returns `None` for any of:
    ///   * `callable` is not a lambda proxy
    ///   * the impl method handle is not InvokeStatic (instance
    ///     methods cannot run on the GPU)
    ///   * gpu-offload feature is off (default impl)
    fn gpu_resolve_lambda_target(
        &self,
        _callable: ObjectRef,
    ) -> Option<(String, String, String, Vec<Value>)> {
        None
    }
}

pub trait NativeSystemAccess: NativeThreadAccess {
    /// Capability boundary: Process, VM, I/O, FFI, metrics, and diagnostics.

    /// Whether this context can construct and dispatch real generated proxy
    /// classes. Lightweight unit-test contexts intentionally return `false`:
    /// they model native object state but do not own a VM-wide class-loader and
    /// proxy-class namespace.
    fn supports_real_proxy_generation(&self) -> bool {
        true
    }

    /// Force-refresh this thread's deposited GC root snapshot (the same
    /// mechanism `NativeContextImpl::deposit_root_snapshot` uses before a
    /// blocking call) without actually blocking.
    ///
    /// Background: a peer-initiated stop-the-world collection has two ways
    /// to see a thread's roots — (1) a live conservative register/stack scan
    /// if that thread is forcibly frozen while executing JIT-compiled code
    /// (`jit::xt_root_scan`), which does NOT know about `native_pin_roots`
    /// (a native-side `Vec` living on the Rust heap, not the JIT frame), or
    /// (2) this thread's last-deposited snapshot
    /// (`collect_all_root_snapshots`/`root_snapshots_for_os_tids`), which
    /// DOES include `native_pin_roots` but is only refreshed at specific
    /// checkpoints: a cooperative interpreter safepoint arrival, entry into
    /// a blocking native region, or this thread itself initiating a GC.
    /// JIT-compiled code has no periodic cooperative safepoint poll at all
    /// (see the comment on `jit::helpers::jit_safepoint_flush_satb`) — it
    /// only touches those checkpoints via specific GC-triggering runtime
    /// helpers, which a hot loop making only fast-path allocations may never
    /// call.
    ///
    /// A native method that pins a long-lived batch of objects (e.g. a
    /// materialized `Stream` of elements, each pinned once up front) and then
    /// drives per-element re-entrant Java execution that can run for a long
    /// time and/or tier up into JIT — without itself ever blocking or
    /// initiating GC — leaves a window where neither mechanism above sees
    /// those pins: not (1), because `native_pin_roots` isn't scanned that
    /// way, and not (2), because nothing has refreshed the deposit since
    /// before the pins were pushed. A peer thread's GC during that window
    /// can reclaim a still-pinned object; the next read through the pin
    /// (correctly re-validated, `via_pin=true`) observes a stale/reused
    /// address. Confirmed live for JUnit 5's `TestTemplateExecutor`/
    /// `ParameterizedTestExtension` dynamic-test dispatch (`ClassCastException:
    /// java.lang.Object cannot be cast to
    /// org.junit.jupiter.api.extension.TestTemplateInvocationContext`,
    /// `obj_cid=0` — see `docs/known-issues/
    /// wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`,
    /// which documents the same family from WildFly's `parallel-extension-add`
    /// boot step) — one more independent occurrence of that already-tracked
    /// "register-invisible root" / cross-thread GC-root-visibility family,
    /// now with this specific closeable checkpoint gap identified.
    ///
    /// Calling this right after establishing such a batch of pins (and
    /// optionally again periodically across a long per-element loop) closes
    /// that window by (re-)publishing a fresh deposit — the exact same
    /// mechanism already relied on for peers parked in a blocking region —
    /// without requiring this thread to actually block. Default impl is a
    /// no-op: test/mock contexts have no cross-thread GC to defend against.
    fn refresh_root_snapshot(&mut self) {}

    /// ES-FAIL-FAMILY-20260710 hunt: arm the GC's dynamic software
    /// write-watchpoint (see `cratonvm_gc::heap::set_dynamic_watch`) at a
    /// raw heap address, so any subsequent write through an instrumented
    /// heap write primitive that covers this address prints its call site.
    /// `addr = 0` disarms. Default no-op so mock/test `NativeContext` impls
    /// don't need to implement it; only the real VM's impl (which has a
    /// live heap to watch) overrides it.
    fn dbg_set_watch_cell(&mut self, _addr: usize) {}

    /// Stable identity for the owning VM/heap.
    ///
    /// Native side caches that store heap `ObjectRef`s must scope entries to
    /// this value; Rust tests can create multiple independent `Vm` instances in
    /// one process, so process-global object caches are otherwise stale across
    /// VM lifetimes. Mock contexts default to a single synthetic scope.
    fn vm_identity(&self) -> usize {
        0
    }

    /// Store a value into the test output buffer (for `tempPrint`).
    fn record_printed_value(&mut self, value: Value);

    /// VM-accelerated primitive-wrapper recognition. `None` means this context
    /// does not implement the fast path; `Some(None)` means the object is not a
    /// wrapper; `Some(Some(value))` is the unboxed primitive.
    fn fast_unbox_primitive_wrapper(&self, _obj: ObjectRef) -> Option<Option<Value>> {
        None
    }

    /// Whether the current Java execution stack already contains the exact
    /// instance method on `receiver`.
    ///
    /// Native shadows occasionally need to distinguish a native-first virtual
    /// entry from an `invokespecial` delegation made by real bytecode already
    /// executing in an override. The default is deliberately conservative for
    /// lightweight test contexts, which do not own a live Java frame stack.
    fn is_executing_instance_method(
        &self,
        _receiver: ObjectRef,
        _method_name: &str,
        _descriptor: &str,
    ) -> bool {
        false
    }

    /// Read `out.len()` bytes of native memory at `addr` into `out`.
    ///
    /// `addr` may be either a real OS pointer (e.g. a mapped buffer) OR one of
    /// the VM's `Unsafe.allocateMemory` arena handles (a synthetic high
    /// address, base `0x10_0000_0000`, NOT a dereferenceable pointer). NIO
    /// native dispatchers (`sun/nio/ch/Net.read0`, `SocketDispatcher` …) that
    /// receive a `DirectByteBuffer.address()` MUST go through this instead of a
    /// raw `copy_nonoverlapping`, because `Util.getTemporaryDirectBuffer` backs
    /// its temp buffers with arena handles — dereferencing one raw SIGSEGVs.
    ///
    /// The default implementation fails closed. Implementations that can prove
    /// the address range is valid must override this and perform their own
    /// pointer/arena validation before copying.
    fn copy_from_native_memory(&self, _addr: i64, _out: &mut [u8]) -> bool {
        false
    }

    /// Write `data` to native memory at `addr`. See [`Self::copy_from_native_memory`]
    /// for the arena-handle vs raw-pointer distinction.
    fn copy_to_native_memory(&mut self, _addr: i64, _data: &[u8]) -> bool {
        false
    }

    /// Record a printed line (for System.out.println capture in tests).
    fn record_printed_line(&mut self, text: String);

    /// Get a system stream object (stdout or stderr).
    fn get_system_stream(&self, name: &str) -> Option<ObjectRef>;

    /// Pin the canonical `System.in` object on the VM so natives and `GETSTATIC`
    /// agree after `initPhase1` allocates it. Default: no-op.
    fn cache_system_stdin(&mut self, _stream: ObjectRef) {}

    /// Look up the canonical `java.lang.Module` mirror for a module name
    /// (`None`/`Some("")` ⇒ the unnamed module). `Class.getModule()` MUST return
    /// the SAME instance for every class in a module — the JDK compares modules
    /// by identity (see `Throwable.validateSuppressedExceptionsList`, HIB-CV-29).
    /// Default: `None` (mock contexts have no persistent store).
    fn get_cached_module_mirror(&self, _module_name: Option<&str>) -> Option<ObjectRef> {
        None
    }

    /// Store the canonical `java.lang.Module` mirror for a module name so that
    /// subsequent `Class.getModule()` calls return the identical instance.
    /// The mirror is registered as a permanent GC root. Default: no-op.
    fn cache_module_mirror(&mut self, _module_name: Option<&str>, _module: ObjectRef) {}

    /// Get a system property by key.
    fn get_system_property(&self, key: &str) -> Option<String>;

    /// Snapshot every system property as a `(key, value)` list.  Used by
    /// `System.getProperties()` to materialise a populated Properties
    /// object when real-JDK's `System.props` static field is null.
    fn list_system_properties(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Set a system property. Returns the old value if any.
    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String>;

    /// Remove a system property from the global store. Returns the old value if
    /// any. Default no-op (mock contexts have no live store); the VM overrides it.
    fn remove_system_property(&mut self, _key: &str) -> Option<String> {
        None
    }

    /// Register (or look up) a minimal synthetic class with the given name
    /// and instance-field count, returning its `ClassId`.
    ///
    /// Unlike [`ensure_class_initialized`], this never fails: when the real
    /// `.class` file cannot be loaded it still produces a usable `ClassId`
    /// whose class declares `num_fields` instance fields. Native allocators
    /// MUST use this (rather than `ClassId::new(0)`) as the fallback class
    /// when allocating an object with a non-zero field count — allocating
    /// with `ClassId::new(0)` (`java/lang/Object`, which declares zero
    /// fields) produces an "undersized object layout" object that the GC's
    /// `get_field` bounds guard rejects on every field access.
    ///
    /// The default implementation falls back to `ClassId::new(0)` so mocks
    /// and non-VM contexts still compile; real VM contexts override it.
    fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId {
        let _ = (name, num_fields);
        ClassId::new(0)
    }

    /// Check if a ClassId represents an interface.
    fn is_interface_class(&self, class_id: ClassId) -> bool;

    /// T1.6.7 — `Thread.holdsLock(Object)`. Returns `true` iff the
    /// current thread currently holds the monitor for `obj`. Default
    /// implementation returns `false` so non-monitor-aware contexts
    /// (mocks, stubs) fall back to the spec-permitted "no" answer.
    fn current_thread_holds_lock(&self, _obj: ObjectRef) -> bool {
        false
    }

    /// T19_K2 — Register a native-spawned OS thread with the VM's
    /// `ThreadRegistry`.
    ///
    /// Used by event-loop schedulers (Vert.x / Netty / XNIO) that spawn
    /// their own carrier OS threads via `std::thread::spawn` rather than
    /// going through `Thread.start0`. Returning the thread ids these
    /// schedulers create through this entry point ensures:
    ///
    /// * the CLI's `wait_for_non_daemon_threads()` waits for them when
    ///   `daemon == false` (otherwise the VM exits as soon as `main()`
    ///   returns even though Quarkus / Keycloak's HTTP listeners are
    ///   still alive),
    /// * GC root scanning sees their stacks (T1.5.1 path),
    /// * JVMTI thread-list APIs see them.
    ///
    /// Parameters:
    /// * `name`     — thread label (shown in `ThreadInfo`, panic logs)
    /// * `daemon`   — `false` for Vert.x / Netty event loops, `true` for
    ///                truly background schedulers (XNIO IO threads, GC
    ///                workers)
    /// * `join_handle_ptr` — opaque `Box<JoinHandle<()>>` raw pointer.
    ///                The VM takes ownership and arranges for it to be
    ///                joined when `wait_for_non_daemon_threads()` runs.
    ///                Pass `0` to register without a join handle (the
    ///                caller is responsible for ensuring the thread
    ///                eventually terminates on its own).
    ///
    /// Returns the registered `ThreadId.0` (a u64) on success. A value
    /// of `0` indicates the call was a no-op (mock context or registry
    /// not available); callers should treat this as "thread spawned but
    /// not VM-tracked" — the OS thread still runs, it just won't keep
    /// the process alive.
    ///
    /// The default implementation is a no-op so mock contexts and
    /// any future trait consumers don't need to implement registry
    /// plumbing. The VM override (`vm/src/vm/vm_exec.rs`) wires it
    /// into `ThreadRegistry::register_with_daemon` + `set_join_handle`.
    fn register_native_thread(
        &mut self,
        _name: &str,
        _daemon: bool,
        _join_handle_ptr: usize,
    ) -> u64 {
        0
    }

    /// Return GC/STW hooks for a thread registered via
    /// [`Self::register_native_thread`].
    ///
    /// The returned object is intentionally independent of `&mut self` so a
    /// spawned host thread can carry it into its event loop and bracket native
    /// waits without a full `NativeContext`.
    fn native_thread_blocker(&self, _thread_id: u64) -> Option<Arc<dyn NativeThreadBlocker>> {
        None
    }

    /// T19_K2 — Mark a previously-registered native thread dead.
    ///
    /// Called from a native-spawned OS thread's exit path right before
    /// the OS thread's `JoinHandle` returns. Flips the registry's
    /// `alive` flag for the given id so `is_alive()` returns false and
    /// `alive_non_daemon_thread_ids()` no longer reports it. The
    /// `JoinHandle` is still kept by the registry — `join()` will
    /// observe the dead flag and return immediately.
    ///
    /// Default impl is a no-op.
    fn unregister_native_thread(&mut self, _thread_id: u64) {}

    /// T19_K2 — Attach a `Box<JoinHandle<()>>` raw pointer to an
    /// already-registered native thread.
    ///
    /// Used by event-loop schedulers that need to know the assigned
    /// `ThreadId` BEFORE spawning the OS thread (so the spawned closure
    /// can capture it and use it on exit). The two-phase API is:
    ///
    ///   1. Call `register_native_thread(name, daemon, 0)` to get the
    ///      `ThreadId.0` without an attached handle.
    ///   2. Spawn the OS thread; capture the id in its closure.
    ///   3. Call `attach_join_handle_to_native_thread(id, raw_ptr)` to
    ///      hand the `JoinHandle<()>` over to the registry.
    ///
    /// `join_handle_ptr` follows the same ownership protocol as
    /// `register_native_thread`: a raw `Box<JoinHandle<()>>` pointer
    /// the VM takes back as `Box::from_raw`.
    ///
    /// Returns `true` if the attach succeeded, `false` if `thread_id`
    /// was unknown (in which case the caller MUST reclaim the
    /// `Box<JoinHandle<()>>` via `Box::from_raw` or it leaks the OS
    /// thread).
    fn attach_join_handle_to_native_thread(
        &mut self,
        _thread_id: u64,
        _join_handle_ptr: usize,
    ) -> bool {
        false
    }

    /// T19_K4 — Attach a `java.lang.Thread` mirror to an
    /// already-registered native thread.
    ///
    /// Used by event-loop schedulers (Vert.x / Netty / XNIO) that
    /// register their carrier OS thread via
    /// [`Self::register_native_thread`] but also need a real
    /// `java.lang.Thread` mirror so:
    ///
    ///  * `Thread.currentThread()` resolves to the right object when
    ///    Java-side code runs on the event-loop carrier (e.g. a
    ///    Runnable that consults the thread name),
    ///  * `ThreadRegistry::find_thread_id_by_thread_obj` works
    ///    cross-thread (so other code that has the mirror handle can
    ///    locate the `ThreadId`),
    ///  * the carrier thread shows up in
    ///    `ThreadRegistry::alive_thread_objects()` (and therefore in
    ///    `Thread.enumerate()` / JVMTI thread-list listings).
    ///
    /// `thread_id` must be a value previously returned by
    /// [`Self::register_native_thread`] / the two-phase
    /// [`Self::attach_join_handle_to_native_thread`] flow. The
    /// `java_thread_obj` should be a freshly-allocated synthetic
    /// `java.lang.Thread` mirror with `name` set on slot 0 and
    /// (optionally) `tid` on slot 2 — `register_native_thread`
    /// already stamped the registry entry, this call just attaches
    /// the mirror so look-ups by `ObjectRef` succeed.
    ///
    /// Returns `true` if the thread id was found and the mirror was
    /// stored, `false` if the id was unknown. Default impl is a
    /// no-op so mock contexts don't need to model a heap.
    fn set_native_thread_java_obj(&mut self, _thread_id: u64, _java_thread_obj: ObjectRef) -> bool {
        false
    }

    /// Emit a `jdk.VirtualThreadPinned` JFR event for the current thread.
    /// Called when a pinned virtual thread is about to block its carrier.
    ///
    /// Round-4: `reason` is `&'static str` (JEP 491 pin-reason taxonomy:
    /// "Synchronized", "Native", "Thread.sleep while pinned", ...).
    fn emit_virtual_thread_pinned_jfr(&mut self, _reason: &'static str) {}

    // -- VM stats methods (for JMX) --

    /// Effective available-processor count, honoring container/cgroup CPU
    /// limits when `-XX:+UseContainerSupport` is active. This backs
    /// `Runtime.availableProcessors()` and the JMX `OperatingSystemMXBean`.
    ///
    /// The default (used by mock/test contexts) returns the host hardware
    /// thread count; the VM overrides it to prefer the cgroup-derived count
    /// from `VmConfig::container_effective_processors` when present.
    fn available_processor_count(&self) -> i32 {
        std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(1)
    }

    /// Maximum heap size in bytes, as reported by `Runtime.maxMemory()` and the
    /// JMX `MemoryMXBean`. The default (mock/test contexts) is the historical
    /// 256 MiB placeholder; the VM overrides it to return the configured
    /// `-Xmx` (which is itself container-aware once sized from a cgroup limit).
    fn max_heap_bytes(&self) -> i64 {
        256 * 1024 * 1024
    }

    /// Initial heap size in bytes, as reported by the JMX `MemoryMXBean`'s
    /// heap `MemoryUsage.getInit()`. The default (mock/test contexts) mirrors
    /// `max_heap_bytes`'s historical placeholder; the VM overrides it to
    /// return the configured `-Xms` (`VmConfig::initial_heap_size`).
    fn initial_heap_bytes(&self) -> i64 {
        16 * 1024 * 1024
    }

    /// Returns the number of classes currently loaded in the VM.
    fn loaded_class_count(&self) -> usize;

    /// Cumulative classes reclaimed by class-loader unloading.
    fn unloaded_class_count(&self) -> u64 {
        0
    }

    /// Returns the cumulative number of GC collections that have occurred.
    fn gc_collection_count(&self) -> u64;

    /// Force a garbage collection cycle and run pending finalizers.
    /// Used by `System.gc()` / `Runtime.gc()`.
    fn force_gc(&mut self);

    /// Read a static field value by class and field index.
    ///
    /// `field_index` must be in range for `class_id`'s static field block
    /// (typically resolved via [`static_field_index_by_name`](Self::static_field_index_by_name)).
    /// The trait does NOT validate it (M4a): the implementation MUST
    /// bounds-check and MUST NOT read out of range — fail safe on a bad index.
    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value;

    /// Write a static field value by class and field index.
    ///
    /// `field_index` must be in range for `class_id`'s static field block
    /// (typically resolved via [`static_field_index_by_name`](Self::static_field_index_by_name)).
    /// The trait does NOT validate it (M4a): the implementation MUST
    /// bounds-check and MUST NOT write out of range — fail safe on a bad index.
    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value);

    /// Write a static field by class name and field name.
    /// Resolves the class and finds the static field index by name.
    /// No-op if the class or field cannot be found.
    fn set_static_field_by_name(&mut self, class_name: &str, field_name: &str, value: Value) {
        if let Some(class_id) = self.class_id_by_name(class_name) {
            if let Some(idx) = self.static_field_index_by_name(class_id, field_name) {
                self.set_static_field(class_id, idx, value);
            }
        }
    }

    /// Find the static field index for a field by name.  Returns `None` if the
    /// class doesn't have a static field with that name.
    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let _ = (class_id, field_name);
        None
    }

    /// Get the file descriptor table for I/O operations.
    fn fd_table(&self) -> &crate::fd_table::FileDescriptorTable;

    // -- Panama FFI (JEP 454, Java 25) --

    /// Allocate off-heap memory. Returns (alloc_id, raw_pointer) or None on failure.
    ///
    /// # Security (M4b — raw pointer use-after-free)
    ///
    /// The returned `*mut u8` is a *bare base pointer* with no length and no
    /// lifetime tie to `alloc_id`: a subsequent
    /// [`free_native_memory`](Self::free_native_memory) of the same id frees
    /// the block, leaving any retained copy of this pointer **dangling**.
    /// Callers MUST NOT retain the bare pointer across any operation that
    /// could free the allocation, and MUST bounds-check their own offset/len
    /// before dereferencing. For validated access, resolve through the
    /// implementation's [`NativeMemoryTable`](crate::ffi::NativeMemoryTable)
    /// (`get_ptr_checked` / generation-tagged `get_ptr_checked_handle`) rather
    /// than caching this pointer.
    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)>;

    /// Free off-heap memory by allocation ID.
    ///
    /// # Security (M4b)
    ///
    /// After this call any `*mut u8` previously obtained for `alloc_id` via
    /// [`allocate_native_memory`](Self::allocate_native_memory) is dangling;
    /// dereferencing it is undefined behaviour. The backing
    /// [`NativeMemoryTable`](crate::ffi::NativeMemoryTable) advances its
    /// generation on free so a recycled id cannot be confused with this
    /// freed allocation by a stale handle.
    fn free_native_memory(&mut self, alloc_id: i64);

    /// Load a native library. Returns library index or error.
    fn load_native_library(
        &mut self,
        path: &str,
    ) -> Result<i64, cratonvm_types::error::MethodCallFailed>;

    /// Find a symbol in a loaded library. Returns the symbol address.
    /// lib_index -1 means search the default/system library.
    fn find_native_symbol(&self, lib_index: i64, name: &str) -> Option<usize>;

    /// Register an upcall entry (Java callback for C). Returns the slot index.
    ///
    /// # Security (M4c — confused deputy via slot reuse)
    ///
    /// The returned bare slot index does NOT distinguish between successive
    /// occupants of a reused slot: after the registration is dropped and the
    /// slot re-registered, the same index resolves to a *different* Java
    /// object. The backing
    /// [`UpcallTable`](crate::ffi::UpcallTable) generation-tags every
    /// registration; implementations that hand a trampoline index to native
    /// code SHOULD carry the generation (see
    /// `UpcallTable::register_handle` / `get_checked`) so a stale trampoline
    /// fails closed rather than invoking the wrong callback.
    fn register_upcall(&mut self, entry: crate::ffi::UpcallEntry) -> usize;

    /// Get upcall info by slot index. Returns (target, param_kinds, return_kind).
    ///
    /// # Security (M4c)
    ///
    /// Resolving by bare `slot` cannot detect slot reuse. If the slot was
    /// removed and re-registered since the trampoline was minted, this may
    /// return info for a *different* callback (confused deputy). Where the
    /// caller holds a generation-tagged handle, prefer resolving it through
    /// the implementation's [`UpcallTable::get_checked`](crate::ffi::UpcallTable::get_checked)
    /// so a stale handle fails closed.
    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)>;

    /// Record a JFR thread sleep event. Called by Thread.sleep implementations.
    /// Default is no-op; the VM overrides this with the real JFR recorder.
    fn record_thread_sleep(&mut self, _sleep_nanos: i64, _actual_duration_nanos: u64) {}

    /// Record a JFR file read event.
    fn record_file_read(&mut self, _fd: i32, _bytes_read: i64, _eof: bool, _duration_nanos: u64) {}

    /// Record a JFR file write event.
    fn record_file_write(&mut self, _fd: i32, _bytes_written: i64, _duration_nanos: u64) {}
}

/// Complete native-call capability set. Native implementations are split
/// into the narrow supertraits above; this marker exists only at the
/// legacy callback ABI boundary.
pub trait NativeContext:
    NativeSystemAccess + NativeExceptionAccess + NativeGpuAccess
{
}

impl<T> NativeContext for T where
    T: NativeSystemAccess + NativeExceptionAccess + NativeGpuAccess + ?Sized
{
}



/// Annotation data extracted from class file attributes.
#[derive(Debug, Clone)]
pub struct AnnotationData {
    /// The annotation type descriptor (e.g. "Ljava/lang/Override;")
    pub type_descriptor: String,
    /// Element-value pairs: (name, value_representation)
    pub elements: Vec<(String, AnnotationElementValue)>,
}

/// A tree of TYPE_USE annotations mirroring the nested-generic shape of a
/// reified `Type`, keyed by `type_argument_index` at each nesting level
/// (JVMS 4.7.20.2's `type_path`).
///
/// `.anns` holds the annotations whose `type_path` ends exactly at this
/// node; `.children[i]` is the subtree reached by descending into the i-th
/// type argument. A plain (non-generic) annotated type has `anns` populated
/// and `children` empty; a nested generic like
/// `ValueExtractor<ArgumentValue<@ExtractedValue ?>>` needs two levels:
/// `children[0]` (the `ArgumentValue<?>` argument) has its own
/// `children[0]` (the wildcard `?`) carrying `@ExtractedValue` in `anns`.
#[derive(Debug, Clone, Default)]
pub struct TypeArgAnnotations {
    /// Annotations directly on this node (empty remaining `type_path`).
    pub anns: Vec<AnnotationData>,
    /// Per-type-argument subtrees, indexed by `type_argument_index`.
    pub children: Vec<TypeArgAnnotations>,
}

/// A simplified representation of an annotation element value.
#[derive(Debug, Clone)]
pub enum AnnotationElementValue {
    /// A constant int/byte/char/short/boolean value.
    Int(i32),
    /// A constant long value.
    Long(i64),
    /// A constant float value.
    Float(f32),
    /// A constant double value.
    Double(f64),
    /// A string value.
    StringVal(String),
    /// An enum constant: (type_descriptor, const_name).
    Enum(String, String),
    /// A class literal: descriptor string.
    Class(String),
    /// A nested annotation.
    Annotation(AnnotationData),
    /// An array of values.
    Array(Vec<AnnotationElementValue>),
}

/// An entry in a captured Java stack trace.
///
/// WP1.9: `byte_code_index` (-1 when unknown, e.g. for native frames or
/// synthetic bootstrap frames) is populated from each frame's `last_instr_pc`
/// when available and used by `java.lang.StackWalker.StackFrame.getByteCodeIndex()`.
#[derive(Debug, Clone)]
pub struct StackTraceEntry {
    pub class_name: Arc<str>,
    pub method_name: Arc<str>,
    pub source_file: Option<Arc<str>>,
    pub line_number: i32, // -1 for unknown, -2 for native methods
    /// Bytecode index of the last-executed instruction in the frame's method.
    /// `-1` for unknown / native. Used by `StackFrame.getByteCodeIndex()`.
    pub byte_code_index: i32,
    /// The frame's own `ClassId`, when captured directly from a live
    /// interpreter frame (`Frame::class_id`) rather than synthesized.
    /// `StackFrame.getDeclaringClass()`/`declaringClass()` implementations
    /// MUST prefer this over re-resolving `class_name` through a global
    /// name-keyed lookup (`class_id_by_name`/`find_class_by_name`): a class
    /// executing its OWN `<clinit>` is guaranteed loaded (this ClassId is
    /// live proof of that) but is not reliably found by a fresh by-name
    /// lookup made from deep inside that same `<clinit>` -- observed via
    /// `SpringFactoriesLoader`/`EntityManagerFactoryUtils` invoking
    /// `LogFactory.getLog()` from their own static initializers, which
    /// walks the stack (log4j-api's `StackLocator`) back to that exact
    /// self-frame and NPEs when `getDeclaringClass()` falls back to null.
    /// `None` only for synthetic entries with no backing interpreter frame.
    pub class_id: Option<ClassId>,
    /// Index of this frame's method within its declaring class's
    /// `Class::methods` list, when the capture path had a `ClassStore` borrow
    /// and resolved it. `None` for synthetic entries and for the deliberately
    /// lock-free cross-thread snapshot (`stackwalker::capture_frames_no_lines`,
    /// which takes no `ClassStore` by design).
    ///
    /// ARCH-2026-07-26 (`cross-owner-closeout`, request CR-SW-1 of
    /// `docs/internal/arch-2026-07-26/stackwalk-and-vtable.md`). This exists so
    /// that *deferred* line-number resolution can be **exact**. `class_name` +
    /// `method_name` + `byte_code_index` are not enough: a class may declare an
    /// overload set under one name, the members have different
    /// `LineNumberTable`s, and picking the wrong one prints a line from the
    /// wrong method body. Carrying the index (rather than the descriptor) keeps
    /// the entry `Arc`-free and makes both ends O(1).
    ///
    /// It is an *index*, never a borrow, and it is **never trusted on its own**:
    /// `stackwalker::resolve_line_numbers_in_place` re-reads
    /// `class.methods[idx]` from the live `ClassStore` and re-checks that its
    /// name equals `method_name` before using it, so a class redefinition that
    /// reorders or removes methods fails closed to "unknown line" rather than
    /// resolving against the wrong body. `ClassId`s are monotonic and never
    /// reused (`ClassStore::remove` leaves a tombstone), so a stale entry can
    /// only ever miss, never alias a different class.
    pub method_index: Option<u32>,
}

/// Callback signature for native method implementations.
///
/// # Arguments
/// - `ctx` — mutable reference to the VM context (implements `NativeContext`)
/// - `args` — method arguments: for instance methods, `args[0]` is the receiver (`this`)
///
/// # Returns
/// - `Ok(Some(value))` — method returned a value
/// - `Ok(None)` — method returned void
/// - `Err(MethodCallFailed)` — method threw an exception or had an internal error
pub type NativeCallback = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

/// Event emitted by the native `ByteArrayOutputStream` implementation before
/// it falls back to growing its in-heap byte buffer.
///
/// Most streams are ordinary buffers, so observers return `Ok(false)` and the
/// I/O crate performs its normal operation.  A small number of bridge APIs
/// expose a ByteArrayOutputStream-shaped Java object while the bytes must go to
/// a native sink immediately (legacy fixed-length HttpURLConnection is one).
/// Keeping that opt-in at the API boundary avoids making native-io depend on a
/// higher-level protocol crate.
pub enum BaosEvent {
    WriteByte(u8),
    /// A Java byte-array slice.  The observer must read it through `ctx` only
    /// after deciding it owns this stream, keeping the ordinary BAOS hot path
    /// allocation-free.
    WriteArray {
        array: ObjectRef,
        offset: usize,
        len: usize,
    },
    Flush,
    Close,
}

/// Return `Ok(true)` when the event was consumed and the ordinary BAOS path
/// must be skipped; `Ok(false)` leaves the receiver's normal buffering intact.
pub type BaosEventHook =
    fn(&mut dyn NativeContext, ObjectRef, BaosEvent) -> Result<bool, MethodCallFailed>;

static BAOS_EVENT_HOOK: OnceLock<BaosEventHook> = OnceLock::new();

/// Install the process-wide optional BAOS bridge hook.  Registration happens
/// during native bootstrap; repeated registrations are harmless because the
/// first (and only) bridge implementation wins.
pub fn install_baos_event_hook(hook: BaosEventHook) {
    let _ = BAOS_EVENT_HOOK.set(hook);
}

/// Offer a BAOS event to the optional bridge hook.
pub fn dispatch_baos_event(
    ctx: &mut dyn NativeContext,
    stream: ObjectRef,
    event: BaosEvent,
) -> Result<bool, MethodCallFailed> {
    match BAOS_EVENT_HOOK.get() {
        Some(hook) => hook(ctx, stream, event),
        None => Ok(false),
    }
}

/// Classification of a registered native method.
///
/// The native overlay is three different things wearing one uniform; this tag
/// records which is which so tooling (census dump, differential harness) and
/// the dispatcher can treat them differently:
///
/// - [`NativeKind::Intrinsic`] — a correct fast-path for a hot method (e.g.
///   `Math.abs`, `String.length`). Returns the same answer the real bytecode
///   would, just faster. Always kept; never gated.
/// - [`NativeKind::Bridge`] — a native the VM genuinely needs because it cannot
///   run the real thing: OS syscalls, `sun.*` internals depending on VM state,
///   classes with no real bytecode. It *is* the real behavior. Never gated.
/// - [`NativeKind::SyntheticStub`] — a fake: placeholder/approximate/wrong
///   return values, fabricated objects, or "fake main" launcher short-circuits.
///   These shadow correct real bytecode and are the removal target. Gateable.
///
/// The registry's `current_category` defaults to `SyntheticStub` — the
/// conservative choice, so anything an author forgets to tag stays visible to
/// the audit and gateable, never silently trusted.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum NativeKind {
    Intrinsic,
    Bridge,
    SyntheticStub,
}

impl NativeKind {
    /// Stable lowercase name for JSON census output.
    pub fn as_str(self) -> &'static str {
        match self {
            NativeKind::Intrinsic => "intrinsic",
            NativeKind::Bridge => "bridge",
            NativeKind::SyntheticStub => "synthetic-stub",
        }
    }
}

/// Registry of native method implementations.
///
/// Maps (class, method, descriptor) triples to Rust function callbacks.
/// Uses pre-computed 128-bit hash keys (`(u64, u64)` pair from two
/// independent FNV-1a passes) for zero-allocation lookups. With a
/// 128-bit keyspace the birthday-collision probability for the ~3,000
/// registrations we do at boot is on the order of 1e-32, so no runtime
/// collision check is needed on the hot path.
/// Width of the [`NativeMethodRegistry::generation`] band reserved for one
/// registry. Far larger than the ~3,100 natives registered at boot, so
/// `registry_epoch + slots.len()` never leaves its own band; ~4,096 registries
/// can be constructed in a process before the `u32` counter wraps and bands
/// could alias (a test binary building thousands of VMs would, at worst, see a
/// stale memo re-validated against the wrong registry — hence the additional
/// full-name verification on every resolve).
const REGISTRY_EPOCH_STRIDE: u32 = 1 << 20;

/// Hands each `NativeMethodRegistry` its own generation band. Starts at one
/// stride so generation `0` is never a live value and stays usable as the
/// "never resolved" sentinel in `NativeCallSite`.
static NEXT_REGISTRY_EPOCH: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(REGISTRY_EPOCH_STRIDE);

/// One entry of the registry's dense slot table — the single place a resolved
/// native lives.
///
/// A [`NativeMethodId`](crate::NativeMethodId) is an index into
/// `NativeMethodRegistry::slots`. Redeeming a handle is therefore a
/// bounds-checked array load, with none of the three string hashes `find`
/// pays. `reg_index` points back at the `registrations` entry that owns this
/// slot; it is what makes the **full-name verification** on every digest hit
/// possible (see `slot_index_for_key`), which is the property the deleted
/// `class_manager::name_to_id` map lacked.
#[derive(Copy, Clone)]
struct NativeSlot {
    callback: NativeCallback,
    kind: NativeKind,
    /// Index into `registrations` of the triple that currently owns this slot.
    /// Re-registration of the same triple rewrites this in place, so the slot
    /// index (and thus any handle already handed out) stays valid.
    reg_index: u32,
}

pub struct NativeMethodRegistry {
    /// Dense slot table. `slots[i]` is the resolved native for
    /// `NativeMethodId(i)`. Append-only: entries are updated in place on
    /// re-registration and never removed, which is what makes handles stable.
    slots: Vec<NativeSlot>,
    /// 128-bit `(class, method, descriptor)` digest -> slot index. A hit here
    /// is a *candidate*, not an answer: `slot_index_for_key` re-checks the full
    /// triple before returning the slot.
    slot_by_key: FxHashMap<(u64, u64), u32>,
    /// [`native_class_hash`] of every class name that has ever been passed to
    /// [`register`](Self::register) — the negative-lookup prefilter for
    /// [`slot_for_exact`](Self::slot_for_exact). See `native_class_hash` for
    /// why this exists and what it is worth.
    ///
    /// Soundness is one-directional and cheap to keep: a class absent from
    /// this set provably has no registration (the single production writer is
    /// `register`, which inserts here in the same statement sequence that
    /// pushes to `registrations`), so answering `None` for it is exact. A hash
    /// collision can only add a FALSE POSITIVE, which falls through to the
    /// full digest + name verification below and is therefore harmless.
    /// Registrations are never removed, so the set never needs to shrink.
    classes_with_natives: FxHashSet<(u64, u64)>,
    /// Process-unique base for [`generation`](Self::generation).
    ///
    /// Without this, `generation()` would just be `slots.len()`, and two
    /// *different* registries with the same number of registrations would
    /// report the same generation — so a `NativeCallSite` that outlives one
    /// registry (a `static` cell in a test binary that builds several `SharedVm`s,
    /// say) could accept a memo taken against a different registry and redeem a
    /// slot index that means something else. Banding each registry into its own
    /// `REGISTRY_EPOCH_STRIDE`-wide range makes that a re-resolve instead of a
    /// wrong answer.
    registry_epoch: u32,
    /// Append-only registration log: the original `(class, method, descriptor)`
    /// triples, kept as `Box<str>` rather than `String` to minimize per-entry
    /// overhead. This replaces the previous `FxHashMap<u64, String>` reverse map
    /// which was inserted into on every `register()` purely to enable collision
    /// detection.
    ///
    /// Two consumers:
    ///
    ///  1. `alias_class` (a rare, slow-path operation called a handful of times
    ///     at boot to copy interface-method registrations down to subinterface
    ///     class names).
    ///  2. **Full-name verification on every digest hit.** Each `NativeSlot`
    ///     carries the `reg_index` of the triple it was registered under, and
    ///     `slot_index_for_key` compares all three strings before returning the slot.
    ///     Without this, an FNV-1a collision would silently hand back the wrong
    ///     callback — the exact defect the `class_manager::name_to_id` shadow map
    ///     had (see `classloading/src/class_manager.rs`, `loaded_classes` field
    ///     doc, "Round 4 audit fix (CRIT)") before it was deleted.
    ///
    /// Index-parallel with `categories`. Only *accepted* registrations are
    /// pushed — every drop arm in `register()` returns before this point.
    registrations: Vec<(Box<str>, Box<str>, Box<str>)>,
    /// AUDIT 2026-05-17 (Fix 5): O(1) index keyed by the 128-bit hash
    /// of `(method_name, descriptor)` (class portion omitted). Used by
    /// `find_by_method_descriptor` to avoid the O(N) linear scan over
    /// `registrations`. Built incrementally on every `register()`.
    by_method_desc: FxHashMap<(u64, u64), NativeCallback>,
    /// Category aligned with `registrations` (index-parallel), for
    /// `dump_registrations` / census output.
    categories: Vec<NativeKind>,
    /// The category applied to subsequent `register()` calls. Scoped via
    /// `with_category`. Defaults to `SyntheticStub` (conservative).
    current_category: NativeKind,
    /// Strict "no synthetic stubs" mode. When true, `register()` DROPS any
    /// registration whose `current_category` is `SyntheticStub` — it is never
    /// inserted, so a call to that method falls through to real JDK bytecode
    /// (if present) or a clear `NoSuchMethodError`/unimplemented diagnostic,
    /// never to a fake. This is the comprehensive "remove synthetic stubs
    /// completely" switch: it makes the entire SyntheticStub bucket vanish from
    /// the build at once. Enabled via the `CRATONVM_NO_STUBS` env var (read
    /// once in `new()`). Off by default — opt-in, because some apps currently
    /// limp on these fakes and dropping them surfaces real gaps as clear
    /// errors. `Intrinsic` and `Bridge` registrations are never affected.
    /// See docs/synthetic-vs-real-explained.md.
    drop_synthetic_stubs: bool,
    /// Real-JDK mode: drop synthetic natives whose hardcoded field-slot layout
    /// corrupts the *real* JDK object. Currently `java/util/StringJoiner`,
    /// `java/io/StringReader`, `java/util/EnumSet`, `LinkedBlockingDeque`, and
    /// `ScheduledThreadPoolExecutor`. `StringJoiner` is registered by
    /// `native-collections::register_string_joiner_natives` with a fake
    /// 5-field layout (delim/prefix/suffix/elements-ArrayList/emptyValue) but
    /// bundles into `register_collections_natives` — a function real-JDK mode
    /// calls for the side-table collection natives. On a real StringJoiner (7
    /// fields: prefix/delimiter/suffix/elts[]/size/len/emptyValue) the synthetic
    /// `add` reads slot 3 (real `elts`, null) and no-ops, so `size` never moves
    /// and `toString` renders just prefix+suffix. The real bytecode is
    /// self-contained and correct, so we drop the synthetic surface and let it
    /// run. `EnumSet` has the same problem in a more dangerous form: the
    /// fallback native surface manufactures an abstract `java/util/EnumSet`
    /// receiver with a two-field synthetic layout. In real-JDK mode that
    /// receiver then bypasses or misroutes `add`/`size`/`iterator`, so
    /// `EnumSet.of(...)` and `allOf(...)` on app enums return an empty object
    /// with `iterator() == null`. Dropping the native surface lets the JDK
    /// factories allocate the concrete `RegularEnumSet`/`JumboEnumSet` classes,
    /// which CratonVM's real collection paths already handle. `LinkedBlockingDeque`
    /// is registered as a SyntheticStub fallback by native-collections with the
    /// four-slot fake blocking-queue layout; on a real JDK deque, that constructor
    /// leaves real final fields such as `lock`/`notEmpty` null, and Tomcat's
    /// `WriteBuffer.clear()` then fails in `LinkedBlockingDeque.clear()`.
    /// `StringReader` has the same drift on modern JDKs: the real class wraps a
    /// final `Reader r`, while the synthetic native constructor writes the old
    /// `(content,pos,length)` slots, leaving `r` null before `mark()` delegates.
    /// `Pattern`/`Matcher` have the same drift: the legacy regex natives allocate
    /// real-layout objects but write the old synthetic slots, leaving fields such
    /// as `Matcher.locals` uninitialized.
    /// Same mechanism as the `CRATONVM_REAL_NET_SOCKETS` Socket drop above,
    /// but set by `vm_init`'s real-JDK arm (not env-gated). Off in synthetic mode
    /// (there the fake layout *is* the object layout). Surfaced via Spring
    /// `UriComponentsBuilder.pathSegment`, which dropped the URL path segment
    /// (`ReleaseScheduleTests`).
    drop_real_layout_synthetic: bool,
    /// PERF (native-ring lazy name map): deferred `cb_ptr -> registrations[idx]`
    /// index for the native-call ring's `fn-ptr → "class.method desc"` map.
    ///
    /// At boot ~3,100 natives are registered. The ring is **off by default**
    /// (`native_ring::is_enabled()` == false), so eagerly building the name
    /// string was pure waste: every `register()` paid a `format!` heap
    /// allocation **plus** a cross-module `name_map()` `Mutex` round-trip for a
    /// diagnostic that is almost never requested.
    ///
    /// We can't simply skip-while-disabled and re-populate later from nothing —
    /// boot registration happens *before* the watchdog arms recording, so the
    /// names would be lost (the WF32-fix rationale). But we DON'T need a second
    /// copy of the parts: the full triple is already persisted in
    /// `registrations` by the unconditional `registrations.push(...)` below.
    /// So we only record a cheap `cb_ptr -> index` here (one `FxHashMap` insert,
    /// no allocation, no cross-module lock), and materialize the actual name
    /// strings lazily via [`flush_native_ring_names`](Self) when — and only
    /// when — a diagnostic dump is actually requested (the ring is enabled).
    ///
    /// First-write-wins (`entry().or_insert(idx)`) to match the eager path's
    /// `register_name`/`or_insert_with` semantics: if the same `fn` pointer is
    /// registered for multiple triples (e.g. compiler function merging), the
    /// first-seen triple is the resolved name, exactly as before.
    name_index: FxHashMap<usize, usize>,
}

impl NativeMethodRegistry {
    pub fn new() -> Self {
        // ~3,100 native methods are registered at boot; size the maps
        // up front so `register()` does not repeatedly rehash/grow.
        const BOOT_REGISTRATION_HINT: usize = 4096;
        Self {
            slots: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            slot_by_key: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            classes_with_natives: FxHashSet::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            registry_epoch: NEXT_REGISTRY_EPOCH.fetch_add(
                REGISTRY_EPOCH_STRIDE,
                std::sync::atomic::Ordering::Relaxed,
            ),
            registrations: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            by_method_desc: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
            categories: Vec::with_capacity(BOOT_REGISTRATION_HINT),
            current_category: NativeKind::SyntheticStub,
            // Read once at construction. `CRATONVM_NO_STUBS` (any non-empty
            // value) enables strict mode: synthetic-stub registrations are
            // dropped so calls hit real bytecode or a clear error.
            drop_synthetic_stubs: cratonvm_types::flags::runtime_var_os("CRATONVM_NO_STUBS")
                .is_some_and(|v| !v.is_empty()),
            drop_real_layout_synthetic: false,
            // PERF: deferred native-ring name index. Sized like the other boot
            // maps so the ~3,100 boot inserts don't rehash/grow.
            name_index: FxHashMap::with_capacity_and_hasher(
                BOOT_REGISTRATION_HINT,
                Default::default(),
            ),
        }
    }

    /// Override the strict no-stubs mode programmatically (e.g. for tests or a
    /// CLI flag), independent of the `CRATONVM_NO_STUBS` env var. Call before
    /// the `register_*` population pass. See [`drop_synthetic_stubs`](Self).
    pub fn set_drop_synthetic_stubs(&mut self, drop: bool) {
        self.drop_synthetic_stubs = drop;
    }

    /// Whether strict no-stubs mode is active (synthetic-stub registrations are
    /// being dropped).
    pub fn drops_synthetic_stubs(&self) -> bool {
        self.drop_synthetic_stubs
    }

    /// Enable real-JDK-mode dropping of synthetic natives whose hardcoded
    /// field-slot layout corrupts the real JDK object (see
    /// [`drop_real_layout_synthetic`](Self)). Call before the `register_*`
    /// population pass in real-JDK mode.
    pub fn set_drop_real_layout_synthetic(&mut self, drop: bool) {
        self.drop_real_layout_synthetic = drop;
    }

    /// Set the category applied to all subsequent `register()` calls until
    /// changed again. Prefer [`with_category`](Self::with_category) for a
    /// scoped set/restore.
    pub fn set_category(&mut self, kind: NativeKind) {
        self.current_category = kind;
    }

    /// The category currently applied to new registrations. Useful for a
    /// save/restore around a nested registrar.
    pub fn current_category(&self) -> NativeKind {
        self.current_category
    }

    /// Run `f` with `current_category` set to `kind`, restoring the previous
    /// category afterwards. This is how a whole `register_*` function tags all
    /// of its registrations without touching individual `register()` calls.
    pub fn with_category(&mut self, kind: NativeKind, f: impl FnOnce(&mut Self)) {
        let prev = self.current_category;
        self.current_category = kind;
        f(self);
        self.current_category = prev;
    }

    /// The category a native was registered under, or `None` if no native is
    /// registered for this exact triple. O(1).
    #[inline]
    pub fn kind_of(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeKind> {
        self.slot_for_exact(class_name, method_name, descriptor)
            .map(|slot| slot.kind)
    }

    /// Snapshot of every registration as `(class, method, descriptor, kind)`,
    /// for the `--dump-native-registry` census. Order follows registration
    /// order; callers sort for diff-stable output.
    pub fn dump_registrations(&self) -> Vec<(&str, &str, &str, NativeKind)> {
        self.registrations
            .iter()
            .zip(self.categories.iter())
            .map(|((c, m, d), k)| (c.as_ref(), m.as_ref(), d.as_ref(), *k))
            .collect()
    }

    /// Register a native method implementation.
    pub fn register(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        callback: NativeCallback,
    ) {
        // Strict no-stubs mode: drop synthetic-stub registrations entirely so
        // the call falls through to real bytecode or a clear error instead of a
        // fake. Bridges and intrinsics are always registered. (See the
        // `drop_synthetic_stubs` field doc.)
        if self.drop_synthetic_stubs && self.current_category == NativeKind::SyntheticStub {
            // CRATONVM_DBG_DROPPED_STUBS=1: list every registration this mode
            // silently drops. Added 2026-07-14 while chasing a real-JDK-mode
            // bootstrap regression (`InternalError: null property: java.home`)
            // that traced back to a whole register_* function's worth of
            // permanent bridges (java.util.Properties' side-table natives)
            // being mis-tagged SyntheticStub by inheriting the wrong ambient
            // category at one of its call sites — this made the drop visible
            // in seconds instead of a multi-round bisection. Cheap/no-op when
            // unset; kept as a permanent diagnostic for the next occurrence.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DROPPED_STUBS").is_some() {
                eprintln!("[DROPPED-STUB] {class_name}.{method_name}{descriptor}");
            }
            return;
        }
        // NIO-SERVER-SOCKET (route 1): when `CRATONVM_REAL_NET_SOCKETS` is set,
        // drop EVERY synthetic native registered on `java/net/Socket` /
        // `java/net/ServerSocket` so the real JDK bytecode runs and drives the
        // real `sun/nio/ch/Net` path (native-io::net). A single registered
        // native shadows the class's bytecode at every interpreter dispatch
        // site (WP0.1 native-override-priority), and the synthetic surface is
        // registered from ~6 different functions (phases_early phase53,
        // phases_late p72, net_phase_e re1/re2, socket_channel, …) — filtering
        // here catches them all in one place. See `reference_server_socket_gap`.
        if real_net_sockets_enabled()
            && (class_name == "java/net/Socket"
                || class_name == "java/net/ServerSocket"
                // DoHead third root cause (2026-07-13): the WildFly bootstrap
                // batch (be6055605) added synthetic `javax/net/SocketFactory`
                // getDefault/createSocket natives (phases_early.rs phase52)
                // that hand out a natively-built `java/net/Socket`. Under
                // CRATONVM_REAL_NET_SOCKETS every java/net/Socket native is
                // dropped (above), so REAL Socket bytecode consumes that
                // object and reads its uninitialized/clobbered real fields:
                // NPE `"socketLock" is null`, bogus `SocketException: Socket
                // is closed` (the port int lands on `state` and can satisfy
                // the CLOSED bit), `NoSuchMethodError:
                // java/lang/String.setOption` (the host String lands on
                // `impl`) — the Tomcat `TestHttpServletDoHead*`
                // testDoHeadHttp2 144/144 cluster
                // (`Http2TestBase.openClientConnection` →
                // `Socket.setSoTimeout`). Drop the factory natives too so
                // real `SocketFactory`/`DefaultSocketFactory` bytecode
                // constructs sockets through the real `Socket` constructors.
                // NOT `javax/net/ServerSocketFactory` — its
                // createServerSocket natives delegate to real constructors
                // and are layout-correct. (Root-cause analysis shared with
                // the concurrent dohead-third-cause session; landed here to
                // complete the DoHead family fix.)
                || class_name == "javax/net/SocketFactory"
                // A legacy SSLSocketFactory stub returns a two-slot Socket.
                // Keep P68's later Bridge registrations: they perform TLS and
                // produce a layout-correct SSLSocket.
                || (class_name == "javax/net/ssl/SSLSocketFactory"
                    && self.current_category == NativeKind::SyntheticStub))
        {
            return;
        }
        // REAL-FORKJOINPOOL (opt-in): drop synthetic ForkJoinPool natives so the
        // real JDK pool bytecode runs (real init + workers, real
        // invokeAll/submit). See `real_forkjoinpool_enabled`.
        //
        // EXCEPTION — keep the synthetic eager-inline `execute`: under the real
        // pool, work submitted via `execute()` runs on a worker thread, and
        // CratonVM's cross-worker memory ordering doesn't reliably publish an
        // object-reference field (e.g. `CompletableFuture.result`) written by
        // one worker to a dependent task on another worker — so
        // `CompletableFuture.*Async` (which schedules every stage via
        // `execute`) reads a stale-null upstream result. Keeping `execute`
        // eager-inline (run on the caller) makes those stages run caller-side,
        // avoiding the cross-worker read, so CF keeps working WHILE the real
        // pool services Weld's `invokeAll`. `ForkJoinWorkerThread` natives are
        // also kept (the real pool needs them).
        //
        // Keep a small Bridge-only pool surface that real-JDK mode cannot
        // safely execute as bytecode under GC stress. These entries are not
        // synthetic layout shims; they are VM policy bridges registered by the
        // real-JDK native path and explicitly forced by the interpreter.
        let keep_real_forkjoinpool_bridge =
            self.current_category == NativeKind::Bridge
                && class_name == "java/util/concurrent/ForkJoinPool"
                && matches!(
                (method_name, descriptor),
                ("commonPool", "()Ljava/util/concurrent/ForkJoinPool;")
                    | (
                        "getFactory",
                        "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;",
                    )
                    | ("getParallelism", "()I")
                    | ("getCommonPoolParallelism", "()I")
                    | ("invoke", "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;")
                    | (
                        "submit",
                        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    | (
                        "externalSubmit",
                        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    // submit(Callable)/submit(Runnable)/submit(Runnable, T): left off
                    // the original allow-list, so real bytecode ran them against a pool
                    // whose commonPool() shortcut never populates queues/runState/mode —
                    // RejectedExecutionException at submissionQueue() (RealFjp.java).
                    | (
                        "submit",
                        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    | (
                        "submit",
                        "(Ljava/lang/Runnable;)Ljava/util/concurrent/ForkJoinTask;",
                    )
                    | (
                        "submit",
                        "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/ForkJoinTask;",
                    )
            );
        if real_forkjoinpool_enabled()
            && class_name == "java/util/concurrent/ForkJoinPool"
            && method_name != "execute"
            && !keep_real_forkjoinpool_bridge
        {
            return;
        }
        // REAL-FORKJOINPOOL (opt-in): drop ordinary synthetic FJP task-family
        // natives so real JDK task bytecode can remain coherent with real pool
        // bootstrap state. The exception is the Bridge subset below: those
        // fork/join/get/result helpers are a deliberate VM policy surface used
        // by the real-FJP GC-stress lane. They share one side-table with the
        // pool Bridge methods above, and that side-table is scanned/remapped by
        // GC; letting `ForkJoinTask.fork()` fall through to bytecode reaches the
        // real WorkQueue/CAS path and reopens the residual timeout/corruption
        // face.
        let keep_real_forkjointask_bridge = self.current_category == NativeKind::Bridge
            && matches!(
                class_name,
                "java/util/concurrent/ForkJoinTask"
                    | "java/util/concurrent/RecursiveTask"
                    | "java/util/concurrent/RecursiveAction"
            )
            && matches!(
                (method_name, descriptor),
                ("fork", "()Ljava/util/concurrent/ForkJoinTask;")
                    | ("join", "()Ljava/lang/Object;")
                    | ("invoke", "()Ljava/lang/Object;")
                    | ("get", "()Ljava/lang/Object;")
                    | (
                        "get",
                        "(JLjava/util/concurrent/TimeUnit;)Ljava/lang/Object;"
                    )
                    | ("getRawResult", "()Ljava/lang/Object;")
                    | ("setRawResult", "(Ljava/lang/Object;)V")
                    | ("isDone", "()Z")
                    | ("isCompletedNormally", "()Z")
                    | ("isCancelled", "()Z")
                    | ("cancel", "(Z)Z")
                    | ("complete", "(Ljava/lang/Object;)V")
            );
        if real_forkjoinpool_enabled()
            && matches!(
                class_name,
                "java/util/concurrent/ForkJoinTask"
                    | "java/util/concurrent/RecursiveTask"
                    | "java/util/concurrent/RecursiveAction"
                    | "java/util/concurrent/CountedCompleter"
            )
            && !keep_real_forkjointask_bridge
        {
            return;
        }
        // Real-JDK mode: drop synthetic `java/io/StringReader` natives. The
        // fake surface uses the historical content/pos/length slot layout, but
        // JDK 25 StringReader wraps a final `Reader r`; the fake constructor
        // leaves that delegate null and real `mark()`/`read()` immediately NPE.
        if self.drop_real_layout_synthetic && class_name == "java/io/StringReader" {
            return;
        }
        // Real-JDK mode: drop synthetic `java/io/PipedInputStream`/
        // `java/io/PipedOutputStream` natives. These were written for a
        // legacy hardcoded 4-slot BufferedInputStream/BufferedOutputStream-
        // style layout (in/buf/pos/count) and reused verbatim for the piped
        // streams, including writing/reading those slot indices directly on
        // whatever object is passed in. On a real JDK 25 PipedOutputStream
        // (single field: `sink`, a connected PipedInputStream), the "out"
        // slot these natives resolve/hardcode to index 0 lands on `sink`
        // instead of a delegate OutputStream, so flush()/write()/close()
        // then invoke_virtual "write"/"flush" on the connected
        // PipedInputStream itself -- which declares neither -- producing a
        // NoSuchMethodError naming PipedInputStream for a completely
        // unrelated method. See
        // docs/known-issues/h2/bug-h2-nosuchmethoderror-cross-class-dispatch.md
        // (H2's TestLob/TestLobApi/TestSQLXML/TestUpdatableResultSet/
        // TestResultSet, which all use real connected Piped stream pairs).
        // Real JDK PipedInputStream/PipedOutputStream bytecode is
        // self-contained (synchronized circular buffer, wait/notifyAll,
        // Thread identity checks -- no missing native dependency), so drop
        // the synthetic surface and let it run, same as StringReader/
        // EnumSet/Pattern/Matcher above.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/io/PipedInputStream" | "java/io/PipedOutputStream"
            )
        {
            return;
        }
        // Real-JDK mode: drop every `java/util/EnumSet` native, including the
        // SyntheticStub-tagged fallback surface. The real JDK factories are
        // self-contained once `Class.getEnumConstantsShared` works, and they
        // allocate concrete RegularEnumSet/JumboEnumSet receivers. Keeping even
        // the fallback `noneOf`/`of` natives in real mode manufactures an
        // abstract EnumSet object with the synthetic two-field layout; later
        // virtual calls then either skip the synthetic methods or read the wrong
        // layout, producing `size() == 0` and `iterator() == null` for non-JDK
        // enums such as Log4j's StandardLevel and Jakarta DispatcherType.
        if self.drop_real_layout_synthetic && class_name == "java/util/EnumSet" {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/security/Permissions` +
        // `java/security/PermissionCollection` natives (`add`, `setReadOnly`,
        // `isReadOnly`). The fake `add` stores the permission into the single
        // `allPermission` field slot instead of the real JDK
        // `permsMap` + per-class `PermissionCollection` structure. On a real
        // JDK `Permissions` object that leaves `permsMap` empty, so `implies`
        // limps via the `allPermission` fallback (true only for the
        // last-added permission's own class) while `elements()`/`size()`
        // iterate the never-populated `permsMap` and return EMPTY. WildFly's
        // Elytron builds a permission-set verifier by COPYING permissions via
        // `Permissions.elements()`
        // (`PermissionMapperDefinitions.createPermissions`): the copy yields
        // nothing, the `$local` identity never receives `LoginPermission`, and
        // JBOSS-LOCAL-USER management authentication is rejected
        // (`ServerRejected` — the entire WildFly integration suite blocker).
        // The real JDK `Permissions`/`PermissionCollection` bytecode is
        // self-contained and correct, so drop the synthetic surface and let it
        // run. The synthetic permissive collection built by
        // `security_manager::build_permissive_collection` seeds its slots
        // directly (not via native `add`) and does not depend on these natives.
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/security/Permissions" | "java/security/PermissionCollection"
            )
        {
            return;
        }
        // Real-JDK mode: drop the synthetic LinkedBlockingDeque fallback surface.
        // Its constructor writes the native-collections four-slot queue layout
        // (array/head/size/capacity). A real JDK LinkedBlockingDeque needs its
        // own constructor to initialize `lock`, `notEmpty`, `notFull`, and the
        // linked-node fields before methods such as `clear()` run.
        if self.drop_real_layout_synthetic
            && class_name == "java/util/concurrent/LinkedBlockingDeque"
        {
            return;
        }
        // Real-JDK mode: drop the synthetic ScheduledThreadPoolExecutor surface.
        // The old constructors only wrote two synthetic slots, leaving real JDK
        // ThreadPoolExecutor fields such as `workQueue` null; Tomcat's
        // ContainerBase then failed in scheduleWithFixedDelay -> delayedExecute.
        // Let the real STPE constructors and scheduling bytecode initialize the
        // inherited executor state coherently.
        // These two methods are real-layout bridges: the constructor delegates
        // to ThreadPoolExecutor's real constructor and the getter resolves the
        // inherited field by name. They are required by Spring's
        // ThreadPoolTaskScheduler anonymous subclass. Every other STPE native
        // remains unsafe against real JDK objects and is dropped.
        let keep_real_scheduled_executor_bridge = self.current_category == NativeKind::Bridge
            && class_name == "java/util/concurrent/ScheduledThreadPoolExecutor"
            && matches!(
                (method_name, descriptor),
                (
                    "<init>",
                    "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V"
                ) | ("getCorePoolSize", "()I")
            );
        if self.drop_real_layout_synthetic
            && class_name == "java/util/concurrent/ScheduledThreadPoolExecutor"
            && !keep_real_scheduled_executor_bridge
        {
            return;
        }
        if self.drop_real_layout_synthetic
            && class_name == "java/util/concurrent/Executors"
            && matches!(
                method_name,
                "newScheduledThreadPool" | "newSingleThreadScheduledExecutor"
            )
        {
            return;
        }
        // Real-JDK mode: drop legacy regex natives. They were written for the
        // old synthetic two-field Pattern / six-field Matcher layout; on real
        // OpenJDK objects they corrupt slots and bypass constructors, so later
        // Pattern/Matcher bytecode observes impossible state (for example a
        // Matcher whose `locals` field is not an int[]). Let the JDK regex
        // bytecode own both object construction and matching in real mode.
        //
        // EXCEPTION (`CRATONVM_NATIVE_MATCHER_FIND`) — keep the real-JDK-layout
        // `Matcher.find()`/`find(int)` fast path. Unlike the legacy natives
        // this drop exists to suppress, that fast path never assumes a
        // synthetic field layout: it resolves every field by name against
        // whatever real OpenJDK object is actually there (see
        // `native-builtins/src/lib.rs`'s `native_matcher_find_realjdk`), so
        // it does not corrupt anything the way the old slot-index bridge did
        // — a correct, same-answer-just-faster fast path is exactly what
        // `NativeKind::Intrinsic` means per this enum's own doc comment.
        //
        // Registered under `NativeKind::Intrinsic` SPECIFICALLY BECAUSE the
        // legacy synthetic-layout `Matcher.find`/`find(int)` registrations
        // this drop targets run under `NativeKind::Bridge` in the real-JDK
        // build (inherited from a persistent `set_category(Bridge)` far
        // above their registration site) — an earlier version of this
        // exception keyed on `Bridge` and, because of that inherited
        // category, ALSO accidentally un-dropped the legacy bridge, which
        // then corrupted every real `Matcher` via its raw synthetic slot
        // indices (symptom: `Matcher.start()` throwing after a second
        // `find()`, reproduced even with `CRATONVM_NATIVE_MATCHER_FIND`
        // unset). `Intrinsic` is registered nowhere else in this file for
        // `java/util/regex/Pattern`/`Matcher` under `drop_real_layout_synthetic`
        // (confirmed: the only other `register_regex_natives()` call site
        // that runs under `Intrinsic` is `register_synthetic_overrides`,
        // which only executes when `drop_real_layout_synthetic` is unset in
        // the first place, so the outer `if` below short-circuits before this
        // exception is even consulted there) — category-matching is
        // otherwise inherently fragile (any future `set_category` reshuffle
        // upstream of either registration site can silently reintroduce this
        // exact collision), so treat `Intrinsic` here as load-bearing: do not
        // change this registration's category without re-auditing every
        // `set_category`/`with_category` call between both `register_regex_natives`
        // call sites and the top of `register_essential_natives`.
        let keep_real_matcher_find_fastpath = self.current_category == NativeKind::Intrinsic
            && class_name == "java/util/regex/Matcher"
            && matches!(
                (method_name, descriptor),
                ("find", "()Z")
                    | ("find", "(I)Z")
                    | ("start", "()I")
                    | ("start", "(I)I")
                    | ("end", "()I")
                    | ("end", "(I)I")
                    | ("group", "()Ljava/lang/String;")
                    | ("group", "(I)Ljava/lang/String;")
            );
        // Pattern is immutable after its constructor finishes.  The two
        // static factories below may therefore return a VM-rooted, fully
        // constructed real-JDK Pattern from a bounded cache; unlike the old
        // synthetic regex bridge they never fabricate or partially initialize
        // a Pattern/Matcher layout.
        let keep_real_pattern_compile_cache = self.current_category == NativeKind::Intrinsic
            && class_name == "java/util/regex/Pattern"
            && method_name == "compile"
            && matches!(
                descriptor,
                "(Ljava/lang/String;)Ljava/util/regex/Pattern;"
                    | "(Ljava/lang/String;I)Ljava/util/regex/Pattern;"
            );
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/util/regex/Pattern" | "java/util/regex/Matcher"
            )
            && !keep_real_matcher_find_fastpath
            && !keep_real_pattern_compile_cache
        {
            return;
        }
        // Real-JDK mode: drop bridge/intrinsic `java/util/StringJoiner` natives
        // with hardcoded synthetic layouts so the real 7-field-layout bytecode
        // runs. Keep SyntheticStub-tagged fallbacks registered: dispatch skips
        // them for a loaded real StringJoiner, but synthetic fallback classes
        // still need a small native surface.
        if self.drop_real_layout_synthetic
            && class_name == "java/util/StringJoiner"
            && self.current_category != NativeKind::SyntheticStub
        {
            return;
        }
        // Real-JDK mode: drop the synthetic `java/lang/ref/Cleaner`/
        // `Cleaner$Cleanable` natives (`create()`, `register(Object,Runnable)`,
        // `Cleanable.clean()`). These were meant only as a fallback for when
        // real class bytes are unavailable (see this block's own comment at
        // the registration site, phases_late.rs::register_p68_cleaner: "real
        // Cleaner bytecode still wins whenever the real class is loaded") --
        // but `create()` is a STATIC factory method, and static dispatch has
        // no per-instance real-vs-synthetic safety net the way concrete
        // instance methods do, so the native unconditionally wins there and
        // allocates a bare Cleaner with its real `impl` field left null.
        // `register(Object,Runnable)` (an instance method) then correctly
        // prefers real bytecode -- which calls `PhantomCleanable.<init>` ->
        // `CleanerImpl.getCleanerImpl(this)` -> reads the null `impl` field
        // and NPEs ("Cannot read field \"queue\" because the return value of
        // ... getCleanerImpl(...) is null"), first seen booting a WildFly
        // Host Controller (`ServiceContainer$Factory.create()` calls
        // `Cleaner.create()` then `.register(...)`). Same half-real-object
        // bug class as the ThreadPoolExecutor/Executors-factory NPEs above --
        // drop the synthetic surface entirely so real bytecode constructs and
        // wires up the Cleaner end-to-end (matches this file's own stated
        // intent, just enforced from the registration side since dispatch
        // does not enforce it uniformly for static factory methods).
        if self.drop_real_layout_synthetic
            && matches!(
                class_name,
                "java/lang/ref/Cleaner" | "java/lang/ref/Cleaner$Cleanable"
            )
        {
            return;
        }
        // NOTE: real-JDK mode used to drop EVERY native registered directly on
        // `java/util/concurrent/ThreadPoolExecutor` here (submit/execute/
        // shutdown included), on the theory that only genuinely-real
        // `ThreadPoolExecutor` instances carry that class name. That's false:
        // `Executors.newSingleThreadExecutor()`/`newFixedThreadPool()`/
        // `newCachedThreadPool()` (registered on `Executors` below) also stamp
        // their synthetic 2-field return object with this exact class name
        // (see `alloc_concurrent_synthetic` call sites in
        // `register_executor_natives`), so a class-name-keyed drop can't tell
        // the two apart — it silently starved the synthetic objects' own
        // `execute()`/`submit()`/`shutdown()` overrides too, sending them
        // straight to real JDK bytecode that dereferences an uninitialized
        // `ctl`/`mainLock` field and NPEs
        // (`docs/internal/threadpoolexecutor-execute-npe-on-ctl-regression-FIXED.md`,
        // `docs/known-issues/threadpoolexecutor-shutdown-npe-on-mainlock-synthetic-executor.md`).
        // A prior narrower fix (merged separately, same day) exempted only
        // `execute(Runnable)` from this drop and pushed the real-vs-synthetic
        // distinction into the interpreter's dispatch layer instead
        // (`force_native_over_real_jdk_bytecode` / `intercept_force_registered_native`
        // in vm/src/runtime/interpreter.rs, plus matching checks in
        // vm/src/vm/vm_exec.rs) — those checks are still in place and harmless,
        // but they don't cover every dispatch path (`try_stackless_invoke`'s own
        // direct native lookup isn't one of the patched call sites), so a real
        // receiver could still reach `native_es_execute` and — in that fix —
        // degrade to synchronous inline execution. This drop is now removed
        // entirely (not just for `execute`), and the real-vs-synthetic
        // distinction happens per-*instance* inside
        // `native_es_execute`/`native_es_submit_*`/the `shutdown` closures
        // (`executor_has_real_workers`), which forward a genuinely-real
        // receiver to real bytecode via
        // `NativeContext::invoke_virtual_bytecode_only` — regardless of which
        // dispatch path reached the native, and preserving true async
        // semantics for a real pool's `execute()` (not just synchronous
        // fallback).
        let key = native_method_hash(class_name, method_name, descriptor);
        // With 128-bit composite keys, collisions on our keyspace are
        // vanishingly unlikely. We keep a cheap `debug_assert!` as
        // defense-in-depth: if `register()` ever overwrites an existing
        // entry, the most common cause is legitimate re-registration of
        // the same triple (e.g. by `alias_class` running twice). A true
        // hash collision would only be flagged if it produced a
        // pre-existing key for a triple we have NOT seen before.
        //
        // Duplicate-registration / collision detection MUST use a stable
        // key — the `(class, name, descriptor)` triple — not `fn` pointer
        // equality. Rust `fn` pointer comparison is unreliable: the
        // compiler may merge identical functions or duplicate them across
        // codegen units, so comparing the prior callback produces no
        // meaningful result (and triggers the
        // `unpredictable_function_pointer_comparisons` lint). If the key is
        // already occupied it is a legitimate re-registration iff the very
        // same triple appears in `registrations`; otherwise it is a true
        // 128-bit hash collision.
        //
        // NOTE (memoization work, 2026-07-26): this remains a `debug_assert`
        // — the *registration-time* check — but it is no longer the only line
        // of defense. `slot_index_for_key` now re-verifies the full triple on every
        // lookup, so in a release build a true collision degrades to "the
        // loser's triple resolves to `None`" rather than "the loser's triple
        // silently dispatches the winner's callback". See the `registrations`
        // field doc for the `class_manager::name_to_id` precedent.
        let prior_slot = self.slot_by_key.get(&key).copied();
        debug_assert!(
            prior_slot.is_none() || self.registrations.iter().any(
                |(c, m, d)| c.as_ref() == class_name
                    && m.as_ref() == method_name
                    && d.as_ref() == descriptor
            ),
            "NativeMethodRegistry 128-bit hash collision for {class_name}.{method_name}{descriptor} (key={key:?})"
        );
        // Index of this registration's triple in `registrations`, used below
        // to back the deferred native-ring name map without a second copy of
        // the parts (see the `name_index` field doc), and by every slot to
        // name-verify a digest hit.
        let reg_index = self.registrations.len();
        self.registrations
            .push((class_name.into(), method_name.into(), descriptor.into()));
        // Arm the negative-lookup prefilter. Kept adjacent to the
        // `registrations` push — the one statement pair that must never drift
        // apart, because a class in `registrations` but absent here would make
        // `slot_for_exact` answer `None` for a native that IS registered.
        self.classes_with_natives.insert(native_class_hash(class_name));
        // Tag this registration with the current category (see `with_category`).
        // Re-registration under a new category — e.g. promoting a fixed stub to
        // `Intrinsic` — takes effect, matching the previous `insert`-not-
        // -`or_insert` semantics of the removed `category_by_key` map.
        self.categories.push(self.current_category);
        // Publish into the dense slot table. Re-registration of a key we have
        // already seen UPDATES THE EXISTING SLOT IN PLACE rather than appending
        // a new one: that is what makes a `NativeMethodId` handed out earlier
        // stay valid (and pick up the new callback, matching the documented
        // last-registration-wins behavior of `register`).
        let category = self.current_category;
        match prior_slot {
            Some(idx) => {
                if let Some(slot) = self.slots.get_mut(idx as usize) {
                    slot.callback = callback;
                    slot.kind = category;
                    slot.reg_index = reg_index as u32;
                }
            }
            None => {
                let idx = self.slots.len() as u32;
                self.slots.push(NativeSlot {
                    callback,
                    kind: category,
                    reg_index: reg_index as u32,
                });
                self.slot_by_key.insert(key, idx);
            }
        }
        // AUDIT 2026-05-17 (Fix 5): also populate the class-agnostic
        // (method, descriptor) index used by `find_by_method_descriptor`.
        // Reuse `native_method_hash` with an empty class string so the
        // key is independent of the registering class.
        //
        // Collision semantics: when two different classes register the
        // same `(method, descriptor)` pair, the FIRST registration wins
        // and is kept. `entry().or_insert()` (not `insert()`) guarantees
        // a later registration never silently overwrites an earlier one,
        // which is what `find_by_method_descriptor`'s "first match"
        // contract requires.
        let md_key = native_method_hash("", method_name, descriptor);
        self.by_method_desc.entry(md_key).or_insert(callback);
        // Native-call ring buffer: associate this callback pointer with its
        // human-readable `class.method desc` name so a watchdog dump can resolve
        // raw `fn` pointers back to method names.
        //
        // WF32-fix (history): the name map used to be populated UNCONDITIONALLY
        // and eagerly here, with a per-`register()` `format!` allocation **plus**
        // a cross-module `name_map()` `Mutex` round-trip. The reason it wasn't
        // gated behind `native_ring::is_enabled()` was correctness: boot
        // registers ~3,100 natives *before* the watchdog arms recording, so a
        // naive skip-while-disabled lost every name and the dump showed only
        // useless `<unknown cb@0x...>` pointers.
        //
        // PERF FIX: the eager work is now deferred. The default (ring disabled)
        // path records ONLY a cheap `cb_ptr -> reg_index` entry — no `format!`
        // heap allocation, no cross-module lock — and the actual name strings
        // are materialized lazily by `flush_native_ring_names()` if/when a
        // diagnostic is actually requested (the ring is enabled). Names are NOT
        // lost: the full triple is already persisted in `registrations[reg_index]`
        // (pushed unconditionally above), so `flush_native_ring_names()` can
        // rebuild every name on demand. This preserves the WF32 guarantee while
        // removing the per-boot-registration `format!` + `Mutex` cost.
        //
        // If recording is ALREADY enabled at register time (rare — e.g. a native
        // registered after the watchdog armed), populate the ring's name map
        // eagerly so a dump that races registration still resolves the name. The
        // cheap index is recorded in both cases so a later `flush_*` is complete.
        // First-write-wins to mirror the prior `register_name`/`or_insert_with`.
        self.name_index
            .entry(callback as usize)
            .or_insert(reg_index);
        if crate::native_ring::is_enabled() {
            let triple = format!("{class_name}.{method_name}{descriptor}");
            crate::native_ring::register_name(callback as usize, &triple);
        }
    }

    /// Materialize the deferred native-call-ring name map: walk the
    /// `cb_ptr -> registration-index` index built cheaply at `register()` time
    /// and publish each `fn-ptr → "class.method desc"` mapping into
    /// `native_ring`'s name map (idempotent / first-write-wins).
    ///
    /// PERF (native-ring lazy name map): this is the lazy counterpart to the
    /// deferral in `register()`. Boot registration no longer pays a `format!`
    /// allocation + `Mutex` lock per native for a diagnostic that is off by
    /// default; instead, the watchdog (or whoever arms the ring) calls this
    /// ONCE when recording is actually turned on, paying the ~3,100 `format!`
    /// allocations only then. Calling it is cheap to repeat: `register_name`
    /// uses `or_insert_with`, so already-published names are left untouched.
    ///
    /// CROSS-FILE FOLLOW-UP (out of this file's scope): the ring arm site —
    /// where `native_ring::enable(true)` is called (per native_ring.rs docs:
    /// `vm-cli/src/main.rs`, on `--stack-dump-on-timeout=N>0` /
    /// `CRATONVM_ENABLE_NATIVE_RING=1`) — should call this on the global
    /// `NativeMethodRegistry` immediately after `enable(true)` so a subsequent
    /// dump resolves names. Until that wiring lands, names registered while the
    /// ring was disabled resolve as `<unknown cb@0x...>` in the dump (same
    /// failure mode the WF32-fix originally addressed, but now opt-in and only
    /// when the wiring is absent — semantics of registration are unchanged).
    pub fn flush_native_ring_names(&self) {
        for (&cb_ptr, &idx) in &self.name_index {
            if let Some((c, m, d)) = self.registrations.get(idx) {
                let triple = format!("{c}.{m}{d}");
                crate::native_ring::register_name(cb_ptr, &triple);
            }
        }
    }

    /// Combined `find` + `kind_of`: computes the 128-bit
    /// `(class, method, descriptor)` hash once and looks up both the
    /// callback and its category from it, instead of the two independent
    /// hashes (one full byte-walk each) `invoke_or_native`'s
    /// synthetic-stub check used to pay on every native dispatch --
    /// `find(...)` to get the callback, then immediately `kind_of(...)`
    /// with the identical three strings to classify it. A gdb sampling
    /// profile of a hung-looking H2 `TestFileSystem.testConcurrent` run
    /// (two real threads, heavy native-call volume) caught both live
    /// threads inside `hash_byte_pair`/`native_method_hash` disproportionately
    /// often, which is this exact redundant second pass. Only covers the
    /// fast exact-hash path (mirroring `find`'s own fast path); falls back
    /// to the slow `find`+`kind_of` pair on a miss so descriptor-quirk
    /// rewriting keeps working unchanged.
    #[inline]
    pub fn find_with_kind(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)> {
        if let Some(slot) = self.slot_for_exact(class_name, method_name, descriptor) {
            return Some((slot.callback, slot.kind));
        }
        // Cold descriptor-quirk path. Semantics deliberately preserved from the
        // pre-memoization implementation: the kind is looked up with the
        // ORIGINAL (un-rewritten) descriptor, so it misses and falls back to
        // `Bridge`. The slot now carries its true kind and we could report it
        // exactly, but that would change which natives the real-JDK
        // `SyntheticStub` drop applies to on quirky descriptors — a dispatch
        // semantics change, out of scope for a perf change. Left as-is,
        // deliberately.
        //
        // MERGE NOTE (dev @ 6495a191c): dev's concurrent edit here was a pure
        // rustfmt reflow of the `methods` / `category_by_key` probe that this
        // change deletes outright. Its formatting of the `kind_of` chain is
        // carried over; the two-map probe itself is gone.
        let cb = self.find_with_descriptor_quirks(class_name, method_name, descriptor)?;
        let kind = self
            .kind_of(class_name, method_name, descriptor)
            .unwrap_or(NativeKind::Bridge);
        Some((cb, kind))
    }

    /// Number of distinct native slots ever allocated. Changes **only** when a
    /// registration introduces a triple the registry has not seen before;
    /// re-registering an existing triple updates its slot in place and leaves
    /// this unchanged.
    ///
    /// This is the invalidation signal for [`NativeCallSite`](crate::NativeCallSite):
    /// a memo — including a memoized *negative* ("no native for this triple") —
    /// is valid exactly as long as the generation it was taken at still holds.
    /// It costs one `u32` load to check, and it is what makes call-site
    /// memoization correct during boot and across the lazy `register_*` passes,
    /// not merely "after the registry stops changing".
    ///
    /// The value is `registry_epoch + slots.len()`, not `slots.len()` alone, so
    /// it also distinguishes *which* registry a memo was taken against — see the
    /// [`registry_epoch`](Self) field doc. Never `0`, so a `NativeCallSite` can
    /// keep using an all-zero word as its "never resolved" sentinel.
    ///
    /// The absolute value is an opaque token: compare it for equality, never
    /// treat it as a count.
    #[inline]
    pub fn generation(&self) -> u32 {
        self.registry_epoch.wrapping_add(self.slots.len() as u32)
    }

    /// Resolve a triple to a stable [`NativeMethodId`](crate::NativeMethodId)
    /// that a call site can cache and redeem later with
    /// [`callback_of`](Self::callback_of) — an array index instead of three
    /// string hashes plus a map probe.
    ///
    /// Semantics are identical to [`find`](Self::find), descriptor-quirk
    /// fallback included: `resolve_id(..).and_then(|id| reg.callback_of(id))`
    /// always equals `find(..)`.
    #[inline]
    pub fn resolve_id(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        let key = native_method_hash(class_name, method_name, descriptor);
        if let Some(idx) = self.slot_index_for_key(key, class_name, method_name, descriptor) {
            return Some(NativeMethodId::from_u32(idx));
        }
        self.resolve_id_with_descriptor_quirks(class_name, method_name, descriptor)
    }

    /// As [`resolve_id`](Self::resolve_id), but with the 128-bit digest already
    /// computed (see [`NativeMethodKey`](crate::NativeMethodKey)) so constant
    /// strings known at class-link time are not re-hashed on every lookup.
    ///
    /// The three strings are still required and still checked: the digest
    /// narrows the search, the names decide the answer. Passing a `key` that
    /// does not correspond to the strings simply misses — it can never return
    /// some other class's native.
    #[inline]
    pub fn resolve_id_by_key(
        &self,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        if let Some(idx) =
            self.slot_index_for_key(key.as_pair(), class_name, method_name, descriptor)
        {
            return Some(NativeMethodId::from_u32(idx));
        }
        self.resolve_id_with_descriptor_quirks(class_name, method_name, descriptor)
    }

    /// Redeem a handle: the callback for `id`, or `None` if the handle does not
    /// belong to this registry. O(1), no hashing, no string comparison — this is
    /// the whole point of the mechanism.
    #[inline]
    pub fn callback_of(&self, id: NativeMethodId) -> Option<NativeCallback> {
        self.slots.get(id.index()).map(|slot| slot.callback)
    }

    /// The category the native behind `id` was registered under.
    #[inline]
    pub fn kind_of_id(&self, id: NativeMethodId) -> Option<NativeKind> {
        self.slots.get(id.index()).map(|slot| slot.kind)
    }

    /// The `(class, method, descriptor)` triple that currently owns `id`.
    /// Diagnostics (native-ring dumps, tracing), not a dispatch input.
    #[inline]
    pub fn triple_of(&self, id: NativeMethodId) -> Option<(&str, &str, &str)> {
        let slot = self.slots.get(id.index())?;
        let (c, m, d) = self.registrations.get(slot.reg_index as usize)?;
        Some((c.as_ref(), m.as_ref(), d.as_ref()))
    }

    /// [`find`](Self::find) with a precomputed digest. See
    /// [`resolve_id_by_key`](Self::resolve_id_by_key) for the verification
    /// contract.
    #[inline]
    pub fn find_by_key(
        &self,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let id = self.resolve_id_by_key(key, class_name, method_name, descriptor)?;
        self.callback_of(id)
    }

    /// [`find_with_kind`](Self::find_with_kind) with a precomputed digest.
    ///
    /// Unlike `find_with_kind` this reports the slot's true kind on the
    /// descriptor-quirk path too; `find_with_kind`'s `Bridge` fallback there is
    /// preserved only for the existing callers that depend on it.
    #[inline]
    pub fn find_with_kind_by_key(
        &self,
        key: NativeMethodKey,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<(NativeCallback, NativeKind)> {
        let id = self.resolve_id_by_key(key, class_name, method_name, descriptor)?;
        let slot = self.slots.get(id.index())?;
        Some((slot.callback, slot.kind))
    }

    /// Slot lookup by exact triple (no descriptor rewriting).
    #[inline]
    fn slot_for_exact(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<&NativeSlot> {
        // Prefilter on the class name alone before finishing the digest: see
        // `native_class_hash`. Exact for a miss, may false-positive into the
        // full path.
        let class_state = native_class_hash(class_name);
        if !self.classes_with_natives.contains(&class_state) {
            return None;
        }
        let key = native_method_hash_from(class_state, method_name, descriptor);
        let idx = self.slot_index_for_key(key, class_name, method_name, descriptor)?;
        self.slots.get(idx as usize)
    }

    /// The one place a 128-bit digest is turned into a slot index — and the one
    /// place the full name is verified.
    ///
    /// A digest hit is treated as a *candidate*: we fetch the triple the
    /// candidate slot was registered under and compare all three strings. A
    /// mismatch is reported as a **miss**, not as a callback.
    ///
    /// This is not defensive theater. `classloading/src/class_manager.rs` (the
    /// `loaded_classes` field doc, "Round 4 audit fix (CRIT)") records a
    /// shipped defect of exactly this shape: a `name_to_id: FxHashMap<u64,
    /// ClassId>` keyed by a raw FNV-1a digest with no name verification, where
    /// any collision returned the wrong `ClassId` and caused silent type
    /// confusion downstream. Three `str` comparisons of already-hot cache lines
    /// are cheaper than the byte-at-a-time hash that produced the key, so this
    /// is bought at essentially no cost.
    #[inline]
    fn slot_index_for_key(
        &self,
        key: (u64, u64),
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<u32> {
        let idx = *self.slot_by_key.get(&key)?;
        let slot = self.slots.get(idx as usize)?;
        let (c, m, d) = self.registrations.get(slot.reg_index as usize)?;
        if c.as_ref() == class_name && m.as_ref() == method_name && d.as_ref() == descriptor {
            Some(idx)
        } else {
            None
        }
    }

    /// Look up a native method implementation (zero allocation on the
    /// fast path; zero allocation on a miss with a clean descriptor).
    #[inline]
    pub fn find(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        if let Some(slot) = self.slot_for_exact(class_name, method_name, descriptor) {
            return Some(slot.callback);
        }

        // AUDIT 2026-05-17 (Fix 4): the compatibility-variants path was
        // previously building a `Vec<String>` on every miss, even for
        // perfectly-formed descriptors that needed no rewriting (which
        // is the common case — a real miss is usually "this native is
        // not implemented", not "the descriptor needed a fixup"). Now
        // we short-circuit: only walk the variant logic when the
        // descriptor *actually* has a quirk worth rewriting. Clean
        // descriptors return `None` with zero allocation.
        Self::find_with_descriptor_quirks(self, class_name, method_name, descriptor)
    }

    /// Cold path of `find`: try compatibility-rewritten descriptor
    /// variants. Returns `None` if the descriptor is already clean
    /// (no whitespace, no NUL, no `\r\n`, and any `L…` return type
    /// already correctly terminated with `;`).
    #[inline]
    fn find_with_descriptor_quirks(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        let id = self.resolve_id_with_descriptor_quirks(class_name, method_name, descriptor)?;
        self.callback_of(id)
    }

    /// Handle-returning form of [`find_with_descriptor_quirks`]. The quirk
    /// rewrite is the only part of resolution that is not a pure function of
    /// the exact triple, so it has to produce a slot index too — otherwise a
    /// call site that memoizes a `NativeMethodId` would silently lose the
    /// compatibility rewrites that `find` performs.
    #[cold]
    #[inline(never)]
    fn resolve_id_with_descriptor_quirks(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeMethodId> {
        // Cheap precheck: if the descriptor has none of the quirks the
        // rewrites target, there are no variants to try — bail before
        // touching the allocator.
        let has_whitespace_or_nul = descriptor
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b == b'\0');
        let object_return_quirk = match descriptor.rfind(')') {
            Some(rparen) => {
                let ret = &descriptor[rparen + 1..];
                ret.starts_with('L') && !ret.ends_with(';')
            }
            None => false,
        };
        if !has_whitespace_or_nul && !object_return_quirk {
            return None;
        }

        // At most 3 candidates (trimmed, no_newlines, return-type fixup).
        // Stash them inline in a small fixed-size array — no Vec growth.
        let mut variants: [Option<String>; 3] = [None, None, None];
        let mut n_variants = 0usize;

        let trimmed = descriptor.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
        if trimmed != descriptor {
            variants[n_variants] = Some(trimmed.to_string());
            n_variants += 1;
        }

        let no_newlines = trimmed.replace(['\r', '\n'], "");
        if no_newlines != descriptor
            && !variants
                .iter()
                .take(n_variants)
                .any(|v| v.as_deref() == Some(no_newlines.as_str()))
        {
            variants[n_variants] = Some(no_newlines.clone());
            n_variants += 1;
        }

        for base in [trimmed, no_newlines.as_str()] {
            if let Some(rparen) = base.rfind(')') {
                let (args_part, ret_part) = base.split_at(rparen + 1);
                if ret_part.starts_with('L') {
                    let candidate = if !ret_part.ends_with(';') {
                        Some(format!("{args_part}{ret_part};"))
                    } else {
                        ret_part
                            .strip_suffix(';')
                            .map(|stripped| format!("{args_part}{stripped}"))
                    };
                    if let Some(cand) = candidate {
                        if cand != descriptor
                            && n_variants < variants.len()
                            && !variants
                                .iter()
                                .take(n_variants)
                                .any(|v| v.as_deref() == Some(cand.as_str()))
                        {
                            variants[n_variants] = Some(cand);
                            n_variants += 1;
                        }
                    }
                }
            }
        }

        for candidate_desc in variants
            .iter()
            .take(n_variants)
            .filter_map(|s| s.as_deref())
        {
            let k = native_method_hash(class_name, method_name, candidate_desc);
            // Verify against the REWRITTEN descriptor — that is the triple the
            // native was registered under, and the one the digest encodes.
            if let Some(idx) =
                self.slot_index_for_key(k, class_name, method_name, candidate_desc)
            {
                return Some(NativeMethodId::from_u32(idx));
            }
        }
        None
    }

    /// NEW-14: copy every native method currently registered under
    /// `from_class` to also be reachable under `to_class`.
    ///
    /// Used by the JDBC registration path to make every
    /// `PreparedStatement` method also dispatch when invoked on a
    /// `CallableStatement` instance (the JDK's `CallableStatement`
    /// interface extends `PreparedStatement`, so every PS method is
    /// valid on a CS instance — but native dispatch is keyed by class
    /// name, not Java inheritance, so we have to populate both keys
    /// explicitly). The function:
    ///
    ///   1. Walks the `registrations` log (the per-`register()`
    ///      append-only list of `(class, method, descriptor)` triples)
    ///      to find every entry whose class equals `from_class`.
    ///   2. For each match, re-registers the same callback under
    ///      `to_class` with the same method name and descriptor.
    ///
    /// Idempotent: calling twice produces the same final state. Any
    /// existing registration on `to_class` is overwritten (matching
    /// the behavior of `register` itself, which re-registers silently
    /// when the triple is identical).
    pub fn alias_class(&mut self, from_class: &str, to_class: &str) {
        // Collect first to avoid mutating while iterating, and to drop
        // the immutable borrow on `self.registrations` before we call
        // `self.register()` below. Two passes (rather than one closure that
        // both walks `registrations` and probes the slot table) so the
        // `registrations` borrow is provably released before the whole-`self`
        // lookup — a rare boot-time path, so the extra `Vec` is free.
        let matched: Vec<(String, String)> = self
            .registrations
            .iter()
            .filter(|(class, _, _)| class.as_ref() == from_class)
            .map(|(_, method, descriptor)| (method.to_string(), descriptor.to_string()))
            .collect();
        let entries: Vec<(String, String, NativeCallback)> = matched
            .into_iter()
            .filter_map(|(method, descriptor)| {
                let callback = self.slot_for_exact(from_class, &method, &descriptor)?.callback;
                Some((method, descriptor, callback))
            })
            .collect();
        for (method_name, descriptor, callback) in entries {
            self.register(to_class, &method_name, &descriptor, callback);
        }
    }

    /// The number of registered native methods (distinct triples — a
    /// re-registration of the same triple does not increase this).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Returns true if no native methods are registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Fallback search: find any registered native callback whose
    /// `(method_name, descriptor)` matches, regardless of class.
    ///
    /// Used by the slow-path dispatcher recovery for the case where a
    /// synthetic native-allocated object (e.g. our `Pattern` instances
    /// allocated with `ClassId::new(0)`) ends up routed through
    /// `java/lang/Object` because the heap reports `class_id_of` as 0
    /// (the literal Object class id). A scoped native like
    /// `java/util/regex/Pattern.matcher(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;`
    /// is uniquely identified by `(method_name, descriptor)` because
    /// `Object` has no such method — so this scan recovers the
    /// correct callback without needing the CP method-ref class.
    ///
    /// Returns the callback of the FIRST class to register this
    /// `(method_name, descriptor)` pair. The lookup is O(1): it consults
    /// the `by_method_desc` index built incrementally at registration
    /// time, not an O(N) scan. The index is populated with
    /// `entry().or_insert()`, so if several classes register the same
    /// `(method, descriptor)` the earliest registration is the one kept
    /// and returned here — the "first match" wording above is therefore
    /// a real guarantee, not an artifact of iteration order.
    ///
    /// Call sites should still gate this on the slow recovery path (NSME
    /// about to be raised), not the hot dispatch.
    pub fn find_by_method_descriptor(
        &self,
        method_name: &str,
        descriptor: &str,
    ) -> Option<NativeCallback> {
        // AUDIT 2026-05-17 (Fix 5): O(1) lookup via the class-agnostic
        // `by_method_desc` index built at registration time. The index
        // is keyed by `native_method_hash("", method, descriptor)` so the
        // class portion is masked out, and first-registration wins on
        // key collision (see `register`).
        let key = native_method_hash("", method_name, descriptor);
        self.by_method_desc.get(&key).copied()
    }

    /// Cheap conservative prefilter for hot call paths that only need to know
    /// whether a class-qualified native lookup could possibly succeed.
    ///
    /// A false result is definitive for clean descriptors: no registered native
    /// has this `(method_name, descriptor)` pair on any class, so callers may
    /// skip class-specific `find()` probes and superclass walks. Descriptors
    /// that would trigger the compatibility rewrite path return true even when
    /// the exact index misses, preserving `find()` semantics.
    #[inline]
    pub fn might_have_method_descriptor(&self, method_name: &str, descriptor: &str) -> bool {
        let key = native_method_hash("", method_name, descriptor);
        if self.by_method_desc.contains_key(&key) {
            return true;
        }
        let has_whitespace_or_nul = descriptor
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b == b'\0');
        let object_return_quirk = match descriptor.rfind(')') {
            Some(rparen) => {
                let ret = &descriptor[rparen + 1..];
                ret.starts_with('L') && !ret.ends_with(';')
            }
            None => false,
        };
        has_whitespace_or_nul || object_return_quirk
    }
}

#[cfg(test)]
impl NativeMethodRegistry {
    /// TEST ONLY: manufacture a 128-bit digest collision.
    ///
    /// Points the digest of `victim` (a triple that is NOT registered) at the
    /// slot owned by `owner` (a triple that IS registered) — exactly the state
    /// a real FNV-1a collision would produce. Everything downstream of the map
    /// probe is the production code path, so this exercises the full-name
    /// verification in `slot_index_for_key` for real. Without that check,
    /// `find(victim)` would return `owner`'s callback: the silent type
    /// confusion the `class_manager::name_to_id` map used to cause.
    ///
    /// Finding a genuine 128-bit collision is computationally infeasible, which
    /// is precisely why the check has to be tested by injection.
    fn inject_digest_collision_for_test(
        &mut self,
        owner: (&str, &str, &str),
        victim: (&str, &str, &str),
    ) {
        let owner_key = native_method_hash(owner.0, owner.1, owner.2);
        let idx = *self
            .slot_by_key
            .get(&owner_key)
            .expect("owner triple must already be registered");
        let victim_key = native_method_hash(victim.0, victim.1, victim.2);
        assert_ne!(
            owner_key, victim_key,
            "test setup: owner and victim must be distinct triples"
        );
        self.slot_by_key.insert(victim_key, idx);
        // The victim triple is deliberately NOT registered, so its class is
        // not in the prefilter — without this the injected collision would be
        // filtered out before `slot_index_for_key` ever ran, and the test
        // would pass for the wrong reason.
        self.classes_with_natives.insert(native_class_hash(victim.0));
    }
}

impl Default for NativeMethodRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for NativeMethodRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeMethodRegistry")
            .field("count", &self.slots.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_mock::MockNativeContext;

    fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(None)
    }

    fn dummy_native_2(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        Ok(Some(Value::Int(42)))
    }

    #[test]
    fn native_handle_scope_releases_nested_roots_on_all_rust_exit_paths() {
        fn root_then_return(ctx: &mut dyn NativeContext, object: ObjectRef) {
            let mut scope = NativeHandleScope::new(ctx);
            let handle = scope.root(object);
            assert_eq!(scope.get(&handle), object);
        }

        let mut ctx = MockNativeContext::new();
        let first = ctx.fresh_object_ref();
        root_then_return(&mut ctx, first);
        assert_eq!(ctx.handle_slot_count(), 0);
        assert_eq!(ctx.handle_scope_depth(), 0);

        let outer_object = ctx.fresh_object_ref();
        let inner_object = ctx.fresh_object_ref();
        {
            let mut outer = NativeHandleScope::new(&mut ctx);
            let outer_handle = outer.root(outer_object);
            {
                let mut inner = NativeHandleScope::new(&mut *outer);
                let inner_handle = inner.root(inner_object);
                assert_eq!(inner.get(&inner_handle), inner_object);
            }
            assert_eq!(outer.get(&outer_handle), outer_object);
        }
        assert_eq!(ctx.handle_slot_count(), 0);
        assert_eq!(ctx.handle_scope_depth(), 0);

        let unwind_object = ctx.fresh_object_ref();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut scope = NativeHandleScope::new(&mut ctx);
            let _handle = scope.root(unwind_object);
            panic!("exercise NativeHandleScope::drop during unwind");
        }));
        assert!(panicked.is_err());
        assert_eq!(ctx.handle_slot_count(), 0);
        assert_eq!(ctx.handle_scope_depth(), 0);
    }

    fn legacy_native_method_hash(class: &str, method: &str, descriptor: &str) -> (u64, u64) {
        fn pass(class: &str, method: &str, descriptor: &str, basis: u64, prime: u64) -> u64 {
            let mut h = basis;
            for (idx, component) in [class, method, descriptor].iter().enumerate() {
                for byte in component.bytes() {
                    h ^= byte as u64;
                    h = h.wrapping_mul(prime);
                }
                if idx < 2 {
                    h ^= b'.' as u64;
                    h = h.wrapping_mul(prime);
                }
            }
            h
        }

        (
            fmix64(pass(
                class,
                method,
                descriptor,
                0xcbf29ce484222325,
                0x100000001b3,
            )),
            fmix64(pass(
                class,
                method,
                descriptor,
                0x9e3779b97f4a7c15,
                0x880355f21e6d1965,
            )),
        )
    }

    // -----------------------------------------------------------------------
    // NativeMethodRegistry basics
    // -----------------------------------------------------------------------

    #[test]
    fn register_and_find() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "registerNatives", "()V", dummy_native);
        assert!(registry
            .find("java/lang/Object", "registerNatives", "()V")
            .is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn find_missing_returns_none() {
        let registry = NativeMethodRegistry::new();
        assert!(registry
            .find("java/lang/Object", "hashCode", "()I")
            .is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn new_registry_is_empty() {
        let registry = NativeMethodRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn default_registry_is_empty() {
        let registry = NativeMethodRegistry::default();
        assert!(registry.is_empty());
    }

    #[test]
    fn register_multiple_methods() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "registerNatives", "()V", dummy_native);
        registry.register("java/lang/Object", "hashCode", "()I", dummy_native_2);
        registry.register("java/lang/System", "currentTimeMillis", "()J", dummy_native);
        assert_eq!(registry.len(), 3);
        assert!(!registry.is_empty());
    }

    #[test]
    fn find_distinguishes_by_class() {
        let mut registry = NativeMethodRegistry::new();
        registry.register(
            "java/lang/Object",
            "toString",
            "()Ljava/lang/String;",
            dummy_native,
        );
        registry.register(
            "java/lang/String",
            "toString",
            "()Ljava/lang/String;",
            dummy_native_2,
        );
        assert!(registry
            .find("java/lang/Object", "toString", "()Ljava/lang/String;")
            .is_some());
        assert!(registry
            .find("java/lang/String", "toString", "()Ljava/lang/String;")
            .is_some());
        // Different class, same method + descriptor
        assert!(registry
            .find("java/lang/Integer", "toString", "()Ljava/lang/String;")
            .is_none());
    }

    #[test]
    fn find_distinguishes_by_descriptor() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Math", "abs", "(I)I", dummy_native);
        registry.register("java/lang/Math", "abs", "(J)J", dummy_native_2);
        assert!(registry.find("java/lang/Math", "abs", "(I)I").is_some());
        assert!(registry.find("java/lang/Math", "abs", "(J)J").is_some());
        assert!(registry.find("java/lang/Math", "abs", "(D)D").is_none());
    }

    #[test]
    fn method_descriptor_prefilter_ignores_class() {
        let mut registry = NativeMethodRegistry::new();
        registry.register(
            "java/lang/Object",
            "toString",
            "()Ljava/lang/String;",
            dummy_native,
        );
        assert!(registry.might_have_method_descriptor("toString", "()Ljava/lang/String;"));
        assert!(!registry.might_have_method_descriptor("toString", "()I"));
        assert!(!registry.might_have_method_descriptor("hashCode", "()Ljava/lang/String;"));
    }

    #[test]
    fn method_descriptor_prefilter_is_conservative_for_quirky_descriptors() {
        let registry = NativeMethodRegistry::new();
        assert!(registry.might_have_method_descriptor("m", " ()V"));
        assert!(registry.might_have_method_descriptor("m", "()Ljava/lang/String"));
    }

    #[test]
    fn overwrite_same_triple() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("A", "b", "()V", dummy_native);
        // Re-registering the same triple should overwrite without panic
        registry.register("A", "b", "()V", dummy_native_2);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn debug_format_shows_count() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("A", "b", "()V", dummy_native);
        let dbg = format!("{:?}", registry);
        assert!(dbg.contains("count: 1"));
    }

    // -----------------------------------------------------------------------
    // Negative-lookup class prefilter
    // -----------------------------------------------------------------------

    /// The one invariant the prefilter can get wrong in a way that MATTERS:
    /// a class that has a surviving registration but is missing from the set
    /// makes `slot_for_exact` answer `None` for a native that exists. Assert
    /// it over the whole registration log rather than for one sample triple,
    /// so a future `register` early-return added above the insert is caught.
    #[test]
    fn every_registered_class_is_in_the_prefilter() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "hashCode", "()I", dummy_native);
        registry.register("java/lang/String", "length", "()I", dummy_native_2);
        registry.with_category(NativeKind::Bridge, |r| {
            r.register("sun/misc/Unsafe", "getInt", "(Ljava/lang/Object;J)I", dummy_native);
        });
        registry.alias_class("java/lang/String", "java/lang/CharSequence");

        for (class, method, descriptor) in &registry.registrations {
            assert!(
                registry
                    .classes_with_natives
                    .contains(&native_class_hash(class)),
                "{class} is in the registration log but not the prefilter, so \
                 {class}.{method}{descriptor} would resolve to None"
            );
            assert!(
                registry.find(class, method, descriptor).is_some(),
                "{class}.{method}{descriptor} must still resolve through the prefilter"
            );
        }
    }

    /// A class with no registration at all must miss — that is the whole point
    /// — and must miss for every method name, not just the ones tried above.
    #[test]
    fn unregistered_class_misses_without_finishing_the_digest() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/Object", "hashCode", "()I", dummy_native);
        assert!(registry.find("org/h2/mvstore/MVMap", "get", "()V").is_none());
        assert!(registry.kind_of("org/h2/mvstore/MVMap", "put", "()V").is_none());
        assert!(registry
            .find_with_kind("org/h2/mvstore/MVMap", "hashCode", "()I")
            .is_none());
        // …while a registered triple on a registered class still resolves.
        assert!(registry.find("java/lang/Object", "hashCode", "()I").is_some());
        // …and an unregistered METHOD on a registered class still misses,
        // which is the prefilter's false-positive path falling through to the
        // full digest + name verification.
        assert!(registry.find("java/lang/Object", "toString", "()V").is_none());
    }

    /// The split hash must be bit-identical to the one-shot form it replaced:
    /// `slot_by_key` entries written by `register` (one-shot) are probed by
    /// `slot_for_exact` (split).
    #[test]
    fn split_hash_matches_the_one_shot_digest() {
        for (c, m, d) in [
            ("java/lang/Object", "hashCode", "()I"),
            ("", "", ""),
            ("a", "b", "c"),
            (
                "jdk/internal/misc/Unsafe",
                "compareAndSetLong",
                "(Ljava/lang/Object;JJJ)Z",
            ),
        ] {
            assert_eq!(
                native_method_hash(c, m, d),
                native_method_hash_from(native_class_hash(c), m, d),
                "split digest diverged for {c}.{m}{d}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // FNV hash function tests
    // -----------------------------------------------------------------------

    #[test]
    fn hash_deterministic() {
        let h1 = native_method_hash("java/lang/Object", "hashCode", "()I");
        let h2 = native_method_hash("java/lang/Object", "hashCode", "()I");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_differs_for_different_inputs() {
        let h1 = native_method_hash("java/lang/Object", "hashCode", "()I");
        let h2 = native_method_hash("java/lang/Object", "toString", "()Ljava/lang/String;");
        let h3 = native_method_hash("java/lang/String", "hashCode", "()I");
        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn hash_differs_for_swapped_components() {
        // "A.B.()V" vs "B.A.()V" — the separator dots are included in hashing
        let h1 = native_method_hash("A", "B", "()V");
        let h2 = native_method_hash("B", "A", "()V");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_empty_strings() {
        // Edge case: empty strings should not panic
        let h = native_method_hash("", "", "");
        // FNV offset basis processed through the separator dots — both
        // halves should be non-zero.
        assert!(h.0 != 0 && h.1 != 0);
    }

    #[test]
    fn hash_two_halves_independent() {
        // The two 64-bit halves are produced by different hash functions
        // (different multipliers) plus independent finalization mixes, so
        // the same triple yields two distinct 64-bit values.
        let (h1, h2) = native_method_hash("java/lang/Object", "hashCode", "()I");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_single_scan_preserves_legacy_keys() {
        let cases = [
            ("java/lang/Object", "hashCode", "()I"),
            (
                "org/hibernate/query/sqm/function/AbstractSqmSelfRenderingFunctionDescriptor",
                "generateSqmExpression",
                "(Ljava/util/List;Lorg/hibernate/query/ReturnableType;Lorg/hibernate/query/spi/QueryEngine;)Lorg/hibernate/query/sqm/tree/expression/SqmExpression;",
            ),
            ("", "methodOnly", "(Ljava/lang/Object;)V"),
        ];

        for (class, method, descriptor) in cases {
            assert_eq!(
                native_method_hash(class, method, descriptor),
                legacy_native_method_hash(class, method, descriptor)
            );
        }
    }

    #[test]
    fn alias_class_copies_registrations() {
        // Regression: `alias_class` used to walk a separate `keys`
        // reverse map. After removing that map, it now walks the
        // `registrations` log instead. Verify the public behavior is
        // unchanged.
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/sql/PreparedStatement", "execute", "()Z", dummy_native);
        registry.register("java/sql/PreparedStatement", "close", "()V", dummy_native_2);
        registry.alias_class("java/sql/PreparedStatement", "java/sql/CallableStatement");
        assert!(registry
            .find("java/sql/CallableStatement", "execute", "()Z")
            .is_some());
        assert!(registry
            .find("java/sql/CallableStatement", "close", "()V")
            .is_some());
        // Original registrations still present.
        assert!(registry
            .find("java/sql/PreparedStatement", "execute", "()Z")
            .is_some());
    }

    #[test]
    fn real_layout_mode_drops_enumset_native_surface() {
        let of_two_desc = "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;";

        let mut normal = NativeMethodRegistry::new();
        normal.set_category(NativeKind::SyntheticStub);
        normal.register("java/util/EnumSet", "of", of_two_desc, dummy_native);
        assert!(normal
            .find("java/util/EnumSet", "of", of_two_desc)
            .is_some());

        let mut real_layout = NativeMethodRegistry::new();
        real_layout.set_drop_real_layout_synthetic(true);
        real_layout.set_category(NativeKind::SyntheticStub);
        real_layout.register("java/util/EnumSet", "of", of_two_desc, dummy_native);
        assert!(real_layout
            .find("java/util/EnumSet", "of", of_two_desc)
            .is_none());

        real_layout.set_category(NativeKind::Intrinsic);
        real_layout.register("java/util/EnumSet", "size", "()I", dummy_native_2);
        assert!(real_layout
            .find("java/util/EnumSet", "size", "()I")
            .is_none());

        real_layout.register("java/util/HashSet", "size", "()I", dummy_native_2);
        assert!(real_layout
            .find("java/util/HashSet", "size", "()I")
            .is_some());

        real_layout.set_category(NativeKind::SyntheticStub);
        real_layout.register(
            "java/io/StringReader",
            "<init>",
            "(Ljava/lang/String;)V",
            dummy_native,
        );
        assert!(real_layout
            .find("java/io/StringReader", "<init>", "(Ljava/lang/String;)V")
            .is_none());

        real_layout.register(
            "java/util/concurrent/LinkedBlockingDeque",
            "<init>",
            "()V",
            dummy_native,
        );
        assert!(real_layout
            .find("java/util/concurrent/LinkedBlockingDeque", "<init>", "()V")
            .is_none());

        real_layout.register(
            "java/util/concurrent/ScheduledThreadPoolExecutor",
            "<init>",
            "(ILjava/util/concurrent/ThreadFactory;)V",
            dummy_native,
        );
        assert!(real_layout
            .find(
                "java/util/concurrent/ScheduledThreadPoolExecutor",
                "<init>",
                "(ILjava/util/concurrent/ThreadFactory;)V",
            )
            .is_none());

        real_layout.set_category(NativeKind::Bridge);
        real_layout.register(
            "java/util/concurrent/ScheduledThreadPoolExecutor",
            "<init>",
            "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V",
            dummy_native,
        );
        assert!(real_layout
            .find(
                "java/util/concurrent/ScheduledThreadPoolExecutor",
                "<init>",
                "(ILjava/util/concurrent/ThreadFactory;Ljava/util/concurrent/RejectedExecutionHandler;)V",
            )
            .is_some());

        real_layout.register(
            "java/util/concurrent/Executors",
            "newScheduledThreadPool",
            "(I)Ljava/util/concurrent/ScheduledExecutorService;",
            dummy_native,
        );
        assert!(real_layout
            .find(
                "java/util/concurrent/Executors",
                "newScheduledThreadPool",
                "(I)Ljava/util/concurrent/ScheduledExecutorService;",
            )
            .is_none());

        real_layout.register(
            "java/util/regex/Pattern",
            "matcher",
            "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
            dummy_native,
        );
        assert!(real_layout
            .find(
                "java/util/regex/Pattern",
                "matcher",
                "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
            )
            .is_none());

        real_layout.register("java/util/regex/Matcher", "matches", "()Z", dummy_native);
        assert!(real_layout
            .find("java/util/regex/Matcher", "matches", "()Z")
            .is_none());
    }

    #[test]
    fn deferred_native_ring_name_flush_resolves() {
        // PERF (native-ring lazy name map): with the ring disabled (the test
        // default), `register()` records only a cheap `cb_ptr -> reg_index`
        // index and does NOT eagerly publish the name. Registration semantics
        // are unchanged (`find` works), and `flush_native_ring_names()` then
        // materializes the name on demand so a diagnostic dump resolves it.
        //
        // A dedicated, uniquely-named fn pointer is used so the resolved name
        // is unambiguous in the process-global (first-write-wins) ring name map.
        // Distinct, non-trivial body so the compiler cannot merge this with the
        // module's other `Ok(None)` dummy natives (function merging would alias
        // the `fn` pointer and make the name lookup ambiguous).
        fn ring_flush_probe_native(
            _ctx: &mut dyn NativeContext,
            args: &[Value],
        ) -> MethodCallResult {
            Ok(Some(Value::Int(0x52_49_4e_47 ^ args.len() as i32)))
        }

        let mut registry = NativeMethodRegistry::new();
        // Distinctive triple unlikely to be registered elsewhere in the suite.
        registry.register(
            "craton/test/RingFlushProbe",
            "probe",
            "()V",
            ring_flush_probe_native,
        );
        // Registration semantics preserved: the method is findable regardless
        // of the deferral.
        assert!(registry
            .find("craton/test/RingFlushProbe", "probe", "()V")
            .is_some());

        // Materialize the deferred names, then the ring can resolve the pointer.
        registry.flush_native_ring_names();
        assert_eq!(
            crate::native_ring::name_of(ring_flush_probe_native as usize).as_deref(),
            Some("craton/test/RingFlushProbe.probe()V"),
        );
    }

    // -----------------------------------------------------------------------
    // AnnotationData / AnnotationElementValue
    // -----------------------------------------------------------------------

    #[test]
    fn annotation_data_clone_and_debug() {
        let ann = AnnotationData {
            type_descriptor: "Ljava/lang/Override;".to_string(),
            elements: vec![
                ("value".to_string(), AnnotationElementValue::Int(42)),
                (
                    "name".to_string(),
                    AnnotationElementValue::StringVal("test".to_string()),
                ),
            ],
        };
        let cloned = ann.clone();
        assert_eq!(cloned.type_descriptor, "Ljava/lang/Override;");
        assert_eq!(cloned.elements.len(), 2);
        // Debug should not panic
        let _ = format!("{:?}", ann);
    }

    #[test]
    fn annotation_element_value_variants() {
        let values: Vec<AnnotationElementValue> = vec![
            AnnotationElementValue::Int(1),
            AnnotationElementValue::Long(2),
            AnnotationElementValue::Float(3.0),
            AnnotationElementValue::Double(4.0),
            AnnotationElementValue::StringVal("s".to_string()),
            AnnotationElementValue::Enum("Lp;".to_string(), "A".to_string()),
            AnnotationElementValue::Class("Lc;".to_string()),
            AnnotationElementValue::Annotation(AnnotationData {
                type_descriptor: "Linner;".to_string(),
                elements: vec![],
            }),
            AnnotationElementValue::Array(vec![AnnotationElementValue::Int(10)]),
        ];
        // All variants should be clonable and debuggable
        for v in &values {
            let _ = v.clone();
            let _ = format!("{:?}", v);
        }
        assert_eq!(values.len(), 9);
    }

    // -----------------------------------------------------------------------
    // StackTraceEntry
    // -----------------------------------------------------------------------

    #[test]
    fn stack_trace_entry_clone_and_debug() {
        let entry = StackTraceEntry {
            class_name: Arc::from("java/lang/Object"),
            method_name: Arc::from("hashCode"),
            source_file: Some(Arc::from("Object.java")),
            line_number: 42,
            byte_code_index: 17,
            class_id: None,
            method_index: Some(3),
        };
        let cloned = entry.clone();
        assert_eq!(&*cloned.class_name, "java/lang/Object");
        assert_eq!(cloned.line_number, 42);
        assert_eq!(cloned.byte_code_index, 17);
        assert_eq!(cloned.method_index, Some(3));
        let _ = format!("{:?}", entry);
    }

    #[test]
    fn stack_trace_entry_native_method() {
        let entry = StackTraceEntry {
            class_name: Arc::from("java/lang/System"),
            method_name: Arc::from("arraycopy"),
            source_file: None,
            line_number: -2, // native method
            byte_code_index: -1,
            class_id: None,
            method_index: None,
        };
        assert_eq!(entry.line_number, -2);
        assert!(entry.source_file.is_none());
        assert_eq!(entry.byte_code_index, -1);
        assert!(
            entry.method_index.is_none(),
            "a synthetic/native entry has no backing Class::methods slot"
        );
    }

    // -----------------------------------------------------------------------
    // Native-dispatch memoization: dense handles, digest verification
    // -----------------------------------------------------------------------

    /// Compare two callbacks by address. `fn`-pointer `==` is unreliable
    /// (the compiler may merge or duplicate identical functions, hence the
    /// `unpredictable_function_pointer_comparisons` lint), but `dummy_native`
    /// and `dummy_native_2` have observably different bodies, so they cannot be
    /// merged — and merging identical ones is exactly the equality we want.
    fn cb_addr(cb: NativeCallback) -> usize {
        cb as usize
    }

    #[test]
    fn handle_lookup_agrees_with_name_lookup() {
        let mut registry = NativeMethodRegistry::new();
        let triples = [
            ("java/lang/Object", "hashCode", "()I"),
            ("java/lang/System", "arraycopy", "(Ljava/lang/Object;ILjava/lang/Object;II)V"),
            ("java/lang/String", "length", "()I"),
        ];
        registry.register(triples[0].0, triples[0].1, triples[0].2, dummy_native);
        registry.register(triples[1].0, triples[1].1, triples[1].2, dummy_native_2);
        registry.register(triples[2].0, triples[2].1, triples[2].2, dummy_native);

        for (class, method, descriptor) in triples {
            let by_name = registry.find(class, method, descriptor).expect("registered");
            let id = registry
                .resolve_id(class, method, descriptor)
                .expect("handle resolves");
            let by_handle = registry.callback_of(id).expect("handle redeems");
            assert_eq!(cb_addr(by_name), cb_addr(by_handle), "{class}.{method}");
            assert_eq!(registry.triple_of(id), Some((class, method, descriptor)));
            assert_eq!(
                registry.kind_of_id(id),
                registry.kind_of(class, method, descriptor)
            );
        }

        // Misses agree too.
        assert!(registry.find("java/lang/Object", "nope", "()V").is_none());
        assert!(registry
            .resolve_id("java/lang/Object", "nope", "()V")
            .is_none());
    }

    #[test]
    fn handle_survives_later_registrations() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", dummy_native);
        let id = registry.resolve_id("a/A", "m", "()V").expect("registered");

        // Registering hundreds of unrelated natives must not move the handle.
        for i in 0..256 {
            let class = format!("filler/C{i}");
            registry.register(&class, "m", "()V", dummy_native_2);
        }
        assert_eq!(registry.resolve_id("a/A", "m", "()V"), Some(id));
        assert_eq!(
            cb_addr(registry.callback_of(id).expect("still redeems")),
            cb_addr(dummy_native as NativeCallback)
        );
        assert_eq!(registry.triple_of(id), Some(("a/A", "m", "()V")));
    }

    #[test]
    fn handle_is_stable_across_reregistration_and_picks_up_new_callback() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("a/A", "m", "()V", dummy_native);
        let id = registry.resolve_id("a/A", "m", "()V").expect("registered");
        let generation_before = registry.generation();

        // Re-registering the SAME triple must update the slot in place: the
        // handle stays valid (an already-memoized call site keeps working) and
        // now resolves to the new callback (last-registration-wins).
        registry.register("a/A", "m", "()V", dummy_native_2);
        assert_eq!(registry.resolve_id("a/A", "m", "()V"), Some(id));
        assert_eq!(
            registry.generation(),
            generation_before,
            "re-registering an existing triple must not allocate a new slot"
        );
        assert_eq!(
            cb_addr(registry.callback_of(id).expect("redeems")),
            cb_addr(dummy_native_2 as NativeCallback)
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn generation_advances_only_for_new_triples() {
        let mut registry = NativeMethodRegistry::new();
        // The absolute value is an opaque per-registry token; only the deltas
        // are contractual.
        let empty = registry.generation();
        assert_ne!(empty, 0, "generation 0 is reserved as a cache sentinel");
        registry.register("a/A", "m", "()V", dummy_native);
        let after_first = registry.generation();
        assert_ne!(after_first, empty);
        // Re-registering an existing triple must NOT move the generation.
        registry.register("a/A", "m", "()V", dummy_native_2);
        assert_eq!(registry.generation(), after_first);
        // A genuinely new triple must.
        registry.register("a/A", "other", "()V", dummy_native);
        assert_ne!(registry.generation(), after_first);
    }

    #[test]
    fn distinct_registries_do_not_share_a_generation() {
        // A `static NativeCallSite` in a test binary outlives any single VM.
        // Two registries with the SAME number of registrations must still report
        // different generations, or a memo taken against one would be accepted
        // by the other and redeem a slot index that means something else.
        let mut a = NativeMethodRegistry::new();
        let mut b = NativeMethodRegistry::new();
        assert_ne!(a.generation(), b.generation());
        a.register("a/A", "m", "()V", dummy_native);
        b.register("b/B", "m", "()V", dummy_native_2);
        assert_eq!(a.len(), b.len());
        assert_ne!(
            a.generation(),
            b.generation(),
            "equal-size registries must not alias"
        );
    }

    #[test]
    fn digest_collision_is_reported_as_a_miss_not_a_wrong_callback() {
        // Regression guard for the `class_manager::name_to_id` defect class: a
        // digest-keyed map with no name verification returns the WRONG entry on
        // a collision (silent type confusion). Inject a collision and prove the
        // full-name check fires.
        let mut registry = NativeMethodRegistry::new();
        registry.register("owner/Owner", "run", "()V", dummy_native);
        let owner_id = registry
            .resolve_id("owner/Owner", "run", "()V")
            .expect("owner registered");

        let victim = ("victim/Victim", "run", "()V");
        assert!(
            registry.find(victim.0, victim.1, victim.2).is_none(),
            "precondition: victim is not registered"
        );
        registry.inject_digest_collision_for_test(("owner/Owner", "run", "()V"), victim);

        // The colliding triple must NOT resolve to the owner's callback.
        assert!(
            registry.find(victim.0, victim.1, victim.2).is_none(),
            "full-name verification did not fire: a digest collision returned another native's callback"
        );
        assert!(registry.resolve_id(victim.0, victim.1, victim.2).is_none());
        assert!(registry.kind_of(victim.0, victim.1, victim.2).is_none());
        assert!(registry
            .find_with_kind(victim.0, victim.1, victim.2)
            .is_none());
        assert!(registry
            .find_by_key(
                NativeMethodKey::new(victim.0, victim.1, victim.2),
                victim.0,
                victim.1,
                victim.2
            )
            .is_none());

        // ...and the legitimate owner is unaffected.
        let owner_cb = registry
            .find("owner/Owner", "run", "()V")
            .expect("owner still resolves");
        assert_eq!(cb_addr(owner_cb), cb_addr(dummy_native as NativeCallback));
        assert_eq!(
            registry.resolve_id("owner/Owner", "run", "()V"),
            Some(owner_id)
        );
    }

    #[test]
    fn precomputed_key_matches_name_lookup() {
        let mut registry = NativeMethodRegistry::new();
        registry.register("java/lang/String", "length", "()I", dummy_native_2);

        let key = NativeMethodKey::new("java/lang/String", "length", "()I");
        let by_key = registry
            .find_by_key(key, "java/lang/String", "length", "()I")
            .expect("key lookup hits");
        let by_name = registry
            .find("java/lang/String", "length", "()I")
            .expect("name lookup hits");
        assert_eq!(cb_addr(by_key), cb_addr(by_name));
        assert_eq!(
            registry.resolve_id_by_key(key, "java/lang/String", "length", "()I"),
            registry.resolve_id("java/lang/String", "length", "()I")
        );
        assert_eq!(
            registry.find_with_kind_by_key(key, "java/lang/String", "length", "()I")
                .map(|(cb, kind)| (cb_addr(cb), kind)),
            registry
                .find_with_kind("java/lang/String", "length", "()I")
                .map(|(cb, kind)| (cb_addr(cb), kind))
        );

        // A key that does not describe the strings simply misses — it can never
        // hand back some other class's native.
        let wrong_key = NativeMethodKey::new("java/lang/Object", "hashCode", "()I");
        assert!(registry
            .find_by_key(wrong_key, "java/lang/String", "length", "()I")
            .is_none());
    }

    #[test]
    fn handles_resolve_through_the_descriptor_quirk_path() {
        // A memoized handle must not silently lose `find`'s compatibility
        // rewrites, or a call site that adopts handles would regress the
        // malformed-descriptor cases the quirk path exists for.
        let mut registry = NativeMethodRegistry::new();
        registry.register("q/Q", "m", "()Ljava/lang/String;", dummy_native_2);

        let quirky = "()Ljava/lang/String";  // missing trailing ';'
        let by_name = registry.find("q/Q", "m", quirky).expect("quirk rewrite hits");
        let id = registry
            .resolve_id("q/Q", "m", quirky)
            .expect("quirk rewrite yields a handle");
        assert_eq!(
            cb_addr(by_name),
            cb_addr(registry.callback_of(id).expect("redeems"))
        );
        // The handle names the triple actually registered, not the quirky input.
        assert_eq!(registry.triple_of(id), Some(("q/Q", "m", "()Ljava/lang/String;")));
    }

    #[test]
    fn kind_travels_with_the_handle() {
        let mut registry = NativeMethodRegistry::new();
        registry.with_category(NativeKind::Intrinsic, |r| {
            r.register("k/K", "fast", "()I", dummy_native);
        });
        registry.with_category(NativeKind::SyntheticStub, |r| {
            r.register("k/K", "fake", "()I", dummy_native_2);
        });
        let fast = registry.resolve_id("k/K", "fast", "()I").expect("registered");
        let fake = registry.resolve_id("k/K", "fake", "()I").expect("registered");
        assert_eq!(registry.kind_of_id(fast), Some(NativeKind::Intrinsic));
        assert_eq!(registry.kind_of_id(fake), Some(NativeKind::SyntheticStub));
        assert_eq!(
            registry
                .find_with_kind("k/K", "fake", "()I")
                .map(|(_, kind)| kind),
            Some(NativeKind::SyntheticStub)
        );
    }

    #[test]
    fn foreign_handle_does_not_panic() {
        let registry = NativeMethodRegistry::new();
        let bogus = NativeMethodId::from_u32(9_999);
        assert!(registry.callback_of(bogus).is_none());
        assert!(registry.kind_of_id(bogus).is_none());
        assert!(registry.triple_of(bogus).is_none());
    }
}
